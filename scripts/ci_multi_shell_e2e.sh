#!/usr/bin/env bash
set -euo pipefail

# Cross-platform Multi-Shell & Full Lifecycle E2E Test Suite for Linux, macOS, FreeBSD, and OpenBSD.
# Tests `tes` across every installed shell (`zsh`, `bash`, `fish`, `sh`, `dash`, `ksh`, `mksh`, `oksh`,
# `yash`, `tcsh`, `csh`, `nu`, `elvish`, `pwsh`) plus zero-hook POSIX session isolation.

TES_BIN="${TES_BIN:-$(command -v tes || echo "./src/target/release/tes")}"
if [[ ! -x "$TES_BIN" ]]; then
  echo "❌ TES_BIN not executable at: $TES_BIN"
  exit 1
fi
TES_DIR="$(cd "$(dirname "$TES_BIN")" && pwd)"
TES_BIN="$TES_DIR/$(basename "$TES_BIN")"
export PATH="$TES_DIR:$PATH"

WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/tes-ci-e2e.XXXXXX")"
trap 'rm -rf "$WORK_ROOT"' EXIT
export TES_WORKSPACES_ROOT="$WORK_ROOT/sandboxes"

REPO="$WORK_ROOT/project"
REMOTE="$WORK_ROOT/remote.git"
mkdir -p "$REPO/src"
unset GIT_DIR GIT_WORK_TREE

git init --bare -b main "$REMOTE" >/dev/null 2>&1

cat << 'EOF' > "$REPO/package.json"
{
  "name": "ci-multi-shell-e2e",
  "version": "1.0.0"
}
EOF

cat << 'EOF' > "$REPO/src/lib.rs"
pub fn alpha() -> i32 {
    10
}

pub fn beta() -> i32 {
    20
}
EOF

cd "$REPO"
"$TES_BIN" init --branch main >/dev/null
git remote add origin "$REMOTE"
"$TES_BIN" push -u origin main >/dev/null

echo "Testing Shell Integrations (tes env) Across Installed Shells"

# Verifies that a POSIX-compatible shell properly evaluates `tes env` hooks
# and that its `tes()` wrapper automatically updates `TES_BRANCH` across switches.
test_posix_like_shell() {
  local sh_name="$1"
  local sh_bin="$2"
  local env_flag="$3"
  local branch_name="feat/shell-${sh_name}"

  echo "  -> Testing $sh_name ($sh_bin) with flag $env_flag on $branch_name..."
  "$sh_bin" -c "
    export PATH=\"$TES_DIR:\$PATH\"
    cd \"$REPO\"
    eval \"\$(tes env $env_flag)\"
    if [ \"\$TES_BRANCH\" != \"main\" ]; then
      echo \"FAIL ($sh_name): expected initial TES_BRANCH=main, got \$TES_BRANCH\"
      exit 1
    fi
    tes switch \"$branch_name\" >/dev/null
    if [ \"\$TES_BRANCH\" != \"$branch_name\" ]; then
      echo \"FAIL ($sh_name): tes() wrapper did not update TES_BRANCH to $branch_name, got \$TES_BRANCH\"
      exit 1
    fi
    tes switch main >/dev/null
    if [ \"\$TES_BRANCH\" != \"main\" ]; then
      echo \"FAIL ($sh_name): tes() wrapper did not restore TES_BRANCH to main, got \$TES_BRANCH\"
      exit 1
    fi
  "
  echo "     ✓ $sh_name passed"
}

for pair in \
  "zsh:zsh:--zsh" \
  "bash:bash:--bash" \
  "sh:sh:--sh" \
  "dash:dash:--sh" \
  "ksh:ksh:--sh" \
  "mksh:mksh:--sh" \
  "oksh:oksh:--sh" \
  "yash:yash:--sh"
do
  IFS=":" read -r name bin flag <<< "$pair"
  if command -v "$bin" >/dev/null 2>&1; then
    test_posix_like_shell "$name" "$(command -v "$bin")" "$flag"
  fi
done

if command -v fish >/dev/null 2>&1; then
  echo "  -> Testing fish ($(command -v fish)) with --fish..."
  fish -c "
    set -gx PATH \"$TES_DIR\" \$PATH
    cd \"$REPO\"
    tes env --fish | source
    if test \"\$TES_BRANCH\" != \"main\"
      echo \"FAIL (fish): expected initial main, got \$TES_BRANCH\"
      exit 1
    end
    tes switch feat/shell-fish >/dev/null
    if test \"\$TES_BRANCH\" != \"feat/shell-fish\"
      echo \"FAIL (fish): expected feat/shell-fish, got \$TES_BRANCH\"
      exit 1
    end
    tes switch main >/dev/null
    if test \"\$TES_BRANCH\" != \"main\"
      echo \"FAIL (fish): expected main after switch back, got \$TES_BRANCH\"
      exit 1
    end
  "
  echo "     ✓ fish passed"
fi

for csh_bin in tcsh csh; do
  if command -v "$csh_bin" >/dev/null 2>&1; then
    echo "  -> Testing $csh_bin ($(command -v "$csh_bin")) with --tcsh..."
    CSH_SCRIPT="$WORK_ROOT/test_${csh_bin}.csh"
    cat << CSH_EOF > "$CSH_SCRIPT"
setenv PATH "${TES_DIR}:\${PATH}"
cd "$REPO"
eval "\`tes env --tcsh\`"
if ( "\$TES_BRANCH" != "main" ) then
  echo "FAIL ($csh_bin): expected initial main, got \$TES_BRANCH"
  exit 1
endif
tes switch feat/shell-${csh_bin}
if ( "\$TES_BRANCH" != "feat/shell-${csh_bin}" ) then
  echo "FAIL ($csh_bin): tes alias did not update TES_BRANCH to feat/shell-${csh_bin}, got \$TES_BRANCH"
  exit 1
endif
tes switch main
if ( "\$TES_BRANCH" != "main" ) then
  echo "FAIL ($csh_bin): tes alias did not restore TES_BRANCH to main, got \$TES_BRANCH"
  exit 1
endif
CSH_EOF
    "$csh_bin" -f "$CSH_SCRIPT" >/dev/null
    echo "     ✓ $csh_bin passed"
  fi
done

if command -v nu >/dev/null 2>&1; then
  echo "  -> Testing nushell ($(command -v nu)) with --nu..."
  NU_HOOK="$WORK_ROOT/tes_hook.nu"
  OUTSIDE_DIR="$WORK_ROOT/outside-repo"
  mkdir -p "$OUTSIDE_DIR"
  "$TES_BIN" env --nu > "$NU_HOOK"

  # Verify loading the Nushell hook outside any Git/Tessera repository does not crash.
  nu -c "
    \$env.PATH = (\$env.PATH | prepend \"$TES_DIR\")
    cd \"$OUTSIDE_DIR\"
    use \"$NU_HOOK\" *
    if (\"TES_BRANCH\" in \$env) {
      error make {msg: \"Unexpected TES_BRANCH outside a Tessera repo\"}
    }
  " >/dev/null

  nu -c "
    \$env.PATH = (\$env.PATH | prepend \"$TES_DIR\")
    cd \"$REPO\"
    use \"$NU_HOOK\" *
    if \$env.TES_BRANCH != \"main\" {
      error make {msg: \"Expected initial Nushell TES_BRANCH=main\"}
    }
    tes switch feat/shell-nu
    if \$env.TES_BRANCH != \"feat/shell-nu\" {
      error make {msg: \"Nushell tes wrapper did not update TES_BRANCH to feat/shell-nu\"}
    }
    tes switch main
    if \$env.TES_BRANCH != \"main\" {
      error make {msg: \"Nushell tes wrapper did not restore TES_BRANCH to main\"}
    }
  " >/dev/null
  echo "     ✓ nushell passed"
fi

if command -v elvish >/dev/null 2>&1; then
  echo "  -> Testing elvish ($(command -v elvish)) with --elvish..."
  ELV_RC="$WORK_ROOT/tes_rc.elv"
  cat << ELV_EOF > "$ELV_RC"
set paths = ["$TES_DIR" \$@paths]
cd "$REPO"
ELV_EOF
  "$TES_BIN" env --elvish >> "$ELV_RC"
  cat << 'ELV_EOF' >> "$ELV_RC"
if (!=s $E:TES_BRANCH main) {
  fail 'Elvish initial TES_BRANCH mismatch'
}
tes switch feat/shell-elvish >/dev/null
if (!=s $E:TES_BRANCH feat/shell-elvish) {
  fail 'Elvish TES_BRANCH mismatch after switch'
}
tes switch main >/dev/null
if (!=s $E:TES_BRANCH main) {
  fail 'Elvish TES_BRANCH mismatch after switch back to main'
}
ELV_EOF
  elvish "$ELV_RC"
  echo "     ✓ elvish passed"
fi

if command -v pwsh >/dev/null 2>&1; then
  echo "  -> Testing PowerShell ($(command -v pwsh)) with --pwsh..."
  pwsh -NoProfile -NonInteractive -Command "
    \$env:PATH = \"$TES_DIR\" + [IO.Path]::PathSeparator + \$env:PATH
    Set-Location \"$REPO\"
    Invoke-Expression (& tes env --pwsh)
    tes switch feat/shell-pwsh | Out-Null
    if (\$env:TES_BRANCH -ne 'feat/shell-pwsh') {
      Write-Error \"Expected feat/shell-pwsh, got \$env:TES_BRANCH\"
      exit 1
    }
    tes switch main | Out-Null
    if (\$env:TES_BRANCH -ne 'main') {
      Write-Error \"Expected main, got \$env:TES_BRANCH\"
      exit 1
    }
  "
  echo "     ✓ pwsh passed"
fi

echo "Testing Concurrent Multi-Agent 3-Way AST Merge, Stash, Restore, and Cherry-Pick"

export TES_SESSION_ID="agent-alpha"
"$TES_BIN" claim --branch feat/alpha --intent "Update alpha() and add gamma()" src/lib.rs >/dev/null
cat << 'EOF' > "$REPO/src/lib.rs"
pub fn alpha() -> i32 {
    100
}

pub fn beta() -> i32 {
    20
}

pub fn gamma() -> i32 {
    300
}
EOF
"$TES_BIN" hook post-tool --path src/lib.rs
"$TES_BIN" commit -m "feat(alpha): update alpha and add gamma" >/dev/null

export TES_SESSION_ID="agent-beta"
"$TES_BIN" claim --branch feat/beta --intent "Update beta() and add delta()" src/lib.rs >/dev/null
cat << 'EOF' > "$REPO/src/lib.rs"
pub fn alpha() -> i32 {
    10
}

pub fn beta() -> i32 {
    200
}

pub fn delta() -> i32 {
    400
}
EOF
"$TES_BIN" hook post-tool --path src/lib.rs
"$TES_BIN" commit -m "feat(beta): update beta and add delta" >/dev/null

export TES_SESSION_ID="editor"
"$TES_BIN" merge feat/alpha feat/beta --into main -m "feat: merge alpha and beta into main" >/dev/null

grep -q "100" "$REPO/src/lib.rs"
grep -q "200" "$REPO/src/lib.rs"
grep -q "pub fn gamma()" "$REPO/src/lib.rs"
grep -q "pub fn delta()" "$REPO/src/lib.rs"

echo "// temporary wip comment" >> "$REPO/src/lib.rs"
"$TES_BIN" stash push -m "wip comment" >/dev/null

if grep -q "temporary wip comment" "$REPO/src/lib.rs"; then
  echo "FAIL: Stash push did not revert working tree"
  exit 1
fi

"$TES_BIN" stash pop >/dev/null
grep -q "temporary wip comment" "$REPO/src/lib.rs"
"$TES_BIN" restore src/lib.rs >/dev/null

export TES_SESSION_ID="agent-cherry"
"$TES_BIN" claim --branch feat/hotfix --intent "Add epsilon() helper" src/lib.rs >/dev/null
cat << 'EOF' >> "$REPO/src/lib.rs"

pub fn epsilon() -> i32 {
    500
}
EOF
"$TES_BIN" hook post-tool --path src/lib.rs
"$TES_BIN" commit -m "feat(hotfix): add epsilon helper" >/dev/null
HOTFIX_SHA="$(git rev-parse refs/heads/feat/hotfix)"

export TES_SESSION_ID="editor"
"$TES_BIN" switch main >/dev/null

if grep -q "pub fn epsilon()" "$REPO/src/lib.rs"; then
  echo "FAIL: main unexpectedly contained epsilon() before cherry-pick"
  exit 1
fi

"$TES_BIN" cherry-pick "$HOTFIX_SHA" >/dev/null
grep -q "pub fn epsilon()" "$REPO/src/lib.rs"
"$TES_BIN" branch -D feat/hotfix >/dev/null

git fsck --full

echo "ALL CROSS-PLATFORM MULTI-SHELL & LIFECYCLE E2E TESTS PASSED!"
