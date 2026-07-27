use std::time::Duration;

use editor::Editor;
use futures::FutureExt as _;
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, BackgroundExecutor, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled, Task, Window,
};
use menu::{Cancel, Confirm};
use settings::{RegisterSetting, Settings};
use ui::prelude::*;
use workspace::{ModalView, Workspace};
use zed_actions::{CreateWorktree, NewWorktreeBranchTarget};

use crate::worktree_service;

/// Settings for [`generate_branch_name`], registered under the `branch_namer`
/// key in `default.json` (see `assets/settings/default.json`).
#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct BranchNamerSettings {
    pub enabled: bool,
    pub command: Option<String>,
}

impl Settings for BranchNamerSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let branch_namer = content.branch_namer.clone().unwrap_or_default();
        Self {
            enabled: branch_namer.enabled.unwrap_or(true),
            command: branch_namer.command,
        }
    }
}

/// How long to wait for the agent CLI before giving up and falling back to a
/// locally-slugified branch name. These CLIs can hang (e.g. waiting on a
/// login prompt or a slow network), and this flow must never block worktree
/// creation on them.
const BRANCH_NAMER_TIMEOUT: Duration = Duration::from_secs(20);

const MAX_BRANCH_NAME_LENGTH: usize = 60;

/// A literal token in a user-supplied `branch_namer.command` override that
/// gets replaced with the actual prompt text.
const PROMPT_PLACEHOLDER: &str = "{prompt}";

/// Shows the "What are you working on?" modal, generates a branch name from
/// the answer (via a local agent CLI, falling back to a local slug), and
/// creates a worktree on that branch using the existing worktree-creation
/// machinery ([`worktree_service::handle_create_worktree`]).
///
/// If the user cancels the modal, nothing happens. If they submit an empty
/// description, worktree creation still proceeds with `worktree_name: None`,
/// which is `CreateWorktree`'s existing behavior of auto-generating a name.
pub fn prompt_and_create_worktree(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (tx, rx) = oneshot::channel::<String>();
    workspace.toggle_modal(window, cx, |window, cx| {
        BranchDescriptionModal::new(tx, window, cx)
    });

    let workspace_handle = workspace.weak_handle();
    cx.spawn_in(window, async move |_workspace, cx| {
        let Ok(description) = rx.await else {
            // Cancelled: don't create a worktree at all.
            return;
        };
        let description = description.trim().to_string();

        let worktree_name = if description.is_empty() {
            None
        } else {
            let name_task = cx.update(|_, cx| generate_branch_name(description, cx));
            match name_task {
                Ok(task) => Some(task.await),
                Err(_) => return, // window closed before we could generate a name.
            }
        };

        workspace_handle
            .update_in(cx, |workspace, window, cx| {
                worktree_service::handle_create_worktree(
                    workspace,
                    &CreateWorktree {
                        worktree_name,
                        branch_target: NewWorktreeBranchTarget::CurrentBranch,
                    },
                    window,
                    None,
                    cx,
                );
            })
            .ok();
    })
    .detach();
}

struct BranchDescriptionModal {
    editor: Entity<Editor>,
    tx: Option<oneshot::Sender<String>>,
}

impl EventEmitter<DismissEvent> for BranchDescriptionModal {}
impl ModalView for BranchDescriptionModal {}

impl Focusable for BranchDescriptionModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl BranchDescriptionModal {
    fn new(tx: oneshot::Sender<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(
                "fix the dropdown z-index bug so menus render above modals",
                window,
                cx,
            );
            editor
        });
        Self {
            editor,
            tx: Some(tx),
        }
    }

    fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        // Dropping `tx` without sending tells the waiter to abort without
        // creating a worktree at all.
        self.tx = None;
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            editor.clear(window, cx);
            text
        });
        if let Some(tx) = self.tx.take() {
            tx.send(text).ok();
        }
        cx.emit(DismissEvent);
    }
}

impl Render for BranchDescriptionModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("BranchDescriptionPrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.cancel(&Cancel, window, cx);
            }))
            .elevation_2(cx)
            .w(rems(34.))
            .child(
                h_flex()
                    .font_buffer(cx)
                    .px(DynamicSpacing::Base12.rems(cx))
                    .pt(DynamicSpacing::Base08.rems(cx))
                    .pb(DynamicSpacing::Base04.rems(cx))
                    .rounded_t_sm()
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                    .child(Headline::new("What are you working on?").size(HeadlineSize::XSmall)),
            )
            .child(
                div()
                    .font_buffer(cx)
                    .text_buffer(cx)
                    .py_2()
                    .px_3()
                    .bg(cx.theme().colors().editor_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .size_full()
                    .overflow_hidden()
                    .child(self.editor.clone()),
            )
    }
}

/// Turns a free-text task description into a branch name, preferring a local
/// agent CLI (`claude`, then `codex`, or a `branch_namer.command` override)
/// and always falling back to a local slug of `description` if the CLI is
/// missing, disabled, times out, or returns something unusable. This never
/// fails and never blocks the UI thread: the CLI call runs on a background
/// thread with a bounded timeout.
pub fn generate_branch_name(description: String, cx: &App) -> Task<String> {
    let fallback = fallback_branch_name(&description);
    let settings = BranchNamerSettings::get_global(cx).clone();

    if !settings.enabled {
        return Task::ready(fallback);
    }

    let executor = cx.background_executor().clone();
    let prompt = format!(
        "Suggest a git branch name for this task: '{description}'. Reply with ONLY the branch \
         name, lowercase, kebab-case, optionally a type/ prefix like fix/ or feat/. No quotes, \
         no explanation."
    );

    cx.background_spawn(async move {
        for (program, args) in candidate_commands(&settings.command, &prompt) {
            match run_provider(&program, &args, &executor).await {
                Ok(stdout) => {
                    return sanitize_branch_name(&stdout).unwrap_or_else(|| {
                        log::debug!(
                            "branch_namer: `{program}` produced unusable output ({stdout:?}); \
                             falling back to local slug"
                        );
                        fallback.clone()
                    });
                }
                Err(ProviderError::NotFound) => {
                    log::debug!(
                        "branch_namer: `{program}` not found on PATH, trying next candidate"
                    );
                    continue;
                }
                Err(error) => {
                    log::debug!(
                        "branch_namer: `{program}` failed ({error}); falling back to local slug"
                    );
                    return fallback;
                }
            }
        }
        fallback
    })
}

/// The provider commands to try, in order. Each is `(program, args)`, where
/// `args` already has the prompt substituted in.
fn candidate_commands(
    override_command: &Option<String>,
    prompt: &str,
) -> Vec<(String, Vec<String>)> {
    if let Some(custom) = override_command.as_deref()
        && let Some(command) = build_custom_command(custom, prompt)
    {
        return vec![command];
    }

    vec![
        (
            "claude".to_string(),
            vec![
                "-p".to_string(),
                prompt.to_string(),
                "--model".to_string(),
                "haiku".to_string(),
            ],
        ),
        // ponytail/unverified: codex isn't installed on the machine this was
        // built on, so this argv is a best guess at codex's documented
        // non-interactive form (`codex exec "<prompt>"`), not something that
        // was actually run. Mirrors the `glab` caveat in
        // `crates/pr_viewer/src/forge.rs`. If this is wrong, fix it via the
        // `branch_namer.command` setting (e.g. `"codex exec {prompt}"`)
        // instead of a rebuild.
        ("codex".to_string(), vec!["exec".to_string(), prompt.to_string()]),
    ]
}

/// Splits a `branch_namer.command` override on whitespace into a program and
/// args, substituting a literal `{prompt}` token if present, or appending the
/// prompt as the final argument otherwise. Returns `None` for an empty
/// override so callers fall back to the built-in candidates.
fn build_custom_command(custom: &str, prompt: &str) -> Option<(String, Vec<String>)> {
    let mut tokens = custom.split_whitespace();
    let program = tokens.next()?.to_string();
    let mut args: Vec<String> = tokens.map(|token| token.to_string()).collect();

    let mut has_placeholder = false;
    for arg in args.iter_mut() {
        if arg == PROMPT_PLACEHOLDER {
            *arg = prompt.to_string();
            has_placeholder = true;
        }
    }
    if !has_placeholder {
        args.push(prompt.to_string());
    }

    Some((program, args))
}

/// Why a single provider invocation didn't produce usable output. `NotFound`
/// is the only variant that causes [`generate_branch_name`] to try the next
/// candidate; every other variant falls straight back to the local slug.
#[derive(Debug)]
enum ProviderError {
    NotFound,
    Timeout,
    CommandFailed(String),
    Io(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::NotFound => write!(f, "binary not found on PATH"),
            ProviderError::Timeout => write!(f, "timed out after {BRANCH_NAMER_TIMEOUT:?}"),
            ProviderError::CommandFailed(stderr) => {
                write!(f, "exited with an error: {}", stderr.trim())
            }
            ProviderError::Io(message) => write!(f, "{message}"),
        }
    }
}

/// Runs `program args` with a bounded timeout, racing it against a timer on
/// the given executor. On timeout the child is abandoned rather than
/// blocking: `kill_on_drop(true)` means dropping the losing future's
/// internally-spawned child kills and reaps the process instead of leaving a
/// zombie behind.
async fn run_provider(
    program: &str,
    args: &[String],
    executor: &BackgroundExecutor,
) -> Result<String, ProviderError> {
    let mut command = util::command::new_command(program);
    command
        .args(args)
        .stdin(util::command::Stdio::null())
        .kill_on_drop(true);

    let mut output_future = Box::pin(command.output()).fuse();
    let mut timeout = executor.timer(BRANCH_NAMER_TIMEOUT).fuse();

    let output = futures::select_biased! {
        output = output_future => output,
        _ = timeout => return Err(ProviderError::Timeout),
    };

    let output = output.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProviderError::NotFound
        } else {
            ProviderError::Io(error.to_string())
        }
    })?;

    if !output.status.success() {
        return Err(ProviderError::CommandFailed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Locally slugifies `description` into a branch name, using the exact same
/// sanitizer that agent-CLI output goes through. This is the flow's ultimate
/// safety net: it never depends on any external process, so it always
/// succeeds (falling back to a fixed name if `description` sanitizes to
/// nothing usable, e.g. because it's empty or pure punctuation).
pub fn fallback_branch_name(description: &str) -> String {
    sanitize_branch_name(description).unwrap_or_else(|| "worktree".to_string())
}

/// Turns `raw` into a value safe to use as a git branch/ref name, or `None`
/// if nothing usable survives sanitization. `raw` must never be trusted: it
/// can be an LLM's stdout, so this is the boundary between "a model said
/// this" and "this text becomes a git ref".
///
/// Only the first non-empty line is considered (models sometimes add
/// explanatory text on later lines despite instructions not to). The result
/// is lowercased, has non-ref-safe characters replaced with `-`, has
/// repeated separators collapsed, and is capped at 60 characters.
pub fn sanitize_branch_name(raw: &str) -> Option<String> {
    let first_line = raw.lines().find(|line| !line.trim().is_empty())?;
    let trimmed = first_line
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`');
    let lowercased = trimmed.to_lowercase();

    let mut replaced = String::with_capacity(lowercased.len());
    for ch in lowercased.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/' | '.') {
            replaced.push(ch);
        } else {
            replaced.push('-');
        }
    }

    let mut collapsed = String::with_capacity(replaced.len());
    let mut last_char: Option<char> = None;
    for ch in replaced.chars() {
        if matches!(ch, '-' | '/' | '.') && last_char == Some(ch) {
            continue;
        }
        collapsed.push(ch);
        last_char = Some(ch);
    }

    let stripped = collapsed.trim_matches(|c: char| matches!(c, '-' | '/' | '.'));
    let mut candidate = stripped.to_string();
    candidate.truncate(MAX_BRANCH_NAME_LENGTH);
    let candidate = candidate
        .trim_end_matches(|c: char| matches!(c, '-' | '/' | '.'))
        .to_string();

    is_valid_git_ref_component(&candidate).then_some(candidate)
}

/// Enforces git's ref-name rules (see `git-check-ref-format(1)`) on a
/// already-lowercased, already-separator-collapsed candidate.
fn is_valid_git_ref_component(name: &str) -> bool {
    if name.is_empty() || name == "-" {
        return false;
    }
    if name.contains("..") || name.contains("//") || name.contains("@{") {
        return false;
    }
    if name
        .contains(['~', '^', ':', '?', '*', '[', '\\', ' ', '\t'])
    {
        return false;
    }
    if name.ends_with(".lock") {
        return false;
    }
    if name.starts_with('/') || name.ends_with('/') {
        return false;
    }
    if name.starts_with('.') || name.ends_with('.') {
        return false;
    }
    if name.starts_with('-') {
        return false;
    }
    if name
        .split('/')
        .any(|component| component.is_empty() || component.starts_with('.'))
    {
        return false;
    }
    if name.chars().any(|c| c.is_control()) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_normal_model_output() {
        assert_eq!(
            sanitize_branch_name("fix/dropdown-z-index-above-modals"),
            Some("fix/dropdown-z-index-above-modals".to_string())
        );
    }

    #[test]
    fn takes_first_non_empty_line() {
        assert_eq!(
            sanitize_branch_name("\n\nfeat/add-login\n\nThis branch adds a login flow."),
            Some("feat/add-login".to_string())
        );
    }

    #[test]
    fn strips_quotes_and_backticks() {
        assert_eq!(
            sanitize_branch_name("`fix/thing`"),
            Some("fix/thing".to_string())
        );
        assert_eq!(
            sanitize_branch_name("\"fix/thing\""),
            Some("fix/thing".to_string())
        );
        assert_eq!(
            sanitize_branch_name("'fix/thing'"),
            Some("fix/thing".to_string())
        );
    }

    #[test]
    fn lowercases_and_replaces_spaces() {
        assert_eq!(
            sanitize_branch_name("Fix The Dropdown Bug"),
            Some("fix-the-dropdown-bug".to_string())
        );
    }

    #[test]
    fn collapses_repeated_separators() {
        assert_eq!(
            sanitize_branch_name("fix///the   bug---now"),
            Some("fix/the-bug-now".to_string())
        );
    }

    #[test]
    fn strips_leading_and_trailing_separators() {
        assert_eq!(
            sanitize_branch_name("--/fix/thing/.--"),
            Some("fix/thing".to_string())
        );
    }

    #[test]
    fn rejects_double_dot() {
        assert_eq!(sanitize_branch_name("fix/../etc/passwd"), None);
    }

    /// Git's disallowed ref characters (`~^:?*[`) and `@`/`{`/`}` aren't in
    /// the sanitizer's allowed charset, so they're neutralized to `-` by the
    /// character-replacement pass rather than surviving to be rejected by
    /// `is_valid_git_ref_component` below. Either strategy satisfies "never
    /// produce a malformed ref" — this asserts the output is safe, not `None`.
    #[test]
    fn neutralizes_git_special_characters_instead_of_producing_them() {
        for raw in ["fix~thing", "fix^thing", "fix:thing", "fix?thing", "fix*thing", "fix[thing"] {
            let sanitized = sanitize_branch_name(raw).unwrap_or_else(|| panic!("{raw:?} should still sanitize to something"));
            assert_eq!(sanitized, "fix-thing", "for input {raw:?}");
            assert!(is_valid_git_ref_component(&sanitized));
        }
    }

    #[test]
    fn neutralizes_at_brace_instead_of_producing_it() {
        let sanitized = sanitize_branch_name("fix@{thing}").expect("should still sanitize to something");
        assert_eq!(sanitized, "fix-thing");
        assert!(is_valid_git_ref_component(&sanitized));
    }

    /// Defense in depth: even though the character-replacement pass makes
    /// these patterns unreachable through the public `sanitize_branch_name`
    /// entry point today, the underlying git-ref-rule checks are exercised
    /// directly so a future change to the allowed charset can't silently
    /// let `..`, `~^:?*[`, or `@{` back into a produced ref.
    #[test]
    fn ref_validity_rules_reject_forbidden_patterns_directly() {
        for name in [
            "fix..thing",
            "fix~thing",
            "fix^thing",
            "fix:thing",
            "fix?thing",
            "fix*thing",
            "fix[thing",
            "fix@{thing}",
            "fix.lock",
        ] {
            assert!(!is_valid_git_ref_component(name), "should reject {name:?}");
        }
    }

    #[test]
    fn rejects_trailing_dot_lock() {
        assert_eq!(sanitize_branch_name("fix/thing.lock"), None);
    }

    #[test]
    fn rejects_bare_dash() {
        assert_eq!(sanitize_branch_name("---"), None);
    }

    #[test]
    fn rejects_empty_and_whitespace_only() {
        assert_eq!(sanitize_branch_name(""), None);
        assert_eq!(sanitize_branch_name("   \n\n  \t "), None);
    }

    #[test]
    fn caps_length() {
        let long_input = "a".repeat(200);
        let sanitized = sanitize_branch_name(&long_input).expect("should still be valid");
        assert!(sanitized.len() <= MAX_BRANCH_NAME_LENGTH);
    }

    #[test]
    fn strips_newlines_embedded_via_crlf() {
        assert_eq!(
            sanitize_branch_name("fix/thing\r\nextra explanation"),
            Some("fix/thing".to_string())
        );
    }

    #[test]
    fn rejects_component_starting_with_dot() {
        assert_eq!(sanitize_branch_name("fix/.hidden"), None);
    }

    #[test]
    fn fallback_never_fails_on_junk_input() {
        for raw in ["", "   ", "----", "...", "@{}", "~^:?*[", "\n\n\t"] {
            let slug = fallback_branch_name(raw);
            assert!(!slug.is_empty(), "fallback should never be empty for {raw:?}");
            assert!(
                is_valid_git_ref_component(&slug),
                "fallback slug {slug:?} for {raw:?} should be a valid ref"
            );
        }
    }

    #[test]
    fn fallback_slugifies_a_normal_description() {
        assert_eq!(
            fallback_branch_name("fix the dropdown z-index bug so menus render above modals"),
            "fix-the-dropdown-z-index-bug-so-menus-render-above-modals"
        );
    }

    #[test]
    fn fallback_handles_overlong_description() {
        let description = "fix ".repeat(50);
        let slug = fallback_branch_name(&description);
        assert!(slug.len() <= MAX_BRANCH_NAME_LENGTH);
        assert!(is_valid_git_ref_component(&slug));
    }

    #[test]
    fn custom_command_substitutes_placeholder() {
        assert_eq!(
            build_custom_command("codex exec {prompt}", "hello world"),
            Some((
                "codex".to_string(),
                vec!["exec".to_string(), "hello world".to_string()]
            ))
        );
    }

    #[test]
    fn custom_command_appends_prompt_without_placeholder() {
        assert_eq!(
            build_custom_command("codex exec", "hello world"),
            Some((
                "codex".to_string(),
                vec!["exec".to_string(), "hello world".to_string()]
            ))
        );
    }

    #[test]
    fn custom_command_empty_is_none() {
        assert_eq!(build_custom_command("   ", "hello"), None);
    }
}
