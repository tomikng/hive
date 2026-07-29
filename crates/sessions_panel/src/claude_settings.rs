//! Turning on Claude Code's notification channel.
//!
//! Claude Code only emits notifications when `preferredNotifChannel` is set.
//! Its default, `auto`, picks a channel from `$TERM_PROGRAM` and recognises
//! only Apple Terminal, iTerm2, kitty and ghostty — so inside Hive it resolves
//! to "no method available" and the agent stays silent. Setting the channel to
//! `iterm2_with_bell` makes it emit `ESC ] 9 ; <message> BEL`, which the PTY
//! tee turns into a [`terminal::Event::Notification`].
//!
//! Hive offers this once and writes it only when the user accepts.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::{Map, Value};

use crate::status;

const CHANNEL_KEY: &str = "preferredNotifChannel";
const HOOKS_KEY: &str = "hooks";
/// Claude Code fires this when the user submits a prompt: the turn begins.
const TURN_START_EVENT: &str = "UserPromptSubmit";
/// ...and this when the main agent has finished responding: the turn ends.
const TURN_END_EVENT: &str = "Stop";
/// Notify *and* ring: the bell is what agents other than Claude use, and Hive
/// falls back to it when a session has never sent an OSC notification.
const CHANNEL_VALUE: &str = "iterm2_with_bell";

/// Claude Code's user settings file, honouring `CLAUDE_CONFIG_DIR`.
fn settings_path() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join("settings.json"),
        None => util::paths::home_dir()
            .join(".claude")
            .join("settings.json"),
    }
}

/// Whether to leave Claude Code's settings alone: the user already chose a
/// channel, or the file is something Hive must not rewrite (unparseable, or not
/// a JSON object — Claude Code accepts comments, `serde_json` does not, and
/// rewriting would delete them).
pub fn notifications_already_configured() -> bool {
    already_configured(std::fs::read_to_string(settings_path()).ok().as_deref())
}

/// Merges the channel setting into Claude Code's settings file, leaving every
/// other key as it was.
pub fn enable_notifications() -> Result<()> {
    let path = settings_path();
    let existing = std::fs::read_to_string(&path).ok();
    let json = with_channel(existing.as_deref())
        .with_context(|| format!("merging into {}", path.display()))?;

    let parent = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(parent)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// `text` is the settings file's current contents, or `None` when there is no
/// file. Missing or empty means "go ahead"; anything Hive can't parse back out
/// as a JSON object means "don't touch it".
fn already_configured(text: Option<&str>) -> bool {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        return false; // no file yet — a fresh one is safe to write
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(settings)) => settings.contains_key(CHANNEL_KEY),
        _ => true,
    }
}

/// The file Hive would write, given its current contents. Errors rather than
/// discarding anything it cannot round-trip.
fn with_channel(text: Option<&str>) -> Result<String> {
    let mut settings: Map<String, Value> = match text.filter(|text| !text.trim().is_empty()) {
        Some(text) => serde_json::from_str(text)?,
        None => Map::new(),
    };
    settings.insert(CHANNEL_KEY.to_string(), Value::from(CHANNEL_VALUE));
    add_turn_hooks(&mut settings);
    let mut json = serde_json::to_string_pretty(&Value::Object(settings))?;
    json.push('\n');
    Ok(json)
}

/// Adds the two hooks that bracket a turn, leaving every other hook alone.
///
/// The marker goes to `/dev/tty` rather than stdout: Claude Code reads a
/// hook's stdout itself (`UserPromptSubmit` output is fed back as context), so
/// anything printed there never reaches the terminal Hive is watching.
fn add_turn_hooks(settings: &mut Map<String, Value>) {
    let hooks = settings
        .entry(HOOKS_KEY)
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        return; // not a shape Hive understands; leave it be
    };

    for (event, marker) in [
        (TURN_START_EVENT, status::TURN_START_MARKER),
        (TURN_END_EVENT, status::TURN_END_MARKER),
    ] {
        let command = marker_command(marker);
        let entries = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        // Compare the command itself, not the serialised entry: the marker is
        // full of backslashes, which JSON escapes and a string match wouldn't.
        let installed = entries.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|hooks| {
                    hooks.iter().any(|hook| {
                        hook.get("command").and_then(Value::as_str) == Some(command.as_str())
                    })
                })
        });
        if installed {
            continue;
        }
        entries.push(serde_json::json!({
            "hooks": [{ "type": "command", "command": command }]
        }));
    }
}

/// Writing the marker must never fail a turn, hence the redirect and `|| true`.
fn marker_command(marker: &str) -> String {
    format!("printf '\\033]9;{marker}\\007' > /dev/tty 2>/dev/null || true")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(existing: Option<&str>) -> Map<String, Value> {
        serde_json::from_str(&with_channel(existing).unwrap()).unwrap()
    }

    #[test]
    fn merging_keeps_existing_settings() {
        let settings = written(Some(r#"{"model": "opus", "verbose": true}"#));
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["verbose"], true);
        assert_eq!(settings[CHANNEL_KEY], CHANNEL_VALUE);
    }

    #[test]
    fn merging_into_nothing_writes_the_channel_and_the_turn_hooks() {
        for empty in [None, Some(""), Some("   \n")] {
            let settings = written(empty);
            assert_eq!(settings.len(), 2, "from {empty:?}");
            assert_eq!(settings[CHANNEL_KEY], CHANNEL_VALUE);
            let hooks = settings[HOOKS_KEY].as_object().unwrap();
            assert_eq!(hooks.len(), 2, "from {empty:?}");
        }
    }

    #[test]
    fn turn_hooks_are_installed_once_and_keep_existing_hooks() {
        let existing = r#"{
            "hooks": {
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "mine.sh"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "theirs.sh"}]}]
            }
        }"#;
        let settings = written(Some(existing));
        let hooks = settings[HOOKS_KEY].as_object().unwrap();

        // The user's own hooks survive, on the events Hive also writes to.
        assert_eq!(hooks["SessionStart"].as_array().unwrap().len(), 1);
        let submit = hooks[TURN_START_EVENT].as_array().unwrap();
        assert_eq!(submit.len(), 2);
        assert!(submit[0].to_string().contains("mine.sh"));
        assert!(submit[1].to_string().contains(status::TURN_START_MARKER));
        assert!(
            hooks[TURN_END_EVENT].as_array().unwrap()[0]
                .to_string()
                .contains(status::TURN_END_MARKER)
        );

        // Writing again adds nothing.
        let twice = written(Some(&serde_json::to_string(&settings).unwrap()));
        assert_eq!(
            twice[HOOKS_KEY][TURN_START_EVENT].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn the_marker_command_writes_to_the_tty() {
        let command = marker_command(status::TURN_START_MARKER);
        // Claude Code reads a hook's stdout itself, so the marker has to go
        // straight to the terminal or Hive never sees it.
        assert!(command.contains("> /dev/tty"));
        assert!(command.contains(r"\033]9;hive:turn-start\007"));
    }

    #[test]
    fn an_existing_channel_is_left_alone() {
        assert!(already_configured(Some(
            r#"{"preferredNotifChannel": "terminal_bell"}"#
        )));
    }

    #[test]
    fn a_settings_file_without_a_channel_is_offered() {
        assert!(!already_configured(Some(r#"{"model": "opus"}"#)));
        assert!(!already_configured(None));
        assert!(!already_configured(Some("  ")));
    }

    #[test]
    fn a_file_hive_cannot_parse_is_left_alone() {
        // Claude Code accepts comments in settings.json; rewriting such a file
        // through serde_json would silently delete them. Hive reports it as
        // already configured so the offer never appears...
        assert!(already_configured(Some("{ // a comment\n}")));
        assert!(already_configured(Some("[1, 2]")));
        assert!(already_configured(Some("not json at all")));
        // ...and the write itself refuses too, rather than discarding keys.
        assert!(with_channel(Some("{ // a comment\n}")).is_err());
    }
}
