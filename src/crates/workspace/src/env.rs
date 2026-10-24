//! Deterministic per-agent runtime environment variable computation (`PORT`, `DATABASE_URL`, `TMPDIR`).

use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs;
use std::path::Path;

#[cfg(unix)]
use core::atomic_write_file;
use core::{resolve_confined_repo_path, AgentMetadata, RepoConfig};

/// Returns the polyglot set of port environment variables (`PORT`, `TES_PORT`, `SERVER_PORT`,
/// `FLASK_RUN_PORT`, `UVICORN_PORT`, `ROCKET_PORT`, `NITRO_PORT`, etc.) mapped to `port` so
/// web servers across Go, Rust, Python, PHP, Ruby, Java/Kotlin/Scala/Clojure, .NET, Elixir,
/// Dart/Flutter, and Node/Bun/Deno automatically bind to the session's isolated port.
#[must_use]
pub fn polyglot_port_env_vars(port: u16) -> Vec<(&'static str, String)> {
    let p = port.to_string();
    let storybook_port = port.saturating_add(6).to_string();
    let docs_port = port.saturating_add(7).to_string();
    let studio_port = port.saturating_add(8).to_string();
    let mobile_port = port.saturating_add(9).to_string();
    let jupyter_port = port.saturating_add(10).to_string();
    let streamlit_port = port.saturating_add(11).to_string();
    let gradio_port = port.saturating_add(12).to_string();
    let bokeh_port = port.saturating_add(13).to_string();
    let dash_port = port.saturating_add(14).to_string();
    let wds_port = port.saturating_add(20).to_string();
    let remix_ws_port = port.saturating_add(21).to_string();

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
        ("WDS_SOCKET_PORT", wds_port),
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

        let unified_script = r#"#!/bin/sh
_tes_self_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
_tes_bin_name=$(basename -- "$0")
_tes_real=""
_tes_search_path="$PATH"
for _tes_root in "${TES_WORKSPACE:-}" "${TES_REPO_ROOT:-}" "$PWD"; do
  if [ -n "$_tes_root" ]; then
    _tes_search_path="$_tes_search_path:$_tes_root/.venv/bin:$_tes_root/venv/bin:$_tes_root/node_modules/.bin:$_tes_root/vendor/bin:$_tes_root/bin"
  fi
done
_tes_old_ifs="$IFS"
IFS=":"
for _tes_dir in $_tes_search_path; do
  if [ -n "$_tes_dir" ] && [ "$_tes_dir" != "$_tes_self_dir" ] && [ -x "$_tes_dir/$_tes_bin_name" ] && [ ! -d "$_tes_dir/$_tes_bin_name" ]; then
    _tes_real="$_tes_dir/$_tes_bin_name"
    break
  fi
done
IFS="$_tes_old_ifs"
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
TES_PORT="$_tes_base_port"
for _tes_arg in "$@"; do
  case "$_tes_arg" in
    --port|--port=*|-p|-p=*|-P|--listen|--listen=*|--bind|--bind=*|-b|--urls|--urls=*|--dev-addr|--dev-addr=*|--web-port|--web-port=*|--server.port|--server.port=*|-Dserver.port=*|-Dquarkus.http.port=*|-Dmicronaut.server.port=*|-Dhttp.port=*|-Djetty.http.port=*|--help|-h|--version|-v|-V)
      exec "$_tes_real" "$@"
      ;;
  esac
done

_tes_is_port_busy() {
  _tes_chk_port="$1"
  case "$_tes_chk_port" in
    ''|*[!0-9]*) return 1 ;;
  esac
  _tes_lease_file="$_tes_self_dir/.leases/$_tes_chk_port/pid"
  if [ -f "$_tes_lease_file" ]; then
    _tes_holder_pid=$(cat "$_tes_lease_file" 2>/dev/null || true)
    if [ -n "$_tes_holder_pid" ] && [ "$_tes_holder_pid" != "$$" ] && kill -0 "$_tes_holder_pid" 2>/dev/null; then
      return 0
    fi
  fi
  if command -v lsof >/dev/null 2>&1; then
    if lsof -nP -iTCP:"$_tes_chk_port" -sTCP:LISTEN >/dev/null 2>&1; then
      return 0
    fi
  elif command -v nc >/dev/null 2>&1; then
    if nc -z 127.0.0.1 "$_tes_chk_port" >/dev/null 2>&1; then
      return 0
    fi
  fi
  return 1
}

_tes_acquire_port() {
  _tes_off="${1:-0}"
  case "$_tes_off" in
    ''|*[!0-9]*) _tes_off=0 ;;
  esac
  case "${PORT:-}" in
    ''|*[!0-9]*)
      _tes_start_port=$((_tes_base_port + _tes_off))
      ;;
    *)
      if [ -n "${TES_PORT:-}" ] && [ "$PORT" != "$TES_PORT" ]; then
        _tes_start_port="$PORT"
      else
        _tes_start_port=$((_tes_base_port + _tes_off))
      fi
      ;;
  esac
  _tes_max_port=$((_tes_base_port + 99))
  if [ "$_tes_start_port" -gt "$_tes_max_port" ]; then
    _tes_max_port=$((_tes_start_port + 50))
  fi
  mkdir -p "$_tes_self_dir/.leases" 2>/dev/null || true
  _tes_try="$_tes_start_port"
  while [ "$_tes_try" -le "$_tes_max_port" ]; do
    if ! _tes_is_port_busy "$_tes_try"; then
      mkdir -p "$_tes_self_dir/.leases/$_tes_try" 2>/dev/null || true
      printf '%s\n' "$$" > "$_tes_self_dir/.leases/$_tes_try/pid" 2>/dev/null || true
      TES_PORT="$_tes_try"
      export PORT="$_tes_try" TES_PORT="$_tes_try" HTTP_PORT="$_tes_try" APP_PORT="$_tes_try" WEB_PORT="$_tes_try" DEV_SERVER_PORT="$_tes_try" BUN_PORT="$_tes_try" VITE_PORT="$_tes_try" NITRO_PORT="$_tes_try" NUXT_PORT="$_tes_try" ASTRO_PORT="$_tes_try" SB_PORT="$_tes_try" STORYBOOK_PORT="$_tes_try" SERVER_PORT="$_tes_try" FLASK_RUN_PORT="$_tes_try" UVICORN_PORT="$_tes_try" DJANGO_RUNSERVER_PORT="$_tes_try" RAILS_PORT="$_tes_try" ROCKET_PORT="$_tes_try" GO_PORT="$_tes_try" ASPNETCORE_HTTP_PORTS="$_tes_try" ASPNETCORE_URLS="http://localhost:$_tes_try" DOTNET_URLS="http://localhost:$_tes_try" QUARKUS_HTTP_PORT="$_tes_try" MICRONAUT_SERVER_PORT="$_tes_try" KTOR_DEPLOYMENT_PORT="$_tes_try" PHX_PORT="$_tes_try"
      return 0
    fi
    _tes_try=$((_tes_try + 1))
  done
  TES_PORT="$_tes_start_port"
  export PORT="$_tes_start_port"
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
      *) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  vitepress)
    case "$_tes_a1" in
      dev|preview|serve|"") _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  astro|svelte-kit)
    case "$_tes_a1" in
      dev|preview) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  next)
    case "$_tes_a1" in
      dev|start) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
    esac
    ;;
  nuxt|nuxi)
    case "$_tes_a1" in
      dev|preview|start) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  ng|webpack)
    case "$_tes_a1" in
      serve|s) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  remix)
    case "$_tes_a1" in
      dev|vite:dev) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  react-router|netlify)
    case "$_tes_a1" in
      dev|serve) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  gatsby)
    case "$_tes_a1" in
      develop|serve) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
    esac
    ;;
  eleventy)
    for _tes_a in "$@"; do
      case "$_tes_a" in
        --serve|--watch) _tes_acquire_port 0; exec "$_tes_real" "$@" --port="$TES_PORT" ;;
      esac
    done
    ;;
  hexo)
    case "$_tes_a1" in
      server|s) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
    esac
    ;;
  vue-cli-service|deno)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --port "$TES_PORT"
    fi
    ;;
  quasar)
    case "$_tes_a1" in
      dev|serve) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
    esac
    ;;
  storybook)
    case "$_tes_a1" in
      dev|start) _tes_acquire_port 6; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
    esac
    ;;
  start-storybook)
    _tes_acquire_port 6
    exec "$_tes_real" "$@" -p "$TES_PORT"
    ;;
  http-server|rackup|puma|unicorn|thin|shotgun|miniserve)
    _tes_acquire_port 0
    exec "$_tes_real" "$@" -p "$TES_PORT"
    ;;
  expo|react-native)
    if [ "$_tes_a1" = "start" ]; then
      _tes_acquire_port 9
      exec "$_tes_real" "$@" --port "$TES_PORT"
    fi
    ;;
  browser-sync)
    if [ "$_tes_a1" = "start" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --port "$TES_PORT"
    fi
    ;;
  wrangler)
    case "$_tes_a1" in
      dev) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
      pages)
        if [ "$_tes_a2" = "dev" ]; then
          _tes_acquire_port 0
          exec "$_tes_real" "$@" --port "$TES_PORT"
        fi
        ;;
    esac
    ;;
  vercel)
    if [ "$_tes_a1" = "dev" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --listen "$TES_PORT"
    fi
    ;;
  prisma|drizzle-kit)
    if [ "$_tes_a1" = "studio" ]; then
      _tes_acquire_port 8
      exec "$_tes_real" "$@" --port "$TES_PORT"
    fi
    ;;
  parcel)
    case "$_tes_a1" in
      serve|watch|*.html|*.js|*.ts) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  docusaurus)
    case "$_tes_a1" in
      start|serve) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  webpack-dev-server|uvicorn|granian|sanic|sphinx-autobuild)
    _tes_acquire_port 0
    exec "$_tes_real" "$@" --port "$TES_PORT"
    ;;
  live-server)
    _tes_acquire_port 0
    exec "$_tes_real" "$@" --port="$TES_PORT"
    ;;
  serve)
    _tes_acquire_port 0
    exec "$_tes_real" "$@" -l "$TES_PORT"
    ;;
  python|python3)
    case "$_tes_a1" in
      manage.py|*/manage.py)
        if [ "$_tes_a2" = "runserver" ]; then
          if ! _tes_has_positional_after $((_tes_skip + 2)) "$@"; then
            _tes_acquire_port 0
            exec "$_tes_real" "$@" "$TES_PORT"
          fi
        fi
        ;;
      -m)
        case "$_tes_a2" in
          http.server)
            if ! _tes_has_positional_after $((_tes_skip + 2)) "$@"; then
              _tes_acquire_port 0
              exec "$_tes_real" "$@" "$TES_PORT"
            fi
            ;;
          uvicorn|granian|sanic|sphinx_autobuild)
            _tes_acquire_port 0
            exec "$_tes_real" "$@" --port "$TES_PORT"
            ;;
          fastapi)
            case "$_tes_a3" in
              dev|run) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
            esac
            ;;
          flask)
            for _tes_a in "$@"; do
              if [ "$_tes_a" = "run" ]; then
                _tes_acquire_port 0
                exec "$_tes_real" "$@" --port "$TES_PORT"
              fi
            done
            ;;
          gunicorn|hypercorn)
            _tes_acquire_port 0
            exec "$_tes_real" "$@" --bind "127.0.0.1:$TES_PORT"
            ;;
          daphne)
            _tes_acquire_port 0
            exec "$_tes_real" "$@" -p "$TES_PORT"
            ;;
          litestar)
            for _tes_a in "$@"; do
              if [ "$_tes_a" = "run" ]; then
                _tes_acquire_port 0
                exec "$_tes_real" "$@" -p "$TES_PORT"
              fi
            done
            ;;
          streamlit)
            if [ "$_tes_a3" = "run" ]; then
              _tes_acquire_port 11
              exec "$_tes_real" "$@" --server.port "$TES_PORT"
            fi
            ;;
          mkdocs)
            if [ "$_tes_a3" = "serve" ]; then
              _tes_acquire_port 0
              exec "$_tes_real" "$@" --dev-addr "127.0.0.1:$TES_PORT"
            fi
            ;;
          jupyter)
            case "$_tes_a3" in
              notebook|lab) _tes_acquire_port 10; exec "$_tes_real" "$@" --port="$TES_PORT" ;;
            esac
            ;;
          panel|bokeh)
            if [ "$_tes_a3" = "serve" ]; then
              _tes_acquire_port 13
              exec "$_tes_real" "$@" --port "$TES_PORT"
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
        exec "$_tes_real" "$@" "$TES_PORT"
      fi
    fi
    ;;
  fastapi)
    case "$_tes_a1" in
      dev|run) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  flask)
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "run" ]; then
        _tes_acquire_port 0
        exec "$_tes_real" "$@" --port "$TES_PORT"
      fi
    done
    ;;
  gunicorn|hypercorn)
    _tes_acquire_port 0
    exec "$_tes_real" "$@" --bind "127.0.0.1:$TES_PORT"
    ;;
  daphne)
    _tes_acquire_port 0
    exec "$_tes_real" "$@" -p "$TES_PORT"
    ;;
  litestar)
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "run" ]; then
        _tes_acquire_port 0
        exec "$_tes_real" "$@" -p "$TES_PORT"
      fi
    done
    ;;
  streamlit)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 11
      exec "$_tes_real" "$@" --server.port "$TES_PORT"
    fi
    ;;
  mkdocs)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --dev-addr "127.0.0.1:$TES_PORT"
    fi
    ;;
  jupyter)
    case "$_tes_a1" in
      notebook|lab) _tes_acquire_port 10; exec "$_tes_real" "$@" --port="$TES_PORT" ;;
    esac
    ;;
  panel|bokeh)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 13
      exec "$_tes_real" "$@" --port "$TES_PORT"
    fi
    ;;
  zola|trunk|dx)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --port "$TES_PORT"
    fi
    ;;
  reflex)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --frontend-port "$TES_PORT"
    fi
    ;;
  pelican)
    for _tes_a in "$@"; do
      case "$_tes_a" in
        -l|--listen) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
      esac
    done
    ;;
  php)
    case "$_tes_a1" in
      artisan|*/artisan)
        case "$_tes_a2" in
          serve|octane:start|octane:frankenphp|octane:roadrunner|octane:swoole)
            _tes_acquire_port 0
            exec "$_tes_real" "$@" --port="$TES_PORT"
            ;;
        esac
        ;;
      spark|*/spark)
        if [ "$_tes_a2" = "serve" ]; then
          _tes_acquire_port 0
          exec "$_tes_real" "$@" --port "$TES_PORT"
        fi
        ;;
      bin/cake|bin/cake.php|*/bin/cake|*/bin/cake.php)
        if [ "$_tes_a2" = "server" ]; then
          _tes_acquire_port 0
          exec "$_tes_real" "$@" -p "$TES_PORT"
        fi
        ;;
      -S)
        if [ -n "$_tes_a2" ] && [ "$_tes_skip" -eq 0 ]; then
          _tes_acquire_port 0
          _tes_host="${_tes_a2%%:*}"
          shift 2
          exec "$_tes_real" -S "${_tes_host}:${TES_PORT}" "$@"
        fi
        ;;
    esac
    ;;
  symfony)
    case "$_tes_a1" in
      server:start|serve) _tes_acquire_port 0; exec "$_tes_real" "$@" --port="$TES_PORT" ;;
    esac
    ;;
  wp)
    if [ "$_tes_a1" = "server" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --port="$TES_PORT"
    fi
    ;;
  ruby)
    _tes_is_httpd=0
    for _tes_a in "$@"; do
      if [ "$_tes_a" = "httpd" ]; then
        _tes_is_httpd=1
      fi
    done
    if [ "$_tes_is_httpd" -eq 1 ] && [ "$_tes_a1" = "-run" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" -p "$TES_PORT"
    fi
    case "$_tes_a1" in
      bin/rails|*/bin/rails|rails)
        case "$_tes_a2" in
          server|s) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
        esac
        ;;
    esac
    ;;
  rails|hanami|middleman)
    case "$_tes_a1" in
      server|s) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
    esac
    ;;
  falcon)
    case "$_tes_a1" in
      serve|host) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  bridgetown)
    case "$_tes_a1" in
      start|s) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
    esac
    ;;
  jekyll)
    case "$_tes_a1" in
      serve|s) _tes_acquire_port 0; exec "$_tes_real" "$@" --port "$TES_PORT" ;;
    esac
    ;;
  hugo)
    case "$_tes_a1" in
      server|serve) _tes_acquire_port 0; exec "$_tes_real" "$@" -p "$TES_PORT" ;;
    esac
    ;;
  mdbook)
    if [ "$_tes_a1" = "serve" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" -p "$TES_PORT"
    fi
    ;;
  caddy)
    if [ "$_tes_a1" = "file-server" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --listen ":$TES_PORT"
    fi
    ;;
  encore)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --port "$TES_PORT"
    fi
    ;;
  dotnet)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "$@" --urls "http://localhost:$TES_PORT"
    fi
    ;;
  flutter)
    if [ "$_tes_a1" = "run" ]; then
      for _tes_a in "$@"; do
        case "$_tes_a" in
          web-server|chrome) _tes_acquire_port 9; exec "$_tes_real" "$@" --web-port="$TES_PORT" ;;
        esac
      done
    fi
    ;;
  mvn)
    for _tes_a in "$@"; do
      case "$_tes_a" in
        *spring-boot:run*) _tes_acquire_port 0; exec "$_tes_real" "$@" "-Dspring-boot.run.arguments=--server.port=$TES_PORT" "-Dserver.port=$TES_PORT" ;;
        *quarkus:dev*) _tes_acquire_port 0; exec "$_tes_real" "$@" "-Dquarkus.http.port=$TES_PORT" ;;
        *micronaut:run*) _tes_acquire_port 0; exec "$_tes_real" "$@" "-Dmicronaut.server.port=$TES_PORT" ;;
        *jetty:run*) _tes_acquire_port 0; exec "$_tes_real" "$@" "-Djetty.http.port=$TES_PORT" ;;
      esac
    done
    ;;
  gradle)
    for _tes_a in "$@"; do
      case "$_tes_a" in
        bootRun|quarkusDev) _tes_acquire_port 0; exec "$_tes_real" "$@" "-Dserver.port=$TES_PORT" "-Dquarkus.http.port=$TES_PORT" ;;
      esac
    done
    ;;
  sbt)
    if [ "$_tes_a1" = "run" ]; then
      _tes_acquire_port 0
      exec "$_tes_real" "-Dhttp.port=$TES_PORT" "$@"
    fi
    ;;
  lein)
    if [ "$_tes_a1" = "ring" ]; then
      case "$_tes_a2" in
        server|server-headless)
          if [ -z "$_tes_a3" ]; then
            _tes_acquire_port 0
            exec "$_tes_real" "$@" "$TES_PORT"
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
        let mut search_dirs: Vec<std::path::PathBuf> = std::env::split_paths(&current_path)
            .filter(|dir| dir != bin_dir)
            .collect();

        for env_root in ["TES_WORKSPACE", "TES_REPO_ROOT"] {
            if let Ok(root_val) = std::env::var(env_root) {
                let root_path = Path::new(&root_val);

                for rel in [".venv/bin", "venv/bin", "node_modules/.bin", "vendor/bin", "bin"] {
                    let candidate_dir = root_path.join(rel);

                    if candidate_dir.is_dir() && !search_dirs.contains(&candidate_dir) {
                        search_dirs.push(candidate_dir);
                    }
                }
            }
        }

        if let Ok(cwd) = std::env::current_dir() {
            for rel in [".venv/bin", "venv/bin", "node_modules/.bin", "vendor/bin", "bin"] {
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

            if !exists_on_path {
                let _ = fs::remove_file(&target);
                continue;
            }

            let needs_write = match fs::read_to_string(&target) {
                Ok(existing) => existing != unified_script,
                Err(_) => true,
            };

            if needs_write && atomic_write_file(&target, unified_script.as_bytes()).is_ok() {
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o755));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = bin_dir;
    }
}

/// Returns `true` if `key` is a privileged dynamic-linker or runtime-execution hijack
/// variable that must never be overridden via `custom_env`.
pub fn is_forbidden_custom_env_key(key: &str) -> bool {
    let trimmed = key.trim();

    if trimmed.is_empty()
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
                | "RUSTC"
                | "RUSTDOC"
                | "RUSTFLAGS"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "PYTHONSTARTUP"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "RUBYOPT"
                | "RUBYLIB"
                | "PERL5OPT"
                | "PERL5LIB"
                | "JAVA_TOOL_OPTIONS"
                | "_JAVA_OPTIONS"
                | "JDK_JAVA_OPTIONS"
                | "DOTNET_STARTUP_HOOKS"
                | "GOFLAGS"
                | "GOWORK"
                | "BASH_ENV"
                | "ENV"
                | "ZDOTDIR"
                | "SSH_ASKPASS"
                | "SSH_ASKPASS_REQUIRE"
                | "SUDO_ASKPASS"
                | "PROMPT_COMMAND"
                | "PS1"
                | "PS2"
                | "PS4"
                | "EDITOR"
                | "VISUAL"
                | "PAGER"
                | "LESSOPEN"
                | "LESSCLOSE"
                | "OPENSSL_CONF"
                | "OPENSSL_ENGINES"
                | "OPENSSL_MODULES"
                | "CURL_HOME"
                | "WGETRC"
        )
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
    let slot_offset = agent.slot.saturating_mul(stride);
    let port = cfg.base_port.saturating_add(slot_offset);
    let tmp_dir = working_dir.join(".tessera").join("tmp").join(&agent.agent_id);
    let branch_name = agent
        .target_branch
        .as_deref()
        .unwrap_or(cfg.trunk_branch.as_str());

    let db_name = format!("test_slot_{}", agent.slot);
    let base_db_url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| lookup_dotenv_var(working_dir, "DATABASE_URL"))
        .unwrap_or_else(|| "postgres://localhost:5432/dev".to_string());
    let base_test_db_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| lookup_dotenv_var(working_dir, "TEST_DATABASE_URL"))
        .unwrap_or_else(|| "postgres://localhost:5432/test".to_string());

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
    env.insert(
        "DATABASE_URL".to_string(),
        suffix_database_url(&base_db_url, agent.slot),
    );
    env.insert(
        "TEST_DATABASE_URL".to_string(),
        suffix_database_url(&base_test_db_url, agent.slot),
    );
    env.insert("TMPDIR".to_string(), tmp_dir.display().to_string());
    env.insert(
        "CARGO_TARGET_DIR".to_string(),
        working_dir.join("target").display().to_string(),
    );

    let mut path_prefixes = Vec::new();

    for venv_name in [".venv", "venv"] {
        if let Ok((_, venv_dir)) = resolve_confined_repo_path(working_dir, venv_name) {
            if venv_dir.is_dir() {
                env.insert("VIRTUAL_ENV".to_string(), venv_dir.display().to_string());

                if let Ok((_, bin_dir)) =
                    resolve_confined_repo_path(working_dir, &format!("{venv_name}/bin"))
                {
                    if bin_dir.is_dir() {
                        path_prefixes.push(bin_dir.display().to_string());
                    }
                }

                break;
            }
        }
    }

    for rel_bin in &cfg.extra_bin_dirs {
        if let Ok((_, bin_dir)) = resolve_confined_repo_path(working_dir, rel_bin) {
            if bin_dir.is_dir() {
                let bin_str = bin_dir.display().to_string();

                if !path_prefixes.contains(&bin_str) {
                    path_prefixes.push(bin_str);
                }
            }
        }
    }

    if !path_prefixes.is_empty() {
        let base_path = std::env::var("PATH").unwrap_or_default();
        let joined = if base_path.is_empty() {
            path_prefixes.join(":")
        } else {
            format!("{}:{base_path}", path_prefixes.join(":"))
        };
        env.insert("PATH".to_string(), joined);
    }

    for (k, template) in &cfg.custom_env {
        if is_forbidden_custom_env_key(k) {
            continue;
        }

        let interpolated = template
            .replace("{slot}", &agent.slot.to_string())
            .replace("{port}", &port.to_string())
            .replace("{branch}", branch_name)
            .replace("{agent_id}", &agent.agent_id);
        env.insert(k.trim().to_string(), interpolated);
    }

    env
}

/// Looks up `key` inside `.env.local`, `.env.development.local`, `.env.development`, or `.env`
/// inside `working_dir` without modifying process state.
fn lookup_dotenv_var(working_dir: &Path, key: &str) -> Option<String> {
    for file_name in [
        ".env.local",
        ".env.development.local",
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

        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let body = trimmed.strip_prefix("export ").map_or(trimmed, str::trim);
            let Some((k, v)) = body.split_once('=') else {
                continue;
            };

            if k.trim() != key {
                continue;
            }

            let val_trimmed = v.trim();
            let unquoted = if (val_trimmed.starts_with('"')
                && val_trimmed.ends_with('"')
                && val_trimmed.len() >= 2)
                || (val_trimmed.starts_with('\'')
                    && val_trimmed.ends_with('\'')
                    && val_trimmed.len() >= 2)
            {
                &val_trimmed[1..val_trimmed.len() - 1]
            } else {
                val_trimmed
                    .split_once(" #")
                    .map_or(val_trimmed, |(before_comment, _)| before_comment.trim())
            };

            if !unquoted.is_empty() && !unquoted.chars().any(|c| c.is_control()) {
                return Some(unquoted.to_string());
            }
        }
    }

    None
}

/// Appends `_slot_<slot>` to the database name in `raw_url` when `raw_url` targets a local
/// network database server (`localhost`, `127.0.0.1`, `[::1]`, `0.0.0.0`), while preserving
/// file-based database URLs (`file:`, `sqlite:`, `*.db`, `*.sqlite*`) and remote cloud hosts.
fn suffix_database_url(raw_url: &str, slot: u16) -> String {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return raw_url.to_string();
    }

    let (before_query, query_opt) = match trimmed.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (trimmed, None),
    };

    let lower = before_query.to_ascii_lowercase();
    if lower.starts_with("sqlite:")
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
    if matches!(scheme_lower.as_str(), "redis" | "rediss" | "valkey") {
        return raw_url.to_string();
    }

    let Some((authority, db_path)) = rest.split_once('/') else {
        return raw_url.to_string();
    };

    if db_path.is_empty() {
        return raw_url.to_string();
    }

    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, hp)| hp);
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

    let suffix = format!("_slot_{slot}");
    if before_query.ends_with(&suffix) {
        return raw_url.to_string();
    }

    if let Some(query) = query_opt {
        format!("{before_query}{suffix}?{query}")
    } else {
        format!("{before_query}{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::{ChangeId, ObjectId};
    use std::collections::BTreeSet;

    #[test]
    fn assigns_non_overlapping_ports_and_db_names_per_slot() {
        let oid = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
        let a1 = AgentMetadata {
            agent_id: "agent-1".to_string(),
            slot: 1,
            change_id: ChangeId::from_seed("a1"),
            base_commit: oid,
            head_commit: oid,
            head_tree: oid,
            intent: "test 1".to_string(),
            intent_inferred: false,
            claimed_symbols: BTreeSet::new(),
            scopes: Vec::new(),
            workspace_path: None,
            read_blobs: BTreeMap::new(),
            own_blobs: BTreeMap::new(),
            deleted_paths: BTreeSet::new(),
            foreign_baseline: BTreeSet::new(),
            integrated_peer_blobs: BTreeMap::new(),
            target_branch: None,
            checkpoints: BTreeMap::new(),
            waypoint: None,
            updated_at_ms: 0,
        };
        let mut a2 = a1.clone();
        a2.agent_id = "agent-2".to_string();
        a2.slot = 2;

        let env1 = build_agent_env(&a1, 3000, Path::new("/repo"));
        let env2 = build_agent_env(&a2, 3000, Path::new("/repo"));

        assert_eq!(env1["PORT"], "3100");
        assert_eq!(env2["PORT"], "3200");
        assert_ne!(env1["TES_DB_NAME"], env2["TES_DB_NAME"]);
        assert!(env1["DATABASE_URL"].ends_with("_slot_1"));
        assert!(env2["DATABASE_URL"].ends_with("_slot_2"));
        assert!(env1["TEST_DATABASE_URL"].ends_with("_slot_1"));
        assert!(env2["TEST_DATABASE_URL"].ends_with("_slot_2"));
        assert_ne!(env1["TMPDIR"], env2["TMPDIR"]);

        let mut cfg = RepoConfig::default();
        cfg.custom_env
            .insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        cfg.custom_env
            .insert("DYLD_INSERT_LIBRARIES".to_string(), "/tmp/evil.dylib".to_string());
        cfg.custom_env
            .insert("APP_PORT".to_string(), "{port}".to_string());

        let env_custom = build_agent_env_with_config(&a1, &cfg, Path::new("/repo"));
        assert!(!env_custom.contains_key("LD_PRELOAD"));
        assert!(!env_custom.contains_key("DYLD_INSERT_LIBRARIES"));
        assert_eq!(env_custom.get("APP_PORT").map(String::as_str), Some("3100"));
    }

    #[test]
    fn suffix_database_url_isolates_local_servers_and_preserves_sqlite_and_remote_urls() {
        assert_eq!(
            suffix_database_url("postgres://localhost:5432/myapp", 1),
            "postgres://localhost:5432/myapp_slot_1"
        );
        assert_eq!(
            suffix_database_url("postgresql://user:pass@127.0.0.1:5432/myapp?schema=public", 2),
            "postgresql://user:pass@127.0.0.1:5432/myapp_slot_2?schema=public"
        );
        assert_eq!(
            suffix_database_url("mysql://root@[::1]:3306/shop", 3),
            "mysql://root@[::1]:3306/shop_slot_3"
        );
        assert_eq!(
            suffix_database_url("file:./dev.db", 1),
            "file:./dev.db"
        );
        assert_eq!(
            suffix_database_url("sqlite:///var/data/app.sqlite3?mode=rwc", 2),
            "sqlite:///var/data/app.sqlite3?mode=rwc"
        );
        assert_eq!(
            suffix_database_url("sqlite::memory:", 1),
            "sqlite::memory:"
        );
        let neon_url =
            "postgresql://owner:secret@ep-cool-frog-123.us-east-2.aws.neon.tech/neondb?sslmode=require";
        assert_eq!(suffix_database_url(neon_url, 1), neon_url);

        let supabase_url = "postgresql://postgres:secret@db.abcdefgh.supabase.co:5432/postgres";
        assert_eq!(suffix_database_url(supabase_url, 2), supabase_url);

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".env"), "DATABASE_URL=\"file:./prisma/dev.db\"\n")
            .expect("write .env");
        assert_eq!(
            lookup_dotenv_var(dir.path(), "DATABASE_URL").as_deref(),
            Some("file:./prisma/dev.db")
        );
    }
}
