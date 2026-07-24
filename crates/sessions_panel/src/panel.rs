use std::collections::HashMap;

use gpui::{
    App, AsyncWindowContext, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    Pixels, Render, WeakEntity, Window, actions, px,
};
use terminal_view::TerminalView;
use ui::{Indicator, ListItem, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::status::{SessionStatus, StatusTracker};

actions!(sessions_panel, [ToggleFocus]);

pub struct SessionsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
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
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let weak = workspace.weak_handle();
            cx.new(|cx| SessionsPanel {
                workspace: weak,
                focus_handle: cx.focus_handle(),
                trackers: HashMap::default(),
            })
        })
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
