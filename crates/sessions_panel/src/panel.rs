use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, App, AsyncWindowContext, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, Pixels, Render, Subscription, SystemNotification, WeakEntity, Window,
    actions, px,
};
use project::git_store::{GitStoreEvent, RepositoryEvent};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{CommonAnimationExt as _, Indicator, ListItem, Tooltip, prelude::*};
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use crate::status::{SessionStatus, StatusTracker, is_agent};

actions!(sessions_panel, [ToggleFocus, NewSession]);

/// Session status and unseen-activity flags shared across every workspace's
/// panel in the window. Each panel polls only its own workspace's terminals,
/// but the rail renders all workspaces' sessions — this global is how one
/// panel sees the others' state.
#[derive(Default)]
struct SharedSessionState {
    statuses: HashMap<EntityId, SessionStatus>,
    unseen: HashSet<EntityId>,
}

impl gpui::Global for SharedSessionState {}

pub struct SessionsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    window_handle: AnyWindowHandle,
    pub(crate) trackers: HashMap<EntityId, StatusTracker>,
    /// Per-terminal (last seen cursor position, when it last moved). The
    /// cursor moves whenever the terminal renders new output, so "cursor
    /// unchanged since `Instant`" is used as a cheap proxy for "no output
    /// since `Instant`" -- there's no dedicated last-output timestamp or byte
    /// counter on `Terminal` to read instead. This can under-count quiet time
    /// for output that redraws in place without moving the cursor (e.g. some
    /// spinners), which is one more reason `NeedsInput` is a heuristic, not a
    /// fact.
    activity: HashMap<EntityId, (terminal::Point, Instant)>,
    /// Repos this panel attached to the workspace because a terminal cd'd
    /// into them, and when a terminal was last seen inside each. Only these
    /// are ever auto-removed — folders the user opened are never touched.
    auto_attached_repos: HashMap<PathBuf, Instant>,
    /// cwd → enclosing repo root, so the `.git` walk runs once per cwd
    /// rather than on every poll.
    repo_root_cache: HashMap<PathBuf, Option<PathBuf>>,
    _git_subscription: Subscription,
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
            let cwd = terminal_view::default_working_directory(workspace, cx);
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
                    activity: HashMap::default(),
                    auto_attached_repos: HashMap::default(),
                    repo_root_cache: HashMap::default(),
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
    fn poll_statuses(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let terminals: Vec<Entity<TerminalView>> =
            workspace.read(cx).items_of_type::<TerminalView>(cx).collect();
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
        let active_item_id = workspace.read(cx).active_item(cx).map(|item| item.item_id());

        let live_ids: HashSet<EntityId> =
            terminals.iter().map(|terminal| terminal.entity_id()).collect();
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
        self.activity.retain(|id, _| live_ids.contains(id));

        let cwds: Vec<PathBuf> = terminals
            .iter()
            .filter_map(|terminal_view| {
                terminal_view.read(cx).terminal().read(cx).working_directory()
            })
            .collect();
        self.follow_terminal_repos(&cwds, cx);

        for terminal_view in terminals {
            let terminal = terminal_view.read(cx).terminal().read(cx);
            let foreground = terminal.foreground_process_command_name();
            let cursor = terminal.last_content().cursor.point;

            let last_activity =
                self.activity.entry(terminal_view.entity_id()).or_insert((cursor, now));
            if last_activity.0 != cursor {
                *last_activity = (cursor, now);
            }
            let quiet_for = now.saturating_duration_since(last_activity.1);
            let agent = foreground.as_deref().is_some_and(is_agent);

            let terminal_id = terminal_view.entity_id();
            let focused =
                window_active && workspace_is_active && active_item_id == Some(terminal_id);
            let previous_status = cx
                .default_global::<SharedSessionState>()
                .statuses
                .get(&terminal_id)
                .cloned();

            let tracker = self
                .trackers
                .entry(terminal_id)
                .or_insert_with(StatusTracker::new);
            let finished_command = tracker.update(foreground.as_deref(), quiet_for, agent, now);
            let new_status = tracker.status().clone();

            // Warp-style attention: a long-running agent that just went quiet
            // is done or waiting on the user. Mark the session unseen and
            // notify — agents like claude never *exit*, so the
            // command-finished path below can't cover them.
            if let (
                Some(SessionStatus::Running { command, since }),
                SessionStatus::NeedsInput { .. },
            ) = (&previous_status, &new_status)
            {
                if is_agent(command) && now.saturating_duration_since(*since) >= threshold {
                    if !focused {
                        cx.default_global::<SharedSessionState>()
                            .unseen
                            .insert(terminal_id);
                    }
                    let title = terminal_view.read(cx).terminal().read(cx).title(true);
                    if !window_active {
                        cx.show_system_notification(SystemNotification {
                            tag: format!("hive-session-{terminal_id}").into(),
                            title: format!("{command} is waiting for you").into(),
                            body: title.into(),
                            actions: Vec::new(),
                        });
                    } else if !focused && let Some(workspace) = self.workspace.upgrade() {
                        let id = NotificationId::composite::<Self>((
                            "session-needs-input",
                            terminal_id,
                        ));
                        workspace.update(cx, |workspace, cx| {
                            workspace.show_toast(
                                Toast::new(id, format!("{command} is waiting for you"))
                                    .autohide(),
                                cx,
                            );
                        });
                    }
                }
            }

            if focused {
                cx.default_global::<SharedSessionState>()
                    .unseen
                    .remove(&terminal_id);
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
                        let message =
                            format!("{} finished · {mins}m {secs}s", finished.command);
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
            let create = project
                .update(cx, |project, cx| project.create_worktree(&root, true, cx));
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
                        log::info!(
                            "sessions_panel: auto-detaching repo {}",
                            root.display()
                        );
                        project.update(cx, |project, cx| {
                            project.remove_worktree(worktree_id, cx)
                        });
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

    /// Uncommitted-changes summary `(changed_files, added_lines, deleted_lines)`
    /// summed over every repository in the workspace's project (a session may
    /// hold several auto-attached repos), or `None` when everything is clean.
    ///
    /// Sums `StatusEntry::diff_stat` (head-to-worktree, i.e. staged and
    /// unstaged combined) rather than the staged/unstaged split, since the
    /// rail shows one total per session.
    fn workspace_diff_stat(
        workspace: &Entity<Workspace>,
        cx: &App,
    ) -> Option<(usize, u32, u32)> {
        let project = workspace.read(cx).project().read(cx);
        let git_store = project.git_store().read(cx);
        let mut files = 0usize;
        let mut added = 0u32;
        let mut deleted = 0u32;
        for repository in git_store.repositories().values() {
            let repository = repository.read(cx);
            files += repository.status_summary().count;
            for entry in repository.status() {
                if let Some(diff_stat) = entry.diff_stat {
                    added += diff_stat.added;
                    deleted += diff_stat.deleted;
                }
            }
        }
        (files > 0).then_some((files, added, deleted))
    }

    fn render_session(
        &self,
        terminal_view: Entity<TerminalView>,
        workspace: Entity<Workspace>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let terminal_id = terminal_view.entity_id();
        let title = terminal_view.read(cx).terminal().read(cx).title(true);
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
            // A running agent gets a spinner; plain commands keep the dot.
            SessionStatus::Running { command, .. } if is_agent(command) => {
                Icon::new(IconName::ArrowCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Accent)
                    .with_keyed_rotate_animation(("session-spinner", terminal_id), 2)
                    .into_any_element()
            }
            SessionStatus::Running { .. } => {
                Indicator::dot().color(Color::Modified).into_any_element()
            }
            // Heuristic-only status (see status.rs) -- distinct amber color so
            // it doesn't read as the same "actively running" state.
            SessionStatus::NeedsInput { .. } => {
                Indicator::dot().color(Color::Warning).into_any_element()
            }
            // Warp-style: quiet session with activity you haven't looked at.
            SessionStatus::Idle if unseen => {
                Indicator::dot().color(Color::Accent).into_any_element()
            }
            SessionStatus::Idle => Indicator::dot().color(Color::Hidden).into_any_element(),
        };
        let activate_workspace = workspace.clone();
        let close_workspace = workspace;
        let close_terminal_view = terminal_view.clone();
        ListItem::new(("session", terminal_view.entity_id()))
            .start_slot(indicator)
            .child(Label::new(title).single_line())
            .end_slot_on_hover(
                IconButton::new(
                    ("close-session", terminal_view.entity_id()),
                    IconName::Close,
                )
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text("Close Session"))
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
        for (name, workspace, terminals) in self.session_groups(cx) {
            let is_active = active_workspace.as_ref() == Some(&workspace);
            let mut header = h_flex().gap_2().child(
                Label::new(name)
                    .size(LabelSize::Small)
                    .color(if is_active { Color::Default } else { Color::Muted }),
            );
            if let Some((files, added, deleted)) = Self::workspace_diff_stat(&workspace, cx) {
                header = header.child(
                    Label::new(format!("±{files} · +{added} −{deleted}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            let header_workspace = workspace.clone();
            root = root.child(
                ListItem::new(("session-group", workspace.entity_id()))
                    .child(header)
                    .on_click(cx.listener(move |_this, _event, window, cx| {
                        Self::activate_workspace(&header_workspace, window, cx);
                    })),
            );
            for terminal_view in terminals {
                root = root.child(self.render_session(terminal_view, workspace.clone(), cx));
            }
        }
        root
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
