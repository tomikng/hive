# Hive git control

Git control, commit history, and stash management are Zed's native git UI,
surfaced here for a terminal-first workflow. No new git logic — this is
keybindings + defaults over Zed's existing `git_ui`/`git_graph` crates.

The git panel is docked on the right (`"dock": "right"` in
`assets/settings/default.json`) so it doesn't collide with the sessions
panel on the left.

## Shortcuts (macOS)

| Key | Action | What it does |
| --- | --- | --- |
| `ctrl-shift-g` | `git_panel::ToggleFocus` | Toggle/focus the git panel (already a Zed default) |
| `cmd-1` (in git panel) | `git_panel::ActivateChangesTab` | Switch to the Changes tab |
| `cmd-2` (in git panel) | `git_panel::ActivateHistoryTab` | Switch to the commit History tab |
| `ctrl-shift-h` | `git_graph::Open` | Open the standalone Git Graph tab (visual commit log) |
| `ctrl-shift-s` | `git::ViewStash` | Open the stash viewer/picker |
| `cmd-enter` | `git::Commit` | Commit staged changes |
| `cmd-shift-enter` | `git::Amend` | Amend the last commit |
| `ctrl-g ctrl-g` / `ctrl-g up` / `ctrl-g down` | `git::Fetch` / `git::Push` / `git::Pull` | Fetch/push/pull (while git panel is focused) |

`git_panel::ActivateHistoryTab` and the stash sub-actions
(`stash_picker::ShowStashItem`, `stash_picker::DropStashItem`, and
`git::StashAll`/`StashPop`/`StashApply`, reachable from the command palette
or the git panel's stash entries) were already bound or reachable in Zed's
default keymap; only `git_graph::Open` and `git::ViewStash` had no binding
anywhere, so those two are the only new keys added.

All of the above are standard Zed actions — see `crates/git_ui` and
`crates/git_graph` — reused as-is.
