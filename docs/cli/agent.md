# tes agent

Manage active agent and terminal sessions and isolated Copy-on-Write workspaces.

## Usage

```bash:no-line-numbers
tes agent <subcommand> [arguments]
```

Alias: `tes session <subcommand>`

## Subcommands

- `tes agent spawn <name> [--intent <TEXT>] [--scope <PREFIX>...]` - Spawn an isolated Copy-on-Write workspace at `.tessera/workspaces/<name>` (optionally restricted to `--scope` directory prefixes).
- `tes agent list` - List all active agent and terminal sessions with their slot number, reverse-hex Change-ID, head commit, and intent.
- `tes agent focus <name>` (alias: `goto`) - Focus the originating terminal tab, multiplexer pane (`tmux`, `zellij`, `WezTerm`, `kitty`), or IDE window for a session ID, conversation ID, or unique prefix.
- `tes agent exec <name> -- <command...>` - Run a command inside an agent's workspace with isolated `TES_PORT` (`PORT` when `port_isolation = true`), `TES_DB_NAME`, and `TMPDIR`.
- `tes agent close [name] [--all]` - Close a specific agent session (defaults to the current session if `<name>` is omitted, or all sessions with `--all`) and remove its isolated Copy-on-Write workspace.
- `tes agent clear [-f | --force]` (alias: `prune`) - Clear idle or finished sessions. Pass `-f` / `--force` to also clear sessions with uncommitted changes.

## Examples

```bash:no-line-numbers
# list all active sessions
tes agent list

# jump directly to a terminal or IDE session
tes session focus term-48210

# spawn an isolated CoW workspace for a background task
tes agent spawn docs-worker --intent "Update API reference" --scope docs

# run tests inside a spawned agent workspace
tes agent exec docs-worker -- npm test

# close a specific session or prune idle sessions
tes agent close docs-worker
tes agent clear
tes agent clear --force
```
