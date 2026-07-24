use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, App, AsyncWindowContext, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, Pixels, Render, SystemNotification, WeakEntity, Window, actions, px,
};
use terminal_view::TerminalView;
use ui::{Indicator, ListItem, prelude::*};
use workspace::{
    Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use crate::status::{SessionStatus, StatusTracker};

actions!(sessions_panel, [ToggleFocus]);

pub struct SessionsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    window_handle: AnyWindowHandle,
    pub(crate) trackers: HashMap<EntityId, StatusTracker>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SessionsPanel>(window, cx);
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
                SessionsPanel {
                    workspace: weak,
                    focus_handle: cx.focus_handle(),
                    window_handle,
                    trackers: HashMap::default(),
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

        self.trackers
            .retain(|id, _| terminals.iter().any(|terminal| terminal.entity_id() == *id));

        for terminal_view in terminals {
            let foreground = terminal_view
                .read(cx)
                .terminal()
                .read(cx)
                .foreground_process_command_name();
            let tracker = self
                .trackers
                .entry(terminal_view.entity_id())
                .or_insert_with(StatusTracker::new);
            if let Some(finished) = tracker.update(foreground.as_deref(), now) {
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
            let name = std::path::Path::new(&project_root)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| project_root.clone());
            root = root.child(Label::new(name).size(LabelSize::Small).color(Color::Muted));
            for terminal_view in terminals {
                root = root.child(self.render_session(terminal_view, cx));
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
        0
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }
}
