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

const CHANNEL_KEY: &str = "preferredNotifChannel";
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
    let path = settings_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false; // no file yet — a fresh one is safe to write
    };
    if text.trim().is_empty() {
        return false;
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(settings)) => settings.contains_key(CHANNEL_KEY),
        _ => true,
    }
}

/// Merges the channel setting into Claude Code's settings file, leaving every
/// other key as it was.
pub fn enable_notifications() -> Result<()> {
    let path = settings_path();
    let mut settings: Map<String, Value> = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        _ => Map::new(),
    };
    settings.insert(CHANNEL_KEY.to_string(), Value::from(CHANNEL_VALUE));

    let parent = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut json = serde_json::to_string_pretty(&Value::Object(settings))?;
    json.push('\n');
    std::fs::write(&path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `enable_notifications` writes through `settings_path()`, which is
    /// $HOME-derived; the merge itself is the part worth pinning down, so it is
    /// exercised directly here.
    fn merge(existing: Option<&str>) -> Result<Map<String, Value>> {
        let mut settings: Map<String, Value> = match existing {
            Some(text) if !text.trim().is_empty() => serde_json::from_str(text)?,
            _ => Map::new(),
        };
        settings.insert(CHANNEL_KEY.to_string(), Value::from(CHANNEL_VALUE));
        Ok(settings)
    }

    #[test]
    fn merging_keeps_existing_settings() {
        let settings = merge(Some(r#"{"model": "opus", "verbose": true}"#)).unwrap();
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["verbose"], true);
        assert_eq!(settings[CHANNEL_KEY], CHANNEL_VALUE);
    }

    #[test]
    fn merging_into_nothing_writes_only_the_channel() {
        let settings = merge(None).unwrap();
        assert_eq!(settings.len(), 1);
        assert_eq!(settings[CHANNEL_KEY], CHANNEL_VALUE);
    }

    #[test]
    fn a_file_hive_cannot_parse_is_left_alone() {
        // Claude Code accepts comments in settings.json; rewriting such a file
        // through serde_json would silently delete them, so Hive treats it as
        // already configured and never offers.
        assert!(merge(Some("{ // a comment\n}")).is_err());
    }
}
