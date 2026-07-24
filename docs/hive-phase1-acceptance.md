# Hive Phase 1 — Acceptance Checklist

The app bundle:

    target/aarch64-apple-darwin/debug/bundle/osx/Hive Dev.app

Launch it (first run: right-click → Open to get past Gatekeeper, since it's ad-hoc signed):

    open "target/aarch64-apple-darwin/debug/bundle/osx/Hive Dev.app"

Run these checks. Each maps to a Phase 1 feature.

| # | Step | Expected |
|---|------|----------|
| 1 | Launch the app, look at the title bar | Shows **Hive Dev** identity; **no sign-in button**, no collab/call controls |
| 2 | Open a git repo folder | The left dock shows the **Sessions panel** with the project name |
| 3 | Press `cmd-t` twice | Two terminal sessions appear under the project; clicking one activates its terminal |
| 4 | In a session run `sleep 35`, then switch to another app before it ends | When it finishes: a **native macOS notification** "sleep finished" fires (first time, accept the notification permission prompt) |
| 5 | Run `sleep 35` again but keep Hive focused (look at another pane) | An **in-app toast** appears when it finishes (instead of the native notification) |
| 6 | While a command runs, watch the session's dot in the sidebar | Dot is **orange while running**, hidden when idle |
| 7 | Press `cmd-shift-n` | Zed's **worktree creation** flow opens (branch prompt / picker) |
| 8 | Focus a terminal, edit a tracked file, press `cmd-d` | The **"Uncommitted Changes" diff** opens; the Commit button works |
| 9 | In a terminal, press `cmd-e` | The **file finder** opens; picking a file opens an editor tab |
| 10 | Add `.zed/tasks.json` (see `docs/hive-saved-commands.md`), press `cmd-shift-r` while a terminal is focused | The **task picker** lists your saved commands and runs the chosen one in a terminal |
| 11 | Quit and relaunch | Terminals are **restored** (Zed's existing session serialization) |

Notes:
- Notification threshold is 30s by default; set `HIVE_NOTIFY_SECS` to change it.
- Native notifications only work from this bundled `.app`, not from `cargo run`.
- If step 1 shows no title bar at all, the title-bar restore fix (commit 7ec6689) did not take — reopen an issue.

## Deferred to Phase 2 (not expected to work yet)
- Warp-style command blocks; the modern multi-line input editor.
- Auto-split editor beside the terminal on `cmd-e` (currently opens as a tab).
- Per-session diff-stat (±files) in the sidebar; needs-input / failed status badges.
- Dragging the Sessions panel to the right dock (it stays left).
