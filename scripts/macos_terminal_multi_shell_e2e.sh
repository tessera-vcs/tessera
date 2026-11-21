#!/usr/bin/env bash
set -euo pipefail

# Automated macOS Terminal.app AppleScript Multi-Shell E2E Test Suite.
# Opens 5 real Terminal.app windows via `osascript` and tests Tessera across:
# - /bin/bash (3.2)
# - Homebrew bash (5.x)
# - Homebrew fish
# - POSIX & C/Structured shells (/bin/sh, dash, ksh, mksh, oksh, yash, tcsh, csh, nu, elvish)
# - Zero-hook vanilla shell (without `tes env` sourced)

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TES_BIN="${TES_BIN:-$REPO_ROOT/src/target/release/tes}"

if [[ ! -x "$TES_BIN" ]]; then
  echo "==> Building release binary at $TES_BIN..."
  cargo build --release -p cli --manifest-path "$REPO_ROOT/src/Cargo.toml"
fi

TES_DIR="$(cd "$(dirname "$TES_BIN")" && pwd)"
TES_BIN="$TES_DIR/$(basename "$TES_BIN")"

BREW_PREFIX="$(brew --prefix 2>/dev/null || { if [ -d /opt/homebrew ]; then echo /opt/homebrew; else echo /usr/local; fi; })"
BREW_BIN="$BREW_PREFIX/bin"

BASH5_BIN=""
if [ -x "$BREW_BIN/bash" ]; then
  BASH5_BIN="$BREW_BIN/bash"
else
  CANDIDATE_BASH="$(command -v bash || true)"
  if [ -n "$CANDIDATE_BASH" ] && [ "$CANDIDATE_BASH" != "/bin/bash" ]; then
    BASH5_BIN="$CANDIDATE_BASH"
  fi
fi

if [ -z "$BASH5_BIN" ]; then
  echo "FAIL: modern bash not found (install with: brew install bash)" >&2
  exit 1
fi

FISH_BIN="$(command -v fish || echo "$BREW_BIN/fish")"
if [ ! -x "$FISH_BIN" ]; then
  echo "FAIL: fish not found (install with: brew install fish)" >&2
  exit 1
fi

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/tes-shells-e2e.XXXXXX")"
REMOTE_DIR="$TEST_ROOT/remote.git"
APP_DIR="$TEST_ROOT/polyglot-service"
LOG_DIR="$TEST_ROOT/logs"

mkdir -p "$REMOTE_DIR" "$APP_DIR" "$LOG_DIR"

echo "==> [Setup] Creating fresh bare Git remote at $REMOTE_DIR"
git init --bare -b main "$REMOTE_DIR" >/dev/null

echo "==> [Setup] Creating fresh real-world project at $APP_DIR"
cd "$APP_DIR"
git init -b main >/dev/null
git config user.name "Tessera Test"
git config user.email "test@tessera.getlunaris.dev"
git remote add origin "$REMOTE_DIR"

mkdir -p src test

cat > package.json << 'EOF'
{
  "name": "polyglot-service",
  "version": "1.0.0",
  "private": true,
  "scripts": {
    "test": "node test/run.js"
  }
}
EOF

cat > src/math.js << 'EOF'
function add(a, b) {
  return a + b;
}

module.exports = { add };
EOF

cat > test/run.js << 'EOF'
const fs = require("fs");
const assert = require("assert");
const code = fs.readFileSync("src/math.js", "utf8");
assert(code.includes("function add"), "add missing");
console.log("Tests passed in", process.cwd(), "PORT=" + (process.env.PORT || "unset"));
EOF

git add .
git commit -m "chore: initial commit" >/dev/null
git push -u origin main

echo "==> [AppleScript] Opening 5 Terminal.app windows for Bash 3.2, Bash 5, Fish, POSIX/C/Structured Shells, and Zero-Hook Vanilla shell..."

WINDOWS_RAW=$(osascript <<EOF
tell application "Terminal"
  activate
  do script "exec /bin/bash --noprofile --norc"
  set win_bash32 to id of front window
  do script "exec '$BASH5_BIN' --noprofile --norc"
  set win_bash5 to id of front window
  do script "exec '$FISH_BIN' --no-config"
  set win_fish to id of front window
  do script "exec /bin/sh"
  set win_posix to id of front window
  do script "exec /bin/sh"
  set win_nohook to id of front window
  return (win_bash32 as string) & " " & (win_bash5 as string) & " " & (win_fish as string) & " " & (win_posix as string) & " " & (win_nohook as string)
end tell
EOF
)

read -r WIN_BASH32 WIN_BASH5 WIN_FISH WIN_POSIX WIN_NOHOOK <<< "$WINDOWS_RAW"
echo "==> Opened 5 Terminal.app windows: WIN_BASH32=$WIN_BASH32, WIN_BASH5=$WIN_BASH5, WIN_FISH=$WIN_FISH, WIN_POSIX=$WIN_POSIX, WIN_NOHOOK=$WIN_NOHOOK"

# Closes all spawned Terminal.app windows and removes temporary test directories on exit.
cleanup_windows() {
  osascript <<EOF >/dev/null 2>&1 || true
tell application "Terminal"
  repeat with wid in {$WIN_BASH32, $WIN_BASH5, $WIN_FISH, $WIN_POSIX, $WIN_NOHOOK}
    if exists (window id wid) then
      close window id wid saving no
    end if
  end repeat
end tell
EOF
  rm -rf "$TEST_ROOT"
}
trap cleanup_windows EXIT

sleep 1

# Escapes metacharacters in sed replacement strings (`\`, `&`, `|`).
escape_sed_replacement() {
  printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'
}

# Executes a shell script inside a target Terminal.app window via `osascript`
# and blocks until its exit status is written to disk.
run_in_window() {
  local window_id="$1"
  local step_name="$2"
  local script_file="$3"
  local shell_kind="${4:-posix}"
  local status_file="$LOG_DIR/${step_name}.status"
  local out_file="$LOG_DIR/${step_name}.out"
  local esc_app_dir esc_tes_dir esc_brew_bin

  rm -f "$status_file" "$out_file"
  esc_app_dir="$(escape_sed_replacement "$APP_DIR")"
  esc_tes_dir="$(escape_sed_replacement "$TES_DIR")"
  esc_brew_bin="$(escape_sed_replacement "$BREW_BIN")"

  sed -i '' "s|__APP_DIR__|${esc_app_dir}|g" "$script_file"
  sed -i '' "s|__TES_DIR__|${esc_tes_dir}|g" "$script_file"
  sed -i '' "s|__BREW_BIN__|${esc_brew_bin}|g" "$script_file"

  if [ "$shell_kind" = "fish" ]; then
    local runner_file="$LOG_DIR/${step_name}.runner.fish"
    cat > "$runner_file" <<EOF
begin
  source '$script_file'
end > '$out_file' 2>&1
echo \$status > '$status_file'
EOF
    osascript <<EOF >/dev/null
tell application "Terminal"
  do script "source '$runner_file'" in tab 1 of window id $window_id
end tell
EOF
  else
    local runner_file="$LOG_DIR/${step_name}.runner.sh"
    cat > "$runner_file" <<EOF
_tes_run_step() {
  . '$script_file'
}
_tes_run_step > '$out_file' 2>&1
echo \$? > '$status_file'
EOF
    osascript <<EOF >/dev/null
tell application "Terminal"
  do script ". '$runner_file'" in tab 1 of window id $window_id
end tell
EOF
  fi

  local waited=0
  while [ ! -f "$status_file" ]; do
    sleep 0.2
    waited=$((waited + 1))
    if [ "$waited" -gt 200 ]; then
      echo "TIMEOUT waiting for '$step_name' in window $window_id" >&2
      cat "$out_file" 2>/dev/null || true
      exit 1
    fi
  done

  local code
  code="$(cat "$status_file" | tr -d '[:space:]')"
  echo "[$step_name] (Window $window_id, Exit=$code)"
  cat "$out_file"

  if [ "$code" != "0" ]; then
    echo "Step '$step_name' failed with exit code $code" >&2
    exit 1
  fi
}

cat > "$LOG_DIR/bash32_init.sh" << 'EOF'
export PATH="__TES_DIR__:$HOME/.cargo/bin:__BREW_BIN__:$PATH"
unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID TES_SESSION_ID
echo "BASH_VERSION=$BASH_VERSION"
cd "__APP_DIR__"
tes init
tes config --verify-cmd "node test/run.js"
eval "$(tes env --bash)"
echo "Bash 3.2 session: TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT"
if [ -z "$TES_SESSION_ID" ] || [ "$TES_BRANCH" != "main" ]; then
  echo "FAIL: Bash 3.2 env not initialized properly" >&2
  return 1
fi
tes claim --branch main --intent "Add subtract in Bash 3.2" src/math.js
tes hook pre-tool --path src/math.js
cat > src/math.js << 'INNER'
function add(a, b) {
  return a + b;
}

function subtract(a, b) {
  return a - b;
}

module.exports = { add, subtract };
INNER
tes hook post-tool --path src/math.js
tes status
EOF
run_in_window "$WIN_BASH32" "bash32_init_and_edit" "$LOG_DIR/bash32_init.sh" "posix"

cat > "$LOG_DIR/bash5_concurrent_and_switch.sh" << 'EOF'
export PATH="__TES_DIR__:$HOME/.cargo/bin:__BREW_BIN__:$PATH"
unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID TES_SESSION_ID
echo "BASH_VERSION=$BASH_VERSION"
cd "__APP_DIR__"
eval "$(tes env --bash)"
echo "Bash 5 session: TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT"
tes claim --branch main --intent "Add multiply in Bash 5" src/math.js
tes hook pre-tool --path src/math.js
cat >> src/math.js << 'INNER'

function multiply(a, b) {
  return a * b;
}
INNER
tes hook post-tool --path src/math.js
grep -q "subtract" src/math.js || { echo "FAIL: subtract missing after Bash 5 AST merge" >&2; return 1; }
grep -q "multiply" src/math.js || { echo "FAIL: multiply missing after Bash 5 AST merge" >&2; return 1; }
echo "✓ Bash 3.2 + Bash 5 concurrent edits cleanly AST-merged on main"

tes commit -m "feat(math): add subtract and multiply from Bash terminals"

tes switch -b feat/bash5-divide
echo "Bash 5 switched to: TES_BRANCH=$TES_BRANCH TES_WORKSPACE=$TES_WORKSPACE"
if [ "$TES_BRANCH" != "feat/bash5-divide" ]; then
  echo "FAIL: TES_BRANCH not updated in Bash 5 after tes switch -b" >&2
  return 1
fi
tes claim --branch feat/bash5-divide --intent "Add divide on feat/bash5-divide" src/math.js
cat >> src/math.js << 'INNER'

function divide(a, b) {
  return a / b;
}
INNER
tes hook post-tool --path src/math.js
tes commit -m "feat(math): add divide on feat/bash5-divide"
EOF
run_in_window "$WIN_BASH5" "bash5_concurrent_and_switch" "$LOG_DIR/bash5_concurrent_and_switch.sh" "posix"

cat > "$LOG_DIR/fish_live_session.fish" << 'EOF'
set -gx PATH "__TES_DIR__" "$HOME/.cargo/bin" "__BREW_BIN__" $PATH
set -e ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID TES_SESSION_ID
echo "FISH_VERSION=$FISH_VERSION"
cd "__APP_DIR__"
tes env --fish | source
echo "Fish session: TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT"
if test -z "$TES_SESSION_ID"; or test "$TES_BRANCH" != "main"
  echo "FAIL: Fish env not initialized properly" >&2
  exit 1
end

tes switch -b feat/fish-power
echo "Fish switched to: TES_BRANCH=$TES_BRANCH TES_WORKSPACE=$TES_WORKSPACE"
if test "$TES_BRANCH" != "feat/fish-power"
  echo "FAIL: TES_BRANCH not updated in Fish after tes switch -b" >&2
  exit 1
end

tes claim --branch feat/fish-power --intent "Add power function in Fish shell" src/math.js
printf "\nfunction power(a, b) {\n  return Math.pow(a, b);\n}\n" >> src/math.js
tes hook post-tool --path src/math.js
tes commit -m "feat(math): add power on feat/fish-power from Fish shell"
echo "✓ Live Fish terminal session switched branch, edited, reconciled, and committed cleanly"
EOF
run_in_window "$WIN_FISH" "fish_live_session" "$LOG_DIR/fish_live_session.fish" "fish"

cat > "$LOG_DIR/all_other_shells.sh" << 'EOF'
export PATH="__TES_DIR__:$HOME/.cargo/bin:__BREW_BIN__:$PATH"
unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID TES_SESSION_ID
cd "__APP_DIR__"

echo "Testing POSIX /bin/sh"
eval "$(tes env --sh)"
echo "POSIX /bin/sh: TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT TES_PROMPT=$TES_PROMPT"
[ "$TES_BRANCH" = "main" ] || { echo "FAIL: /bin/sh expected main" >&2; return 1; }
tes switch -b feat/posix-modulo
[ "$TES_BRANCH" = "feat/posix-modulo" ] || { echo "FAIL: /bin/sh switch failed" >&2; return 1; }
tes switch main
echo "✓ /bin/sh passed"

for shell_name in dash ksh mksh oksh yash; do
  posix_bin="$(command -v "$shell_name" 2>/dev/null || true)"
  if [ -n "$posix_bin" ] && [ -x "$posix_bin" ]; then
    echo "Testing $posix_bin"
    "$posix_bin" -e -c '
      export PATH="__TES_DIR__:$HOME/.cargo/bin:__BREW_BIN__:$PATH"
      unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID TES_SESSION_ID
      cd "__APP_DIR__"
      eval "$(tes env --sh)"
      echo "'"$posix_bin"': TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT TES_PROMPT=$TES_PROMPT"
      [ "$TES_BRANCH" = "main" ] || exit 1
      tes switch feat/bash5-divide >/dev/null
      [ "$TES_BRANCH" = "feat/bash5-divide" ] || exit 1
      tes switch main >/dev/null
      [ "$TES_BRANCH" = "main" ] || exit 1
    ' || { echo "FAIL: $posix_bin failed" >&2; return 1; }
    echo "✓ $posix_bin parsed tes env --sh and switched branches cleanly"
  fi
done

for csh_bin in /bin/tcsh /bin/csh; do
  if [ -x "$csh_bin" ]; then
    echo "Testing $csh_bin"
    "$csh_bin" -f -e << 'CSH_INNER' || { echo "FAIL: $csh_bin failed" >&2; return 1; }
setenv PATH "__TES_DIR__:${HOME}/.cargo/bin:__BREW_BIN__:${PATH}"
unsetenv ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID TES_SESSION_ID
cd "__APP_DIR__"
eval "`tes env --tcsh`"
echo "tcsh/csh: TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT TES_PROMPT=$TES_PROMPT"
if ( "$TES_BRANCH" != "main" ) exit 1
tes switch feat/fish-power
if ( "$TES_BRANCH" != "feat/fish-power" ) exit 1
tes switch main
if ( "$TES_BRANCH" != "main" ) exit 1
CSH_INNER
    echo "✓ $csh_bin parsed tes env --tcsh and switched branches cleanly"
  fi
done

NU_BIN="$(command -v nu 2>/dev/null || true)"
if [ -n "$NU_BIN" ] && [ -x "$NU_BIN" ]; then
  echo "Testing Nushell ($NU_BIN)"
  command tes env --nu > "__APP_DIR__/.tes_init.nu"
  "$NU_BIN" -c '
    $env.PATH = ($env.PATH | prepend "__TES_DIR__" | prepend $"($env.HOME)/.cargo/bin" | prepend "__BREW_BIN__")
    hide-env -i ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID TES_SESSION_ID
    cd "__APP_DIR__"
    use "__APP_DIR__/.tes_init.nu" *
    print $"nu: TES_SESSION_ID=($env.TES_SESSION_ID) TES_BRANCH=($env.TES_BRANCH) TES_PORT=($env.TES_PORT) TES_PROMPT=($env.TES_PROMPT)"
    if $env.TES_BRANCH != "main" { exit 1 }
    tes switch feat/fish-power
    if $env.TES_BRANCH != "feat/fish-power" { exit 1 }
    tes switch main
    if $env.TES_BRANCH != "main" { exit 1 }
  ' || { rm -f "__APP_DIR__/.tes_init.nu"; echo "FAIL: Nushell failed" >&2; return 1; }
  rm -f "__APP_DIR__/.tes_init.nu"
  echo "✓ Nushell (nu) loaded tes env --nu and switched branches cleanly"
fi

ELVISH_BIN="$(command -v elvish 2>/dev/null || true)"
if [ -n "$ELVISH_BIN" ] && [ -x "$ELVISH_BIN" ]; then
  echo "Testing Elvish ($ELVISH_BIN)"
  "$ELVISH_BIN" -c '
    set E:PATH = "__TES_DIR__:"$E:HOME"/.cargo/bin:__BREW_BIN__:"$E:PATH
    unset-env ANTIGRAVITY_AGENT
    unset-env ANTIGRAVITY_CONVERSATION_ID
    unset-env TES_SESSION_ID
    cd "__APP_DIR__"
    eval (e:tes env --elvish | slurp)
    echo "elvish: TES_SESSION_ID="$E:TES_SESSION_ID" TES_BRANCH="$E:TES_BRANCH" TES_PORT="$E:TES_PORT" TES_PROMPT="$E:TES_PROMPT
    if (not-eq $E:TES_BRANCH "main") { exit 1 }
    tes switch feat/bash5-divide >/dev/null
    if (not-eq $E:TES_BRANCH "feat/bash5-divide") { exit 1 }
    tes switch main >/dev/null
    if (not-eq $E:TES_BRANCH "main") { exit 1 }
  ' || { echo "FAIL: Elvish failed" >&2; return 1; }
  echo "✓ Elvish evaluated tes env --elvish and switched branches cleanly"
fi
EOF
run_in_window "$WIN_POSIX" "posix_csh_nu_elvish_shells" "$LOG_DIR/all_other_shells.sh" "posix"

cat > "$LOG_DIR/nohook_vanilla_shell.sh" << 'EOF'
export PATH="__TES_DIR__:$HOME/.cargo/bin:__BREW_BIN__:$PATH"
unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID TES_SESSION_ID
cd "__APP_DIR__"

echo "Testing Zero-Hook Vanilla Shell (no tes env sourced)"
echo "TERM_SESSION_ID=${TERM_SESSION_ID:-none}"
S1=$(tes status --json | node -e 'const d=JSON.parse(require("fs").readFileSync(0,"utf8")); console.log(d.session.agent_id);')
S2=$(tes status --json | node -e 'const d=JSON.parse(require("fs").readFileSync(0,"utf8")); console.log(d.session.agent_id);')
echo "Detected stable session ID across invocations without tes env: $S1 vs $S2"
if [ -z "$S1" ] || [ "$S1" != "$S2" ]; then
  echo "FAIL: Session ID was not stable across commands without tes env ($S1 != $S2)" >&2
  return 1
fi

tes switch feat/bash5-divide
ACTIVE_BR=$(tes status --json | node -e 'const d=JSON.parse(require("fs").readFileSync(0,"utf8")); console.log(d.active_branch);')
echo "Zero-hook shell active branch after tes switch: $ACTIVE_BR"
if [ "$ACTIVE_BR" != "feat/bash5-divide" ]; then
  echo "FAIL: Zero-hook shell did not persist branch switch ($ACTIVE_BR)" >&2
  return 1
fi

tes switch main
tes merge feat/bash5-divide feat/fish-power --into main -m "feat: octopus merge Bash 5 and Fish branches from vanilla shell" --verify
grep -q "divide" src/math.js || { echo "FAIL: divide missing on main after merge" >&2; return 1; }
grep -q "power" src/math.js || { echo "FAIL: power missing on main after merge" >&2; return 1; }
grep -q "subtract" src/math.js || { echo "FAIL: subtract missing on main after merge" >&2; return 1; }
grep -q "multiply" src/math.js || { echo "FAIL: multiply missing on main after merge" >&2; return 1; }
git fsck --full --no-dangling
echo "✓ Zero-hook vanilla shell switched, octopus-merged Bash 5 + Fish branches, and verified cleanly"
EOF
run_in_window "$WIN_NOHOOK" "nohook_vanilla_shell" "$LOG_DIR/nohook_vanilla_shell.sh" "posix"

echo "ALL NON-ZSH SHELLS IN TERMINAL.APP PASSED!"
