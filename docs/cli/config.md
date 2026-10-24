# tes config

View or update repository configuration settings.

## Usage

```bash:no-line-numbers
tes config <subcommand> [arguments]
```

## Subcommands

- `show` - Display the effective configuration for the repository.
- `set <KEY> <VALUE>` - Update `trunk_branch`, `base_port`, `port_stride`, `verify_command`, `co_author`, `co_author_trailer`, or `pager`.
- `trust` - Trust the repository's `tessera.toml` so `verify_command`, `pager`, and `extra_bin_dirs` defined in `tessera.toml` are allowed to execute.
- `untrust` - Revoke trust for the repository's `tessera.toml`.
- `add-cache-dir <DIRS...>` - Add directories to `cow_cache_dirs`.
- `remove-cache-dir <DIRS...>` - Remove directories from `cow_cache_dirs`.
- `add-bin-dir <DIRS...>` - Add directories to `extra_bin_dirs`.
- `remove-bin-dir <DIRS...>` - Remove directories from `extra_bin_dirs`.
- `set-env <KEY> <TEMPLATE>` - Set a custom environment variable template (`{slot}`, `{port}`, `{branch}`, `{agent_id}`).
- `unset-env <KEY>` - Remove a custom environment variable template.

## Examples

```bash:no-line-numbers
# view current configuration
tes config show

# update base port and verification command
tes config set base_port 4000
tes config set verify_command "npm test"

# enable Co-authored-by trailer on Tessera commits (false by default)
tes config set co_author true

# configure a custom terminal pager (e.g. delta or less -FRX)
tes config set pager "delta --paging=always"

# trust a cloned repository's tessera.toml
tes config trust

# add a Copy-on-Write cache directory
tes config add-cache-dir .nx

# set a custom environment variable template
tes config set-env DATABASE_URL "postgres://localhost:5432/app_slot_{slot}"
```

## See Also

- [Configuration Guide](/guides/configuration) - Full `tessera.toml` reference
