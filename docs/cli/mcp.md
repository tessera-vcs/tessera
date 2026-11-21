# tes mcp

Start the built-in Model Context Protocol (MCP) JSON-RPC 2.0 server over `stdio`.

## Usage

```bash:no-line-numbers
tes mcp
```

## Description

`tes mcp` exposes Tessera's multi-agent coordination, branch lifecycle, conflict resolution, and diagnostics capabilities as a local MCP server over `stdin`/`stdout`. `tes init` automatically registers `tes mcp` in supported editor configurations (`.mcp.json`, `.cursor/mcp.json`, `.kiro/settings/mcp.json`, `opencode.json`, `.codex/config.toml`, and `.vscode/mcp.json`).

> **Security Note:** To prevent untrusted MCP tool calls from executing arbitrary shell commands, `merge` and `commit` with `verify: true` only run the repository's configured and trusted `verify_command` (set via `tes config --verify-cmd` or trusted `tessera.toml`).

## MCP Tools

| Tool | Input Schema | Description |
| ---- | ------------ | ----------- |
| `status` | `{ agent_id? }` | Snapshot and return the session's status, Change-ID, commit SHA, intent, and claimed symbols |
| `radar` | `{}` | Return live multi-agent radar showing active sessions, intents, and overlapping edits |
| `claim` | `{ intent?, branch?, from?, symbols?, agent_id? }` | Claim a task intent, optional target branch and parent base (`from`), and file/symbol paths |
| `commit` | `{ message?, branch?, amend?, author?, co_author?, no_co_author?, verify?, agent_id? }` | Commit the session's isolated changes onto its target branch |
| `diff` | `{ branch?, staged?, rev_range?, paths?, agent_id? }` | Inspect unified diffs for the session, staged edits, branch, or revision range |
| `log` | `{ branch?, limit? }` | Return commit history for a branch |
| `oplog` | `{ limit? }` | Return recent entries from the lock-free Operation Log |
| `push` | `{ remote?, branch?, force?, tags?, dry_run?, agent_id? }` | Push the session's active (or specified) branch to a Git remote |
| `pull` | `{ remote?, branch?, agent_id? }` | Fetch and fast-forward or 3-way AST-merge a remote branch and rebase active sessions |
| `checkpoint` | `{ label, agent_id? }` | Create a named atomic checkpoint in the Operation Log |
| `restore` | `{ source?, branch?, paths?, agent_id? }` | Restore a branch or specific paths to a commit SHA, `HEAD~N`, branch, or checkpoint |
| `branch` | `{ action?: "list"\|"switch"\|"delete"\|"rename", branch?, new_branch?, from?, force?, agent_id? }` | List, switch, delete, or rename branches across Git refs, sandboxes, and sessions |
| `stash` | `{ action?: "push"\|"pop"\|"apply"\|"list"\|"drop"\|"clear", message?, branch?, paths?, index?, agent_id? }` | Save or reapply session-isolated uncommitted changes via 3-way AST merge |
| `cherry-pick` | `{ commit, branch?, agent_id? }` | Apply the delta of an existing commit onto the target branch via 3-way AST merge |
| `conflicts` | `{}` | Return all active first-class conflicts with AST snippets and competing intents |
| `resolve` | `{ path, strategy: "ours"\|"theirs"\|"ast-union"\|"working"\|"content", content? }` | Resolve a conflict at `path` using the specified strategy |
| `sync` | `{}` | Non-blocking auto-rebase of all active sessions onto the latest trunk commit |
| `undo` | `{ op?, checkpoint? }` | Atomically roll back repository state to the previous operation, an `OpId` prefix, or a checkpoint |
| `merge` | `{ agents?, into?, message?, verify?, keep_open?, dry_run?, author?, co_author?, no_co_author? }` | Semantically merge sessions or branches into a target branch (or preview with `dry_run`) |
| `agent` | `{ action?: "list"\|"close"\|"clear", agent_id?, force? }` | List, close, or clear active agent sessions |
| `config` | `{ action?: "show" }` | Inspect effective repository configuration |
| `doctor` | `{}` | Run repository health, config trust, filesystem, and integration diagnostics |

## MCP Resources

The server also exposes read-only MCP resources via `resources/list` and `resources/read`:

- `tessera://status` - Current session status and branch state snapshot.
- `tessera://radar` - Live multi-agent symbol collision radar across all active sessions.
- `tessera://conflicts` - Active first-class conflicts with snippets and competing agent intents.

## MCP Prompts

The server exposes reusable MCP prompts via `prompts/list` and `prompts/get`:

- `resolve-conflicts` - Structured workflow prompt for inspecting active conflicts, comparing competing intents and snippets, and resolving them safely.
