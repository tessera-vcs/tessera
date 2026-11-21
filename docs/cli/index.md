# CLI Commands

The `tes` CLI provides commands for initializing repositories, coordinating multiple AI coding agents, managing per-terminal branches, running sandboxed builds, and merging branches.

## Command Overview

| Command | Purpose |
|---------|---------|
| [tes init](/cli/init) | Initialize Tessera in a Git repository, install Git hooks, and configure AI editor rules (`--harness`, `--local`) |
| [tes env](/cli/env) | Configure shell prompt integration (`(tes:<branch>)`), tab completion, and sandbox routing |
| [tes switch](/cli/switch) | Switch the active terminal session or editor working tree to a branch |
| [tes branch](/cli/branch) | List (`-l`, `-v`), create, rename (`-m`), or delete (`-d`/`-D`) branches and sandboxes |
| [tes claim](/cli/claim) | Claim a task intent, target branch, and file paths for an agent session |
| [tes status](/cli/status) | Show session changes, peer changes on the same branch, and branch commits (`-s/--short/--porcelain`, `-b`) |
| [tes diff](/cli/diff) | Show unified diffs or diffstats (`--stat`, `--name-only`, `--name-status`, `-U`, `-w`) for the session, working tree (`--all`), branch, or `<revA>..<revB>` |
| [tes stash](/cli/stash) | Session-isolated AST stash (`push`, `pop`, `apply`, `list`, `drop`, `clear`) |
| [tes radar](/cli/radar) | Show all active AI agent and terminal sessions in real time |
| [tes exec](/cli/exec) | Run a build, test, or server command inside a branch's Copy-on-Write sandbox |
| [tes merge](/cli/merge) | Semantically merge sessions or multiple feature branches into a target branch (`--dry-run`, `--verify`) |
| [tes cherry-pick](/cli/cherry-pick) | Apply a commit (`<commit>`) onto the active branch via 3-way AST merge |
| [tes commit](/cli/commit) | Commit the active session's changes onto its target branch (`-m`, `--amend`, `--no-edit`, `--author`, `--co-author`) |
| [tes pull](/cli/pull) | Fetch and fast-forward or 3-way AST-merge a remote branch while rebasing sessions |
| [tes push](/cli/push) | Synchronize session changes and push a branch to a Git remote |
| [tes log](/cli/log) | Show commit history with branch refs, patches (`-p`), diffstats (`--stat`), graphs (`--graph`), filters (`--grep`, `-S`, `--author`), and pathspecs (`-- <paths...>`) |
| [tes conflicts](/cli/conflicts) | List unresolved merge conflicts |
| [tes resolve](/cli/resolve) | Resolve a conflicted file using `ast-union`, `ours`, `theirs`, `working`, or `content` (`--content`, `--content-file`) |
| [tes checkpoint](/cli/checkpoint) | Save a named checkpoint in the operation log |
| [tes restore](/cli/restore) | Restore a branch or specific files to a checkpoint, commit, or branch |
| [tes oplog](/cli/oplog) | Inspect the content-addressed Operation Log history |
| [tes undo](/cli/undo) | Undo the most recent operation or roll back to a specific operation ID or checkpoint |
| [tes sync](/cli/sync) | Auto-rebase all active sessions onto the latest trunk commit |
| [tes gc](/cli/gc) | Compact ephemeral micro-snapshot operations in `.git/tessera/oplog/` |
| [tes watch](/cli/watch) | Watch the working tree and auto-reconcile edits for hookless editors or harnesses |
| [tes agent](/cli/agent) (`session`) | Spawn, list, focus (`tes session focus <id>`), exec, close, or clear sessions |
| [tes config](/cli/config) | View or update repository configuration settings and trust (`tessera.toml`) |
| [tes doctor](/cli/doctor) | Run repository health, config trust, filesystem CoW, hook, and session diagnostics |
| [tes upgrade](/cli/upgrade) (`update`) | Check for (`--check`) or install the latest Tessera CLI release in place with SHA-256 verification |
| [tes mcp](/cli/mcp) | Start the built-in Model Context Protocol (MCP) JSON-RPC 2.0 server over `stdio` |

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

For AI coding editors that support the Model Context Protocol (MCP), [tes mcp](/cli/mcp) starts a JSON-RPC 2.0 server over `stdio` exposing the following tools, resources, and prompts:

### Tools

| Tool | Input Schema | Description |
|------|--------------|-------------|
| `status` | `{ agent_id? }` | Snapshot and return session status, branch state, and claimed symbols |
| `radar` | `{}` | Return all active agent sessions, target branches, and overlapping edits |
| `claim` | `{ intent?, branch?, from?, symbols?, agent_id? }` | Claim a task intent, optional target branch and parent base, and file/symbol list |
| `commit` | `{ message?, branch?, amend?, dry_run?, author?, co_author?, no_co_author?, verify?, agent_id? }` | Commit the session's isolated changes onto its target branch |
| `diff` | `{ branch?, staged?, all?, range?, stat?, name_status?, paths?, agent_id? }` | Inspect unified diffs for the session, staged edits, branch, or revision range |
| `log` | `{ branch?, limit?, agent_id? }` | Return commit history for a branch |
| `oplog` | `{ limit? }` | Return recent entries from the Operation Log |
| `push` | `{ remote?, branch?, message?, set_upstream?, force?, force_with_lease?, tags?, delete?, dry_run?, no_auto_commit?, agent_id? }` | Push the active (or specified) branch to a Git remote |
| `pull` | `{ remote?, branch?, agent_id? }` | Fetch and fast-forward or 3-way AST-merge a remote branch and rebase sessions |
| `checkpoint` | `{ label, agent_id? }` | Create a named atomic checkpoint in the Operation Log |
| `restore` | `{ source?, revision?, branch?, paths?, agent_id? }` | Restore a branch or file paths to a commit, branch, or checkpoint |
| `branch` | `{ action?: "list"\|"switch"\|"delete"\|"rename", branch?, new_branch?, from?, force?, agent_id? }` | List, switch, delete, or rename branches across Git refs, sandboxes, and sessions |
| `stash` | `{ action?: "push"\|"pop"\|"apply"\|"list"\|"drop"\|"clear", message?, branch?, paths?, index?, agent_id? }` | Session-isolated AST stash operations |
| `cherry-pick` | `{ commit, branch?, agent_id? }` | 3-way AST cherry-pick a commit onto a target branch |
| `conflicts` | `{}` | List active structural conflicts with base, ours, and theirs snippets |
| `resolve` | `{ path, strategy: "ours"\|"theirs"\|"ast-union"\|"working"\|"content", content? }` | Resolve a conflict at `path` using the chosen strategy |
| `sync` | `{}` | Non-blocking auto-rebase of active sessions onto the latest trunk commit |
| `undo` | `{ op?, checkpoint? }` | Atomically roll back to the previous operation, an `OpId` prefix, or a checkpoint |
| `merge` | `{ agents?, into?, message?, verify?, keep_open?, dry_run?, author?, co_author?, no_co_author? }` | Commit the session or AST-merge multiple branches into a target branch |
| `agent` | `{ action?: "list"\|"close"\|"clear", name?, all?, force?, agent_id? }` | List, close, or clear active agent sessions |
| `config` | `{ action?: "show" }` | Inspect effective repository configuration |
| `doctor` | `{}` | Run repository health, config trust, filesystem, and integration diagnostics |

### Resources & Prompts

- **Resources**: `tessera://status`, `tessera://radar`, `tessera://conflicts`
- **Prompt**: `resolve-conflicts`

---

## Configuration

Tessera projects can be configured via `tessera.toml` or `tes config`.

**See:** [Configuration Guide](/guides/configuration) for complete documentation.
