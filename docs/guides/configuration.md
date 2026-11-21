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
base_port = 3000
port_stride = 100
port_isolation = true
auto_isolate_dirty_claims = true
watch_interval_ms = 75
watch_timeout_secs = 90

cow_cache_dirs = [
  "node_modules",
  ".pnpm-store",
  "target",
  ".venv",
  "venv",
  "vendor"
]

package_globs = [
  "crates/*",
  "packages/*",
  "apps/*",
  "services/*"
]

# Execution-sensitive fields below require `tes config trust`
verify_command = "npm run build"
pager = "less -FRX"

extra_bin_dirs = [
  ".venv/bin",
  "venv/bin",
  "node_modules/.bin",
  "vendor/bin"
]

[custom_env]
DATABASE_URL = "postgres://localhost:5432/app_slot_{slot}"
REDIS_URL = "redis://localhost:6379/{slot}"
VITE_PORT = "{port}"
```

## Configuration Options

### Declarative Settings (Loaded Automatically)

These settings are declarative and load automatically from `.git/tessera/config.toml` and any checked-in `tessera.toml`.

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
- **Default:** `3000`
- **Description:** Starting port used when calculating isolated session ports (`PORT = base_port + slot * port_stride`). Must be `>= 1024` in `tessera.toml`.

```toml
base_port = 3000
```

Update via CLI:

```bash:no-line-numbers
tes config set base_port 3000
```

#### `port_stride`

- **Type:** `number`
- **Default:** `100`
- **Description:** Port increment between session slots (for example, Slot 1 receives `3100`, Slot 2 receives `3200`). Must be `>= 25` in `tessera.toml`.

```toml
port_stride = 100
```

Update via CLI:

```bash:no-line-numbers
tes config set port_stride 100
```

#### `port_isolation`

- **Type:** `boolean`
- **Default:** `false`
- **Description:** When enabled (`true`) or when `TES_PORT_ISOLATION=1` is set in the environment, `tes env` and `tes exec` automatically export `PORT` and framework port environment variables (`VITE_PORT`, `FLASK_RUN_PORT`, `UVICORN_PORT`, `RAILS_PORT`, and more) and prepend POSIX CLI port-wrapper shims (`TES_SHIM_BIN`) for each session slot.

```toml
port_isolation = true
```

Update via CLI:

```bash:no-line-numbers
tes config set port_isolation true
```

#### `auto_isolate_dirty_claims`

- **Type:** `boolean`
- **Default:** `true`
- **Description:** When `true`, running `tes claim` without `--branch` on a file that already has uncommitted changes from the user or a peer session automatically isolates the new session onto `agent/<session_id>` instead of mixing edits into in-progress work. Pass `--branch <branch>` explicitly when same-branch collaboration is intended, or set this option to `false`.

```toml
auto_isolate_dirty_claims = true
```

Update via CLI:

```bash:no-line-numbers
tes config set auto_isolate_dirty_claims false
```

#### `watch_interval_ms`

- **Type:** `number`
- **Default:** `75`
- **Description:** Polling interval in milliseconds for the background write watcher started on `tes claim` (clamped to a minimum of `15` ms in `tessera.toml`).

```toml
watch_interval_ms = 75
```

Update via CLI:

```bash:no-line-numbers
tes config set watch_interval_ms 75
```

#### `watch_timeout_secs`

- **Type:** `number`
- **Default:** `90`
- **Description:** Idle timeout in seconds before the background write watcher exits automatically (clamped to a minimum of `5` seconds in `tessera.toml`).

```toml
watch_timeout_secs = 90
```

Update via CLI:

```bash:no-line-numbers
tes config set watch_timeout_secs 90
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

#### `cow_cache_dirs`

- **Type:** `string[]`
- **Default:** `["target", "node_modules", ".pnpm-store", ".turbo", ".next", ".nuxt", ".svelte-kit", "dist", "build", ".venv", "venv", "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", "vendor", "_build", "deps", ".gradle", ".build", ".zig-cache", "zig-out", ".dart_tool", "cmake-build-debug", "cmake-build-release"]`
- **Description:** Untracked dependency and build cache directories cloned via Copy-on-Write into every branch sandbox.

Add directories via CLI:

```bash:no-line-numbers
tes config add-cache-dir .nx .angular
```

#### `package_globs`

- **Type:** `string[]`
- **Default:** `["crates/*", "packages/*", "apps/*", "services/*"]`
- **Description:** Monorepo workspace or package glob patterns scanned when discovering nested dependency caches.

Add package globs via CLI:

```bash:no-line-numbers
tes config add-package-glob "modules/*" "plugins/*"
```

### Trust-Gated Execution Settings

Because these settings influence executed commands, `PATH` resolution, or runtime environment variables, values defined in a checked-in `tessera.toml` are ignored until you explicitly run `tes config trust`. Local overrides set directly via `tes config` in `.git/tessera/config.toml` take effect immediately.

#### `verify_command`

- **Type:** `string | null`
- **Default:** `null`
- **Description:** Optional verification command executed when running `tes merge --verify` or `tes commit --verify` (and over MCP when `verify: true` is passed).

```toml
verify_command = "npm test"
```

Update via CLI:

```bash:no-line-numbers
tes config set verify_command "npm test"
# or using the shorthand flag:
tes config --verify-cmd "npm test"
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

#### `extra_bin_dirs`

- **Type:** `string[]`
- **Default:** `[".venv/bin", "venv/bin", "node_modules/.bin", "vendor/bin"]`
- **Description:** Relative directories inside the branch sandbox prepended to `PATH` during `tes exec`. Generic project directories such as `bin` and `.bin` are excluded from the untrusted defaults for security so cloned repositories cannot shadow standard system utilities; add them explicitly via `tes config add-bin-dir` or a trusted `tessera.toml`.

Add binary directories via CLI:

```bash:no-line-numbers
tes config add-bin-dir scripts/bin tools/bin
```

#### `[custom_env]`

- **Type:** `table`
- **Default:** `{}`
- **Description:** Custom environment variable templates injected into `tes exec`. Supports `{slot}`, `{port}`, `{branch}`, and `{agent_id}` placeholders (interpolated in a single pass). Sensitive loader, shell, proxy, and toolchain injection keys (such as `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`, `COMSPEC`, `NODE_EXTRA_CA_CERTS`, `HTTP_PROXY`, and `SSH_AUTH_SOCK`) are rejected automatically.

```toml
[custom_env]
DATABASE_URL = "postgres://localhost:5432/app_test_{slot}"
APP_URL = "http://localhost:{port}"
```

Set environment templates via CLI:

```bash:no-line-numbers
tes config set-env DATABASE_URL "postgres://localhost:5432/app_test_{slot}"
tes config set-env APP_URL "http://localhost:{port}"
```

### Repository Trust (`tessera.toml`)

When cloning an external repository that includes a checked-in `tessera.toml`, declarative settings (`trunk_branch`, `base_port`, `port_stride`, `watch_interval_ms`, `watch_timeout_secs`, `auto_isolate_dirty_claims`, `co_author`, `co_author_trailer`, `port_isolation`, `cow_cache_dirs`, and `package_globs`) are loaded automatically.

To prevent untrusted repositories from executing arbitrary commands, shadowing binaries, or injecting environment variables automatically, `verify_command`, `pager`, `extra_bin_dirs`, and `custom_env` in `tessera.toml` are ignored until you explicitly trust the current file digest:

```bash:no-line-numbers
# trust the repository's current tessera.toml
tes config trust

# revoke trust for tessera.toml
tes config untrust
```

If `tessera.toml` changes later (for example, after a `tes pull`), its Blake3 digest changes and execution-sensitive fields remain inactive until you review the file and run `tes config trust` again.

## See Also

- [tes config](/cli/config) - CLI command reference
- [Sandboxes](/guides/running-tests-and-servers) - How branch sandboxes use cache directories and ports
- [Getting Started](/getting-started/) - Project initialization guide
