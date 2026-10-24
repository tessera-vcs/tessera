# tes undo

Undo the most recent operation or roll back to a specific operation ID.

## Usage

```bash:no-line-numbers
tes undo [options]
```

## Options

- `--op <OP_ID>` - Roll back to a specific operation ID from `tes oplog`.
- `--checkpoint <LABEL>` - Roll back to a named checkpoint created with `tes checkpoint <LABEL>`.

## Description

Every state change in `.git/tessera` is recorded in the operation log (`tes oplog`). Running `tes undo` reverses the most recent operation and restores the working tree and branch refs to their prior state.

## Examples

```bash:no-line-numbers
# view recent operations
tes oplog

# undo the most recent operation
tes undo

# roll back to a specific operation ID
tes undo --op 8f3a1c2

# roll back to a named checkpoint
tes undo --checkpoint before-refactor
```
