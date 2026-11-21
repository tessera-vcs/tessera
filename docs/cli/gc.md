# tes gc

Compact ephemeral working-copy micro-snapshot operations in `.git/tessera/oplog/` and prune orphaned branch sandboxes, stale sandbox sidecars, and dead port leases.

## Usage

```bash:no-line-numbers
tes gc [options]
```

## Options

- `-n`, `--dry-run` - Preview what would be compacted and pruned without deleting any files.
- `--json` - Emit structured JSON output (`dry_run`, `compacted_operations`, `pruned_sandboxes`, `pruned_sidecars`, `pruned_dead_leases`, `skipped_active_sandboxes`).

## Description

`tes gc` reclaims stale Tessera state while preserving all active work and history:

- **Operation Log compaction**: Compacts high-frequency `ephemeral` working-tree micro-snapshots in `.git/tessera/oplog/` while preserving all milestone operations, commits, merges, and named checkpoints (`tes checkpoint`).
- **Orphaned sandbox pruning**: Removes cached branch sandbox directories (and their `.slot`, `.tree`, and `.pending-merge` sidecar files) that no longer correspond to any local Git branch, `OpLog` branch, or active session. Any orphaned sandbox still hosting a live leased process is skipped and reported.
- **Dead port lease cleanup**: Removes stale `<sandboxes_root>/bin/.leases/<port>` directories whose owning process is no longer alive.

## Examples

```bash:no-line-numbers
# preview what tes gc would compact and prune
tes gc --dry-run

# compact ephemeral micro-snapshots and prune orphaned sandboxes/leases
tes gc

# run garbage collection with structured JSON output
tes gc --json
```
