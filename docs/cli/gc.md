# tes gc

Compact ephemeral working-copy micro-snapshot operations in `.git/tessera/oplog/` and prune orphaned or merged branch sandboxes, crashed verification sandboxes, orphaned agent workspaces, stale temporary files, stale metadata caches, and dead port leases.

## Usage

```bash:no-line-numbers
tes gc [options]
```

## Options

- `-n`, `--dry-run` - Preview what would be compacted and pruned without deleting any files.
- `--keep-merged` - Keep cached sandboxes belonging to local branches already merged into the trunk branch.
- `--stale <DAYS>` - Also evict clean, idle non-trunk branch sandboxes older than `<DAYS>` days.
- `--prune-caches` - Strip configured `cow_cache_dirs` (such as `target`, `node_modules`, `.venv`) inside idle surviving sandboxes without removing the sandboxes themselves.
- `--all` - Also sweep orphaned sandbox caches belonging to deleted repositories in the global workspaces cache directory.
- `--json` - Emit structured JSON output (`dry_run`, `compacted_operations`, `pruned_sandboxes`, `pruned_merged_sandboxes`, `pruned_stale_sandboxes`, `pruned_verify_sandboxes`, `pruned_agent_workspaces`, `pruned_tmp_dirs`, `pruned_sidecars`, `pruned_dead_leases`, `pruned_stat_caches`, `pruned_locks`, `pruned_session_slots`, `pruned_stale_view_branches`, `pruned_caches`, `pruned_global_repos`, `skipped_active_sandboxes`, `reclaimed_bytes`).

## Description

`tes gc` reclaims stale Tessera state while preserving all active work, uncommitted edits, and history:

- **Operation Log compaction**: Compacts high-frequency `ephemeral` working-tree micro-snapshots in `.git/tessera/oplog/` while preserving all milestone operations, commits, merges, and named checkpoints (`tes checkpoint`).
- **Orphaned and merged sandbox pruning**: Reconciles `OpLog` branch state against actual Git refs (catching branches deleted externally via `git branch -d`), removes orphaned branch sandboxes and their `.slot`, `.tree`, and `.pending-merge` sidecars, and evicts clean sandboxes of local branches already merged into trunk (unless `--keep-merged` is passed). Any sandbox hosting a live leased process, an active dirty session, or uncommitted working edits is never deleted and is reported in `skipped_active_sandboxes`.
- **Crashed verify sandboxes and agent workspaces**: Removes leftover `verify-sandbox-<pid>-*` directories whose owning process is dead, orphaned `.tessera/workspaces/<agent>` directories, and stale `.tessera/tmp/<agent>` directories.
- **Metadata and lease hygiene**: Removes dead `<sandboxes_root>/bin/.leases/<port>` directories, expired `.git/tessera/stat_cache/*` files, stale `locks/sandbox-*.lock` files, and expired `session-slots/*.slot` files.
- **APFS-aware space accounting**: Reports estimated unique physical bytes reclaimed (`reclaimed_bytes`), accounting for macOS APFS Copy-on-Write clones so shared blocks are not double-counted.

## Examples

```bash:no-line-numbers
# preview what tes gc would compact and prune
tes gc --dry-run

# compact ephemeral micro-snapshots and prune orphaned/merged sandboxes and leases
tes gc

# prune idle sandboxes older than 14 days and strip build caches from surviving idle sandboxes
tes gc --stale 14 --prune-caches

# also sweep caches left behind by deleted repositories across the global cache root
tes gc --all

# run garbage collection with structured JSON output
tes gc --json
```
