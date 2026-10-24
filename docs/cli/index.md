# CLI Commands

The `tes` CLI provides commands for initializing repositories, coordinating multiple AI coding agents, managing per-terminal branches, running sandboxed builds, and merging branches.

## Command Overview

| Command | Purpose |
|---------|---------|
| [tes init](/cli/init) | Initialize Tessera in a Git repository and install AI editor rules |
| [tes env](/cli/env) | Configure shell prompt integration (`(tes:<branch>)`) and tab completion |
| [tes switch](/cli/switch) | Switch the active terminal session or editor working tree to a branch |
| [tes branch](/cli/branch) | List (`-l`, `-v`), create, rename (`-m`), or delete (`-d`/`-D`) branches and sandboxes |
| [tes claim](/cli/claim) | Claim a task intent, target branch, and file paths for an agent session |
| [tes status](/cli/status) | Show session changes, peer changes on the same branch, and branch commits (`-s/--short/--porcelain`, `-b`) |
| [tes diff](/cli/diff) | Show unified diffs or diffstats (`--stat`, `--name-only`, `--name-status`, `-U`, `-w`) for the session, working tree (`--all`), branch, or `<revA>..<revB>` |
| [tes stash](/cli/stash) | Session-isolated AST stash (`push`, `pop`, `apply`, `list`, `drop`, `clear`) |
| [tes radar](/cli/radar) | Show all active AI agent and terminal sessions in real time |
| [tes exec](/cli/exec) | Run a build, test, or server command inside a branch's Copy-on-Write sandbox |
| [tes merge](/cli/merge) | Semantically merge sessions or multiple feature branches into a target branch |
| [tes cherry-pick](/cli/cherry-pick) | Apply a commit (`<rev>`) onto the active branch via 3-way AST merge |
| [tes commit](/cli/commit) | Commit the active session's changes onto its target branch (`-m`, `--amend`, `--no-edit`, `--author`, `--co-author`) |
| [tes pull](/cli/pull) | Fetch and fast-forward or 3-way AST-merge a remote branch while rebasing sessions |
| [tes push](/cli/push) | Synchronize session changes and push a branch to a Git remote |
| [tes log](/cli/log) | Show commit history with branch refs, patches (`-p`), diffstats (`--stat`), graphs (`--graph`), filters (`--grep`, `-S`, `--author`), and pathspecs (`-- <paths...>`) |
| [tes conflicts](/cli/conflicts) | List unresolved merge conflicts |
| [tes resolve](/cli/resolve) | Resolve a conflicted file using `ast-union`, `ours`, `theirs`, or `working` |
| [tes checkpoint](/cli/checkpoint) | Save a named checkpoint in the operation log |
| [tes restore](/cli/restore) | Restore a branch or specific files to a checkpoint, commit, or branch |
| [tes undo](/cli/undo) | Undo the most recent operation or roll back to a specific operation ID |
| [tes config](/cli/config) | View or update repository configuration settings |
| `tes session focus <id>` | Focus the originating terminal tab, multiplexer pane, or IDE window for an active session |

---

## Global Options

These options can be used with any `tes` command:

| Option | Description |
|--------|-------------|
| `--json` | Emit structured JSON output (never paged) |
| `-P, --no-pager` | Do not pipe output into a pager |
| `--paginate` | Pipe output into a pager when `stdout` is an interactive terminal (even for commands like `tes status` and `tes radar` that do not page by default) |
| `-C, --repo <PATH>` | Run against the repository at `<PATH>` |
| `-V, --version` | Display the `tes` version |
| `-h, --help` | Show help for a command |

By default, `tes diff`, `tes log`, `tes oplog`, `tes branch` (when listing), and `tes stash list` automatically pipe output through a pager (`TES_PAGER` -> `tessera.toml` `pager` -> `GIT_PAGER` -> `git config core.pager` -> `PAGER` -> `less -FRX`) when `stdout` is an interactive terminal. `tes status`, `tes radar`, and `tes conflicts` print directly to the terminal unless `--paginate` is passed.

---

## MCP Server

For AI coding editors that support the Model Context Protocol (MCP), `tes mcp` starts a JSON-RPC 2.0 server over `stdio` exposing the following tools:

| Tool | Description |
|------|-------------|
| `claim` | Claim a task intent, target branch (`branch`), and file/symbol list (`symbols`) |
| `status` | Reconcile edits and return session status and branch state |
| `radar` | Return all active agent sessions, target branches, and claimed symbols |
| `branch` | List, delete (`delete` / `force_delete`), or rename (`rename`) branches |
| `stash` | Session-isolated AST stash (`push`, `pop`, `apply`, `list`, `drop`, `clear`) |
| `pull` | Fetch and fast-forward or 3-way AST-merge a remote branch and rebase sessions |
| `cherry-pick` | 3-way AST cherry-pick a commit (`rev`) onto a target branch |
| `conflicts` | List active structural conflicts with base, ours, and theirs snippets |
| `resolve` | Resolve a conflict using `ast-union`, `ours`, `theirs`, or `working` |
| `checkpoint` | Create a named checkpoint in the operation log |
| `restore` | Restore a branch or file paths to a commit, branch, or checkpoint |
| `undo` | Roll back the latest operation or restore to a specific `op_id` |
| `sync` | Synchronize the current session with the latest branch `HEAD` |
| `merge` | Commit the session or AST-merge multiple branches into a target branch |

---

## Configuration

Tessera projects can be configured via `tessera.toml` or `tes config`.

**See:** [Configuration Guide](/guides/configuration) for complete documentation.
