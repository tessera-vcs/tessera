# tes doctor

Run repository health, configuration, filesystem, and integration diagnostics.

## Usage

```bash:no-line-numbers
tes doctor [options]
```

## Options

- `--json` - Emit structured JSON diagnostic checks and status summary.

## Description

`tes doctor` inspects the current repository and local environment and reports actionable pass/warn/fail checks across:

1. **Git and `.git/tessera` State**: Verifies the Git repository, `.git/tessera` store initialization, operation log heads, and branch reference consistency.
2. **Configuration and Trust**: Validates `.git/tessera/config.toml` and any checked-in `tessera.toml`, reporting whether execution-sensitive fields (`verify_command`, `pager`, `extra_bin_dirs`, `custom_env`) are trusted via `tes config trust`.
3. **Copy-on-Write Filesystem Support**: Checks whether the repository filesystem supports native Copy-on-Write clones (`clonefile` on macOS APFS, `FICLONE` on Linux Btrfs/XFS/bcachefs) or uses the portable fallback.
4. **Hooks and Harnesses**: Checks installed Git sync hooks (`.git/hooks/post-commit`, `post-merge`, `post-checkout`) and AI editor rule files.
5. **Sessions, Sandboxes, and Conflicts**: Surfaces active conflicts, dead port leases, and orphaned branch sandboxes (guiding you to `tes gc` to clean them up).

## Examples

```bash:no-line-numbers
# run repository and environment health checks
tes doctor

# output structured diagnostics as JSON for scripts or AI agents
tes doctor --json
```
