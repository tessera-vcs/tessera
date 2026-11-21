//! Deterministic per-agent runtime environment variable computation (`PORT`, `DATABASE_URL`, `TMPDIR`).

use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

use core::{AgentMetadata, RepoConfig, resolve_confined_repo_path};

/// Process-local sequence counter for atomic executable shim writes on Unix.
#[cfg(unix)]
static SHIM_TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Computes a port offset safely without wrapping silently onto 65535 or colliding at 65535.
fn offset_port(base: u16, offset: u16) -> u16 {
    if let Some(sum) = base.checked_add(offset) {
        sum
    } else {
        1024u16.saturating_add((u32::from(base) + u32::from(offset) - 65535) as u16)
    }
}

/// Returns the polyglot set of port environment variables (`PORT`, `TES_PORT`, `SERVER_PORT`,
/// `FLASK_RUN_PORT`, `UVICORN_PORT`, `ROCKET_PORT`, `NITRO_PORT`, etc.) mapped to `port` so
/// web servers across Go, Rust, Python, PHP, Ruby, Java/Kotlin/Scala/Clojure, .NET, Elixir,
/// Dart/Flutter, and Node/Bun/Deno automatically bind to the session's isolated port.
#[must_use]
pub fn polyglot_port_env_vars(port: u16) -> Vec<(&'static str, String)> {
    let p = port.to_string();
    let storybook_port = offset_port(port, 6).to_string();
    let docs_port = offset_port(port, 7).to_string();
    let studio_port = offset_port(port, 8).to_string();
    let mobile_port = offset_port(port, 9).to_string();
    let jupyter_port = offset_port(port, 10).to_string();
    let streamlit_port = offset_port(port, 11).to_string();
    let gradio_port = offset_port(port, 12).to_string();
    let bokeh_port = offset_port(port, 13).to_string();
    let dash_port = offset_port(port, 14).to_string();
    let remix_ws_port = offset_port(port, 21).to_string();

    vec![
        ("PORT", p.clone()),
        ("TES_PORT", p.clone()),
        ("HTTP_PORT", p.clone()),
        ("APP_PORT", p.clone()),
        ("WEB_PORT", p.clone()),
        ("DEV_SERVER_PORT", p.clone()),
        ("BUN_PORT", p.clone()),
        ("VITE_PORT", p.clone()),
        ("NITRO_PORT", p.clone()),
        ("NUXT_PORT", p.clone()),
        ("ASTRO_PORT", p.clone()),
        ("SB_PORT", storybook_port.clone()),
        ("STORYBOOK_PORT", storybook_port),
        ("TES_DOCS_PORT", docs_port),
        ("TES_STUDIO_PORT", studio_port),
        ("RCT_METRO_PORT", mobile_port.clone()),
        ("EXPO_DEV_SERVER_PORT", mobile_port),
        ("WEBPACK_DEV_SERVER_PORT", p.clone()),
        ("WDS_SOCKET_PORT", p.clone()),
        ("REMIX_DEV_SERVER_WS_PORT", remix_ws_port),
        ("PARCEL_PORT", p.clone()),
        ("ANGULAR_PORT", p.clone()),
        ("SVELTEKIT_PORT", p.clone()),
        ("FLASK_RUN_PORT", p.clone()),
        ("UVICORN_PORT", p.clone()),
        ("DJANGO_RUNSERVER_PORT", p.clone()),
        ("GRADIO_SERVER_PORT", gradio_port),
        ("STREAMLIT_SERVER_PORT", streamlit_port),
        ("GRANIAN_PORT", p.clone()),
        ("LITESTAR_PORT", p.clone()),
        ("SANIC_PORT", p.clone()),
        ("JUPYTER_PORT", jupyter_port),
        ("BOKEH_PORT", bokeh_port),
        ("DASH_PORT", dash_port),
        ("SERVER_PORT", p.clone()),
        ("OCTANE_PORT", p.clone()),
        ("CLI_SERVER_PORT", p.clone()),
        ("RAILS_PORT", p.clone()),
        ("RACK_PORT", p.clone()),
        ("HANAMI_PORT", p.clone()),
        ("ROCKET_PORT", p.clone()),
        ("TRUNK_SERVE_PORT", p.clone()),
        ("LEPTOS_SITE_ADDR", format!("127.0.0.1:{port}")),
        ("ACTIX_PORT", p.clone()),
        ("AXUM_PORT", p.clone()),
        ("TAURI_DEV_SERVER_PORT", p.clone()),
        ("GO_PORT", p.clone()),
        ("BEEGO_RUNPORT", p.clone()),
        ("CADDY_PORT", p.clone()),
        ("QUARKUS_HTTP_PORT", p.clone()),
        ("MICRONAUT_SERVER_PORT", p.clone()),
        ("KTOR_DEPLOYMENT_PORT", p.clone()),
        ("PLAY_HTTP_PORT", p.clone()),
        ("VERTX_HTTP_PORT", p.clone()),
        ("ASPNETCORE_HTTP_PORTS", p.clone()),
        ("ASPNETCORE_URLS", format!("http://localhost:{port}")),
        ("DOTNET_URLS", format!("http://localhost:{port}")),
        ("PHX_PORT", p.clone()),
        ("MIX_PORT", p),
    ]
}

/// Names of all polyglot port environment variables managed by Tessera.
pub const POLYGLOT_PORT_ENV_KEYS: &[&str] = &[
    "PORT",
    "TES_PORT",
    "HTTP_PORT",
    "APP_PORT",
    "WEB_PORT",
    "DEV_SERVER_PORT",
    "BUN_PORT",
    "VITE_PORT",
    "NITRO_PORT",
    "NUXT_PORT",
    "ASTRO_PORT",
    "SB_PORT",
    "STORYBOOK_PORT",
    "TES_DOCS_PORT",
    "TES_STUDIO_PORT",
    "RCT_METRO_PORT",
    "EXPO_DEV_SERVER_PORT",
    "WEBPACK_DEV_SERVER_PORT",
    "WDS_SOCKET_PORT",
    "REMIX_DEV_SERVER_WS_PORT",
    "PARCEL_PORT",
    "ANGULAR_PORT",
    "SVELTEKIT_PORT",
    "FLASK_RUN_PORT",
    "UVICORN_PORT",
    "DJANGO_RUNSERVER_PORT",
    "GRADIO_SERVER_PORT",
    "STREAMLIT_SERVER_PORT",
    "GRANIAN_PORT",
    "LITESTAR_PORT",
    "SANIC_PORT",
    "JUPYTER_PORT",
    "BOKEH_PORT",
    "DASH_PORT",
    "SERVER_PORT",
    "OCTANE_PORT",
    "CLI_SERVER_PORT",
    "RAILS_PORT",
    "RACK_PORT",
    "HANAMI_PORT",
    "ROCKET_PORT",
    "TRUNK_SERVE_PORT",
    "LEPTOS_SITE_ADDR",
    "ACTIX_PORT",
    "AXUM_PORT",
    "TAURI_DEV_SERVER_PORT",
    "GO_PORT",
    "BEEGO_RUNPORT",
    "CADDY_PORT",
    "QUARKUS_HTTP_PORT",
    "MICRONAUT_SERVER_PORT",
    "KTOR_DEPLOYMENT_PORT",
    "PLAY_HTTP_PORT",
    "VERTX_HTTP_PORT",
    "ASPNETCORE_HTTP_PORTS",
    "ASPNETCORE_URLS",
    "DOTNET_URLS",
    "PHX_PORT",
    "MIX_PORT",
];

/// Writes `bytes` to a sibling temporary file, sets mode `0o755` BEFORE renaming, and atomically
/// renames it onto `target` so concurrent processes never observe an `EACCES` non-executable window.
#[cfg(unix)]
fn atomic_write_executable_unix(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path has no parent directory",
        )
    })?;

    let seq = SHIM_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("shim");
    let tmp_path = parent.join(format!(".{name}.tmp.{pid}.{seq}"));

    {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    if let Err(err) = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755)) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = fs::rename(&tmp_path, target) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

/// Materializes executable POSIX wrapper shims in `bin_dir` for installed CLI runtimes, package
/// runners, and dev servers across Node/Bun/Deno, Python, PHP, Ruby, Rust, Go, .NET, Java/Scala/Clojure,
/// Dart/Flutter, and static site generators that require CLI port flags when `PORT` / `TES_PORT` is set.
pub fn ensure_polyglot_port_shims(bin_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if fs::create_dir_all(bin_dir).is_err() {
            return;
        }

        let _ = fs::write(bin_dir.join(".tes-shim-dir"), b"1\n");

        let unified_script = r#"#!/bin/sh
_tes_self_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
_tes_bin_name=$(basename -- "$0")
_tes_real=""
_tes_local_path=""
for _tes_root in "${TES_WORKSPACE:-}" "$PWD" "${TES_REPO_ROOT:-}"; do
  if [ -n "$_tes_root" ]; then
    _tes_local_path="${_tes_local_path:+${_tes_local_path}:}$_tes_root/.venv/bin:$_tes_root/venv/bin:$_tes_root/node_modules/.bin:$_tes_root/vendor/bin:$_tes_root/bin"
  fi
done
_tes_search_path="${_tes_local_path:+${_tes_local_path}:}$PATH"
_tes_old_ifs="$IFS"
_tes_had_noglob=0
case "$-" in *f*) _tes_had_noglob=1 ;; esac
set -f
IFS=":"
for _tes_dir in $_tes_search_path; do
  if [ -n "$_tes_dir" ] && [ "$_tes_dir" != "$_tes_self_dir" ] && [ ! -f "$_tes_dir/.tes-shim-dir" ] && [ -x "$_tes_dir/$_tes_bin_name" ] && [ ! -d "$_tes_dir/$_tes_bin_name" ]; then
    _tes_real="$_tes_dir/$_tes_bin_name"
    break
  fi
done
IFS="$_tes_old_ifs"
if [ "$_tes_had_noglob" -eq 0 ]; then
  set +f
fi
if [ -z "$_tes_real" ]; then
  echo "tes: $_tes_bin_name: command not found" >&2
  exit 127
fi
case "${TES_PORT_ISOLATION:-}" in
  1|true|TRUE|yes|YES|on|ON) ;;
  *) exec "$_tes_real" "$@" ;;
esac
_tes_base_port="${TES_PORT:-${PORT:-}}"
case "$_tes_base_port" in
  ''|*[!0-9]*) exec "$_tes_real" "$@" ;;
esac
_tes_port="$_tes_base_port"

_tes_is_port_busy() {
  _tes_chk_port="$1"
  case "$_tes_chk_port" in
    ''|*[!0-9]*) return 1 ;;
  esac
  _tes_lease_file="$_tes_self_dir/.leases/$_tes_chk_port/pid"
  if [ -f "$_tes_lease_file" ]; then
    _tes_holder_pid=$(cat "$_tes_lease_file" 2>/dev/null || true)
    if [ -n "$_tes_holder_pid" ] && [ "$_tes_holder_pid" != "$$" ] && [ "$_tes_holder_pid" != "${PPID:-}" ] && kill -0 "$_tes_holder_pid" 2>/dev/null; then
      return 0
    fi
  fi
  if [ -n "${TES_SKIP_TCP_PROBE:-}" ]; then
    return 1
  fi
  if command -v lsof >/dev/null 2>&1; then
    if lsof -nP -iTCP:"$_tes_chk_port" -sTCP:LISTEN >/dev/null 2>&1; then
      return 0
    fi
  elif command -v ss >/dev/null 2>&1; then
    if ss -ltnH "sport = :$_tes_chk_port" 2>/dev/null | grep -q .; then
      return 0
    fi
  elif command -v nc >/dev/null 2>&1; then
    if nc -z 127.0.0.1 "$_tes_chk_port" >/dev/null 2>&1 || nc -z ::1 "$_tes_chk_port" >/dev/null 2>&1; then
      return 0
    fi
  fi
  return 1
}

_tes_try_claim_lease() {
  _tes_cl_port="$1"
  _tes_cl_dir="$_tes_self_dir/.leases/$_tes_cl_port"
  _tes_cl_lock="$_tes_self_dir/.leases/${_tes_cl_port}.lock"
  if ! mkdir "$_tes_cl_lock" 2>/dev/null; then
    return 1
  fi
  if [ -f "$_tes_cl_dir/pid" ]; then
    _tes_holder_pid=$(cat "$_tes_cl_dir/pid" 2>/dev/null || true)
    if [ -n "$_tes_holder_pid" ] && [ "$_tes_holder_pid" != "$$" ] && [ "$_tes_holder_pid" != "${PPID:-}" ] && kill -0 "$_tes_holder_pid" 2>/dev/null; then
      rmdir "$_tes_cl_lock" 2>/dev/null || true
      return 1
    fi
  fi
  mkdir -p "$_tes_cl_dir" 2>/dev/null || true
  printf '%s\n' "$$" > "$_tes_cl_dir/pid" 2>/dev/null || true
  printf '%s\n' "${TES_BRANCH:-}" > "$_tes_cl_dir/branch" 2>/dev/null || true
  printf '%s\n' "${TES_WORKSPACE:-$PWD}" > "$_tes_cl_dir/workspace" 2>/dev/null || true
  rmdir "$_tes_cl_lock" 2>/dev/null || true
  return 0
}

_tes_export_acquired_port() {
  _tes_off_kind="$1"
  _tes_chosen="$2"
  _tes_port="$_tes_chosen"
  case "$_tes_off_kind" in
    0)
      TES_PORT="$_tes_chosen"
      export PORT="$_tes_chosen" TES_PORT="$_tes_chosen" HTTP_PORT="$_tes_chosen" APP_PORT="$_tes_chosen" WEB_PORT="$_tes_chosen" DEV_SERVER_PORT="$_tes_chosen" BUN_PORT="$_tes_chosen" VITE_PORT="$_tes_chosen" NITRO_PORT="$_tes_chosen" NUXT_PORT="$_tes_chosen" ASTRO_PORT="$_tes_chosen" WEBPACK_DEV_SERVER_PORT="$_tes_chosen" WDS_SOCKET_PORT="$_tes_chosen" SERVER_PORT="$_tes_chosen" FLASK_RUN_PORT="$_tes_chosen" UVICORN_PORT="$_tes_chosen" DJANGO_RUNSERVER_PORT="$_tes_chosen" RAILS_PORT="$_tes_chosen" ROCKET_PORT="$_tes_chosen" GO_PORT="$_tes_chosen" ASPNETCORE_HTTP_PORTS="$_tes_chosen" ASPNETCORE_URLS="http://localhost:$_tes_chosen" DOTNET_URLS="http://localhost:$_tes_chosen" QUARKUS_HTTP_PORT="$_tes_chosen" MICRONAUT_SERVER_PORT="$_tes_chosen" KTOR_DEPLOYMENT_PORT="$_tes_chosen" PHX_PORT="$_tes_chosen"
      ;;
    6)
      export SB_PORT="$_tes_chosen" STORYBOOK_PORT="$_tes_chosen"
      ;;
    7)
      export TES_DOCS_PORT="$_tes_chosen"
      ;;
    8)
      export TES_STUDIO_PORT="$_tes_chosen"
      ;;
    9)
      export RCT_METRO_PORT="$_tes_chosen" EXPO_DEV_SERVER_PORT="$_tes_chosen"
      ;;
    10)
      export JUPYTER_PORT="$_tes_chosen"
      ;;
    11)
      export STREAMLIT_SERVER_PORT="$_tes_chosen"
      ;;
    13)
      export BOKEH_PORT="$_tes_chosen"
      ;;
  esac
}

_tes_acquire_port() {
  _tes_off="${1:-0}"
  case "$_tes_off" in
    ''|*[!0-9]*) _tes_off=0 ;;
  esac
  if [ "$_tes_off" -eq 0 ]; then
    case "${PORT:-}" in
      ''|*[!0-9]*)
        _tes_start_port=$((_tes_base_port + _tes_off))
        ;;
      *)
        if [ -n "${TES_PORT:-}" ] && [ "$PORT" != "$TES_PORT" ]; then
          _tes_start_port="$PORT"
        else
          _tes_start_port="$_tes_base_port"
        fi
        ;;
    esac
  else
    _tes_start_port=$((_tes_base_port + _tes_off))
  fi
  _tes_max_port=$((_tes_base_port + 99))
  if [ "$_tes_start_port" -gt "$_tes_max_port" ]; then
    _tes_max_port=$((_tes_start_port + 50))
  fi
  if [ "$_tes_max_port" -gt 65535 ]; then
    _tes_max_port=65535
  fi
  if [ "$_tes_start_port" -gt 65535 ]; then
    _tes_start_port=65535
  fi
  mkdir -p "$_tes_self_dir/.leases" 2>/dev/null || true
  _tes_try="$_tes_start_port"
  while [ "$_tes_try" -le "$_tes_max_port" ]; do
    if ! _tes_is_port_busy "$_tes_try" && _tes_try_claim_lease "$_tes_try"; then
      _tes_export_acquired_port "$_tes_off" "$_tes_try"
      return 0
    fi
    _tes_try=$((_tes_try + 1))
  done
  _tes_export_acquired_port "$_tes_off" "$_tes_start_port"
}

_tes_exec_inject() {
  _tes_nflags="$1"
  shift
  if [ "$_tes_nflags" -eq 1 ]; then
    _tes_f1="$1"
    shift
  elif [ "$_tes_nflags" -eq 2 ]; then
    _tes_f1="$1"
    _tes_f2="$2"
    shift 2
  fi
  _tes_inserted=0
  _tes_count=$#
  _tes_i=0
  while [ "$_tes_i" -lt "$_tes_count" ]; do
    _tes_cur="$1"
    shift
    if [ "$_tes_inserted" -eq 0 ] && [ "$_tes_cur" = "--" ]; then
      if [ "$_tes_nflags" -eq 1 ]; then
        set -- "$@" "$_tes_f1" "--"
      else
        set -- "$@" "$_tes_f1" "$_tes_f2" "--"
      fi
      _tes_inserted=1
    else
      set -- "$@" "$_tes_cur"
    fi
    _tes_i=$((_tes_i + 1))
  done
  if [ "$_tes_inserted" -eq 0 ]; then
    if [ "$_tes_nflags" -eq 1 ]; then
      set -- "$@" "$_tes_f1"
    else
      set -- "$@" "$_tes_f1" "$_tes_f2"
    fi
  fi
  exec "$_tes_real" "$@"
}

_tes_has_positional_after() {
  _tes_skip_count="$1"
  shift
  _tes_idx=0
  _tes_skip_next=0
  for _tes_a in "$@"; do
    _tes_idx=$((_tes_idx + 1))
    if [ "$_tes_idx" -le "$_tes_skip_count" ]; then
      continue
    fi
    if [ "$_tes_a" = "--" ]; then
      break
    fi
    if [ "$_tes_skip_next" -eq 1 ]; then
      _tes_skip_next=0
      continue
    fi
    case "$_tes_a" in
      -b|--bind|-d|--directory|--protocol|--settings|--pythonpath|-c|--config) _tes_skip_next=1 ;;
      -*) ;;
      *) return 0 ;;
    esac
  done
  return 1
}

_tes_eff_tool="$_tes_bin_name"
_tes_a1="${1:-}"
_tes_a2="${2:-}"
_tes_a3="${3:-}"
_tes_skip=0

case "$_tes_eff_tool" in
  uv|poetry|pipenv|pdm|hatch)
    if [ "$_tes_a1" = "run" ] && [ -n "$_tes_a2" ]; then
      _tes_eff_tool=$(basename -- "$_tes_a2")
      _tes_a1="$_tes_a3"
      _tes_a2="${4:-}"
      _tes_a3="${5:-}"
      _tes_skip=2
    fi
    ;;
  bundle)
    if [ "$_tes_a1" = "exec" ] && [ -n "$_tes_a2" ]; then
      _tes_eff_tool=$(basename -- "$_tes_a2")
      _tes_a1="$_tes_a3"
      _tes_a2="${4:-}"
      _tes_a3="${5:-}"
      _tes_skip=2
    fi
    ;;
  npx|pnpx|bunx)
    if [ -n "$_tes_a1" ]; then
      _tes_eff_tool=$(basename -- "$_tes_a1")
      _tes_a1="$_tes_a2"
      _tes_a2="$_tes_a3"
      _tes_a3="${4:-}"
      _tes_skip=1
    fi
    ;;
  bun)
    if [ "$_tes_a1" = "x" ] && [ -n "$_tes_a2" ]; then
      _tes_eff_tool=$(basename -- "$_tes_a2")
      _tes_a1="$_tes_a3"
      _tes_a2="${4:-}"
      _tes_a3="${5:-}"
      _tes_skip=2
    fi
    ;;
esac

case "$_tes_eff_tool" in
  node|bun)
    _tes_mapped=""
    case "$_tes_a1" in
      */vite|*/vite.js|*/vite/bin/*|vite) _tes_mapped="vite" ;;
      */vitepress|*/vitepress.js|*/vitepress/bin/*|vitepress) _tes_mapped="vitepress" ;;
      */astro|*/astro.js|*/astro/astro.js|astro) _tes_mapped="astro" ;;
      */next|*/next/dist/bin/next|next) _tes_mapped="next" ;;
      */nuxt|*/nuxi|*/nuxi.mjs|*/nuxi/bin/*|nuxt|nuxi) _tes_mapped="nuxt" ;;
      */ng|*/@angular/cli/bin/ng*|ng) _tes_mapped="ng" ;;
      */remix|*/@remix-run/dev/*|remix) _tes_mapped="remix" ;;
      */react-router|*/@react-router/dev/*|react-router) _tes_mapped="react-router" ;;
      */svelte-kit|*/@sveltejs/kit/*|svelte-kit) _tes_mapped="svelte-kit" ;;
      */gatsby|*/gatsby/cli.js|gatsby) _tes_mapped="gatsby" ;;
      */eleventy|*/@11ty/eleventy/*|eleventy) _tes_mapped="eleventy" ;;
      */hexo|*/hexo-cli/bin/hexo|hexo) _tes_mapped="hexo" ;;
      */vue-cli-service|*/@vue/cli-service/*|vue-cli-service) _tes_mapped="vue-cli-service" ;;
      */quasar|*/@quasar/*/bin/quasar|quasar) _tes_mapped="quasar" ;;
      */storybook|*/storybook/index.js|*/@storybook/*/bin/*|storybook) _tes_mapped="storybook" ;;
      */start-storybook|start-storybook) _tes_mapped="start-storybook" ;;
      */expo|*/expo/bin/cli*|expo) _tes_mapped="expo" ;;
      */react-native|*/react-native/cli.js|react-native) _tes_mapped="react-native" ;;
      */wrangler|*/wrangler.js|*/wrangler/bin/*|wrangler) _tes_mapped="wrangler" ;;
      */netlify|*/netlify-cli/bin/*|netlify) _tes_mapped="netlify" ;;
      */vercel|*/vercel/dist/*|vercel) _tes_mapped="vercel" ;;
      */prisma|*/prisma/build/index.js|prisma) _tes_mapped="prisma" ;;
      */drizzle-kit|*/drizzle-kit/bin.cjs|drizzle-kit) _tes_mapped="drizzle-kit" ;;
      */parcel|*/parcel.js|*/parcel/lib/bin.js|parcel) _tes_mapped="parcel" ;;
      */docusaurus|*/docusaurus.mjs|*/@docusaurus/core/bin/*|docusaurus) _tes_mapped="docusaurus" ;;
      */webpack|*/webpack.js|*/webpack/bin/*|webpack) _tes_mapped="webpack" ;;
      */webpack-dev-server|*/webpack-dev-server.js|webpack-dev-server) _tes_mapped="webpack-dev-server" ;;
      */http-server|*/http-server/bin/*|http-server) _tes_mapped="http-server" ;;
      */live-server|*/live-server.js|live-server) _tes_mapped="live-server" ;;
      */serve|*/serve/build/main.js|serve) _tes_mapped="serve" ;;
      */browser-sync|*/browser-sync/dist/bin.js|browser-sync) _tes_mapped="browser-sync" ;;
    esac
    if [ -n "$_tes_mapped" ]; then
      _tes_eff_tool="$_tes_mapped"
      _tes_a1="$_tes_a2"
      _tes_a2="$_tes_a3"
      _tes_skip=$((_tes_skip + 1))
    fi
    ;;
esac

if [ "$_tes_a1" = "-v" ]; then
  exec "$_tes_real" "$@"
fi
for _tes_arg in "$@"; do
  if [ "$_tes_arg" = "--" ]; then
    break
  fi
  case "$_tes_arg" in
    --port|--port=*|--bind|--bind=*|--urls|--urls=*|--dev-addr|--dev-addr=*|--web-port|--web-port=*|--frontend-port|--frontend-port=*|--server.port|--server.port=*|-Dserver.port=*|-Dquarkus.http.port=*|-Dmicronaut.server.port=*|-Dhttp.port=*|-Djetty.http.port=*|--help|-h|--version|-V)
      exec "$_tes_real" "$@"
      ;;
    --listen|--listen=*)
      if [ "$_tes_eff_tool" != "pelican" ]; then
        exec "$_tes_real" "$@"
      fi
      ;;
    -l|-l=*)
      if [ "$_tes_eff_tool" = "serve" ]; then
        exec "$_tes_real" "$@"
      fi
      ;;
    -b)
      case "$_tes_eff_tool" in
        gunicorn|hypercorn|python|python3|django-admin|rails|rackup|puma|unicorn|thin|shotgun|hanami|middleman|bridgetown|jekyll|ruby|daphne|pelican)
          exec "$_tes_real" "$@"
          ;;
      esac
      ;;
    -p|-p=*|-P)
      case "$_tes_eff_tool" in
        mvn|gradle|dotnet|flutter|sbt|lein|caddy|encore) ;;
        *) exec "$_tes_real" "$@" ;;
      esac
      ;;
  esac
done

case "$_tes_eff_tool" in
  node|bun)
    case "$_tes_a1" in
      ""|-*|*/npm*|*/pnpm*|*/yarn*|*/corepack*|*/npx*|*/bunx*|*/node_modules/*|npm|pnpm|yarn|corepack|npx|bunx|install|add|remove|run|test|build|init|create|x|pm)
        ;;
      *)
        _tes_acquire_port 0
        exec "$_tes_real" "$@"
        ;;
    esac
    ;;
  vite)
    case "$_tes_a1" in
      build|optimize|info) exec "$_tes_real" "$@" ;;
      *) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  vitepress)
    case "$_tes_a1" in
      dev|preview|serve|"") _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  astro|svelte-kit)
    case "$_tes_a1" in
      dev|preview) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  next)
    case "$_tes_a1" in
      dev|start) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
    esac
    ;;
  nuxt|nuxi)
    case "$_tes_a1" in
      dev|preview|start) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  ng|webpack)
    case "$_tes_a1" in
      serve|s) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  remix)
    case "$_tes_a1" in
      dev|vite:dev) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  react-router|netlify)
    case "$_tes_a1" in
      dev|serve) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  gatsby)
    case "$_tes_a1" in
      develop|serve) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
    esac
    ;;
  eleventy)
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "--" ]; then break; fi
      case "$_tes_a" in
        --serve|--watch) _tes_acquire_port 0; _tes_exec_inject 1 "--port=$_tes_port" "$@" ;;
      esac
    done
    ;;
  hexo)
    case "$_tes_a1" in
      server|s) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
    esac
    ;;
  vue-cli-service|deno)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 --port "$_tes_port" "$@"
    fi
    ;;
  quasar)
    case "$_tes_a1" in
      dev|serve) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
    esac
    ;;
  storybook)
    case "$_tes_a1" in
      dev|start) _tes_acquire_port 6; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
    esac
    ;;
  start-storybook)
    _tes_acquire_port 6
    _tes_exec_inject 2 -p "$_tes_port" "$@"
    ;;
  http-server|rackup|puma|unicorn|thin|shotgun|miniserve)
    _tes_acquire_port 0
    _tes_exec_inject 2 -p "$_tes_port" "$@"
    ;;
  expo|react-native)
    if [ "$_tes_a1" = "start" ]; then
      _tes_acquire_port 9
      _tes_exec_inject 2 --port "$_tes_port" "$@"
    fi
    ;;
  browser-sync)
    if [ "$_tes_a1" = "start" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 --port "$_tes_port" "$@"
    fi
    ;;
  wrangler)
    case "$_tes_a1" in
      dev) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
      pages)
        if [ "$_tes_a2" = "dev" ]; then
          _tes_acquire_port 0
          _tes_exec_inject 2 --port "$_tes_port" "$@"
        fi
        ;;
    esac
    ;;
  vercel)
    if [ "$_tes_a1" = "dev" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 --listen "$_tes_port" "$@"
    fi
    ;;
  prisma|drizzle-kit)
    if [ "$_tes_a1" = "studio" ]; then
      _tes_acquire_port 8
      _tes_exec_inject 2 --port "$_tes_port" "$@"
    fi
    ;;
  parcel)
    case "$_tes_a1" in
      serve|watch|*.html|*.js|*.ts) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  docusaurus)
    case "$_tes_a1" in
      start|serve) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  webpack-dev-server|uvicorn|granian|sanic|sphinx-autobuild)
    _tes_acquire_port 0
    _tes_exec_inject 2 --port "$_tes_port" "$@"
    ;;
  live-server)
    _tes_acquire_port 0
    _tes_exec_inject 1 "--port=$_tes_port" "$@"
    ;;
  serve)
    _tes_acquire_port 0
    _tes_exec_inject 2 -l "$_tes_port" "$@"
    ;;
  python|python3)
    case "$_tes_a1" in
      manage.py|*/manage.py)
        if [ "$_tes_a2" = "runserver" ]; then
          if ! _tes_has_positional_after $((_tes_skip + 2)) "$@"; then
            _tes_acquire_port 0
            _tes_exec_inject 1 "$_tes_port" "$@"
          fi
        fi
        ;;
      -m)
        case "$_tes_a2" in
          http.server)
            if ! _tes_has_positional_after $((_tes_skip + 2)) "$@"; then
              _tes_acquire_port 0
              _tes_exec_inject 1 "$_tes_port" "$@"
            fi
            ;;
          uvicorn|granian|sanic|sphinx_autobuild)
            _tes_acquire_port 0
            _tes_exec_inject 2 --port "$_tes_port" "$@"
            ;;
          fastapi)
            case "$_tes_a3" in
              dev|run) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
            esac
            ;;
          flask)
            for _tes_a in "$@"; do
              if [ "$_tes_a" = "--" ]; then break; fi
              if [ "$_tes_a" = "run" ]; then
                _tes_acquire_port 0
                _tes_exec_inject 2 --port "$_tes_port" "$@"
              fi
            done
            ;;
          gunicorn|hypercorn)
            _tes_acquire_port 0
            _tes_exec_inject 2 --bind "127.0.0.1:$_tes_port" "$@"
            ;;
          daphne)
            _tes_acquire_port 0
            _tes_exec_inject 2 -p "$_tes_port" "$@"
            ;;
          litestar)
            for _tes_a in "$@"; do
              if [ "$_tes_a" = "--" ]; then break; fi
              if [ "$_tes_a" = "run" ]; then
                _tes_acquire_port 0
                _tes_exec_inject 2 -p "$_tes_port" "$@"
              fi
            done
            ;;
          streamlit)
            if [ "$_tes_a3" = "run" ]; then
              _tes_acquire_port 11
              _tes_exec_inject 2 --server.port "$_tes_port" "$@"
            fi
            ;;
          mkdocs)
            if [ "$_tes_a3" = "serve" ]; then
              _tes_acquire_port 0
              _tes_exec_inject 2 --dev-addr "127.0.0.1:$_tes_port" "$@"
            fi
            ;;
          jupyter)
            case "$_tes_a3" in
              notebook|lab) _tes_acquire_port 10; _tes_exec_inject 1 "--port=$_tes_port" "$@" ;;
            esac
            ;;
          panel|bokeh)
            if [ "$_tes_a3" = "serve" ]; then
              _tes_acquire_port 13
              _tes_exec_inject 2 --port "$_tes_port" "$@"
            fi
            ;;
        esac
        ;;
    esac
    ;;
  django-admin)
    if [ "$_tes_a1" = "runserver" ]; then
      if ! _tes_has_positional_after $((_tes_skip + 1)) "$@"; then
        _tes_acquire_port 0
        _tes_exec_inject 1 "$_tes_port" "$@"
      fi
    fi
    ;;
  fastapi)
    case "$_tes_a1" in
      dev|run) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  flask)
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "--" ]; then break; fi
      if [ "$_tes_a" = "run" ]; then
        _tes_acquire_port 0
        _tes_exec_inject 2 --port "$_tes_port" "$@"
      fi
    done
    ;;
  gunicorn|hypercorn)
    _tes_acquire_port 0
    _tes_exec_inject 2 --bind "127.0.0.1:$_tes_port" "$@"
    ;;
  daphne)
    _tes_acquire_port 0
    _tes_exec_inject 2 -p "$_tes_port" "$@"
    ;;
  litestar)
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "--" ]; then break; fi
      if [ "$_tes_a" = "run" ]; then
        _tes_acquire_port 0
        _tes_exec_inject 2 -p "$_tes_port" "$@"
      fi
    done
    ;;
  streamlit)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 11
      _tes_exec_inject 2 --server.port "$_tes_port" "$@"
    fi
    ;;
  mkdocs)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 --dev-addr "127.0.0.1:$_tes_port" "$@"
    fi
    ;;
  jupyter)
    case "$_tes_a1" in
      notebook|lab) _tes_acquire_port 10; _tes_exec_inject 1 "--port=$_tes_port" "$@" ;;
    esac
    ;;
  panel|bokeh)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 13
      _tes_exec_inject 2 --port "$_tes_port" "$@"
    fi
    ;;
  zola|trunk|dx)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 --port "$_tes_port" "$@"
    fi
    ;;
  reflex)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 --frontend-port "$_tes_port" "$@"
    fi
    ;;
  pelican)
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "--" ]; then break; fi
      case "$_tes_a" in
        -l|--listen) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
      esac
    done
    ;;
  php)
    case "$_tes_a1" in
      artisan|*/artisan)
        case "$_tes_a2" in
          serve|octane:start|octane:frankenphp|octane:roadrunner|octane:swoole)
            _tes_acquire_port 0
            _tes_exec_inject 1 "--port=$_tes_port" "$@"
            ;;
        esac
        ;;
      spark|*/spark)
        if [ "$_tes_a2" = "serve" ]; then
          _tes_acquire_port 0
          _tes_exec_inject 2 --port "$_tes_port" "$@"
        fi
        ;;
      bin/cake|bin/cake.php|*/bin/cake|*/bin/cake.php)
        if [ "$_tes_a2" = "server" ]; then
          _tes_acquire_port 0
          _tes_exec_inject 2 -p "$_tes_port" "$@"
        fi
        ;;
      -S)
        if [ -n "$_tes_a2" ] && [ "$_tes_skip" -eq 0 ]; then
          case "$_tes_a2" in
            *:[0-9]*)
              _tes_host="${_tes_a2%:*}"
              _tes_orig_port="${_tes_a2##*:}"
              case "$_tes_orig_port" in
                8000|8080|3000|8888)
                  _tes_acquire_port 0
                  shift 2
                  exec "$_tes_real" -S "${_tes_host}:${_tes_port}" "$@"
                  ;;
              esac
              ;;
          esac
        fi
        ;;
    esac
    ;;
  symfony)
    case "$_tes_a1" in
      server:start|serve) _tes_acquire_port 0; _tes_exec_inject 1 "--port=$_tes_port" "$@" ;;
    esac
    ;;
  wp)
    if [ "$_tes_a1" = "server" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 1 "--port=$_tes_port" "$@"
    fi
    ;;
  ruby)
    _tes_is_httpd=0
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "--" ]; then break; fi
      if [ "$_tes_a" = "httpd" ]; then
        _tes_is_httpd=1
      fi
    done
    if [ "$_tes_is_httpd" -eq 1 ] && [ "$_tes_a1" = "-run" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 -p "$_tes_port" "$@"
    fi
    case "$_tes_a1" in
      bin/rails|*/bin/rails|rails)
        case "$_tes_a2" in
          server|s) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
        esac
        ;;
    esac
    ;;
  rails|hanami|middleman)
    case "$_tes_a1" in
      server|s) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
    esac
    ;;
  falcon)
    case "$_tes_a1" in
      serve|host) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  bridgetown)
    case "$_tes_a1" in
      start|s) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
    esac
    ;;
  jekyll)
    case "$_tes_a1" in
      serve|s) _tes_acquire_port 0; _tes_exec_inject 2 --port "$_tes_port" "$@" ;;
    esac
    ;;
  hugo)
    case "$_tes_a1" in
      server|serve) _tes_acquire_port 0; _tes_exec_inject 2 -p "$_tes_port" "$@" ;;
    esac
    ;;
  mdbook)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 -p "$_tes_port" "$@"
    fi
    ;;
  caddy)
    if [ "$_tes_a1" = "file-server" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 --listen ":$_tes_port" "$@"
    fi
    ;;
  encore)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 0
      _tes_exec_inject 2 --port "$_tes_port" "$@"
    fi
    ;;
  dotnet)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@"
    fi
    ;;
  flutter)
    if [ "$_tes_a1" = "run" ]; then
      for _tes_a in "$@"; do
        if [ "$_tes_a" = "--" ]; then break; fi
        case "$_tes_a" in
          web-server|chrome) _tes_acquire_port 9; _tes_exec_inject 1 "--web-port=$_tes_port" "$@" ;;
        esac
      done
    fi
    ;;
  mvn)
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "--" ]; then break; fi
      case "$_tes_a" in
        *spring-boot:run*) _tes_acquire_port 0; _tes_exec_inject 2 "-Dspring-boot.run.arguments=--server.port=$_tes_port" "-Dserver.port=$_tes_port" "$@" ;;
        *quarkus:dev*) _tes_acquire_port 0; _tes_exec_inject 1 "-Dquarkus.http.port=$_tes_port" "$@" ;;
        *micronaut:run*) _tes_acquire_port 0; _tes_exec_inject 1 "-Dmicronaut.server.port=$_tes_port" "$@" ;;
        *jetty:run*) _tes_acquire_port 0; _tes_exec_inject 1 "-Djetty.http.port=$_tes_port" "$@" ;;
      esac
    done
    ;;
  gradle)
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "--" ]; then break; fi
      case "$_tes_a" in
        bootRun|quarkusDev) _tes_acquire_port 0; _tes_exec_inject 2 "-Dserver.port=$_tes_port" "-Dquarkus.http.port=$_tes_port" "$@" ;;
      esac
    done
    ;;
  sbt)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "-Dhttp.port=$_tes_port" "$@"
    fi
    ;;
  lein)
    if [ "$_tes_a1" = "ring" ]; then
      case "$_tes_a2" in
        server|server-headless)
          if [ -z "$_tes_a3" ]; then
            _tes_acquire_port 0
            _tes_exec_inject 1 "$_tes_port" "$@"
          fi
          ;;
      esac
    fi
    ;;
esac

exec "$_tes_real" "$@"
"#;

        let shim_names: &[&str] = &[
            "node",
            "bun",
            "deno",
            "npx",
            "pnpx",
            "bunx",
            "vite",
            "vitepress",
            "astro",
            "next",
            "nuxt",
            "nuxi",
            "ng",
            "remix",
            "react-router",
            "svelte-kit",
            "gatsby",
            "eleventy",
            "hexo",
            "vue-cli-service",
            "quasar",
            "storybook",
            "start-storybook",
            "expo",
            "react-native",
            "wrangler",
            "netlify",
            "vercel",
            "prisma",
            "drizzle-kit",
            "parcel",
            "docusaurus",
            "webpack",
            "webpack-dev-server",
            "http-server",
            "live-server",
            "serve",
            "browser-sync",
            "python",
            "python3",
            "uv",
            "poetry",
            "pipenv",
            "pdm",
            "hatch",
            "django-admin",
            "uvicorn",
            "fastapi",
            "flask",
            "gunicorn",
            "hypercorn",
            "daphne",
            "granian",
            "sanic",
            "litestar",
            "streamlit",
            "mkdocs",
            "jupyter",
            "panel",
            "bokeh",
            "reflex",
            "pelican",
            "sphinx-autobuild",
            "php",
            "symfony",
            "wp",
            "ruby",
            "bundle",
            "rails",
            "rackup",
            "puma",
            "unicorn",
            "thin",
            "shotgun",
            "falcon",
            "hanami",
            "middleman",
            "bridgetown",
            "jekyll",
            "hugo",
            "mdbook",
            "zola",
            "trunk",
            "dx",
            "miniserve",
            "caddy",
            "encore",
            "dotnet",
            "flutter",
            "mvn",
            "gradle",
            "sbt",
            "lein",
        ];

        let current_path = std::env::var_os("PATH").unwrap_or_default();
        let mut search_dirs: Vec<PathBuf> = std::env::split_paths(&current_path)
            .filter(|dir| dir != bin_dir)
            .collect();

        for env_root in ["TES_WORKSPACE", "TES_REPO_ROOT"] {
            if let Ok(root_val) = std::env::var(env_root) {
                let root_path = Path::new(&root_val);

                for rel in [
                    ".venv/bin",
                    "venv/bin",
                    "node_modules/.bin",
                    "vendor/bin",
                    "bin",
                ] {
                    let candidate_dir = root_path.join(rel);

                    if candidate_dir.is_dir() && !search_dirs.contains(&candidate_dir) {
                        search_dirs.push(candidate_dir);
                    }
                }
            }
        }

        if let Ok(cwd) = std::env::current_dir() {
            for rel in [
                ".venv/bin",
                "venv/bin",
                "node_modules/.bin",
                "vendor/bin",
                "bin",
            ] {
                let candidate_dir = cwd.join(rel);

                if candidate_dir.is_dir() && !search_dirs.contains(&candidate_dir) {
                    search_dirs.push(candidate_dir);
                }
            }
        }

        for name in shim_names {
            let target = bin_dir.join(name);

            let exists_on_path = search_dirs.iter().any(|dir| {
                let candidate = dir.join(name);
                candidate
                    .metadata()
                    .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
                    .unwrap_or(false)
            });

            // Never delete shared shims from `bin_dir` when a tool is absent in the current
            // branch's cwd, because other active branches share `bin_dir`.
            if !exists_on_path {
                continue;
            }

            let needs_write = match fs::read_to_string(&target) {
                Ok(existing) => existing != unified_script,
                Err(_) => true,
            };

            if needs_write {
                let _ = atomic_write_executable_unix(&target, unified_script.as_bytes());
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = bin_dir;
    }
}

/// Returns `true` if `key` is a privileged dynamic-linker, compiler, proxy, credential, or
/// runtime-execution hijack variable that must never be overridden via `custom_env`.
pub fn is_forbidden_custom_env_key(key: &str) -> bool {
    let trimmed = key.trim();

    if trimmed.is_empty()
        || trimmed
            .as_bytes()
            .first()
            .is_none_or(|b| b.is_ascii_digit())
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return true;
    }

    let upper = trimmed.to_ascii_uppercase();

    upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || upper.starts_with("GIT_")
        || upper.starts_with("TES_")
        || upper.starts_with("CARGO_")
        || upper.starts_with("BASH_FUNC_")
        || matches!(
            upper.as_str(),
            "PATH"
                | "HOME"
                | "SHELL"
                | "IFS"
                | "CDPATH"
                | "FPATH"
                | "SHELLOPTS"
                | "GCONV_PATH"
                | "TMPDIR"
                | "TEMP"
                | "TMP"
                | "COMSPEC"
                | "PATHEXT"
                | "SYSTEMROOT"
                | "WINDIR"
                | "PSMODULEPATH"
                | "USERPROFILE"
                | "APPDATA"
                | "XDG_CONFIG_HOME"
                | "CC"
                | "CXX"
                | "AR"
                | "LD"
                | "MAKE"
                | "MAKEFLAGS"
                | "CMAKE_TOOLCHAIN_FILE"
                | "RUSTC"
                | "RUSTDOC"
                | "RUSTFLAGS"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "RUSTUP_HOME"
                | "RUSTUP_TOOLCHAIN"
                | "GOFLAGS"
                | "GOWORK"
                | "GOENV"
                | "GOPATH"
                | "GOBIN"
                | "CGO_CFLAGS"
                | "CGO_LDFLAGS"
                | "GOPROXY"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "NODE_EXTRA_CA_CERTS"
                | "NODE_TLS_REJECT_UNAUTHORIZED"
                | "NPM_CONFIG_SCRIPT_SHELL"
                | "NPM_CONFIG_USERCONFIG"
                | "NPM_CONFIG_GLOBALCONFIG"
                | "NPM_CONFIG_REGISTRY"
                | "PYTHONSTARTUP"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "PYTHONUSERBASE"
                | "PYTHONBREAKPOINT"
                | "PIP_CONFIG_FILE"
                | "PIP_INDEX_URL"
                | "PIP_EXTRA_INDEX_URL"
                | "UV_INDEX_URL"
                | "RUBYOPT"
                | "RUBYLIB"
                | "GEM_HOME"
                | "BUNDLE_GEMFILE"
                | "PERL5OPT"
                | "PERL5LIB"
                | "PERL5DB"
                | "PERLLIB"
                | "JAVA_TOOL_OPTIONS"
                | "_JAVA_OPTIONS"
                | "JDK_JAVA_OPTIONS"
                | "CLASSPATH"
                | "MAVEN_OPTS"
                | "GRADLE_OPTS"
                | "SBT_OPTS"
                | "ANT_OPTS"
                | "DOTNET_STARTUP_HOOKS"
                | "DOTNET_ADDITIONAL_DEPS"
                | "DOTNET_ROOT"
                | "CORECLR_ENABLE_PROFILING"
                | "CORECLR_PROFILER"
                | "CORECLR_PROFILER_PATH"
                | "PHPRC"
                | "PHP_INI_SCAN_DIR"
                | "ERL_FLAGS"
                | "ERL_AFLAGS"
                | "ERL_ZFLAGS"
                | "ELIXIR_ERL_OPTIONS"
                | "LUA_INIT"
                | "BASH_ENV"
                | "ENV"
                | "ZDOTDIR"
                | "SSH_ASKPASS"
                | "SSH_ASKPASS_REQUIRE"
                | "SSH_AUTH_SOCK"
                | "SUDO_ASKPASS"
                | "PROMPT_COMMAND"
                | "PS1"
                | "PS2"
                | "PS4"
                | "EDITOR"
                | "VISUAL"
                | "PAGER"
                | "BROWSER"
                | "LESSOPEN"
                | "LESSCLOSE"
                | "OPENSSL_CONF"
                | "OPENSSL_ENGINES"
                | "OPENSSL_MODULES"
                | "SSL_CERT_FILE"
                | "REQUESTS_CA_BUNDLE"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "CURL_HOME"
                | "WGETRC"
        )
}

/// Performs single-pass placeholder interpolation for `{slot}`, `{port}`, `{branch}`, and
/// `{agent_id}` so substituted values containing placeholder-like syntax cannot double-interpolate.
fn interpolate_env_template(
    template: &str,
    slot: &str,
    port: &str,
    branch: &str,
    agent_id: &str,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("{slot}") {
            out.push_str(slot);
            rest = after;
        } else if let Some(after) = rest.strip_prefix("{port}") {
            out.push_str(port);
            rest = after;
        } else if let Some(after) = rest.strip_prefix("{branch}") {
            out.push_str(branch);
            rest = after;
        } else if let Some(after) = rest.strip_prefix("{agent_id}") {
            out.push_str(agent_id);
            rest = after;
        } else if let Some(ch) = rest.chars().next() {
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }

    out
}

/// Computes deterministic runtime environment variables for an agent session so parallel
/// test suites and dev servers never collide on `PORT`, `DATABASE_URL`, or `TMPDIR`.
pub fn build_agent_env(
    agent: &AgentMetadata,
    base_port: u16,
    working_dir: &Path,
) -> BTreeMap<String, String> {
    let cfg = RepoConfig {
        base_port,
        port_isolation: true,
        ..RepoConfig::default()
    };
    build_agent_env_with_config(agent, &cfg, working_dir)
}

/// Computes deterministic runtime environment variables using the full [`RepoConfig`]
/// (`base_port`, `port_stride`, `extra_bin_dirs`, and `custom_env` template interpolation).
pub fn build_agent_env_with_config(
    agent: &AgentMetadata,
    cfg: &RepoConfig,
    working_dir: &Path,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let stride = if cfg.port_stride == 0 {
        100
    } else {
        cfg.port_stride
    };

    // Keep base slot ports inside [0, 65435] so polyglot sub-port offsets (+21) and shim probing
    // never wrap or silently collide at 65535.
    let raw_port = u32::from(cfg.base_port) + u32::from(agent.slot) * u32::from(stride);
    let max_base_port: u32 = 65535 - 100;
    let port = if raw_port <= max_base_port {
        raw_port as u16
    } else {
        let band_start: u32 = 10000;
        let band_len = max_base_port - band_start + 1;
        (band_start + ((raw_port - band_start) % band_len)) as u16
    };

    let tmp_dir = working_dir
        .join(".tessera")
        .join("tmp")
        .join(&agent.agent_id);
    let branch_name = agent
        .target_branch
        .as_deref()
        .unwrap_or(cfg.trunk_branch.as_str());

    let db_name = format!("test_slot_{}", agent.slot);
    let base_db_url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| lookup_dotenv_var(working_dir, "DATABASE_URL"));
    let base_test_db_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| lookup_dotenv_var(working_dir, "TEST_DATABASE_URL"));

    env.insert("TES_AGENT_ID".to_string(), agent.agent_id.clone());
    env.insert("TES_AGENT_SLOT".to_string(), agent.slot.to_string());
    env.insert("TES_CHANGE_ID".to_string(), agent.change_id.to_string());
    env.insert("TES_INTENT".to_string(), agent.intent.clone());
    env.insert("TES_BRANCH".to_string(), branch_name.to_string());
    env.insert("TES_PORT".to_string(), port.to_string());

    if cfg.is_port_isolation_enabled() {
        env.insert("TES_PORT_ISOLATION".to_string(), "1".to_string());

        for (k, v) in polyglot_port_env_vars(port) {
            env.insert(k.to_string(), v);
        }
    }

    env.insert("TES_DB_NAME".to_string(), db_name);

    if let Some(url) = base_db_url {
        env.insert(
            "DATABASE_URL".to_string(),
            suffix_database_url(&url, agent.slot),
        );
    }

    if let Some(url) = base_test_db_url {
        env.insert(
            "TEST_DATABASE_URL".to_string(),
            suffix_database_url(&url, agent.slot),
        );
    }

    let tmp_str = tmp_dir.display().to_string();
    env.insert("TMPDIR".to_string(), tmp_str.clone());

    #[cfg(windows)]
    {
        env.insert("TEMP".to_string(), tmp_str.clone());
        env.insert("TMP".to_string(), tmp_str);
    }

    if working_dir.join("Cargo.toml").is_file() {
        env.insert(
            "CARGO_TARGET_DIR".to_string(),
            working_dir.join("target").display().to_string(),
        );
    }

    let mut path_prefixes: Vec<PathBuf> = Vec::new();
    let venv_bin_subdir = if cfg!(windows) { "Scripts" } else { "bin" };

    for venv_name in [".venv", "venv"] {
        if let Ok((_, venv_dir)) = resolve_confined_repo_path(working_dir, venv_name) {
            if venv_dir.is_dir() {
                env.insert("VIRTUAL_ENV".to_string(), venv_dir.display().to_string());

                if let Ok((_, bin_dir)) = resolve_confined_repo_path(
                    working_dir,
                    &format!("{venv_name}/{venv_bin_subdir}"),
                ) {
                    if bin_dir.is_dir() {
                        path_prefixes.push(bin_dir);
                    }
                }

                break;
            }
        }
    }

    for rel_bin in &cfg.extra_bin_dirs {
        if let Ok((_, bin_dir)) = resolve_confined_repo_path(working_dir, rel_bin) {
            if bin_dir.is_dir() && !path_prefixes.contains(&bin_dir) {
                path_prefixes.push(bin_dir);
            }
        }
    }

    if !path_prefixes.is_empty() {
        let mut all_paths = path_prefixes;

        if let Some(base_path) = std::env::var_os("PATH") {
            all_paths.extend(std::env::split_paths(&base_path));
        }

        if let Ok(joined) = std::env::join_paths(&all_paths) {
            env.insert("PATH".to_string(), joined.to_string_lossy().into_owned());
        }
    }

    let slot_str = agent.slot.to_string();
    let port_str = port.to_string();

    for (k, template) in &cfg.custom_env {
        if is_forbidden_custom_env_key(k) || template.contains('\0') {
            continue;
        }

        let interpolated =
            interpolate_env_template(template, &slot_str, &port_str, branch_name, &agent.agent_id);

        if interpolated.contains('\0') {
            continue;
        }

        env.insert(k.trim().to_string(), interpolated);
    }

    env
}

/// Parses a `.env` value expression, supporting double-quoted values (with trailing `# comment`),
/// single-quoted values (with trailing `# comment`), and unquoted values with inline `# comments`.
fn parse_dotenv_value(raw_val: &str) -> String {
    let val_trimmed = raw_val.trim();

    if let Some(after_open) = val_trimmed.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = after_open.chars();

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    match next {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        other => {
                            out.push('\\');
                            out.push(other);
                        }
                    }
                } else {
                    out.push('\\');
                }
            } else if ch == '"' {
                return out;
            } else {
                out.push(ch);
            }
        }

        return out;
    }

    if let Some(after_open) = val_trimmed.strip_prefix('\'') {
        if let Some((inside, _)) = after_open.split_once('\'') {
            return inside.to_string();
        }

        return after_open.to_string();
    }

    let mut prev_ws = false;

    for (idx, ch) in val_trimmed.char_indices() {
        if ch == '#' && prev_ws {
            return val_trimmed[..idx].trim_end().to_string();
        }

        prev_ws = ch == ' ' || ch == '\t';
    }

    val_trimmed.to_string()
}

/// Looks up `key` inside `.env.development.local`, `.env.local`, `.env.development`, or `.env`
/// inside `working_dir` without modifying process state, using last-match-wins within each file.
fn lookup_dotenv_var(working_dir: &Path, key: &str) -> Option<String> {
    for file_name in [
        ".env.development.local",
        ".env.local",
        ".env.development",
        ".env",
    ] {
        let Ok((_, path)) = resolve_confined_repo_path(working_dir, file_name) else {
            continue;
        };

        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };

        if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > 64 * 1024 {
            continue;
        }

        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };

        let content = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let mut last_match: Option<String> = None;

        for line in content.lines() {
            let trimmed = line.trim();

            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let body = if let Some(rest) = trimmed.strip_prefix("export") {
                if rest.starts_with([' ', '\t']) {
                    rest.trim_start()
                } else {
                    trimmed
                }
            } else {
                trimmed
            };

            let Some((k, v)) = body.split_once('=') else {
                continue;
            };

            if k.trim() != key {
                continue;
            }

            let parsed = parse_dotenv_value(v);

            if !parsed.is_empty() && !parsed.chars().any(|c| c.is_control()) {
                last_match = Some(parsed);
            }
        }

        if last_match.is_some() {
            return last_match;
        }
    }

    None
}

/// Strips any trailing `_slot_<digits>` suffixes from a database name to prevent double-suffixing.
fn strip_slot_suffix(db_path: &str) -> &str {
    let mut cur = db_path;

    while let Some((prefix, digits)) = cur.rsplit_once("_slot_") {
        if !prefix.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            cur = prefix;
        } else {
            break;
        }
    }

    cur
}

/// Appends `_slot_<slot>` to the database name in `raw_url` when `raw_url` targets a local
/// network database server (`localhost`, `127.0.0.1`, `[::1]`, `0.0.0.0`), while preserving
/// file-based database URLs (`file:`, `sqlite:`, `sqlite3:`, `sqlite+*:`, `*.db`, `*.sqlite*`),
/// HTTP/WebSocket endpoints, Redis/Valkey URLs, trailing slashes, `#fragment`, and remote hosts.
fn suffix_database_url(raw_url: &str, slot: u16) -> String {
    let trimmed = raw_url.trim();

    if trimmed.is_empty() {
        return raw_url.to_string();
    }

    let (before_frag, frag_opt) = match trimmed.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (trimmed, None),
    };
    let (before_query, query_opt) = match before_frag.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (before_frag, None),
    };

    let lower = before_query.to_ascii_lowercase();

    if lower.starts_with("sqlite:")
        || lower.starts_with("sqlite3:")
        || lower.starts_with("sqlite+")
        || lower.starts_with("file:")
        || lower.starts_with("duckdb:")
        || lower.starts_with("libsql://file:")
        || lower.contains(":memory:")
        || lower.ends_with(".db")
        || lower.ends_with(".sqlite")
        || lower.ends_with(".sqlite3")
        || lower.ends_with(".duckdb")
    {
        return raw_url.to_string();
    }

    let Some((scheme, rest)) = before_query.split_once("://") else {
        return raw_url.to_string();
    };

    let scheme_lower = scheme.to_ascii_lowercase();

    if matches!(
        scheme_lower.as_str(),
        "redis" | "rediss" | "valkey" | "http" | "https" | "ws" | "wss" | "sqlite" | "sqlite3"
    ) || scheme_lower.starts_with("sqlite+")
    {
        return raw_url.to_string();
    }

    let Some((authority, db_path)) = rest.split_once('/') else {
        return raw_url.to_string();
    };

    let trimmed_db_path = db_path.trim_end_matches('/');
    let trailing_slashes = &db_path[trimmed_db_path.len()..];

    if trimmed_db_path.is_empty() {
        return raw_url.to_string();
    }

    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let host = if host_port.starts_with('[') {
        host_port
            .split_once(']')
            .map_or(host_port, |(h, _)| &h[1..])
    } else {
        host_port.split_once(':').map_or(host_port, |(h, _)| h)
    };

    let host_lower = host.to_ascii_lowercase();
    let is_local_host = host_lower.is_empty()
        || matches!(
            host_lower.as_str(),
            "localhost" | "127.0.0.1" | "::1" | "0.0.0.0" | "host.docker.internal"
        )
        || host_lower.ends_with(".localhost");

    if !is_local_host {
        return raw_url.to_string();
    }

    let base_db_path = strip_slot_suffix(trimmed_db_path);
    let mut out = format!("{scheme}://{authority}/{base_db_path}_slot_{slot}{trailing_slashes}");

    if let Some(query) = query_opt {
        out.push('?');
        out.push_str(query);
    }

    if let Some(frag) = frag_opt {
        out.push('#');
        out.push_str(frag);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{ChangeId, ObjectId};
    use std::collections::BTreeSet;

    fn sample_agent(agent_id: &str, slot: u16, branch: Option<&str>) -> AgentMetadata {
        let oid = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
        AgentMetadata {
            agent_id: agent_id.to_string(),
            slot,
            change_id: ChangeId::from_seed(agent_id),
            base_commit: oid,
            head_commit: oid,
            head_tree: oid,
            intent: "test".to_string(),
            intent_inferred: false,
            claimed_symbols: BTreeSet::new(),
            scopes: Vec::new(),
            workspace_path: None,
            read_blobs: BTreeMap::new(),
            own_blobs: BTreeMap::new(),
            deleted_paths: BTreeSet::new(),
            foreign_baseline: BTreeSet::new(),
            integrated_peer_blobs: BTreeMap::new(),
            target_branch: branch.map(ToString::to_string),
            checkpoints: BTreeMap::new(),
            waypoint: None,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn assigns_non_overlapping_ports_and_db_names_per_slot() {
        let a1 = sample_agent("agent-1", 1, None);
        let a2 = sample_agent("agent-2", 2, None);

        // Repository with no `.env` must not fabricate DATABASE_URL or CARGO_TARGET_DIR.
        let empty_repo = tempfile::tempdir().expect("empty tempdir");
        let env1_unset = build_agent_env(&a1, 3000, empty_repo.path());
        let env2_unset = build_agent_env(&a2, 3000, empty_repo.path());

        assert_eq!(env1_unset["PORT"], "3100");
        assert_eq!(env2_unset["PORT"], "3200");
        assert_eq!(env1_unset["WDS_SOCKET_PORT"], "3100");
        assert_ne!(env1_unset["TES_DB_NAME"], env2_unset["TES_DB_NAME"]);
        assert_ne!(env1_unset["TMPDIR"], env2_unset["TMPDIR"]);
        assert!(!env1_unset.contains_key("CARGO_TARGET_DIR"));

        if std::env::var("DATABASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_none()
        {
            assert!(!env1_unset.contains_key("DATABASE_URL"));
        }

        if std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_none()
        {
            assert!(!env1_unset.contains_key("TEST_DATABASE_URL"));
        }

        // Repository with `.env` local Postgres URLs and `Cargo.toml` suffixes per slot and sets CARGO_TARGET_DIR.
        let db_repo = tempfile::tempdir().expect("db tempdir");
        std::fs::write(
            db_repo.path().join(".env"),
            "DATABASE_URL=postgres://localhost:5432/dev\nTEST_DATABASE_URL=postgres://localhost:5432/test\n",
        )
        .expect("write .env");
        std::fs::write(
            db_repo.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .expect("write Cargo.toml");

        let env1 = build_agent_env(&a1, 3000, db_repo.path());
        let env2 = build_agent_env(&a2, 3000, db_repo.path());

        assert!(env1["DATABASE_URL"].ends_with("_slot_1"));
        assert!(env2["DATABASE_URL"].ends_with("_slot_2"));
        assert!(env1["TEST_DATABASE_URL"].ends_with("_slot_1"));
        assert!(env2["TEST_DATABASE_URL"].ends_with("_slot_2"));
        assert!(env1.contains_key("CARGO_TARGET_DIR"));

        // Single-pass template interpolation prevents `{branch}` from injecting `{agent_id}`.
        let tricky_agent = sample_agent("agent-secret", 1, Some("feat/{agent_id}"));
        let mut cfg = RepoConfig::default();
        cfg.custom_env
            .insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        cfg.custom_env.insert(
            "DYLD_INSERT_LIBRARIES".to_string(),
            "/tmp/evil.dylib".to_string(),
        );
        cfg.custom_env
            .insert("BAD_NUL".to_string(), "abc\0def".to_string());
        cfg.custom_env
            .insert("APP_PORT".to_string(), "{port}".to_string());
        cfg.custom_env
            .insert("BRANCH_TAG".to_string(), "{branch}".to_string());

        let env_custom = build_agent_env_with_config(&tricky_agent, &cfg, empty_repo.path());
        assert!(!env_custom.contains_key("LD_PRELOAD"));
        assert!(!env_custom.contains_key("DYLD_INSERT_LIBRARIES"));
        assert!(!env_custom.contains_key("BAD_NUL"));
        assert_eq!(env_custom.get("APP_PORT").map(String::as_str), Some("3100"));
        assert_eq!(
            env_custom.get("BRANCH_TAG").map(String::as_str),
            Some("feat/{agent_id}")
        );
    }

    #[test]
    fn suffix_database_url_isolates_local_servers_and_preserves_sqlite_and_remote_urls() {
        assert_eq!(
            suffix_database_url("postgres://localhost:5432/myapp", 1),
            "postgres://localhost:5432/myapp_slot_1"
        );
        assert_eq!(
            suffix_database_url("postgres://localhost:5432/myapp_slot_1", 2),
            "postgres://localhost:5432/myapp_slot_2"
        );
        assert_eq!(
            suffix_database_url("postgres://localhost:5432/myapp_slot_1_slot_2/", 3),
            "postgres://localhost:5432/myapp_slot_3/"
        );
        assert_eq!(
            suffix_database_url(
                "postgresql://user:pass@127.0.0.1:5432/myapp?schema=public#frag",
                2
            ),
            "postgresql://user:pass@127.0.0.1:5432/myapp_slot_2?schema=public#frag"
        );
        assert_eq!(
            suffix_database_url("postgres://localhost:5432/myapp#section", 1),
            "postgres://localhost:5432/myapp_slot_1#section"
        );
        assert_eq!(
            suffix_database_url("mysql://root@[::1]:3306/shop", 3),
            "mysql://root@[::1]:3306/shop_slot_3"
        );
        assert_eq!(suffix_database_url("file:./dev.db", 1), "file:./dev.db");
        assert_eq!(
            suffix_database_url("sqlite:///var/data/app.sqlite3?mode=rwc", 2),
            "sqlite:///var/data/app.sqlite3?mode=rwc"
        );
        assert_eq!(
            suffix_database_url("sqlite+aiosqlite:///./app.db", 1),
            "sqlite+aiosqlite:///./app.db"
        );
        assert_eq!(
            suffix_database_url("sqlite3:///tmp/data", 1),
            "sqlite3:///tmp/data"
        );
        assert_eq!(
            suffix_database_url("sqlite+pysqlite:///./test.db", 1),
            "sqlite+pysqlite:///./test.db"
        );
        assert_eq!(suffix_database_url("sqlite::memory:", 1), "sqlite::memory:");
        assert_eq!(
            suffix_database_url("http://localhost:8080/db", 1),
            "http://localhost:8080/db"
        );
        assert_eq!(
            suffix_database_url("https://localhost:8443/db", 1),
            "https://localhost:8443/db"
        );
        assert_eq!(
            suffix_database_url("ws://localhost:8080/ws", 1),
            "ws://localhost:8080/ws"
        );
        assert_eq!(
            suffix_database_url("wss://localhost:8443/ws", 1),
            "wss://localhost:8443/ws"
        );

        let neon_url = "postgresql://owner:secret@ep-cool-frog-123.us-east-2.aws.neon.tech/neondb?sslmode=require";
        assert_eq!(suffix_database_url(neon_url, 1), neon_url);

        let supabase_url = "postgresql://postgres:secret@db.abcdefgh.supabase.co:5432/postgres";
        assert_eq!(suffix_database_url(supabase_url, 2), supabase_url);
    }

    #[test]
    fn lookup_dotenv_handles_precedence_bom_export_tab_last_match_and_quoted_inline_comments() {
        let dir = tempfile::tempdir().expect("tempdir");

        std::fs::write(
            dir.path().join(".env"),
            "\u{feff}DATABASE_URL=\"postgres://localhost:5432/first\" # initial\nexport\tDATABASE_URL=\"postgres://localhost:5432/second\" # override\n",
        )
        .expect("write .env");
        assert_eq!(
            lookup_dotenv_var(dir.path(), "DATABASE_URL").as_deref(),
            Some("postgres://localhost:5432/second")
        );

        std::fs::write(
            dir.path().join(".env.local"),
            "DATABASE_URL='postgres://localhost:5432/local' # local comment\n",
        )
        .expect("write .env.local");
        assert_eq!(
            lookup_dotenv_var(dir.path(), "DATABASE_URL").as_deref(),
            Some("postgres://localhost:5432/local")
        );

        std::fs::write(
            dir.path().join(".env.development.local"),
            "DATABASE_URL=postgres://localhost:5432/devlocal # dev local\n",
        )
        .expect("write .env.development.local");
        assert_eq!(
            lookup_dotenv_var(dir.path(), "DATABASE_URL").as_deref(),
            Some("postgres://localhost:5432/devlocal")
        );
    }

    #[test]
    fn forbidden_custom_env_key_blocks_rce_and_invalid_identifiers() {
        for forbidden in [
            "1INVALID",
            "0_FOO",
            "COMSPEC",
            "PATHEXT",
            "SYSTEMROOT",
            "WINDIR",
            "PSMODULEPATH",
            "USERPROFILE",
            "APPDATA",
            "CC",
            "CXX",
            "AR",
            "LD",
            "MAKE",
            "MAKEFLAGS",
            "CMAKE_TOOLCHAIN_FILE",
            "GOENV",
            "GOPATH",
            "GOBIN",
            "CGO_CFLAGS",
            "CGO_LDFLAGS",
            "GOPROXY",
            "RUSTUP_HOME",
            "RUSTUP_TOOLCHAIN",
            "BROWSER",
            "PYTHONUSERBASE",
            "PYTHONBREAKPOINT",
            "PIP_CONFIG_FILE",
            "PIP_INDEX_URL",
            "PIP_EXTRA_INDEX_URL",
            "UV_INDEX_URL",
            "NPM_CONFIG_SCRIPT_SHELL",
            "NPM_CONFIG_USERCONFIG",
            "NPM_CONFIG_GLOBALCONFIG",
            "NPM_CONFIG_REGISTRY",
            "NODE_EXTRA_CA_CERTS",
            "NODE_TLS_REJECT_UNAUTHORIZED",
            "PHPRC",
            "PHP_INI_SCAN_DIR",
            "CORECLR_ENABLE_PROFILING",
            "CORECLR_PROFILER",
            "CORECLR_PROFILER_PATH",
            "DOTNET_ADDITIONAL_DEPS",
            "DOTNET_ROOT",
            "CLASSPATH",
            "MAVEN_OPTS",
            "GRADLE_OPTS",
            "SBT_OPTS",
            "ANT_OPTS",
            "PERL5DB",
            "PERLLIB",
            "ERL_FLAGS",
            "ERL_AFLAGS",
            "ERL_ZFLAGS",
            "ELIXIR_ERL_OPTIONS",
            "LUA_INIT",
            "GEM_HOME",
            "BUNDLE_GEMFILE",
            "FPATH",
            "SHELLOPTS",
            "GCONV_PATH",
            "TMPDIR",
            "TEMP",
            "TMP",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "SSL_CERT_FILE",
            "REQUESTS_CA_BUNDLE",
            "SSH_AUTH_SOCK",
            "XDG_CONFIG_HOME",
        ] {
            assert!(
                is_forbidden_custom_env_key(forbidden),
                "expected '{forbidden}' to be forbidden"
            );
        }

        assert!(!is_forbidden_custom_env_key("NEXT_PUBLIC_API_URL"));
        assert!(!is_forbidden_custom_env_key("MY_APP_FLAG_1"));
    }

    #[cfg(unix)]
    #[test]
    fn polyglot_shims_handle_dotnet_ipv6_php_pelican_double_dash_and_non_deletion() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let tmp = tempfile::tempdir().expect("tempdir");
        let shim_dir = tmp.path().join("shims");
        let fake_bin = tmp.path().join("fake-bin");
        fs::create_dir_all(&fake_bin).expect("mkdir fake-bin");

        for tool in ["dotnet", "php", "pelican", "vite", "storybook"] {
            let p = fake_bin.join(tool);
            fs::write(
                &p,
                "#!/bin/sh\nprintf 'ARGS:%s|PORT:%s|SB:%s' \"$*\" \"${PORT:-}\" \"${SB_PORT:-}\"\n",
            )
            .expect("write fake tool");
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        // Write a pre-existing shim for a tool not in fake_bin to confirm it is never deleted.
        fs::create_dir_all(&shim_dir).expect("mkdir shim_dir");
        fs::write(shim_dir.join("uvicorn"), "#!/bin/sh\necho keep\n").expect("write uvicorn shim");

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", fake_bin.display());
        let workspace_bin = tmp.path().join("ws");
        fs::create_dir_all(workspace_bin.join("node_modules/.bin")).expect("mkdir local bin");
        fs::write(
            workspace_bin.join("node_modules/.bin/vite"),
            "#!/bin/sh\nprintf 'LOCAL_VITE:%s' \"$*\"\n",
        )
        .expect("write local vite");
        fs::set_permissions(
            workspace_bin.join("node_modules/.bin/vite"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("chmod local vite");

        // Ensure shims exist by invoking with PATH containing fake_bin (via sub-invocations below).
        // Materialize shims using fake_bin in TES_WORKSPACE/bin via current process call or copy.
        let fake_ws_bin = tmp.path().join("ws/bin");
        fs::create_dir_all(&fake_ws_bin).expect("mkdir ws/bin");

        for tool in ["dotnet", "php", "pelican", "vite", "storybook"] {
            fs::copy(fake_bin.join(tool), fake_ws_bin.join(tool)).expect("copy tool");
        }

        // Temporarily place fake_bin via PATH for ensure_polyglot_port_shims? Instead, avoid mutating
        // process env; invoke ensure_polyglot_port_shims after checking which tools are installed or
        // test the generated script directly by creating shims for tools present via `bin_dir`.
        // Note: `sh` is always on PATH, whereas fake tools are in `fake_bin`. Let's check `ensure_polyglot_port_shims`
        // preserves pre-existing `uvicorn` shim even when absent from current cwd.
        ensure_polyglot_port_shims(&shim_dir);
        assert!(
            shim_dir.join("uvicorn").is_file(),
            "shared shim in bin_dir must not be deleted when absent in current cwd"
        );

        // Extract the unified shim script from any materialized shim (e.g. python3/ruby/node) or
        // verify our atomic writer directly so test does not depend on host tool installations.
        let sample_shim = ["python3", "ruby", "node", "php"]
            .into_iter()
            .map(|name| shim_dir.join(name))
            .find(|p| p.is_file());

        if let Some(script_path) = sample_shim {
            let script_bytes = fs::read(&script_path).expect("read shim script");

            for tool in ["dotnet", "php", "pelican", "vite", "storybook"] {
                atomic_write_executable_unix(&shim_dir.join(tool), &script_bytes)
                    .expect("write test shim");
            }

            let run = |tool: &str, args: &[&str], ws: Option<&Path>| -> String {
                let mut cmd = Command::new(shim_dir.join(tool));
                cmd.current_dir(tmp.path())
                    .env("PATH", format!("{}:{test_path}", shim_dir.display()))
                    .env("PORT", "3100")
                    .env("TES_PORT", "3100")
                    .env("SB_PORT", "3106")
                    .env("STORYBOOK_PORT", "3106")
                    .env("TES_PORT_ISOLATION", "1")
                    .env("TES_SKIP_TCP_PROBE", "1");

                if let Some(w) = ws {
                    cmd.env("TES_WORKSPACE", w);
                }

                let out = cmd.args(args).output().expect("run shim");
                assert!(
                    out.status.success(),
                    "shim {tool} failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8_lossy(&out.stdout).to_string()
            };

            // `dotnet run` must not inject `--urls`, relying on ASPNETCORE_URLS instead.
            let dotnet_out = run("dotnet", &["run"], None);
            assert_eq!(dotnet_out, "ARGS:run|PORT:3100|SB:3106");

            // `php -S [::1]:8000` preserves IPv6 host brackets; explicit custom port is not clobbered.
            let php_v6 = run("php", &["-S", "[::1]:8000", "-t", "public"], None);
            assert_eq!(php_v6, "ARGS:-S [::1]:3100 -t public|PORT:3100|SB:3106");

            let php_custom = run("php", &["-S", "[::1]:9123", "-t", "public"], None);
            assert_eq!(php_custom, "ARGS:-S [::1]:9123 -t public|PORT:3100|SB:3106");

            // `pelican --listen` must not early-bail and must inject `-p 3100`.
            let pelican_out = run("pelican", &["--listen"], None);
            assert_eq!(pelican_out, "ARGS:--listen -p 3100|PORT:3100|SB:3106");

            // Port flags must be inserted BEFORE `--`, and local workspace `node_modules/.bin` wins over global PATH.
            let vite_out = run("vite", &["dev", "--", "--extra"], Some(&workspace_bin));
            assert_eq!(vite_out, "LOCAL_VITE:dev --port 3100 -- --extra");

            // Offset port (storybook +6 = 3106) must NOT clobber main PORT (3100).
            let sb_out = run("storybook", &["dev"], None);
            assert_eq!(sb_out, "ARGS:dev -p 3106|PORT:3100|SB:3106");

            // Lease held by parent `$PPID` does not self-collide, while a live sibling PID causes stepping to 3101.
            let lease_3100 = shim_dir.join(".leases/3100");
            fs::create_dir_all(&lease_3100).expect("mkdir lease 3100");
            fs::write(lease_3100.join("pid"), format!("{}\n", std::process::id()))
                .expect("write parent pid");
            let same_parent_out = run("pelican", &["--listen"], None);
            assert_eq!(same_parent_out, "ARGS:--listen -p 3100|PORT:3100|SB:3106");

            let mut sibling = Command::new("sleep")
                .arg("10")
                .spawn()
                .expect("spawn sibling sleep");
            fs::write(lease_3100.join("pid"), format!("{}\n", sibling.id()))
                .expect("write sibling pid");
            let stepped_out = run("pelican", &["--listen"], None);
            let _ = sibling.kill();
            let _ = sibling.wait();
            assert_eq!(stepped_out, "ARGS:--listen -p 3101|PORT:3101|SB:3106");
        }
    }
}
