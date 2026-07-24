use std::path::PathBuf;

use git::{GitHostingProviderRegistry, parse_git_remote_url};
use gpui::{App, AppContext as _, Entity, Task};
use project::git_store::Repository;
use serde::Deserialize;

/// Which forge a repository's PRs/MRs should be fetched from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeKind {
    GitHub,
    GitLab,
}

impl ForgeKind {
    fn cli(&self) -> &'static str {
        match self {
            ForgeKind::GitHub => "gh",
            ForgeKind::GitLab => "glab",
        }
    }
}

/// A detected forge for a repository: which CLI to run and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forge {
    pub kind: ForgeKind,
    pub cli: &'static str,
    pub owner: String,
    pub repo: String,
    pub root: PathBuf,
}

/// Inspects `repo`'s origin remote and maps it to a supported forge, if any.
///
/// Only GitHub and GitLab (including self-hosted instances, which the hosting
/// provider registry reports with names like "GitHub Self-Hosted") are
/// supported for the MVP. Any other provider (Bitbucket, Gitea, ...) or a
/// repository with no parseable remote returns `None`.
pub fn detect(repo: &Entity<Repository>, cx: &App) -> Option<Forge> {
    let repository = repo.read(cx);
    let remote_url = repository.remote_origin_url.clone()?;
    let root = repository.work_directory_abs_path.to_path_buf();

    let provider_registry = GitHostingProviderRegistry::global(cx);
    let (provider, parsed_remote) = parse_git_remote_url(provider_registry, &remote_url)?;

    let name = provider.name();
    let kind = if name.contains("GitHub") {
        ForgeKind::GitHub
    } else if name.contains("GitLab") {
        ForgeKind::GitLab
    } else {
        return None;
    };

    Some(Forge {
        kind,
        cli: kind.cli(),
        owner: parsed_remote.owner.to_string(),
        repo: parsed_remote.repo.to_string(),
        root,
    })
}

/// A single row in the PR/MR list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub number: u32,
    pub title: String,
    pub author: String,
    pub state: String,
    pub url: String,
}

/// Why fetching the PR/MR list failed, distinguished so the UI can show a
/// targeted empty-state instead of a generic error.
#[derive(Debug, Clone)]
pub enum ListPrsError {
    /// The `gh`/`glab` binary itself could not be spawned (not on PATH).
    CliNotFound { cli: &'static str, source: String },
    /// The CLI ran but exited non-zero (e.g. not authenticated, or not run
    /// inside a repository it recognizes).
    CommandFailed { cli: &'static str, stderr: String },
    /// The CLI exited successfully but its stdout wasn't the JSON we expected.
    Parse { cli: &'static str, message: String },
}

impl std::fmt::Display for ListPrsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListPrsError::CliNotFound { cli, source } => {
                write!(f, "`{cli}` was not found on PATH ({source})")
            }
            ListPrsError::CommandFailed { cli, stderr } => {
                write!(f, "`{cli}` exited with an error: {}", stderr.trim())
            }
            ListPrsError::Parse { cli, message } => {
                write!(f, "failed to parse `{cli}` output: {message}")
            }
        }
    }
}

impl std::error::Error for ListPrsError {}

/// Runs `gh pr list` / `glab mr list` for `forge` and parses the result.
///
/// ponytail: glab's list JSON flag is assumed to be `--output json` (mirrors
/// gitlab's REST `MergeRequest` shape: `iid`/`author.username`/`web_url`).
/// This is not verified against a live `glab` install; if it's wrong, only
/// `parse_glab_mr_list` below needs to change.
pub fn list_prs(forge: Forge, cx: &App) -> Task<Result<Vec<PrSummary>, ListPrsError>> {
    cx.background_spawn(async move {
        let args: &[&str] = match forge.kind {
            ForgeKind::GitHub => &[
                "pr",
                "list",
                "--json",
                "number,title,author,state,url",
                "--limit",
                "50",
            ],
            ForgeKind::GitLab => &["mr", "list", "--output", "json"],
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
        parse_pr_list(forge.kind, &stdout)
    })
}

fn parse_pr_list(kind: ForgeKind, stdout: &str) -> Result<Vec<PrSummary>, ListPrsError> {
    match kind {
        ForgeKind::GitHub => parse_gh_pr_list(stdout),
        ForgeKind::GitLab => parse_glab_mr_list(stdout),
    }
}

#[derive(Debug, Deserialize)]
struct GhAuthor {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GhPullRequest {
    number: u32,
    title: String,
    author: GhAuthor,
    state: String,
    url: String,
}

fn parse_gh_pr_list(stdout: &str) -> Result<Vec<PrSummary>, ListPrsError> {
    let entries: Vec<GhPullRequest> =
        serde_json::from_str(stdout).map_err(|error| ListPrsError::Parse {
            cli: "gh",
            message: error.to_string(),
        })?;

    Ok(entries
        .into_iter()
        .map(|entry| PrSummary {
            number: entry.number,
            title: entry.title,
            author: entry.author.login,
            state: entry.state,
            url: entry.url,
        })
        .collect())
}

#[derive(Debug, Deserialize)]
struct GlabAuthor {
    username: String,
}

#[derive(Debug, Deserialize)]
struct GlabMergeRequest {
    iid: u32,
    title: String,
    author: GlabAuthor,
    state: String,
    web_url: String,
}

fn parse_glab_mr_list(stdout: &str) -> Result<Vec<PrSummary>, ListPrsError> {
    let entries: Vec<GlabMergeRequest> =
        serde_json::from_str(stdout).map_err(|error| ListPrsError::Parse {
            cli: "glab",
            message: error.to_string(),
        })?;

    Ok(entries
        .into_iter()
        .map(|entry| PrSummary {
            number: entry.iid,
            title: entry.title,
            author: entry.author.username,
            state: entry.state,
            url: entry.web_url,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from `gh pr list --json number,title,author,state,url --limit 50`.
    const GH_SAMPLE: &str = r#"[
        {
            "number": 42,
            "title": "Add PR viewer",
            "author": {"login": "octocat"},
            "state": "OPEN",
            "url": "https://github.com/zed-industries/zed/pull/42"
        },
        {
            "number": 41,
            "title": "Fix flaky test",
            "author": {"login": "monalisa"},
            "state": "MERGED",
            "url": "https://github.com/zed-industries/zed/pull/41"
        }
    ]"#;

    const GLAB_SAMPLE: &str = r#"[
        {
            "iid": 7,
            "title": "Improve error messages",
            "author": {"username": "jdoe"},
            "state": "opened",
            "web_url": "https://gitlab.com/zed-industries/zed/-/merge_requests/7"
        }
    ]"#;

    #[test]
    fn parses_gh_pr_list_output() {
        let prs = parse_gh_pr_list(GH_SAMPLE).expect("valid gh json should parse");

        assert_eq!(
            prs,
            vec![
                PrSummary {
                    number: 42,
                    title: "Add PR viewer".into(),
                    author: "octocat".into(),
                    state: "OPEN".into(),
                    url: "https://github.com/zed-industries/zed/pull/42".into(),
                },
                PrSummary {
                    number: 41,
                    title: "Fix flaky test".into(),
                    author: "monalisa".into(),
                    state: "MERGED".into(),
                    url: "https://github.com/zed-industries/zed/pull/41".into(),
                },
            ]
        );
    }

    #[test]
    fn parses_gh_pr_list_empty_array() {
        let prs = parse_gh_pr_list("[]").expect("empty array is valid");
        assert!(prs.is_empty());
    }

    #[test]
    fn parses_glab_mr_list_output() {
        let mrs = parse_glab_mr_list(GLAB_SAMPLE).expect("valid glab json should parse");

        assert_eq!(
            mrs,
            vec![PrSummary {
                number: 7,
                title: "Improve error messages".into(),
                author: "jdoe".into(),
                state: "opened".into(),
                url: "https://gitlab.com/zed-industries/zed/-/merge_requests/7".into(),
            }]
        );
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        let result = parse_gh_pr_list("{ this is not valid json");
        assert!(matches!(result, Err(ListPrsError::Parse { cli: "gh", .. })));

        let result = parse_glab_mr_list("not json at all");
        assert!(matches!(
            result,
            Err(ListPrsError::Parse { cli: "glab", .. })
        ));
    }

    #[test]
    fn unexpected_shape_is_an_error_not_a_panic() {
        // Valid JSON, but not an array of PRs - e.g. gh emitted an error object.
        let result = parse_gh_pr_list(r#"{"error": "not authenticated"}"#);
        assert!(result.is_err());
    }
}
