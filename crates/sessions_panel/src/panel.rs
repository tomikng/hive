use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AnyWindowHandle, App, AsyncWindowContext, Context, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, Pixels, Render, Subscription, SystemNotification,
    WeakEntity, Window, actions, px,
};
use project::git_store::{GitStoreEvent, RepositoryEvent};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{Indicator, ListItem, Tooltip, prelude::*};
use util::ResultExt;
use workspace::{
    OpenOptions, OpenVisible, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use crate::file_tree::{self, DirState, TreeEntry};
use crate::status::{SessionStatus, StatusTracker, is_agent};

actions!(sessions_panel, [ToggleFocus, NewSession]);

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
    _git_subscription: Subscription,
    /// Root of the file tree rendered below the sessions list: the focused
    /// terminal's cwd, falling back to the last terminal that was focused,
    /// then the first project root. `None` when there's nothing to show it
    /// for yet (e.g. panel just opened, no terminals exist).
    tree_root: Option<PathBuf>,
    last_terminal_cwd: Option<PathBuf>,
    tree_expanded: HashSet<PathBuf>,
    tree_cache: HashMap<PathBuf, DirState>,
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
                    _git_subscription: git_subscription,
                    tree_root: None,
                    last_terminal_cwd: None,
                    tree_expanded: HashSet::default(),
                    tree_cache: HashMap::default(),
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
        self.update_tree_root(cx);

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

        self.trackers
            .retain(|id, _| terminals.iter().any(|terminal| terminal.entity_id() == *id));
        self.activity
            .retain(|id, _| terminals.iter().any(|terminal| terminal.entity_id() == *id));

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

            let tracker = self
                .trackers
                .entry(terminal_view.entity_id())
                .or_insert_with(StatusTracker::new);
            if let Some(finished) = tracker.update(foreground.as_deref(), quiet_for, agent, now) {
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

    /// All terminal items in the window's center panes, grouped by
    /// worktree root (project) path.
    fn sessions(&self, cx: &App) -> Vec<(String, Vec<Entity<TerminalView>>)> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };
        let workspace = workspace.read(cx);
        let project = workspace.project().read(cx);

        let mut groups: Vec<(String, Vec<Entity<TerminalView>>)> = project
            .visible_worktrees(cx)
            .map(|worktree| {
                let worktree = worktree.read(cx);
                (worktree.abs_path().to_string_lossy().into_owned(), Vec::new())
            })
            .collect();

        for terminal_view in workspace.items_of_type::<TerminalView>(cx) {
            let cwd = terminal_view
                .read(cx)
                .terminal()
                .read(cx)
                .working_directory();
            // ponytail: position + index instead of iter_mut().find() to sidestep
            // the borrow held across the None arm's groups.push().
            let index = cwd.as_deref().and_then(|cwd| {
                groups
                    .iter()
                    .position(|(root, _)| cwd.starts_with(root.as_str()))
            });
            match index {
                Some(index) => groups[index].1.push(terminal_view),
                None => {
                    let label = cwd
                        .map(|cwd| cwd.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "terminal".into());
                    groups.push((label, vec![terminal_view]));
                }
            }
        }
        groups
    }

    /// Uncommitted-changes summary `(changed_files, added_lines, deleted_lines)`
    /// for the repository whose work directory contains `worktree_abs_path`,
    /// or `None` when there are no uncommitted changes.
    ///
    /// Sums `StatusEntry::diff_stat` (head-to-worktree, i.e. staged and
    /// unstaged combined) rather than `staged_diff_stat`/`unstaged_diff_stat`,
    /// since the sidebar shows one total per project rather than a
    /// staged/unstaged split.
    fn project_diff_stat(&self, worktree_abs_path: &Path, cx: &App) -> Option<(usize, u32, u32)> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        let git_store = project.git_store().read(cx);
        // Match the innermost repository whose work directory contains the
        // worktree root, mirroring GitStore::set_active_repo_for_worktree.
        let repository = git_store
            .repositories()
            .values()
            .filter(|repository| {
                worktree_abs_path.starts_with(repository.read(cx).work_directory_abs_path.as_ref())
            })
            .max_by_key(|repository| repository.read(cx).work_directory_abs_path.as_os_str().len())?
            .read(cx);

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

    /// The cwd of the terminal that's the active item in the workspace's
    /// active pane, or `None` if the active item isn't a terminal at all
    /// (e.g. an editor tab is focused).
    fn focused_terminal_cwd(&self, cx: &App) -> Option<PathBuf> {
        let workspace = self.workspace.upgrade()?;
        let active_terminal = workspace.read(cx).active_item(cx)?.downcast::<TerminalView>()?;
        active_terminal.read(cx).terminal().read(cx).working_directory()
    }

    /// Re-derives the file tree's root (see the `tree_root` doc comment) and,
    /// on change, drops the stale cache/expansion state and kicks off a load
    /// of the new root's listing. Called from `poll_statuses` rather than a
    /// separate timer -- that loop already runs every 2s and already reads
    /// terminal cwds.
    fn update_tree_root(&mut self, cx: &mut Context<Self>) {
        let focused_cwd = self.focused_terminal_cwd(cx);
        if let Some(cwd) = focused_cwd.clone() {
            self.last_terminal_cwd = Some(cwd);
        }
        let new_root = focused_cwd
            .or_else(|| self.last_terminal_cwd.clone())
            .or_else(|| self.sessions(cx).into_iter().next().map(|(root, _)| PathBuf::from(root)));

        if new_root == self.tree_root {
            return;
        }
        self.tree_root = new_root.clone();
        self.tree_cache.clear();
        self.tree_expanded.clear();
        if let Some(root) = new_root {
            self.tree_expanded.insert(root.clone());
            self.ensure_dir_loaded(root, cx);
        }
    }

    /// Lazily loads one directory level in the background and caches it.
    /// No-op if a load for `path` is already in flight or already loaded --
    /// callers that want a fresh read (e.g. a root change) clear
    /// `tree_cache` first.
    fn ensure_dir_loaded(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if matches!(self.tree_cache.get(&path), Some(DirState::Loading | DirState::Loaded(_))) {
            return;
        }
        self.tree_cache.insert(path.clone(), DirState::Loading);
        cx.spawn(async move |this, cx| {
            let state = cx
                .background_spawn({
                    let path = path.clone();
                    async move { file_tree::read_dir(&path) }
                })
                .await;
            this.update(cx, |this, cx| {
                this.tree_cache.insert(path, state);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    fn toggle_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.tree_expanded.remove(&path) {
            cx.notify();
            return;
        }
        self.tree_expanded.insert(path.clone());
        self.ensure_dir_loaded(path, cx);
        cx.notify();
    }

    /// The file tree rendered below the sessions list, rooted at
    /// `self.tree_root`. `None` when there's no root yet (nothing to show).
    fn render_tree(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let root = self.tree_root.clone()?;
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        let full_path = root.to_string_lossy().into_owned();

        let mut rows = v_flex();
        for row in self.render_dir_rows(&root, 1, cx) {
            rows = rows.child(row);
        }

        Some(
            v_flex()
                .mt_2()
                .child(
                    div()
                        .id("tree-root-header")
                        .px_1()
                        .child(Label::new(name).size(LabelSize::Small).color(Color::Muted))
                        .tooltip(Tooltip::text(full_path)),
                )
                .child(rows),
        )
    }

    /// Rows for `dir`'s cached listing plus, recursively, rows for any
    /// expanded subdirectories. Reads only `tree_cache`/`tree_expanded` --
    /// loading is triggered from `toggle_dir`/`update_tree_root`, never here,
    /// so this stays a cheap, side-effect-free pass over already-fetched data.
    fn render_dir_rows(&self, dir: &Path, depth: usize, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        match self.tree_cache.get(dir) {
            Some(DirState::Loaded(listing)) => {
                for entry in &listing.entries {
                    rows.push(self.render_entry_row(entry, depth, cx).into_any_element());
                    if entry.is_dir && self.tree_expanded.contains(&entry.path) {
                        rows.extend(self.render_dir_rows(&entry.path, depth + 1, cx));
                    }
                }
                if listing.truncated {
                    rows.push(
                        ListItem::new(format!("tree-truncated-{}", dir.to_string_lossy()))
                            .indent_level(depth)
                            .child(Label::new("…").size(LabelSize::Small).color(Color::Muted))
                            .into_any_element(),
                    );
                }
            }
            Some(DirState::Loading) => {
                rows.push(
                    ListItem::new(format!("tree-loading-{}", dir.to_string_lossy()))
                        .indent_level(depth)
                        .child(Label::new("Loading…").size(LabelSize::Small).color(Color::Muted))
                        .into_any_element(),
                );
            }
            Some(DirState::Error(message)) => {
                rows.push(
                    ListItem::new(format!("tree-error-{}", dir.to_string_lossy()))
                        .indent_level(depth)
                        .child(Label::new(message.clone()).size(LabelSize::Small).color(Color::Error))
                        .into_any_element(),
                );
            }
            None => {}
        }
        rows
    }

    fn render_entry_row(&self, entry: &TreeEntry, depth: usize, cx: &Context<Self>) -> impl IntoElement {
        let path = entry.path.clone();
        let is_dir = entry.is_dir;
        let expanded = is_dir && self.tree_expanded.contains(&path);
        let icon = if is_dir {
            if expanded { IconName::FolderOpen } else { IconName::Folder }
        } else {
            IconName::File
        };
        let workspace = self.workspace.clone();

        ListItem::new(entry.path.to_string_lossy().into_owned())
            .indent_level(depth)
            .start_slot(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
            .child(Label::new(entry.name.clone()).single_line())
            .on_click(cx.listener(move |this, _event, window, cx| {
                if is_dir {
                    this.toggle_dir(path.clone(), cx);
                    return;
                }
                let Some(workspace) = workspace.upgrade() else {
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .open_abs_path(
                            path.clone(),
                            OpenOptions { visible: Some(OpenVisible::None), ..Default::default() },
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                });
            }))
    }

    fn render_session(
        &self,
        terminal_view: Entity<TerminalView>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let title = terminal_view.read(cx).terminal().read(cx).title(true);
        let status = self
            .trackers
            .get(&terminal_view.entity_id())
            .map(|tracker| tracker.status().clone())
            .unwrap_or(SessionStatus::Idle);
        let indicator = match status {
            SessionStatus::Running { .. } => Indicator::dot().color(Color::Modified),
            // Heuristic-only status (see status.rs) -- distinct amber color so
            // it doesn't read as the same "actively running" state.
            SessionStatus::NeedsInput { .. } => Indicator::dot().color(Color::Warning),
            SessionStatus::Idle => Indicator::dot().color(Color::Hidden),
        };
        let workspace = self.workspace.clone();
        ListItem::new(("session", terminal_view.entity_id()))
            .start_slot(indicator)
            .child(Label::new(title).single_line())
            .on_click(cx.listener(move |_this, _event, window, cx| {
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.activate_item(&terminal_view, true, true, window, cx);
                    });
                }
            }))
    }
}

impl Render for SessionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = v_flex().size_full().p_1();
        for (project_root, terminals) in self.sessions(cx) {
            let project_root_path = Path::new(&project_root);
            let name = project_root_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| project_root.clone());
            let mut header = h_flex()
                .gap_2()
                .child(Label::new(name).size(LabelSize::Small).color(Color::Muted));
            if let Some((files, added, deleted)) =
                self.project_diff_stat(project_root_path, cx)
            {
                header = header.child(
                    Label::new(format!("±{files} · +{added} −{deleted}"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            }
            root = root.child(header);
            for terminal_view in terminals {
                root = root.child(self.render_session(terminal_view, cx));
            }
        }
        if let Some(tree) = self.render_tree(cx) {
            root = root.child(tree);
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
        // collision). 0=agent, 1=project, 2=terminal, 3=git, 6=outline, 7=debugger.
        4
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }
}
