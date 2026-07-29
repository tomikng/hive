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
/// The events that bracket an agent's work, and the marker each one sends.
///
/// The same set Warp's own Claude Code plugin listens to. `UserPromptSubmit`
/// alone is not enough: approving a permission mid-turn submits no prompt, so
/// without `PostToolUse` the session would sit there looking idle while the
/// agent worked. The matcher narrows `Notification`, which also fires for
/// things that are not a request for the user.
const TURN_HOOKS: [(&str, Option<&str>, &str); 5] = [
    ("UserPromptSubmit", None, status::TURN_START_MARKER),
    ("PostToolUse", None, status::TURN_START_MARKER),
    ("Stop", None, status::TURN_END_MARKER),
    ("Notification", Some("idle_prompt"), status::TURN_END_MARKER),
    ("PermissionRequest", None, status::TURN_END_MARKER),
];
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
        // Both halves matter: the channel is how the agent says it wants you,
        // the hooks are how it says it's working. Stale hooks (an older Hive
        // wrote a command that no longer reaches the terminal) count as
        // missing, or the offer would never come back to repair them.
        Ok(Value::Object(settings)) => {
            settings.contains_key(CHANNEL_KEY) && turn_hooks_current(&settings)
        }
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
    // A channel the user picked themselves stays; Hive only fills the gap.
    settings
        .entry(CHANNEL_KEY)
        .or_insert_with(|| Value::from(CHANNEL_VALUE));
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

    for (event, matcher, marker) in TURN_HOOKS {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        // Drop Hive's own older markers rather than stacking a new one beside
        // them — the command has changed once already and will again.
        entries.retain(|entry| !mentions(entry, marker));
        let mut entry = serde_json::json!({
            "hooks": [{ "type": "command", "command": marker_command(marker) }]
        });
        if let Some(matcher) = matcher {
            entry["matcher"] = Value::from(matcher);
        }
        entries.push(entry);
    }
}

/// Whether the settings already carry the turn hooks Hive writes today.
fn turn_hooks_current(settings: &Map<String, Value>) -> bool {
    let Some(hooks) = settings.get(HOOKS_KEY).and_then(Value::as_object) else {
        return false;
    };
    TURN_HOOKS.iter().all(|(event, _matcher, marker)| {
        let command = marker_command(marker);
        hooks
            .get(*event)
            .and_then(Value::as_array)
            .is_some_and(|entries| entries.iter().any(|entry| has_command(entry, &command)))
    })
}

/// Whether a hook entry runs exactly `command`. Compares the command itself,
/// not the serialised entry: the marker is full of backslashes, which JSON
/// escapes and a string match wouldn't.
fn has_command(entry: &Value, command: &str) -> bool {
    hook_commands(entry).any(|candidate| candidate == command)
}

/// Whether a hook entry is one of Hive's, of any vintage.
fn mentions(entry: &Value, marker: &str) -> bool {
    hook_commands(entry).any(|command| command.contains(marker))
}

fn hook_commands(entry: &Value) -> impl Iterator<Item = &str> {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
}

/// Gets the marker to the terminal, by whichever of the two routes works.
///
/// Not `/dev/tty`: Claude Code runs hooks without a controlling terminal, so
/// opening it fails with ENXIO. Two ways around that, the same pair Warp's own
/// plugin uses:
///
/// - the agent process still owns the pty, and a hook may write to it, so the
///   command walks up its ancestors looking for one with a tty;
/// - failing that, Claude Code 2.1.141 and later emit a `terminalSequence`
///   field from a hook's JSON output for exactly this reason.
///
/// Order matters: older Claude Code rejects the unknown field ("JSON
/// validation failed"), so the JSON is only printed when the write didn't
/// happen. A marker that can't be delivered must never fail the turn either,
/// hence the swallowed errors and the trailing `true`.
fn marker_command(marker: &str) -> String {
    format!(
        "p=$PPID; for _ in 1 2 3; do \
t=$(ps -o tty= -p $p 2>/dev/null | tr -d ' '); \
case \"$t\" in ''|'??') p=$(ps -o ppid= -p $p 2>/dev/null | tr -d ' ');; \
*) printf '\\033]9;{marker}\\007' > \"/dev/$t\" 2>/dev/null && exit 0;; esac; \
[ -n \"$p\" ] || break; done; \
printf '%s' '{{\"terminalSequence\":\"\\u001b]9;{marker}\\u0007\"}}'; true"
    )
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
            assert_eq!(hooks.len(), TURN_HOOKS.len(), "from {empty:?}");
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
        let submit = hooks["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(submit.len(), 2);
        assert!(submit[0].to_string().contains("mine.sh"));
        assert!(mentions(&submit[1], status::TURN_START_MARKER));
        assert!(mentions(
            &hooks["Stop"].as_array().unwrap()[0],
            status::TURN_END_MARKER
        ));

        // Writing again adds nothing.
        let twice = written(Some(&serde_json::to_string(&settings).unwrap()));
        assert_eq!(twice[HOOKS_KEY]["UserPromptSubmit"].as_array().unwrap().len(), 2);
        assert!(turn_hooks_current(&twice));
    }

    #[test]
    fn a_stale_marker_is_replaced_not_stacked() {
        // What an earlier Hive wrote: same marker, a command that could never
        // reach the terminal.
        let existing = r#"{
            "preferredNotifChannel": "iterm2_with_bell",
            "hooks": {
                "UserPromptSubmit": [{"hooks": [{"type": "command",
                    "command": "printf '\\033]9;hive:turn-start\\007' > /dev/tty 2>/dev/null || true"}]}],
                "Stop": [{"hooks": [{"type": "command",
                    "command": "printf '\\033]9;hive:turn-end\\007' > /dev/tty 2>/dev/null || true"}]}]
            }
        }"#;
        // Stale hooks must bring the offer back, or there is no way to repair them.
        assert!(!already_configured(Some(existing)));

        let settings = written(Some(existing));
        let submit = settings[HOOKS_KEY]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(submit.len(), 1);
        assert!(!submit[0].to_string().contains("/dev/tty\""));
        assert!(turn_hooks_current(&settings));
    }

    #[test]
    fn the_marker_command_has_both_delivery_routes() {
        let command = marker_command(status::TURN_START_MARKER);
        // Claude Code runs hooks without a controlling terminal: /dev/tty is
        // ENXIO there, so the command finds the tty of an ancestor...
        assert!(!command.contains("> /dev/tty "));
        assert!(command.contains("ps -o tty="));
        assert!(command.contains(r#"> "/dev/$t""#));
        assert!(command.contains(r"\033]9;hive:turn-start\007"));
        // ...and only prints the JSON when that didn't happen, since older
        // Claude Code rejects the field.
        assert!(command.contains("&& exit 0"));
        assert!(command.contains("terminalSequence"));
    }

    #[test]
    fn the_json_route_is_valid_json_with_escaped_controls() {
        let command = marker_command(status::TURN_END_MARKER);
        let json = command
            .split_once("printf '%s' '")
            .unwrap()
            .1
            .split_once('\'')
            .unwrap()
            .0;
        let parsed: Value = serde_json::from_str(json).expect("hook must print valid JSON");
        assert_eq!(
            parsed["terminalSequence"],
            format!("\u{1b}]9;{}\u{7}", status::TURN_END_MARKER)
        );
    }

    #[test]
    fn the_notification_hook_is_narrowed_to_idle_prompts() {
        let settings = written(None);
        let notification = &settings[HOOKS_KEY]["Notification"].as_array().unwrap()[0];
        assert_eq!(notification["matcher"], "idle_prompt");
        // Everything else Hive writes stays unmatched.
        assert!(settings[HOOKS_KEY]["Stop"].as_array().unwrap()[0]
            .get("matcher")
            .is_none());
    }

    #[test]
    fn a_channel_the_user_picked_is_kept() {
        let settings = written(Some(r#"{"preferredNotifChannel": "terminal_bell"}"#));
        assert_eq!(settings[CHANNEL_KEY], "terminal_bell");
        // ...but the hooks are still missing, so the offer stands.
        assert!(!already_configured(Some(
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
