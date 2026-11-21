# tes exec

Run a command inside an isolated Copy-on-Write branch sandbox.

## Usage

```bash:no-line-numbers
tes exec [-b <branch>] -- <command...>
```

## Options

- `-b, --branch <BRANCH>` - Target branch sandbox to run the command in (defaults to the current session's branch).
- `-a, --agent <ID>` - Explicit session ID.

## Description

`tes exec` materializes the target branch's files into a Copy-on-Write sandbox, clones configured dependency and build caches (`cow_cache_dirs`: `node_modules`, `target`, `.venv`, `vendor`, and more), prepends `extra_bin_dirs` (`.venv/bin`, `venv/bin`, `node_modules/.bin`, `vendor/bin` by default) to `PATH`, and exports session environment variables:

- Always exported: `TES_PORT`, `TES_BRANCH`, `TES_DB_NAME` (`test_slot_<N>`), `TES_AGENT_ID`, `TES_AGENT_SLOT`, `TES_CHANGE_ID`, `TES_INTENT`, and `TMPDIR` (plus `CARGO_TARGET_DIR` when `Cargo.toml` is present).
- Exported when `port_isolation = true` (or `TES_PORT_ISOLATION=1`; defaults to `false`): `PORT`, polyglot framework port variables, and POSIX CLI port-wrapper shims (`TES_SHIM_BIN`).
- Suffixed only when already configured locally: if `DATABASE_URL` or `TEST_DATABASE_URL` is set in the environment or `.env*` and points to a local database host (`localhost`, `127.0.0.1`, `[::1]`, `0.0.0.0`), Tessera appends `_slot_<N>`. Unset variables are not fabricated, and SQLite (`sqlite:`, `file:`) and remote/cloud URLs are left untouched.

## Examples

```bash:no-line-numbers
# run a build on the current session's branch
tes exec -- npm run build

# run a build on a specific branch without switching your working tree
tes exec -b feat/pricing -- npm run build

# run tests on two branches in parallel
tes exec -b feat/pricing -- npm test
tes exec -b feat/faq -- npm test
```
