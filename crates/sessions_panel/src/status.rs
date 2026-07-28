use std::time::{Duration, Instant};

const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "nu", "tcsh", "csh", "dash"];

/// Commands treated as long-running interactive agents for the `NeedsInput`
/// heuristic below. Anything not on this list can only ever be `Running`,
/// even if it goes quiet for a long time (e.g. a slow build), since a quiet
/// non-agent command is far more likely to just be slow than waiting on a
/// prompt.
const AGENTS: &[&str] = &["claude", "aider", "codex", "cursor-agent"];

/// How long an agent must produce no terminal output before we guess it's
/// sitting at a prompt waiting on the user.
// Was 10s when "quiet" was inferred from the cursor position, which TUI
// agents perturb constantly — the panel now hashes visible content, a much
// stronger signal, so the threshold can be tight without false positives.
pub const NEEDS_INPUT_QUIET_THRESHOLD: Duration = Duration::from_secs(4);

pub fn is_agent(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name).trim_start_matches('-');
    AGENTS.contains(&base)
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionStatus {
    Idle,
    Running { command: String, since: Instant },
    /// Best-effort HEURISTIC: a known agent command is in the foreground and
    /// the terminal has been quiet for `NEEDS_INPUT_QUIET_THRESHOLD`. This is
    /// not a real "waiting for input" detection (that needs OSC 133 shell
    /// integration, Phase 3) -- it will false-positive on agents that are
    /// just thinking/working slowly, and it will never fire for agents not
    /// listed in `AGENTS`.
    NeedsInput { command: String, since: Instant },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandFinished {
    pub command: String,
    pub duration: Duration,
}

pub struct StatusTracker {
    status: SessionStatus,
}

impl StatusTracker {
    pub fn new() -> Self {
        Self { status: SessionStatus::Idle }
    }

    pub fn status(&self) -> &SessionStatus {
        &self.status
    }

    /// Feed the current foreground process name (from
    /// `Terminal::foreground_process_command_name`, hive/crates/terminal/src/terminal.rs:2767),
    /// how long the terminal has produced no output (`quiet_for`), and
    /// whether that command is a known long-running agent (`is_agent`, see
    /// [`is_agent`]). Returns Some when a tracked command just finished.
    ///
    /// `quiet_for` and `is_agent` only affect the Running/NeedsInput split;
    /// callers of non-agent commands can pass `Duration::ZERO` / `false` and
    /// get the old two-state behavior.
    pub fn update(
        &mut self,
        foreground: Option<&str>,
        quiet_for: Duration,
        is_agent: bool,
        now: Instant,
    ) -> Option<CommandFinished> {
        let running = foreground.filter(|name| !is_shell(name));
        match (self.status.clone(), running) {
            (SessionStatus::Idle, Some(cmd)) => {
                self.status = SessionStatus::Running { command: cmd.to_string(), since: now };
                None
            }
            (SessionStatus::Running { command, since }, None)
            | (SessionStatus::NeedsInput { command, since }, None) => {
                let finished = CommandFinished { command, duration: now - since };
                self.status = SessionStatus::Idle;
                Some(finished)
            }
            (SessionStatus::Running { since, .. }, Some(cmd))
            | (SessionStatus::NeedsInput { since, .. }, Some(cmd)) => {
                // ponytail: pipeline/subcommand handoff keeps the original start time
                self.status = if is_agent && quiet_for >= NEEDS_INPUT_QUIET_THRESHOLD {
                    SessionStatus::NeedsInput { command: cmd.to_string(), since }
                } else {
                    SessionStatus::Running { command: cmd.to_string(), since }
                };
                None
            }
            (SessionStatus::Idle, None) => None,
        }
    }
}

fn is_shell(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name).trim_start_matches('-');
    SHELLS.contains(&base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn shell_alone_is_idle() {
        let mut t = StatusTracker::new();
        assert_eq!(t.update(Some("zsh"), Duration::ZERO, false, Instant::now()), None);
        assert_eq!(*t.status(), SessionStatus::Idle);
        // login shells report with a leading dash
        assert_eq!(t.update(Some("-zsh"), Duration::ZERO, false, Instant::now()), None);
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    #[test]
    fn command_runs_then_finishes_with_duration() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        assert_eq!(t.update(Some("claude"), Duration::ZERO, true, t0), None);
        match t.status() {
            SessionStatus::Running { command, .. } => assert_eq!(command, "claude"),
            other => panic!("expected Running, got {other:?}"),
        }
        let done = t
            .update(Some("zsh"), Duration::ZERO, false, t0 + Duration::from_secs(90))
            .unwrap();
        assert_eq!(done.command, "claude");
        assert_eq!(done.duration, Duration::from_secs(90));
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    #[test]
    fn none_foreground_counts_as_idle() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("cargo"), Duration::ZERO, false, t0);
        assert!(t.update(None, Duration::ZERO, false, t0 + Duration::from_secs(5)).is_some());
    }

    #[test]
    fn pipeline_stage_change_keeps_start_time() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("cargo"), Duration::ZERO, false, t0);
        assert_eq!(
            t.update(Some("rustc"), Duration::ZERO, false, t0 + Duration::from_secs(10)),
            None
        );
        let done = t
            .update(Some("zsh"), Duration::ZERO, false, t0 + Duration::from_secs(60))
            .unwrap();
        assert_eq!(done.command, "rustc");
        assert_eq!(done.duration, Duration::from_secs(60)); // measured from t0
    }

    #[test]
    fn shell_path_is_recognized() {
        let mut t = StatusTracker::new();
        assert_eq!(t.update(Some("/bin/bash"), Duration::ZERO, false, Instant::now()), None);
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    #[test]
    fn quiet_agent_becomes_needs_input() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), Duration::ZERO, true, t0);
        t.update(Some("claude"), NEEDS_INPUT_QUIET_THRESHOLD, true, t0 + Duration::from_secs(20));
        match t.status() {
            SessionStatus::NeedsInput { command, since } => {
                assert_eq!(command, "claude");
                assert_eq!(*since, t0); // start time survives the state change
            }
            other => panic!("expected NeedsInput, got {other:?}"),
        }
    }

    #[test]
    fn agent_producing_output_stays_running() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), Duration::ZERO, true, t0);
        t.update(Some("claude"), Duration::from_secs(2), true, t0 + Duration::from_secs(20));
        match t.status() {
            SessionStatus::Running { command, .. } => assert_eq!(command, "claude"),
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[test]
    fn quiet_non_agent_never_becomes_needs_input() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("cargo"), Duration::ZERO, false, t0);
        t.update(Some("cargo"), NEEDS_INPUT_QUIET_THRESHOLD * 10, false, t0 + Duration::from_secs(200));
        match t.status() {
            SessionStatus::Running { command, .. } => assert_eq!(command, "cargo"),
            other => panic!("expected Running (never NeedsInput for non-agents), got {other:?}"),
        }
    }

    #[test]
    fn needs_input_returns_to_idle_when_process_exits() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), Duration::ZERO, true, t0);
        t.update(Some("claude"), NEEDS_INPUT_QUIET_THRESHOLD, true, t0 + Duration::from_secs(20));
        assert!(matches!(t.status(), SessionStatus::NeedsInput { .. }));
        let done = t
            .update(None, Duration::ZERO, false, t0 + Duration::from_secs(25))
            .unwrap();
        assert_eq!(done.command, "claude");
        assert_eq!(done.duration, Duration::from_secs(25));
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    #[test]
    fn needs_input_resumes_running_when_agent_produces_output_again() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), Duration::ZERO, true, t0);
        t.update(Some("claude"), NEEDS_INPUT_QUIET_THRESHOLD, true, t0 + Duration::from_secs(20));
        assert!(matches!(t.status(), SessionStatus::NeedsInput { .. }));
        t.update(Some("claude"), Duration::ZERO, true, t0 + Duration::from_secs(21));
        match t.status() {
            SessionStatus::Running { command, since } => {
                assert_eq!(command, "claude");
                assert_eq!(*since, t0);
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }
}
