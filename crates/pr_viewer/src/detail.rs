use editor::{Editor, EditorEvent};
use gpui::{Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity};
use serde::Deserialize;
use ui::prelude::*;
use util::ResultExt;
use workspace::{Item, Workspace, item::ItemEvent};

use crate::forge::{Forge, ForgeKind, ListPrsError, PrSummary};

/// Opens a read-only detail tab for `pr`: its metadata (title, author, state,
/// branches, description) and unified diff. Metadata and diff are fetched
/// independently, so a failure in one still shows what succeeded in the
/// other, with an in-view error note instead of a panic or blank tab.
///
/// Re-opening a PR that already has a tab focuses that tab rather than
/// fetching and opening a duplicate.
pub fn open_pr(
    forge: Forge,
    pr: PrSummary,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let focused_existing = workspace
        .update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            let existing_ix = pane.read(cx).items().position(|item| {
                item.downcast::<PrDetail>()
                    .is_some_and(|detail| detail.read(cx).pr_number == pr.number)
            });
            if let Some(ix) = existing_ix {
                pane.update(cx, |pane, cx| pane.activate_item(ix, true, true, window, cx));
            }
            existing_ix.is_some()
        })
        .log_err()
        .unwrap_or(false);

    if focused_existing {
        return;
    }

    let metadata_task = fetch_pr_metadata(forge.clone(), pr.number, cx);
    let diff_task = fetch_pr_diff(forge, pr.number, cx);

    window
        .spawn(cx, async move |cx| {
            let (metadata, diff) = futures::join!(metadata_task, diff_task);
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let detail = cx.new(|cx| PrDetail::new(pr, metadata, diff, window, cx));
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.add_item(Box::new(detail), true, true, None, window, cx);
                    });
                })
                .log_err();
        })
        .detach();
}

#[derive(Debug, Clone)]
struct PrMetadata {
    title: String,
    author: String,
    body: String,
    state: String,
    head_ref: String,
    base_ref: String,
}

fn fetch_pr_metadata(forge: Forge, number: u32, cx: &App) -> Task<Result<PrMetadata, ListPrsError>> {
    cx.background_spawn(async move {
        let number = number.to_string();
        let args: &[&str] = match forge.kind {
            ForgeKind::GitHub => &[
                "pr",
                "view",
                &number,
                "--json",
                "title,body,author,state,url,headRefName,baseRefName",
            ],
            // ponytail: mirrors `mr list`'s assumed `--output json` flag (see
            // forge.rs); unverified against a live glab install.
            ForgeKind::GitLab => &["mr", "view", &number, "--output", "json"],
        };

        let output = util::command::new_command(forge.cli)
            .args(args)
            .current_dir(&forge.root)
            .output()
            .await
            .map_err(|error| ListPrsError::CliNotFound {
                cli: forge.cli,
                source: error.to_string(),
            })?;

        if !output.status.success() {
            return Err(ListPrsError::CommandFailed {
                cli: forge.cli,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_pr_metadata(forge.kind, &stdout)
    })
}

fn fetch_pr_diff(forge: Forge, number: u32, cx: &App) -> Task<Result<String, ListPrsError>> {
    cx.background_spawn(async move {
        let number = number.to_string();
        let args: &[&str] = match forge.kind {
            ForgeKind::GitHub => &["pr", "diff", &number],
            ForgeKind::GitLab => &["mr", "diff", &number],
        };

        let output = util::command::new_command(forge.cli)
            .args(args)
            .current_dir(&forge.root)
            .output()
            .await
            .map_err(|error| ListPrsError::CliNotFound {
                cli: forge.cli,
                source: error.to_string(),
            })?;

        if !output.status.success() {
            return Err(ListPrsError::CommandFailed {
                cli: forge.cli,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })
}

fn parse_pr_metadata(kind: ForgeKind, stdout: &str) -> Result<PrMetadata, ListPrsError> {
    match kind {
        ForgeKind::GitHub => parse_gh_pr_view(stdout),
        ForgeKind::GitLab => parse_glab_mr_view(stdout),
    }
}

#[derive(Debug, Deserialize)]
struct GhViewAuthor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhPrView {
    title: String,
    body: String,
    // Deleted GitHub accounts report `"author": null`.
    author: Option<GhViewAuthor>,
    state: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
}

fn parse_gh_pr_view(stdout: &str) -> Result<PrMetadata, ListPrsError> {
    let view: GhPrView = serde_json::from_str(stdout).map_err(|error| ListPrsError::Parse {
        cli: "gh",
        message: error.to_string(),
    })?;

    Ok(PrMetadata {
        title: view.title,
        author: view
            .author
            .map(|author| author.login)
            .unwrap_or_else(|| "unknown".to_string()),
        body: view.body,
        state: view.state,
        head_ref: view.head_ref_name,
        base_ref: view.base_ref_name,
    })
}

#[derive(Debug, Deserialize)]
struct GlabViewAuthor {
    username: String,
}

// ponytail: field names mirror GitLab's REST MergeRequest resource
// (`description`, `source_branch`, `target_branch`) rather than gh's
// camelCase equivalents; unverified against a live glab install (see
// forge.rs for the same caveat on `mr list`).
#[derive(Debug, Deserialize)]
struct GlabMrView {
    title: String,
    description: Option<String>,
    author: Option<GlabViewAuthor>,
    state: String,
    source_branch: String,
    target_branch: String,
}

fn parse_glab_mr_view(stdout: &str) -> Result<PrMetadata, ListPrsError> {
    let view: GlabMrView = serde_json::from_str(stdout).map_err(|error| ListPrsError::Parse {
        cli: "glab",
        message: error.to_string(),
    })?;

    Ok(PrMetadata {
        title: view.title,
        author: view
            .author
            .map(|author| author.username)
            .unwrap_or_else(|| "unknown".to_string()),
        body: view.description.unwrap_or_default(),
        state: view.state,
        head_ref: view.source_branch,
        base_ref: view.target_branch,
    })
}

/// A read-only tab showing a PR/MR's metadata and unified diff.
///
/// The diff is rendered in a single-buffer, read-only `Editor` rather than a
/// native multibuffer diff view (as `git_ui`'s `CommitView` uses) — a
/// deliberate first cut per the task brief; upgrading to a hunk-aware
/// multibuffer diff is deferred.
pub struct PrDetail {
    pr_number: u32,
    title: SharedString,
    author: SharedString,
    state: SharedString,
    head_ref: SharedString,
    base_ref: SharedString,
    body: SharedString,
    metadata_error: Option<SharedString>,
    diff_editor: Entity<Editor>,
}

impl PrDetail {
    fn new(
        pr: PrSummary,
        metadata: Result<PrMetadata, ListPrsError>,
        diff: Result<String, ListPrsError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (title, author, state, head_ref, base_ref, body, metadata_error): (
            SharedString,
            SharedString,
            SharedString,
            SharedString,
            SharedString,
            SharedString,
            Option<SharedString>,
        ) = match metadata {
            Ok(metadata) => (
                metadata.title.into(),
                metadata.author.into(),
                metadata.state.into(),
                metadata.head_ref.into(),
                metadata.base_ref.into(),
                metadata.body.into(),
                None,
            ),
            Err(error) => (
                pr.title.clone().into(),
                pr.author.clone().into(),
                pr.state.clone().into(),
                SharedString::default(),
                SharedString::default(),
                SharedString::default(),
                Some(format!("Could not load full PR details: {error}").into()),
            ),
        };

        let diff_text = match diff {
            Ok(text) if !text.trim().is_empty() => text,
            Ok(_) => "No changes.".to_string(),
            Err(error) => format!("Failed to load diff: {error}"),
        };

        let diff_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_text(diff_text, window, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_read_only(true);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor
        });

        Self {
            pr_number: pr.number,
            title,
            author,
            state,
            head_ref,
            base_ref,
            body,
            metadata_error,
            diff_editor,
        }
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .w_full()
            .p_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new(self.title.clone()).size(LabelSize::Large))
                    .child(Label::new(format!("#{}", self.pr_number)).color(Color::Muted)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(self.author.clone())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(self.state.clone())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .when(
                        !self.base_ref.is_empty() && !self.head_ref.is_empty(),
                        |this| {
                            this.child(
                                Label::new(format!("{} ← {}", self.base_ref, self.head_ref))
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                        },
                    ),
            )
            .when_some(self.metadata_error.clone(), |this, error| {
                this.child(Label::new(error).color(Color::Error).size(LabelSize::Small))
            })
            .when(!self.body.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors().text_muted)
                        .child(self.body.clone()),
                )
            })
    }
}

impl EventEmitter<EditorEvent> for PrDetail {}

impl Focusable for PrDetail {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff_editor.focus_handle(cx)
    }
}

impl Item for PrDetail {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("#{} {}", self.pr_number, self.title).into()
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f);
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("PR Detail Opened")
    }
}

impl Render for PrDetail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PrDetail")
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(div().flex_grow(1.).child(self.diff_editor.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GH_VIEW_SAMPLE: &str = r#"{
        "title": "Add PR viewer",
        "body": "Adds a read-only PR/MR viewer.",
        "author": {"login": "octocat"},
        "state": "OPEN",
        "url": "https://github.com/zed-industries/zed/pull/42",
        "headRefName": "pr-viewer",
        "baseRefName": "main"
    }"#;

    const GH_VIEW_NULL_AUTHOR_SAMPLE: &str = r#"{
        "title": "Add PR viewer",
        "body": "",
        "author": null,
        "state": "OPEN",
        "url": "https://github.com/zed-industries/zed/pull/42",
        "headRefName": "pr-viewer",
        "baseRefName": "main"
    }"#;

    const GLAB_VIEW_SAMPLE: &str = r#"{
        "title": "Improve error messages",
        "description": "Better errors for CLI failures.",
        "author": {"username": "jdoe"},
        "state": "opened",
        "web_url": "https://gitlab.com/zed-industries/zed/-/merge_requests/7",
        "source_branch": "better-errors",
        "target_branch": "main"
    }"#;

    #[test]
    fn parses_gh_pr_view_output() {
        let metadata = parse_gh_pr_view(GH_VIEW_SAMPLE).expect("valid gh json should parse");
        assert_eq!(metadata.title, "Add PR viewer");
        assert_eq!(metadata.author, "octocat");
        assert_eq!(metadata.body, "Adds a read-only PR/MR viewer.");
        assert_eq!(metadata.state, "OPEN");
        assert_eq!(metadata.head_ref, "pr-viewer");
        assert_eq!(metadata.base_ref, "main");
    }

    #[test]
    fn parses_gh_pr_view_with_null_author() {
        let metadata =
            parse_gh_pr_view(GH_VIEW_NULL_AUTHOR_SAMPLE).expect("null author should not fail");
        assert_eq!(metadata.author, "unknown");
    }

    #[test]
    fn parses_glab_mr_view_output() {
        let metadata = parse_glab_mr_view(GLAB_VIEW_SAMPLE).expect("valid glab json should parse");
        assert_eq!(metadata.title, "Improve error messages");
        assert_eq!(metadata.author, "jdoe");
        assert_eq!(metadata.body, "Better errors for CLI failures.");
        assert_eq!(metadata.state, "opened");
        assert_eq!(metadata.head_ref, "better-errors");
        assert_eq!(metadata.base_ref, "main");
    }

    #[test]
    fn malformed_metadata_json_is_an_error_not_a_panic() {
        let result = parse_gh_pr_view("{ this is not valid json");
        assert!(matches!(result, Err(ListPrsError::Parse { cli: "gh", .. })));

        let result = parse_glab_mr_view("not json at all");
        assert!(matches!(
            result,
            Err(ListPrsError::Parse { cli: "glab", .. })
        ));
    }
}
