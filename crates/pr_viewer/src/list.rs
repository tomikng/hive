use std::sync::Arc;

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Rems, SharedString,
    Subscription, Task, TaskExt, WeakEntity, Window, actions, rems,
};
use picker::{Picker, PickerDelegate};
use project::git_store::Repository;
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

use crate::forge::{self, ListPrsError, PrSummary};

actions!(
    pr_viewer,
    [
        /// Opens the pull request / merge request list for the active repository.
        Open
    ]
);

/// Called with the confirmed row when the user selects a PR/MR. `PrList::new`
/// builds this once it knows the `Forge` for the active repository, wiring it
/// to `detail::open_pr`.
pub type OnSelectPr = Arc<dyn Fn(&PrSummary, &mut Window, &mut App)>;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &Open, window, cx| {
        open(workspace, window, cx);
    });
}

pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let repository = workspace.project().read(cx).active_repository(cx);
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, |window, cx| {
        PrList::new(repository, workspace_handle, rems(34.), window, cx)
    })
}

enum PrListState {
    Loading,
    NoForgeDetected,
    Error(ListPrsError),
    Loaded,
}

pub struct PrList {
    width: Rems,
    pub picker: Entity<Picker<PrListDelegate>>,
    picker_focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl PrList {
    fn new(
        repository: Option<Entity<Repository>>,
        workspace: WeakEntity<Workspace>,
        width: Rems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let forge = repository.as_ref().and_then(|repo| forge::detect(repo, cx));

        let on_select: Option<OnSelectPr> = forge.clone().map(|forge| {
            Arc::new(move |pr: &PrSummary, window: &mut Window, cx: &mut App| {
                crate::detail::open_pr(forge.clone(), pr.clone(), workspace.clone(), window, cx);
            }) as OnSelectPr
        });

        let delegate = PrListDelegate::new(on_select, window, cx);
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(width)
                .show_scrollbar(true)
        });
        let picker_focus_handle = picker.focus_handle(cx);
        picker.update(cx, |picker, _| {
            picker.delegate.focus_handle = picker_focus_handle.clone();
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&picker, |_, _, _, cx| {
            cx.emit(DismissEvent);
        }));

        let this = Self {
            picker,
            picker_focus_handle,
            width,
            _subscriptions: subscriptions,
        };

        let Some(forge) = forge else {
            this.picker.update(cx, |picker, cx| {
                picker.delegate.state = PrListState::NoForgeDetected;
                cx.notify();
            });
            return this;
        };

        let list_prs = forge::list_prs(forge, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = list_prs.await;
            this.update_in(cx, |this, window, cx| {
                this.picker.update(cx, |picker, cx| {
                    match result {
                        Ok(prs) => {
                            picker.delegate.all_prs = prs;
                            picker.delegate.state = PrListState::Loaded;
                        }
                        Err(error) => {
                            picker.delegate.state = PrListState::Error(error);
                        }
                    }
                    picker.refresh(window, cx);
                })
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);

        this
    }
}

impl ModalView for PrList {}
impl EventEmitter<DismissEvent> for PrList {}
impl Focusable for PrList {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.picker_focus_handle.clone()
    }
}

impl Render for PrList {
    fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PrList")
            .w(self.width)
            .child(self.picker.clone())
    }
}

pub struct PrListDelegate {
    state: PrListState,
    all_prs: Vec<PrSummary>,
    matches: Vec<PrSummary>,
    on_select: Option<OnSelectPr>,
    selected_index: usize,
    last_query: String,
    focus_handle: FocusHandle,
}

impl PrListDelegate {
    fn new(on_select: Option<OnSelectPr>, _window: &mut Window, cx: &mut Context<PrList>) -> Self {
        Self {
            state: PrListState::Loading,
            all_prs: Vec::new(),
            matches: Vec::new(),
            on_select,
            selected_index: 0,
            last_query: String::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    fn recompute_matches(&mut self, query: &str) {
        self.matches = if query.is_empty() {
            self.all_prs.clone()
        } else {
            let query = query.to_lowercase();
            self.all_prs
                .iter()
                .filter(|pr| {
                    pr.title.to_lowercase().contains(&query)
                        || pr.author.to_lowercase().contains(&query)
                })
                .cloned()
                .collect()
        };
        self.selected_index = 0;
    }
}

impl PickerDelegate for PrListDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "pr list"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Filter pull requests…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.recompute_matches(&query);
        self.last_query = query;
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(pr) = self.matches.get(self.selected_index).cloned() else {
            return;
        };
        if let Some(on_select) = self.on_select.clone() {
            on_select(&pr, window, cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match &self.state {
            PrListState::Loading => "Loading…".into(),
            PrListState::NoForgeDetected => {
                "No GitHub or GitLab remote detected for this repository.".into()
            }
            PrListState::Error(ListPrsError::CliNotFound { cli, .. }) => format!(
                "`{cli}` not found — install it and run `{cli} auth login`."
            )
            .into(),
            PrListState::Error(ListPrsError::CommandFailed { cli, .. }) => format!(
                "`{cli}` failed — make sure you're authenticated (`{cli} auth login`)."
            )
            .into(),
            PrListState::Error(ListPrsError::Parse { cli, .. }) => {
                format!("Could not parse `{cli}` output.").into()
            }
            PrListState::Loaded if self.all_prs.is_empty() => "No open pull requests.".into(),
            PrListState::Loaded => "No matches".into(),
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let pr = self.matches.get(ix)?;

        Some(
            ListItem::new(("pr-list-item", ix))
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .min_w_0()
                        .w_full()
                        .gap_2()
                        .child(
                            Label::new(format!("#{}", pr.number))
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .child(Label::new(pr.title.clone()).truncate())
                        .child(
                            Label::new("·")
                                .alpha(0.5)
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .child(
                            Label::new(pr.author.clone())
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .child(
                            Label::new("·")
                                .alpha(0.5)
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        )
                        .child(
                            Label::new(pr.state.clone())
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                ),
        )
    }
}
