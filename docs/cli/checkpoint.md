# tes checkpoint

Save a named checkpoint in the operation log.

## Usage

```bash:no-line-numbers
tes checkpoint <name>
```

## Arguments

- `<name>` - Name for the checkpoint (e.g., `before-refactor`).

## Description

`tes checkpoint` snapshots the current working tree and session state and records a named checkpoint in `.git/tessera`. You can restore the repository back to this checkpoint at any time using `tes restore <name>`.

## Examples

```bash:no-line-numbers
# save a named checkpoint
tes checkpoint before-refactor

# restore back to the checkpoint later
tes restore before-refactor
```
