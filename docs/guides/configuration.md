# Configuration

Tessera includes default settings for JavaScript/TypeScript, Rust, Python, Go, PHP, Ruby, Elixir, JVM, Swift, C/C++, Zig, .NET, and Dart projects.

You can customize ports, verification gates, Copy-on-Write cache directories, and environment variable templates using either a `tessera.toml` file in your project root or the `tes config` command.

## Quick Start

View the active configuration for your repository:

```bash:no-line-numbers
tes config show
```

## Configuration File (`tessera.toml`)

To share configuration across your team in version control, create a `tessera.toml` file in your repository root.

### Example Configuration

```toml
trunk_branch = "main"
base_port = 4000
port_stride = 100
verify_cmd = ["npm", "run", "build"]

cow_cache_dirs = [
  "node_modules",
  ".pnpm-store",
  "target",
  ".venv",
  "venv",
  "vendor"
]

extra_bin_dirs = [
  "node_modules/.bin",
  ".venv/bin",
  "vendor/bin",
  "bin"
]

[custom_env]
DATABASE_URL = "postgres://localhost:5432/app_slot_{slot}"
REDIS_URL = "redis://localhost:6379/{slot}"
VITE_PORT = "{port}"
```

## Configuration Options

### Core Settings

#### `trunk_branch`

- **Type:** `string`
- **Default:** `"main"`
- **Description:** Default primary branch for the repository.

```toml
trunk_branch = "main"
```

Update via CLI:

```bash:no-line-numbers
tes config set trunk_branch main
```

#### `base_port`

- **Type:** `number`
- **Default:** `4000`
- **Description:** Starting port used when calculating isolated session ports (`PORT = base_port + slot * port_stride`).

```toml
base_port = 4000
```

Update via CLI:

```bash:no-line-numbers
tes config set base_port 4000
```

#### `port_stride`

- **Type:** `number`
- **Default:** `100`
- **Description:** Port increment between session slots (e.g., Slot 1 receives `4100`, Slot 2 receives `4200`).

```toml
port_stride = 100
```

Update via CLI:

```bash:no-line-numbers
tes config set port_stride 100
```

#### `verify_cmd`

- **Type:** `string[]`
- **Default:** `[]`
- **Description:** Optional verification command executed automatically before `tes merge` finalizes.

```toml
verify_cmd = ["npm", "test"]
```

Update via CLI:

```bash:no-line-numbers
tes config set verify_cmd "npm test"
```

#### `co_author`

- **Type:** `boolean`
- **Default:** `false`
- **Description:** When enabled (`true`), appends a `Co-authored-by:` trailer (`Co-authored-by: Tessera <noreply@tessera.getlunaris.dev>` by default) to commits created by Tessera (`tes commit`, `tes merge`, `tes resolve`, `tes pull`, and `tes cherry-pick`). Commits always record your active Git/system user as the primary `Author`.

```toml
co_author = true
```

Update via CLI:

```bash:no-line-numbers
tes config set co_author true
```

#### `co_author_trailer`

- **Type:** `string`
- **Default:** `"Tessera <noreply@tessera.getlunaris.dev>"`
- **Description:** Custom identity used in the `Co-authored-by: <trailer>` line when `co_author = true`.

```toml
co_author = true
co_author_trailer = "Tessera <noreply@tessera.getlunaris.dev>"
```

Update via CLI:

```bash:no-line-numbers
tes config set co_author_trailer "Tessera <noreply@tessera.getlunaris.dev>"
```

#### `pager`

- **Type:** `string | null`
- **Default:** `null` (falls back to `TES_PAGER` -> `GIT_PAGER` -> `git config core.pager` -> `PAGER` -> `less -FRX`)
- **Description:** Terminal pager command used for `tes diff`, `tes log`, `tes oplog`, `tes branch`, `tes stash list`, and any command invoked with `--paginate` when `stdout` is an interactive terminal. Set to `"cat"` or pass `-P` / `--no-pager` to disable paging.

```toml
pager = "less -FRX"
```

Update via CLI:

```bash:no-line-numbers
tes config set pager "delta --paging=always"
```

### Sandbox Options

#### `cow_cache_dirs`

- **Type:** `string[]`
- **Default:** `["node_modules", ".pnpm-store", ".next", ".nuxt", ".svelte-kit", ".turbo", ".parcel-cache", ".vite", "target", ".venv", "venv", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".tox", "vendor", ".bundle", "_build", "deps", ".gradle", "build", ".build", "cmake-build-debug", "cmake-build-release", ".zig-cache", "zig-out", "bin", "obj", ".dart_tool"]`
- **Description:** Dependency and build cache directories cloned via Copy-on-Write into every branch sandbox.

Add or remove directories via CLI:

```bash:no-line-numbers
tes config add-cache-dir .nx .angular
tes config remove-cache-dir .angular
```

#### `extra_bin_dirs`

- **Type:** `string[]`
- **Default:** `["node_modules/.bin", ".venv/bin", "venv/bin", "vendor/bin", "bin"]`
- **Description:** Relative directories inside the branch sandbox prepended to `PATH` during `tes exec`.

Add or remove binary directories via CLI:

```bash:no-line-numbers
tes config add-bin-dir ./scripts ./tools/bin
tes config remove-bin-dir ./scripts
```

### Environment Variables

#### `[custom_env]`

- **Type:** `table`
- **Default:** `{}`
- **Description:** Custom environment variables injected into `tes exec` and switched terminals. Supports `{slot}`, `{port}`, `{branch}`, and `{agent_id}` placeholders.

```toml
[custom_env]
DATABASE_URL = "postgres://localhost:5432/app_test_{slot}"
APP_URL = "http://localhost:{port}"
```

Set or remove environment templates via CLI:

```bash:no-line-numbers
tes config set-env DATABASE_URL "postgres://localhost:5432/app_test_{slot}"
tes config set-env APP_URL "http://localhost:{port}"
tes config unset-env APP_URL
```

### Repository Trust (`tessera.toml`)

When cloning an external repository that includes a checked-in `tessera.toml`, non-executable settings (`trunk_branch`, `base_port`, `port_stride`, `co_author`, `co_author_trailer`, `cow_cache_dirs`, and safe `custom_env` templates) are loaded automatically.

To prevent untrusted repositories from executing arbitrary commands or overriding system binaries automatically, `verify_command`, `pager`, and `extra_bin_dirs` in `tessera.toml` require explicit repository trust:

```bash:no-line-numbers
# trust the repository's tessera.toml
tes config trust

# revoke trust for tessera.toml
tes config untrust
```

## See Also

- [tes config](/cli/config) - CLI command reference
- [Sandboxes](/guides/running-tests-and-servers) - How branch sandboxes use cache directories and ports
- [Getting Started](/getting-started/) - Project initialization guide
