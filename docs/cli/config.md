# tes config

View or update repository configuration settings.

## Usage

```bash:no-line-numbers
tes config <subcommand> [arguments]
```

## Subcommands

- `show` - Display the effective configuration for the repository (combining `.git/tessera/config.toml` and `tessera.toml`).
- `set <KEY> <VALUE>` - Update `trunk_branch`, `base_port`, `port_stride`, `port_isolation`, `verify_command`, `watch_interval_ms`, `watch_timeout_secs`, `co_author`, `co_author_trailer`, `auto_isolate_dirty_claims`, `pager`, `portable_copy_cache_max_mb`, `max_warm_sandboxes`, `shared_cargo_target`, or `cow_exclude_dirs`.
- `trust` - Trust the current digest of the repository's checked-in `tessera.toml` so its execution-sensitive settings (`verify_command`, `pager`, `extra_bin_dirs`, and `custom_env`) are honored.
- `untrust` - Revoke local trust for the repository's `tessera.toml`.
- `add-cache-dir <DIRS...>` - Add directories to `cow_cache_dirs`.
- `add-exclude-dir <DIRS...>` - Add directory names or relative paths (such as `target`, `src/target`, or `node_modules`) to `cow_exclude_dirs` so they are never cloned into branch sandboxes.
- `add-bin-dir <DIRS...>` - Add relative binary directories to `extra_bin_dirs` (defaults to `.venv/bin`, `venv/bin`, `node_modules/.bin`, `vendor/bin`; `bin` and `.bin` are excluded by default for security).
- `add-package-glob <GLOBS...>` - Add monorepo package glob patterns to `package_globs` (defaults to `crates/*`, `packages/*`, `apps/*`, `services/*`).
- `set-env <KEY> <TEMPLATE>` - Set a custom environment variable template (`{slot}`, `{port}`, `{branch}`, `{agent_id}`).

## Options

- `--verify-cmd <CMD>` - Shorthand to set `verify_command` directly (`tes config --verify-cmd "npm test"`).

## Examples

```bash:no-line-numbers
# view current configuration
tes config show

# update base port, port isolation, and verification command
tes config set base_port 3000
tes config set port_isolation true
tes config set verify_command "npm test"

# enable Co-authored-by trailer on Tessera commits (false by default)
tes config set co_author true

# configure a custom terminal pager (e.g. delta or less -FRX)
tes config set pager "delta --paging=always"

# trust a cloned repository's tessera.toml
tes config trust

# add a Copy-on-Write cache directory
tes config add-cache-dir .nx

# exclude heavy nested build caches or opt into a shared Cargo target directory
tes config add-exclude-dir src/target
tes config set shared_cargo_target true
tes config set max_warm_sandboxes 6
tes config set portable_copy_cache_max_mb 64

# set a custom environment variable template
tes config set-env DATABASE_URL "postgres://localhost:5432/app_slot_{slot}"
```

## See Also

- [Configuration Guide](/guides/configuration) - Full `tessera.toml` reference
