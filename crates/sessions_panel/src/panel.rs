use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, App, AsyncWindowContext, Context, DismissEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, Pixels, Render, Subscription, SystemNotification,
    WeakEntity, Window, actions, anchored, deferred, px,
};
use project::git_store::{GitStoreEvent, RepositoryEvent};
use terminal_view::{RenameTerminal, TerminalView, terminal_panel::TerminalPanel};
use ui::{
    CommonAnimationExt as _, ContextMenu, Indicator, ListItem, PopoverMenu, Tooltip, prelude::*,
};
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use util::ResultExt as _;

use crate::claude_settings;
use crate::status::{SessionStatus, StatusTracker, is_agent, is_claude_code};

actions!(sessions_panel, [ToggleFocus, NewSession]);

/// Session status and unseen-activity flags shared across every workspace's
/// panel in the window. Each panel polls only its own workspace's terminals,
/// but the rail renders all workspaces' sessions — this global is how one
/// panel sees the others' state.
#[derive(Default)]
struct SharedSessionState {
    statuses: HashMap<EntityId, SessionStatus>,
    unseen: HashSet<EntityId>,
    /// Sessions (by workspace) whose terminal rows are folded away. Shared for
    /// the same reason as the rest: collapsing a session must stay collapsed
    /// when you switch to another one and its panel takes over the rail.
    collapsed: HashSet<EntityId>,
}

impl gpui::Global for SharedSessionState {}

pub struct SessionsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    window_handle: AnyWindowHandle,
    pub(crate) trackers: HashMap<EntityId, StatusTracker>,
    /// Repos this panel attached to the workspace because a terminal cd'd
    /// into them, and when a terminal was last seen inside each. Only these
    /// are ever auto-removed — folders the user opened are never touched.
    auto_attached_repos: HashMap<PathBuf, Instant>,
    /// cwd → enclosing repo root, so the `.git` walk runs once per cwd
    /// rather than on every poll.
    repo_root_cache: HashMap<PathBuf, Option<PathBuf>>,
    /// Terminals we've already tried to auto-title, successful or not — one
    /// agent-CLI call per session, ever.
    naming_attempted: HashSet<EntityId>,
    /// Per-terminal subscriptions to the signals that mean "the program wants
    /// you": OSC 9 / OSC 777 notifications, and the bell. Attention flips the
    /// moment the signal arrives rather than up to one poll interval later.
    signal_subscriptions: HashMap<EntityId, Subscription>,
    /// Terminals that have emitted an OSC notification. Claude Code's
    /// `iterm2_with_bell` channel sends the notification AND rings, so once a
    /// session is known to speak OSC its bell is ignored — the notification
    /// carries better wording and would otherwise be doubled.
    osc_capable: HashSet<EntityId>,
    /// Whether this panel has already offered to turn on Claude Code's
    /// notification channel. One offer per window, ever; accepting writes the
    /// setting, after which the check that triggers it stops matching.
    claude_notify_offered: bool,
    context_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, Subscription)>,
    _git_subscription: Subscription,
}

/// A session row picked up in the rail, carried until it's dropped on another
/// row.
#[derive(Clone)]
struct DraggedSession {
    workspace: Entity<Workspace>,
    name: SharedString,
}

impl Render for DraggedSession {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_2()
            .py_1()
            .rounded_sm()
            .bg(cx.theme().colors().elevated_surface_background)
            .child(Label::new(self.name.clone()).size(LabelSize::Small))
    }
}

/// The session's root: its first visible worktree — the project this session
/// was opened as — regardless of where its terminals have wandered since.
fn session_root(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
}

/// Walks up from `path` to the enclosing git repository root. `.git` may be
/// a directory or, for linked worktrees, a file — `exists()` covers both.
fn repo_root_for(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SessionsPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &NewSession, window, cx| {
            // New terminals open at the session's root, not wherever the
            // focused terminal happens to be.
            let cwd = session_root(workspace, cx)
                .or_else(|| terminal_view::default_working_directory(workspace, cx));
            TerminalPanel::add_center_terminal(workspace, window, cx, move |project, cx| {
                project.create_terminal_shell(cwd, cx)
            })
            .detach_and_log_err(cx);
        });
    })
    .detach();
}

impl SessionsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let weak = workspace.weak_handle();
            let window_handle = window.window_handle();
            let git_store = workspace.project().read(cx).git_store().clone();
            cx.new(|cx| {
                cx.spawn(async move |this: WeakEntity<Self>, cx| {
                    loop {
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                        let Ok(()) = this.update(cx, |this, cx| this.poll_statuses(cx)) else {
                            break; // panel dropped
                        };
                    }
                })
                .detach();
                // Repaints the project header rows' diff-stats on any status change,
                // for any repository (not just the active one) since the panel lists
                // every open project. Mirrors the subscribe_in pattern used by
                // git_panel.rs's own GitStoreEvent subscription.
                let git_subscription = cx.subscribe_in(
                    &git_store,
                    window,
                    |_this, _git_store, event, _window, cx| match event {
                        GitStoreEvent::RepositoryUpdated(
                            _,
                            RepositoryEvent::StatusesChanged | RepositoryEvent::HeadChanged,
                            _,
                        )
                        | GitStoreEvent::RepositoryAdded
                        | GitStoreEvent::RepositoryRemoved(_)
                        | GitStoreEvent::ActiveRepositoryChanged(_) => {
                            cx.notify();
                        }
                        _ => {}
                    },
                );
                SessionsPanel {
                    workspace: weak,
                    focus_handle: cx.focus_handle(),
                    window_handle,
                    trackers: HashMap::default(),
                    auto_attached_repos: HashMap::default(),
                    repo_root_cache: HashMap::default(),
                    naming_attempted: HashSet::default(),
                    signal_subscriptions: HashMap::default(),
                    osc_capable: HashSet::default(),
                    claude_notify_offered: false,
                    context_menu: None,
                    _git_subscription: git_subscription,
                }
            })
        })
    }

    // ponytail: env var instead of settings plumbing; wire into SettingsContent when
    // someone actually asks to configure it in the UI
    fn notify_threshold() -> Duration {
        std::env::var("HIVE_NOTIFY_SECS")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30))
    }

    /// Reads each terminal's foreground process, feeds the per-terminal
    /// `StatusTracker`, and on a command finishing at or above the
    /// threshold, notifies the user: a native notification while the
    /// window is inactive, or an in-app toast while it's active.
    ///
    /// Polling answers "what is running", nothing more. "Does this session
    /// want me" arrives as a signal — see [`Self::on_agent_signal`].
    fn poll_statuses(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let terminals: Vec<Entity<TerminalView>> = workspace
            .read(cx)
            .items_of_type::<TerminalView>(cx)
            .collect();
        let window_active = self
            .window_handle
            .update(cx, |_, window, _| window.is_window_active())
            .unwrap_or(true);
        let now = Instant::now();
        let threshold = Self::notify_threshold();

        // Whether this panel's workspace is the window's active one — a
        // terminal only counts as "focused" (activity seen) when its
        // workspace is showing, it is the active item, and the window is
        // active.
        let workspace_is_active = workspace
            .read(cx)
            .multi_workspace()
            .and_then(|multi_workspace| multi_workspace.upgrade())
            .map(|multi_workspace| multi_workspace.read(cx).workspace() == &workspace)
            .unwrap_or(true);
        let active_item_id = workspace
            .read(cx)
            .active_item(cx)
            .map(|item| item.item_id());

        let live_ids: HashSet<EntityId> = terminals
            .iter()
            .map(|terminal| terminal.entity_id())
            .collect();
        {
            let shared = cx.default_global::<SharedSessionState>();
            for id in self.trackers.keys() {
                if !live_ids.contains(id) {
                    shared.statuses.remove(id);
                    shared.unseen.remove(id);
                }
            }
        }
        self.trackers.retain(|id, _| live_ids.contains(id));
        self.signal_subscriptions
            .retain(|id, _| live_ids.contains(id));
        self.osc_capable.retain(|id| live_ids.contains(id));

        let cwds: Vec<PathBuf> = terminals
            .iter()
            .filter_map(|terminal_view| {
                terminal_view
                    .read(cx)
                    .terminal()
                    .read(cx)
                    .working_directory()
            })
            .collect();
        self.follow_terminal_repos(&cwds, cx);

        for terminal_view in terminals {
            let terminal_id = terminal_view.entity_id();
            // Attention is signal-driven: the OSC notification or bell flips
            // the badge and notifies the moment it arrives, not on the next
            // poll. This loop only tracks what is actually running.
            self.signal_subscriptions
                .entry(terminal_id)
                .or_insert_with(|| {
                    let terminal = terminal_view.read(cx).terminal().clone();
                    let signalled = terminal_view.downgrade();
                    cx.subscribe(&terminal, move |this, _terminal, event, cx| {
                        let Some(terminal_view) = signalled.upgrade() else {
                            return;
                        };
                        match event {
                            terminal::Event::Notification(message) => {
                                this.on_agent_signal(&terminal_view, Some(message.clone()), cx);
                            }
                            terminal::Event::Bell => {
                                this.on_agent_signal(&terminal_view, None, cx);
                            }
                            _ => {}
                        }
                    })
                });

            let foreground = terminal_view
                .read(cx)
                .terminal()
                .read(cx)
                .foreground_process_command_name();

            let focused =
                window_active && workspace_is_active && active_item_id == Some(terminal_id);

            let tracker = self
                .trackers
                .entry(terminal_id)
                .or_insert_with(StatusTracker::new);
            let finished_command = tracker.update(foreground.as_deref(), now);
            if focused {
                // Looking at a session answers it.
                tracker.acknowledge();
            }
            let new_status = tracker.status().clone();

            if focused {
                cx.default_global::<SharedSessionState>()
                    .unseen
                    .remove(&terminal_id);
            }

            if foreground.as_deref().is_some_and(is_claude_code) {
                self.offer_claude_notifications(cx);
            }

            // Auto-title agent sessions: once the agent has run long enough
            // for the transcript to show what it's doing, ask the agent CLI
            // for a short label. One attempt per terminal, and never over a
            // title the user set themselves.
            if let SessionStatus::Running { command, since } = &new_status
                && is_agent(command)
                && terminal_view.read(cx).custom_title().is_none()
                && now.saturating_duration_since(*since) >= Duration::from_secs(20)
                && self.naming_attempted.insert(terminal_id)
            {
                let transcript = terminal_view
                    .read(cx)
                    .terminal()
                    .read(cx)
                    .last_n_non_empty_lines(30)
                    .join("\n");
                let prompt = format!(
                    "This is a snippet of a coding agent session transcript:\n\n{transcript}\n\n\
                     Reply with ONLY a 2-4 word title describing what this session is working \
                     on. No quotes, no punctuation, no explanation."
                );
                let generate = git_ui::branch_namer::generate_agent_text(prompt, cx);
                let terminal_view = terminal_view.clone();
                cx.spawn(async move |_, cx| {
                    if let Some(text) = generate.await {
                        let mut title = text.lines().next().unwrap_or_default().trim().to_string();
                        title.truncate(40);
                        if !title.is_empty() {
                            terminal_view.update(cx, |terminal_view, cx| {
                                terminal_view.set_custom_title(Some(title), cx);
                            });
                        }
                    }
                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }

            cx.default_global::<SharedSessionState>()
                .statuses
                .insert(terminal_id, new_status);

            if let Some(finished) = finished_command {
                if !focused {
                    cx.default_global::<SharedSessionState>()
                        .unseen
                        .insert(terminal_id);
                }
                if finished.duration >= threshold {
                    let mins = finished.duration.as_secs() / 60;
                    let secs = finished.duration.as_secs() % 60;
                    let title = terminal_view.read(cx).terminal().read(cx).title(true);
                    if !window_active {
                        cx.show_system_notification(SystemNotification {
                            tag: format!("hive-session-{}", terminal_view.entity_id()).into(),
                            title: format!("{} finished", finished.command).into(),
                            body: format!("{title} · {mins}m {secs}s").into(),
                            actions: Vec::new(),
                        });
                    } else if let Some(workspace) = self.workspace.upgrade() {
                        let id = NotificationId::composite::<Self>((
                            "session-finished",
                            terminal_view.entity_id(),
                        ));
                        let message = format!("{} finished · {mins}m {secs}s", finished.command);
                        workspace.update(cx, |workspace, cx| {
                            workspace.show_toast(Toast::new(id, message).autohide(), cx);
                        });
                    }
                }
            }
        }
        cx.notify(); // re-render badges
    }

    // ponytail: env var opt-out like notify_threshold; wire into
    // SettingsContent when someone asks to configure it in the UI
    fn auto_follow_enabled() -> bool {
        std::env::var("HIVE_NO_AUTO_REPO").is_err()
    }

    /// A program in `terminal_view` said it wants the user. `message` is the
    /// wording it sent over OSC 9 / OSC 777; `None` means it only rang the
    /// bell.
    ///
    /// This is the ONLY route to `NeedsInput`. Hive used to infer it from a
    /// terminal whose visible content had not changed for four seconds, which
    /// reported every long think, tool call and slow build as "waiting for
    /// you". Silence is not a signal — being told is.
    fn on_agent_signal(
        &mut self,
        terminal_view: &Entity<TerminalView>,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let terminal_id = terminal_view.entity_id();
        let foreground = terminal_view
            .read(cx)
            .terminal()
            .read(cx)
            .foreground_process_command_name();

        // A turn starting is the one signal that isn't about the user: it says
        // the agent has work in hand, so it moves the indicator and stops.
        if let Some(crate::status::TurnMarker::Started) =
            message.as_deref().and_then(crate::status::turn_marker)
        {
            let Some(tracker) = self.trackers.get_mut(&terminal_id) else {
                return;
            };
            tracker.signal_turn_started();
            let status = tracker.status().clone();
            cx.default_global::<SharedSessionState>()
                .statuses
                .insert(terminal_id, status);
            cx.notify();
            return;
        }

        let message = match message {
            Some(message) => {
                // This session speaks OSC, so its bell stops counting: Claude
                // Code's `iterm2_with_bell` channel sends both, and the
                // notification carries the better wording.
                self.osc_capable.insert(terminal_id);
                // A turn ending means the agent wants the user, and is the
                // only signal that arrives for every such moment — the
                // notification channel stays quiet for a question asked
                // mid-turn. Both may fire; they share a notification id, so
                // the second replaces the first rather than stacking.
                match crate::status::turn_marker(&message) {
                    Some(_) => format!(
                        "{} is waiting for you",
                        foreground.as_deref().unwrap_or("The agent")
                    ),
                    None => message,
                }
            }
            None => {
                if self.osc_capable.contains(&terminal_id) {
                    return;
                }
                // A bell only means "your turn" from a known agent — shells
                // ring it for tab-completion misses and every stray key.
                let Some(command) = foreground.as_deref().filter(|name| is_agent(name)) else {
                    return;
                };
                format!("{command} is waiting for you")
            }
        };

        let Some(tracker) = self.trackers.get_mut(&terminal_id) else {
            return; // first poll hasn't seen this terminal yet
        };
        tracker.signal_needs_input();
        let status = tracker.status().clone();
        cx.default_global::<SharedSessionState>()
            .statuses
            .insert(terminal_id, status);

        let window_active = self.window_active(cx);
        if !self.session_focused(&workspace, terminal_id, cx) {
            cx.default_global::<SharedSessionState>()
                .unseen
                .insert(terminal_id);
            let session = terminal_view.read(cx).terminal().read(cx).title(true);
            self.notify_session_activity(
                terminal_view,
                &workspace,
                window_active,
                "session-needs-input",
                message,
                session,
                cx,
            );
        }
        cx.notify();
    }

    /// Claude Code stays silent in Hive until its notification channel is set
    /// (see [`crate::claude_settings`]), so the first time an agent runs, offer
    /// to switch it on. Once per window, and never when the user has already
    /// chosen a channel.
    fn offer_claude_notifications(&mut self, cx: &mut Context<Self>) {
        if self.claude_notify_offered {
            return;
        }
        self.claude_notify_offered = true;
        cx.spawn(async move |this, cx| {
            let configured = cx
                .background_executor()
                .spawn(async { claude_settings::notifications_already_configured() })
                .await;
            if configured {
                return;
            }
            this.update(cx, |this, cx| {
                let Some(toast_target) = this.active_workspace(cx) else {
                    return;
                };
                let id = NotificationId::composite::<Self>("claude-notifications");
                toast_target.update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(
                            id,
                            "Claude Code can't tell Hive when it's working or when it needs you.",
                        )
                        .on_click("Enable", {
                            let toast_target = toast_target.downgrade();
                            move |_window, cx| {
                                // Say what happened: the write is invisible,
                                // and Claude Code reads its settings once at
                                // startup, so nothing changes in the session
                                // that prompted this.
                                let toast_target = toast_target.clone();
                                cx.spawn(async move |cx| {
                                    let wrote = cx
                                        .background_executor()
                                        .spawn(async {
                                            claude_settings::enable_notifications().log_err()
                                        })
                                        .await;
                                    let message = if wrote.is_some() {
                                        "Claude Code will report its turns to Hive once you restart it."
                                    } else {
                                        "Hive could not write Claude Code's settings — see the log."
                                    };
                                    toast_target
                                        .update(cx, |workspace, cx| {
                                            let id = NotificationId::composite::<Self>(
                                                "claude-notifications-result",
                                            );
                                            workspace
                                                .show_toast(Toast::new(id, message).autohide(), cx);
                                        })
                                        .ok();
                                })
                                .detach();
                            }
                        }),
                        cx,
                    );
                });
            })
            .ok();
        })
        .detach();
    }

    /// The workspace whose tab strip is on screen. Toasts render per-workspace,
    /// so one shown on a background session is invisible until the user
    /// happens to switch there.
    fn active_workspace(&self, cx: &App) -> Option<Entity<Workspace>> {
        let workspace = self.workspace.upgrade()?;
        Some(
            workspace
                .read(cx)
                .multi_workspace()
                .and_then(|multi_workspace| multi_workspace.upgrade())
                .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
                .unwrap_or(workspace),
        )
    }

    fn window_active(&self, cx: &mut Context<Self>) -> bool {
        self.window_handle
            .update(cx, |_, window, _| window.is_window_active())
            .unwrap_or(true)
    }

    /// Whether the user is looking at this session right now: the window has
    /// focus, this panel's workspace is the one on screen, and the terminal is
    /// its active item.
    fn session_focused(
        &self,
        workspace: &Entity<Workspace>,
        terminal_id: EntityId,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.window_active(cx) {
            return false;
        }
        let workspace_is_active = workspace
            .read(cx)
            .multi_workspace()
            .and_then(|multi_workspace| multi_workspace.upgrade())
            .map(|multi_workspace| multi_workspace.read(cx).workspace() == workspace)
            .unwrap_or(true);
        workspace_is_active
            && workspace
                .read(cx)
                .active_item(cx)
                .map(|item| item.item_id())
                == Some(terminal_id)
    }

    fn auto_detach_after() -> Duration {
        std::env::var("HIVE_AUTO_DETACH_SECS")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30))
    }

    /// Keeps the workspace's folders in sync with where the terminals are.
    /// A terminal whose cwd is inside a git repo the project doesn't cover
    /// attaches that repo as a visible worktree, so the git UI (diffs,
    /// stashes, history) works there without an explicit "open as project".
    /// A repo only Hive attached is removed again once no terminal has been
    /// inside it for [`Self::auto_detach_after`] and none of its files are
    /// open in a pane.
    fn follow_terminal_repos(&mut self, cwds: &[PathBuf], cx: &mut Context<Self>) {
        if !Self::auto_follow_enabled() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let project = workspace.read(cx).project().clone();
        if !project.read(cx).is_local() {
            return;
        }
        let now = Instant::now();

        self.repo_root_cache.retain(|cwd, _| cwds.contains(cwd));
        for cwd in cwds {
            // A terminal's cwd can be sampled mid-spawn as an empty/relative
            // path; `"".join(".git")` would then resolve against the *app
            // process's* cwd and attach garbage (seen live: attached "").
            if !cwd.is_absolute() {
                continue;
            }
            let root = self
                .repo_root_cache
                .entry(cwd.clone())
                .or_insert_with(|| repo_root_for(cwd))
                .clone();
            let Some(root) = root else { continue };
            if !root.is_absolute() {
                continue;
            }
            if let Some(last_seen) = self.auto_attached_repos.get_mut(&root) {
                *last_seen = now;
                continue;
            }
            // Never attach the home directory: scanning it is expensive and
            // almost certainly not what a `cd ~` meant.
            if root.as_path() == util::paths::home_dir().as_path() {
                continue;
            }
            if project
                .read(cx)
                .project_path_for_absolute_path(cwd, cx)
                .is_some()
            {
                continue;
            }
            log::info!("sessions_panel: auto-attaching repo {}", root.display());
            self.auto_attached_repos.insert(root.clone(), now);
            let create = project.update(cx, |project, cx| project.create_worktree(&root, true, cx));
            let workspace = self.workspace.clone();
            let window_handle = self.window_handle;
            cx.spawn(async move |_, cx| {
                create.await?;
                // Freshly attached worktrees start restricted (git and tasks
                // disabled) — surface the standard trust prompt, once per
                // repo; trusting persists.
                window_handle.update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.show_worktree_trust_security_modal(false, window, cx);
                    })
                })?
            })
            .detach_and_log_err(cx);
        }

        let detach_after = Self::auto_detach_after();
        let mut detached = Vec::new();
        for (root, last_seen) in &self.auto_attached_repos {
            if cwds.iter().any(|cwd| cwd.starts_with(root)) {
                continue;
            }
            if now.saturating_duration_since(*last_seen) < detach_after {
                continue;
            }
            let worktree_id = project.read(cx).worktrees(cx).find_map(|worktree| {
                let worktree = worktree.read(cx);
                (worktree.abs_path().as_ref() == root.as_path()).then(|| worktree.id())
            });
            match worktree_id {
                // The user already removed (or attach failed) — just forget it.
                None => detached.push(root.clone()),
                Some(worktree_id) => {
                    let has_open_items = workspace.read(cx).items(cx).any(|item| {
                        item.project_path(cx)
                            .is_some_and(|path| path.worktree_id == worktree_id)
                    });
                    if !has_open_items {
                        log::info!("sessions_panel: auto-detaching repo {}", root.display());
                        project.update(cx, |project, cx| project.remove_worktree(worktree_id, cx));
                        detached.push(root.clone());
                    }
                }
            }
        }
        for root in detached {
            self.auto_attached_repos.remove(&root);
        }
    }

    /// One rail group per workspace in the window (a session = a workspace
    /// with its own tabs). Falls back to just this panel's workspace when the
    /// window has no MultiWorkspace.
    fn session_groups(
        &self,
        cx: &App,
    ) -> Vec<(String, Entity<Workspace>, Vec<Entity<TerminalView>>)> {
        let Some(own_workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        Self::session_groups_in(&own_workspace, cx)
    }

    fn session_groups_in(
        own_workspace: &Entity<Workspace>,
        cx: &App,
    ) -> Vec<(String, Entity<Workspace>, Vec<Entity<TerminalView>>)> {
        let workspaces: Vec<Entity<Workspace>> = own_workspace
            .read(cx)
            .multi_workspace()
            .and_then(|multi_workspace| multi_workspace.upgrade())
            .map(|multi_workspace| multi_workspace.read(cx).workspaces().cloned().collect())
            .unwrap_or_else(|| vec![own_workspace.clone()]);

        workspaces
            .into_iter()
            .map(|workspace| {
                let terminals: Vec<Entity<TerminalView>> = workspace
                    .read(cx)
                    .items_of_type::<TerminalView>(cx)
                    .collect();
                let label = workspace
                    .read(cx)
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .next()
                    .map(|worktree| {
                        let path = worktree.read(cx).abs_path();
                        path.file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.to_string_lossy().into_owned())
                    })
                    .or_else(|| {
                        terminals.first().and_then(|terminal_view| {
                            terminal_view
                                .read(cx)
                                .terminal()
                                .read(cx)
                                .working_directory()
                                .and_then(|cwd| {
                                    cwd.file_name()
                                        .map(|name| name.to_string_lossy().into_owned())
                                })
                        })
                    })
                    .unwrap_or_else(|| "session".into());
                (label, workspace, terminals)
            })
            .collect()
    }

    /// The innermost repository whose work directory contains `path`, from
    /// the workspace's project.
    fn repository_containing(
        workspace: &Workspace,
        path: &Path,
        cx: &App,
    ) -> Option<Entity<project::git_store::Repository>> {
        let git_store = workspace.project().read(cx).git_store().read(cx);
        git_store
            .repositories()
            .values()
            .filter(|repository| {
                path.starts_with(repository.read(cx).work_directory_abs_path.as_ref())
            })
            .max_by_key(|repository| {
                repository
                    .read(cx)
                    .work_directory_abs_path
                    .as_os_str()
                    .len()
            })
            .cloned()
    }

    /// Uncommitted-changes summary `(changed_files, added_lines, deleted_lines)`
    /// for one repository, or `None` when it's clean.
    ///
    /// Sums `StatusEntry::diff_stat` (head-to-worktree, i.e. staged and
    /// unstaged combined) rather than the staged/unstaged split, since the
    /// rail shows one total.
    fn repository_diff_stat(
        repository: &Entity<project::git_store::Repository>,
        cx: &App,
    ) -> Option<(usize, u32, u32)> {
        let repository = repository.read(cx);
        let summary = repository.status_summary();
        if summary.count == 0 {
            return None;
        }
        let (added, deleted) = repository
            .status()
            .filter_map(|entry| entry.diff_stat)
            .fold((0u32, 0u32), |(added, deleted), diff_stat| {
                (added + diff_stat.added, deleted + diff_stat.deleted)
            });
        Some((summary.count, added, deleted))
    }

    fn render_session(
        &self,
        terminal_view: Entity<TerminalView>,
        workspace: Entity<Workspace>,
        session_root_repo: Option<&Path>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let terminal_id = terminal_view.entity_id();
        let title = terminal_view.read(cx).terminal().read(cx).title(true);
        // A terminal sitting in a different repo than the session root shows
        // that repo's diff on its own row — "where this terminal is now".
        let foreign_repo_stat = terminal_view
            .read(cx)
            .terminal()
            .read(cx)
            .working_directory()
            .and_then(|cwd| Self::repository_containing(workspace.read(cx), &cwd, cx))
            .filter(|repository| {
                session_root_repo != Some(repository.read(cx).work_directory_abs_path.as_ref())
            })
            .and_then(|repository| Self::repository_diff_stat(&repository, cx));
        let (status, unseen) = cx
            .try_global::<SharedSessionState>()
            .map(|shared| {
                (
                    shared
                        .statuses
                        .get(&terminal_id)
                        .cloned()
                        .unwrap_or(SessionStatus::Idle),
                    shared.unseen.contains(&terminal_id),
                )
            })
            .unwrap_or((SessionStatus::Idle, false));
        let indicator = match &status {
            // Only a turn in flight spins. An agent whose TUI is merely open
            // is waiting on you, and used to spin forever.
            SessionStatus::Working { .. } => Icon::new(IconName::ArrowCircle)
                .size(IconSize::XSmall)
                .color(Color::Accent)
                .with_keyed_rotate_animation(("session-spinner", terminal_id), 2)
                .into_any_element(),
            SessionStatus::Running { command, .. } if is_agent(command) => {
                Icon::new(IconName::Check)
                    .size(IconSize::XSmall)
                    .color(Color::Success)
                    .into_any_element()
            }
            SessionStatus::Running { .. } => {
                Indicator::dot().color(Color::Modified).into_any_element()
            }
            // The session asked for you (see status.rs) -- a green tick reads
            // as "your turn", where a dot read as another shade of running.
            SessionStatus::NeedsInput { .. } => Icon::new(IconName::Check)
                .size(IconSize::XSmall)
                .color(Color::Success)
                .into_any_element(),
            // Warp-style: quiet session with activity you haven't looked at.
            SessionStatus::Idle if unseen => {
                Indicator::dot().color(Color::Accent).into_any_element()
            }
            SessionStatus::Idle => Indicator::dot().color(Color::Hidden).into_any_element(),
        };
        let activate_workspace = workspace.clone();
        let menu_workspace = workspace.clone();
        let menu_terminal_view = terminal_view.clone();
        let close_workspace = workspace;
        let close_terminal_view = terminal_view.clone();
        ListItem::new(("session", terminal_view.entity_id()))
            .start_slot(indicator)
            .on_secondary_mouse_down(cx.listener(
                move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.deploy_terminal_context_menu(
                        menu_terminal_view.clone(),
                        menu_workspace.clone(),
                        event.position,
                        window,
                        cx,
                    );
                },
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(title).single_line())
                    .when_some(foreign_repo_stat, |this, (files, added, deleted)| {
                        this.child(
                            Label::new(format!("±{files} +{added} −{deleted}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .end_slot_on_hover(
                IconButton::new(
                    ("close-session", terminal_view.entity_id()),
                    IconName::Close,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Close Terminal"))
                .on_click(cx.listener(move |_this, _event, window, cx| {
                    cx.stop_propagation();
                    close_workspace.update(cx, |workspace, cx| {
                        if let Some(pane) = workspace.pane_for(&close_terminal_view) {
                            pane.update(cx, |pane, cx| {
                                pane.close_item_by_id(
                                    close_terminal_view.entity_id(),
                                    workspace::SaveIntent::Close,
                                    window,
                                    cx,
                                )
                            })
                            .detach_and_log_err(cx);
                        }
                    });
                })),
            )
            .on_click(cx.listener(move |_this, _event, window, cx| {
                cx.default_global::<SharedSessionState>()
                    .unseen
                    .remove(&terminal_view.entity_id());
                Self::activate_workspace(&activate_workspace, window, cx);
                activate_workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(&terminal_view, true, true, window, cx);
                });
            }))
    }

    /// Right-click menu for a terminal row.
    ///
    /// Rename hands off to the terminal's own rename flow, which draws its
    /// editor in the tab — so the session is activated first, otherwise the
    /// rename box would open on a tab strip you can't see.
    fn deploy_terminal_context_menu(
        &mut self,
        terminal_view: Entity<TerminalView>,
        workspace: Entity<Workspace>,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let context_menu = ContextMenu::build(window, cx, |menu, _window, _cx| {
            menu.entry("Rename Terminal", None, {
                let terminal_view = terminal_view.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    Self::activate_workspace(&workspace, window, cx);
                    workspace.update(cx, |workspace, cx| {
                        workspace.activate_item(&terminal_view, true, true, window, cx);
                    });
                    terminal_view.update(cx, |terminal_view, cx| {
                        terminal_view.rename_terminal(&RenameTerminal, window, cx);
                    });
                }
            })
            .entry("New Terminal in Session", None, {
                let workspace = workspace.clone();
                move |window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        let cwd = session_root(workspace, cx)
                            .or_else(|| terminal_view::default_working_directory(workspace, cx));
                        TerminalPanel::add_center_terminal(
                            workspace,
                            window,
                            cx,
                            move |project, cx| project.create_terminal_shell(cwd, cx),
                        )
                        .detach_and_log_err(cx);
                    });
                }
            })
            .separator()
            .entry("Close Terminal", None, {
                let terminal_view = terminal_view.clone();
                let workspace = workspace.clone();
                move |window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        if let Some(pane) = workspace.pane_for(&terminal_view) {
                            pane.update(cx, |pane, cx| {
                                pane.close_item_by_id(
                                    terminal_view.entity_id(),
                                    workspace::SaveIntent::Close,
                                    window,
                                    cx,
                                )
                            })
                            .detach_and_log_err(cx);
                        }
                    });
                }
            })
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription =
            cx.subscribe_in(&context_menu, window, |this, _, _: &DismissEvent, _, cx| {
                this.context_menu.take();
                cx.notify();
            });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    /// Notifies about activity in a session: always a system notification
    /// (macOS shows banners even for the frontmost app), plus — while the
    /// window is active — an in-app toast with a jump-to-session action.
    /// The toast is shown on the window's ACTIVE workspace: toasts render
    /// per-workspace, so one shown on a background session would be
    /// invisible until the user happened to switch there.
    fn notify_session_activity(
        &self,
        terminal_view: &Entity<TerminalView>,
        session_workspace: &Entity<Workspace>,
        window_active: bool,
        tag_scope: &'static str,
        message: String,
        body: String,
        cx: &mut Context<Self>,
    ) {
        let terminal_id = terminal_view.entity_id();
        cx.show_system_notification(SystemNotification {
            tag: format!("hive-session-{terminal_id}").into(),
            title: message.clone().into(),
            body: body.into(),
            actions: Vec::new(),
        });

        if window_active {
            let toast_target = session_workspace
                .read(cx)
                .multi_workspace()
                .and_then(|multi_workspace| multi_workspace.upgrade())
                .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
                .unwrap_or_else(|| session_workspace.clone());
            let open_workspace = session_workspace.clone();
            let open_terminal = terminal_view.clone();
            let id = NotificationId::composite::<Self>((tag_scope, terminal_id));
            toast_target.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(id, message)
                        .on_click("Open Session", move |window, cx| {
                            Self::activate_workspace(&open_workspace, window, cx);
                            open_workspace.update(cx, |workspace, cx| {
                                workspace.activate_item(&open_terminal, true, true, window, cx);
                            });
                        })
                        .autohide(),
                    cx,
                );
            });
        }
    }

    /// Moves the dragged session to `target`'s place in the rail. The order is
    /// the window's retained-workspace order, so it lasts as long as the
    /// window does but isn't serialized.
    fn move_session(
        &self,
        dragged: &Entity<Workspace>,
        target: &Entity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        let Some(multi_workspace) = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).multi_workspace())
            .and_then(|multi_workspace| multi_workspace.upgrade())
        else {
            return;
        };
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.move_workspace(dragged, target, cx);
        });
        cx.notify();
    }

    /// Makes `workspace` the window's active workspace (swapping the whole
    /// tab strip to that session), if it isn't already.
    fn activate_workspace(workspace: &Entity<Workspace>, window: &mut Window, cx: &mut App) {
        let Some(multi_workspace) = workspace
            .read(cx)
            .multi_workspace()
            .and_then(|multi_workspace| multi_workspace.upgrade())
        else {
            return;
        };
        if multi_workspace.read(cx).workspace() != workspace {
            multi_workspace.update(cx, |multi_workspace, cx| {
                multi_workspace.activate(workspace.clone(), None, window, cx);
            });
        }
    }
}

impl Render for SessionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active_workspace = self.workspace.upgrade().and_then(|workspace| {
            workspace
                .read(cx)
                .multi_workspace()
                .and_then(|multi_workspace| multi_workspace.upgrade())
                .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
                .or(Some(workspace))
        });

        let mut root = v_flex().size_full().p_1();

        // Header: panel label plus the two creation entry points — a new
        // terminal inside the current session, and a new session (project).
        root = root.child(
            h_flex()
                .px_1()
                .pb_1()
                .justify_between()
                .child(
                    Label::new("Sessions")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child({
                            let unseen_count = cx
                                .try_global::<SharedSessionState>()
                                .map(|shared| shared.unseen.len())
                                .unwrap_or(0);
                            let menu_workspace = self.workspace.clone();
                            h_flex()
                                .child(
                                    PopoverMenu::new("session-notifications")
                                        .trigger(
                                            IconButton::new(
                                                "session-notifications-bell",
                                                IconName::Bell,
                                            )
                                            .icon_size(IconSize::Small)
                                            .icon_color(if unseen_count > 0 {
                                                Color::Accent
                                            } else {
                                                Color::Muted
                                            }),
                                        )
                                        .menu(move |window, cx| {
                                            let own = menu_workspace.upgrade()?;
                                            let unseen = cx
                                                .try_global::<SharedSessionState>()
                                                .map(|shared| shared.unseen.clone())
                                                .unwrap_or_default();
                                            let mut entries = Vec::new();
                                            for (name, workspace, terminals) in
                                                Self::session_groups_in(&own, cx)
                                            {
                                                for terminal_view in terminals {
                                                    if unseen
                                                        .contains(&terminal_view.entity_id())
                                                    {
                                                        let title = terminal_view
                                                            .read(cx)
                                                            .terminal()
                                                            .read(cx)
                                                            .title(true);
                                                        entries.push((
                                                            format!("{name} · {title}"),
                                                            workspace.clone(),
                                                            terminal_view,
                                                        ));
                                                    }
                                                }
                                            }
                                            Some(ContextMenu::build(
                                                window,
                                                cx,
                                                |mut menu, _window, _cx| {
                                                    if entries.is_empty() {
                                                        menu = menu.header("No new activity");
                                                    } else {
                                                        menu = menu.header("Finished sessions");
                                                        for (label, workspace, terminal_view) in
                                                            entries
                                                        {
                                                            menu = menu.entry(
                                                                label,
                                                                None,
                                                                move |window, cx| {
                                                                    cx.default_global::<SharedSessionState>()
                                                                        .unseen
                                                                        .remove(&terminal_view.entity_id());
                                                                    Self::activate_workspace(
                                                                        &workspace, window, cx,
                                                                    );
                                                                    workspace.update(cx, |workspace, cx| {
                                                                        workspace.activate_item(
                                                                            &terminal_view,
                                                                            true,
                                                                            true,
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    });
                                                                },
                                                            );
                                                        }
                                                    }
                                                    menu
                                                },
                                            ))
                                        }),
                                )
                                .when(unseen_count > 0, |this| {
                                    this.child(
                                        Label::new(unseen_count.to_string())
                                            .size(LabelSize::XSmall)
                                            .color(Color::Accent),
                                    )
                                })
                        })
                        .child(
                            IconButton::new("new-terminal-in-session", IconName::Terminal)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(|_window, cx| {
                                    Tooltip::for_action(
                                        "New Terminal in Session",
                                        &NewSession,
                                        cx,
                                    )
                                })
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    let Some(workspace) = this.workspace.upgrade() else {
                                        return;
                                    };
                                    workspace.update(cx, |workspace, cx| {
                                        let cwd = session_root(workspace, cx).or_else(|| {
                                            terminal_view::default_working_directory(
                                                workspace, cx,
                                            )
                                        });
                                        TerminalPanel::add_center_terminal(
                                            workspace,
                                            window,
                                            cx,
                                            move |project, cx| {
                                                project.create_terminal_shell(cwd, cx)
                                            },
                                        )
                                        .detach_and_log_err(cx);
                                    });
                                })),
                        )
                        .child(
                            IconButton::new("new-session", IconName::Plus)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(|_window, cx| {
                                    Tooltip::for_action(
                                        "New Session (Open Project)",
                                        &zed_actions::OpenRecent::default(),
                                        cx,
                                    )
                                })
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    let Some(workspace) = this.workspace.upgrade() else {
                                        return;
                                    };
                                    workspace
                                        .read(cx)
                                        .focus_handle(cx)
                                        .dispatch_action(
                                            &zed_actions::OpenRecent::default(),
                                            window,
                                            cx,
                                        );
                                })),
                        ),
                ),
        );

        for (name, workspace, terminals) in self.session_groups(cx) {
            let is_active = active_workspace.as_ref() == Some(&workspace);
            // The header stat is anchored to the session's ROOT repo. Repos
            // that terminals wandered into via auto-attach are shown on the
            // terminal rows instead, not summed here.
            let root_repository = session_root(workspace.read(cx), cx)
                .and_then(|root| Self::repository_containing(workspace.read(cx), &root, cx));
            let root_repo_path = root_repository
                .as_ref()
                .map(|repository| repository.read(cx).work_directory_abs_path.to_path_buf());
            let mut header =
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .color(if is_active {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                    );
            if let Some((files, added, deleted)) = root_repository
                .as_ref()
                .and_then(|repository| Self::repository_diff_stat(repository, cx))
            {
                header = header.child(
                    Label::new(format!("±{files} · +{added} −{deleted}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            let session_id = workspace.entity_id();
            let collapsed = cx
                .try_global::<SharedSessionState>()
                .is_some_and(|shared| shared.collapsed.contains(&session_id));
            let header_workspace = workspace.clone();
            let close_session_workspace = workspace.clone();
            let dragged_session = DraggedSession {
                workspace: workspace.clone(),
                name: name.into(),
            };
            let drop_target = workspace.clone();
            root = root.child(
                // The click sits on the same element as the drag: starting a
                // drag only clears the pending click of the element that owns
                // the drag listener, so a click left on a child would still
                // fire when a drag ended over its own row.
                div()
                    .id(("session-group-drag", workspace.entity_id()))
                    .cursor_pointer()
                    .on_click(cx.listener(move |_this, _event, window, cx| {
                        Self::activate_workspace(&header_workspace, window, cx);
                    }))
                    .on_drag(dragged_session, |session, _offset, _window, cx| {
                        cx.new(|_| session.clone())
                    })
                    .drag_over::<DraggedSession>(|style, _, _, cx| {
                        style.bg(cx.theme().colors().drop_target_background)
                    })
                    .on_drop(
                        cx.listener(move |this, dragged: &DraggedSession, _window, cx| {
                            this.move_session(&dragged.workspace, &drop_target, cx);
                        }),
                    )
                    .child(
                        ListItem::new(("session-group", workspace.entity_id()))
                            .toggle(!collapsed)
                            .on_toggle(cx.listener(move |_this, _event, _window, cx| {
                                cx.stop_propagation();
                                let collapsed =
                                    &mut cx.default_global::<SharedSessionState>().collapsed;
                                if !collapsed.remove(&session_id) {
                                    collapsed.insert(session_id);
                                }
                                cx.notify();
                            }))
                            .child(header)
                            .end_slot_on_hover(
                                IconButton::new(
                                    ("close-session-group", workspace.entity_id()),
                                    IconName::Close,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Close Session"))
                                .on_click(cx.listener(
                                    move |_this, _event, window, cx| {
                                        cx.stop_propagation();
                                        let Some(multi_workspace) = close_session_workspace
                                            .read(cx)
                                            .multi_workspace()
                                            .and_then(|multi_workspace| multi_workspace.upgrade())
                                        else {
                                            return;
                                        };
                                        multi_workspace
                                            .update(cx, |multi_workspace, cx| {
                                                multi_workspace.close_workspace(
                                                    &close_session_workspace,
                                                    window,
                                                    cx,
                                                )
                                            })
                                            .detach_and_log_err(cx);
                                    },
                                )),
                            ),
                    ),
            );
            if collapsed {
                continue;
            }
            for terminal_view in terminals {
                root = root.child(self.render_session(
                    terminal_view,
                    workspace.clone(),
                    root_repo_path.as_deref(),
                    cx,
                ));
            }
        }
        root.children(self.context_menu.as_ref().map(|(menu, position, _)| {
            deferred(
                anchored()
                    .position(*position)
                    .anchor(gpui::Anchor::TopLeft)
                    .child(menu.clone()),
            )
            .with_priority(1)
        }))
    }
}

impl EventEmitter<PanelEvent> for SessionsPanel {}

impl Focusable for SessionsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SessionsPanel {
    fn persistent_name() -> &'static str {
        "SessionsPanel"
    }

    fn panel_key() -> &'static str {
        "SessionsPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::Terminal)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Sessions Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Must be globally unique across all panels (Zed panics in debug builds on a
        // collision). 0=agent, 1=project, 2=terminal, 3=git, 5=file tree, 6=outline,
        // 7=debugger.
        4
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }
}
