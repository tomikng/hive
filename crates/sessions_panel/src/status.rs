use std::time::{Duration, Instant};

const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "nu", "tcsh", "csh", "dash"];

#[derive(Clone, Debug, PartialEq)]
pub enum SessionStatus {
    Idle,
    Running { command: String, since: Instant },
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
    /// `Terminal::foreground_process_command_name`, hive/crates/terminal/src/terminal.rs:2767).
    /// Returns Some when a tracked command just finished.
    pub fn update(&mut self, foreground: Option<&str>, now: Instant) -> Option<CommandFinished> {
        let running = foreground.filter(|name| !is_shell(name));
        match (&self.status, running) {
            (SessionStatus::Idle, Some(cmd)) => {
                self.status = SessionStatus::Running { command: cmd.to_string(), since: now };
                None
            }
            (SessionStatus::Running { command, since }, None) => {
                let finished = CommandFinished { command: command.clone(), duration: now - *since };
                self.status = SessionStatus::Idle;
                Some(finished)
            }
            (SessionStatus::Running { command, since }, Some(cmd)) if command != cmd => {
                // ponytail: pipeline/subcommand handoff keeps the original start time
                self.status = SessionStatus::Running { command: cmd.to_string(), since: *since };
                None
            }
            _ => None,
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
}
