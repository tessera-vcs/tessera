# tes oplog

Inspect the lock-free, content-addressed Operation Log history.

## Usage

```bash:no-line-numbers
tes oplog [options]
```

## Options

- `-n, --limit <N>` - Maximum number of operations to display (default: `20`).
- `--json` - Emit structured JSON operation entries.

## Description

Every state transition in `.git/tessera` (initialization, session claims, file reconciliations, checkpoints, commits, stashes, restores, and merges) is recorded as an immutable Blake3-hashed operation in `.git/tessera/oplog/`.

`tes oplog` lists recent operations newest-first, showing each operation's short `OpId`, session ID, classification (`milestone` or `ephemeral`), and description. Pass any `OpId` prefix to `tes undo --op <op-id>` to roll back repository state atomically.

## Examples

```bash:no-line-numbers
# view the 20 most recent operations
tes oplog

# view the last 50 operations without paging
tes -P oplog -n 50

# emit operation history as JSON
tes oplog --json
```
