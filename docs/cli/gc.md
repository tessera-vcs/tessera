# tes gc

Compact ephemeral working-copy micro-snapshot operations in `.git/tessera/oplog/`.

## Usage

```bash:no-line-numbers
tes gc [options]
```

## Options

- `--json` - Emit structured JSON output (`{ "compacted_operations": <N> }`).

## Description

High-frequency working-tree snapshots recorded while agents edit files are marked as `ephemeral` in the Operation Log. Running `tes gc` compacts those ephemeral micro-snapshots to keep `.git/tessera/oplog/` small while preserving all milestone operations, commits, merges, and named checkpoints (`tes checkpoint`).

## Examples

```bash:no-line-numbers
# compact ephemeral micro-snapshots
tes gc

# run compaction with JSON output
tes gc --json
```
