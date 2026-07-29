use std::time::{Duration, Instant};

const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "nu", "tcsh", "csh", "dash"];

/// Commands treated as long-running interactive agents. They get a spinner
/// instead of a dot, their bell is taken as a request for attention (a shell's
/// is not), and they are the sessions worth auto-titling.
const AGENTS: &[&str] = &["claude", "aider", "codex", "cursor-agent"];

pub fn is_agent(name: &str) -> bool {
    AGENTS.contains(&base_name(name))
}

/// Claude Code specifically — the one agent whose notification settings Hive
/// knows how to turn on.
pub fn is_claude_code(name: &str) -> bool {
    base_name(name) == "claude"
}

/// The command itself, without its path or a login shell's leading dash.
fn base_name(name: &str) -> &str {
    name.rsplit('/')
        .next()
        .unwrap_or(name)
        .trim_start_matches('-')
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionStatus {
    Idle,
    Running {
        command: String,
        since: Instant,
    },
    /// The program in this terminal SAID it wants the user — an OSC 9 / OSC 777
    /// notification, or a bell from a known agent. Never inferred: Hive used to
    /// guess this from a screen that had stopped changing, which reported every
    /// long think as "finished". Set only by [`StatusTracker::signal_needs_input`].
    NeedsInput {
        command: String,
        since: Instant,
    },
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
        Self {
            status: SessionStatus::Idle,
        }
    }

    pub fn status(&self) -> &SessionStatus {
        &self.status
    }

    /// Feed the current foreground process name (from
    /// `Terminal::foreground_process_command_name`, hive/crates/terminal/src/terminal.rs:2767).
    /// Returns Some when a tracked command just finished.
    pub fn update(&mut self, foreground: Option<&str>, now: Instant) -> Option<CommandFinished> {
        let running = foreground.filter(|name| !is_shell(name));
        match (self.status.clone(), running) {
            (SessionStatus::Idle, Some(cmd)) => {
                self.status = SessionStatus::Running {
                    command: cmd.to_string(),
                    since: now,
                };
                None
            }
            (SessionStatus::Running { command, since }, None)
            | (SessionStatus::NeedsInput { command, since }, None) => {
                let finished = CommandFinished {
                    command,
                    duration: now - since,
                };
                self.status = SessionStatus::Idle;
                Some(finished)
            }
            // A waiting agent keeps waiting. Polling the process table is no
            // evidence it stopped: only exiting, or the user looking at the
            // session ([`acknowledge`]), clears this.
            (SessionStatus::NeedsInput { command, since }, Some(cmd)) if command == cmd => {
                self.status = SessionStatus::NeedsInput { command, since };
                None
            }
            (SessionStatus::Running { since, .. }, Some(cmd))
            | (SessionStatus::NeedsInput { since, .. }, Some(cmd)) => {
                // ponytail: pipeline/subcommand handoff keeps the original start time
                self.status = SessionStatus::Running {
                    command: cmd.to_string(),
                    since,
                };
                None
            }
            (SessionStatus::Idle, None) => None,
        }
    }

    /// The program said it wants the user. The only route into
    /// [`SessionStatus::NeedsInput`].
    pub fn signal_needs_input(&mut self) {
        if let SessionStatus::Running { command, since } = self.status.clone() {
            self.status = SessionStatus::NeedsInput { command, since };
        }
    }

    /// The user is looking at this session, so it is no longer waiting on them.
    pub fn acknowledge(&mut self) {
        if let SessionStatus::NeedsInput { command, since } = self.status.clone() {
            self.status = SessionStatus::Running { command, since };
        }
    }
}

fn is_shell(name: &str) -> bool {
    SHELLS.contains(&base_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn shell_alone_is_idle() {
        let mut t = StatusTracker::new();
        assert_eq!(t.update(Some("zsh"), Instant::now()), None);
        assert_eq!(*t.status(), SessionStatus::Idle);
        // login shells report with a leading dash
        assert_eq!(t.update(Some("-zsh"), Instant::now()), None);
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    #[test]
    fn command_runs_then_finishes_with_duration() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        assert_eq!(t.update(Some("claude"), t0), None);
        match t.status() {
            SessionStatus::Running { command, .. } => assert_eq!(command, "claude"),
            other => panic!("expected Running, got {other:?}"),
        }
        let done = t.update(Some("zsh"), t0 + Duration::from_secs(90)).unwrap();
        assert_eq!(done.command, "claude");
        assert_eq!(done.duration, Duration::from_secs(90));
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    #[test]
    fn none_foreground_counts_as_idle() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("cargo"), t0);
        assert!(t.update(None, t0 + Duration::from_secs(5)).is_some());
    }

    #[test]
    fn pipeline_stage_change_keeps_start_time() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("cargo"), t0);
        assert_eq!(t.update(Some("rustc"), t0 + Duration::from_secs(10)), None);
        let done = t.update(Some("zsh"), t0 + Duration::from_secs(60)).unwrap();
        assert_eq!(done.command, "rustc");
        assert_eq!(done.duration, Duration::from_secs(60)); // measured from t0
    }

    #[test]
    fn shell_path_is_recognized() {
        let mut t = StatusTracker::new();
        assert_eq!(t.update(Some("/bin/bash"), Instant::now()), None);
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    /// The bug this design replaced: a working agent that stops drawing must
    /// never drift into "waiting for you" on its own.
    #[test]
    fn quiet_agent_stays_running_without_a_signal() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), t0);
        for minute in 1..=10 {
            t.update(Some("claude"), t0 + Duration::from_secs(60 * minute));
        }
        match t.status() {
            SessionStatus::Running { command, since } => {
                assert_eq!(command, "claude");
                assert_eq!(*since, t0);
            }
            other => panic!("expected Running after 10 silent minutes, got {other:?}"),
        }
    }

    #[test]
    fn signal_marks_needs_input_and_keeps_start_time() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), t0);
        t.signal_needs_input();
        match t.status() {
            SessionStatus::NeedsInput { command, since } => {
                assert_eq!(command, "claude");
                assert_eq!(*since, t0); // start time survives the state change
            }
            other => panic!("expected NeedsInput, got {other:?}"),
        }
    }

    #[test]
    fn signal_is_ignored_when_nothing_is_running() {
        let mut t = StatusTracker::new();
        t.signal_needs_input();
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    #[test]
    fn needs_input_survives_polling() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), t0);
        t.signal_needs_input();
        t.update(Some("claude"), t0 + Duration::from_secs(2));
        t.update(Some("claude"), t0 + Duration::from_secs(4));
        assert!(matches!(t.status(), SessionStatus::NeedsInput { .. }));
    }

    #[test]
    fn acknowledging_returns_to_running() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), t0);
        t.signal_needs_input();
        t.acknowledge();
        match t.status() {
            SessionStatus::Running { command, since } => {
                assert_eq!(command, "claude");
                assert_eq!(*since, t0);
            }
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[test]
    fn needs_input_returns_to_idle_when_process_exits() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), t0);
        t.signal_needs_input();
        let done = t.update(None, t0 + Duration::from_secs(25)).unwrap();
        assert_eq!(done.command, "claude");
        assert_eq!(done.duration, Duration::from_secs(25));
        assert_eq!(*t.status(), SessionStatus::Idle);
    }

    #[test]
    fn a_different_command_taking_over_clears_needs_input() {
        let mut t = StatusTracker::new();
        let t0 = Instant::now();
        t.update(Some("claude"), t0);
        t.signal_needs_input();
        t.update(Some("cargo"), t0 + Duration::from_secs(5));
        match t.status() {
            SessionStatus::Running { command, .. } => assert_eq!(command, "cargo"),
            other => panic!("expected Running, got {other:?}"),
        }
    }
}
