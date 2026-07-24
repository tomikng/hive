# Hive saved commands

Hive reuses Zed's task system for per-project saved commands. Create
`.zed/tasks.json` in a project:

```json
[
  { "label": "claude", "command": "claude", "cwd": "${ZED_WORKTREE_ROOT}", "use_new_terminal": true },
  { "label": "worktree: prune", "command": "git worktree prune" }
]
```

Run them with `cmd-shift-r` (task: spawn) or from the command palette.

Variables: `${ZED_WORKTREE_ROOT}`, `${ZED_FILE}`, `${ZED_GIT_REF}`, and other
`ZED_*` vars (see `crates/task/src/task.rs`, `VariableName`).
