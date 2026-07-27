use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, Render, WeakEntity, Window, actions, px,
};
use terminal_view::TerminalView;
use ui::{ListItem, Tooltip, prelude::*};
use util::ResultExt;
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::file_tree::{self, DirState, TreeEntry};

actions!(file_tree_panel, [ToggleFocus]);

pub struct FileTreePanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    /// Root of the rendered file tree: the focused terminal's cwd, falling
    /// back to the last terminal that was focused, then the first project
    /// root. `None` when there's nothing to show it for yet (e.g. panel just
    /// opened, no terminals exist).
    tree_root: Option<PathBuf>,
    last_terminal_cwd: Option<PathBuf>,
    tree_expanded: HashSet<PathBuf>,
    tree_cache: HashMap<PathBuf, DirState>,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<FileTreePanel>(window, cx);
        });
    })
    .detach();
}

impl FileTreePanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let weak = workspace.weak_handle();
            cx.new(|cx| {
                cx.spawn(async move |this: WeakEntity<Self>, cx| {
                    loop {
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                        let Ok(()) = this.update(cx, |this, cx| this.update_tree_root(cx)) else {
                            break; // panel dropped
                        };
                    }
                })
                .detach();
                FileTreePanel {
                    workspace: weak,
                    focus_handle: cx.focus_handle(),
                    tree_root: None,
                    last_terminal_cwd: None,
                    tree_expanded: HashSet::default(),
                    tree_cache: HashMap::default(),
                }
            })
        })
    }

    /// The cwd of the terminal that's the active item in the workspace's
    /// active pane, or `None` if the active item isn't a terminal at all
    /// (e.g. an editor tab is focused).
    fn focused_terminal_cwd(&self, cx: &App) -> Option<PathBuf> {
        let workspace = self.workspace.upgrade()?;
        let active_terminal = workspace.read(cx).active_item(cx)?.downcast::<TerminalView>()?;
        active_terminal.read(cx).terminal().read(cx).working_directory()
    }

    /// Root fallback for when no terminal has ever been focused: the first
    /// visible project worktree.
    fn first_project_root(&self, cx: &App) -> Option<PathBuf> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        project.visible_worktrees(cx).next().map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    }

    /// Re-derives the tree's root (see the `tree_root` doc comment) and, on
    /// change, drops the stale cache/expansion state and kicks off a load of
    /// the new root's listing. Called every 2s from the timer loop started in
    /// `load`.
    fn update_tree_root(&mut self, cx: &mut Context<Self>) {
        let focused_cwd = self.focused_terminal_cwd(cx);

        // hive: let the title bar follow the focused terminal too. Only
        // publish (and re-read `.git/HEAD`) when the cwd actually changed --
        // this runs on the same 2s poll as the tree root, never on render.
        if focused_cwd != workspace::ActiveTerminalLocation::get(cx).path {
            workspace::ActiveTerminalLocation::set(focused_cwd.clone(), cx);
        }

        if let Some(cwd) = focused_cwd.clone() {
            self.last_terminal_cwd = Some(cwd);
        }
        let new_root = focused_cwd
            .or_else(|| self.last_terminal_cwd.clone())
            .or_else(|| self.first_project_root(cx));

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
        cx.notify();
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

    /// The file tree, rooted at `self.tree_root`. `None` when there's no
    /// root yet (nothing to show).
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
}

impl Render for FileTreePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = v_flex().size_full().p_1();
        if let Some(tree) = self.render_tree(cx) {
            root = root.child(tree);
        }
        root
    }
}

impl EventEmitter<PanelEvent> for FileTreePanel {}

impl Focusable for FileTreePanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for FileTreePanel {
    fn persistent_name() -> &'static str {
        "FileTreePanel"
    }

    fn panel_key() -> &'static str {
        "FileTreePanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::FileTree)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("File Tree")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Must be globally unique across all panels (Zed panics in debug builds on a
        // collision). 0=agent, 1=project, 2=terminal, 3=git, 4=sessions, 6=outline, 7=debugger.
        5
    }

    fn starts_open(&self, _window: &Window, _cx: &App) -> bool {
        true
    }
}
