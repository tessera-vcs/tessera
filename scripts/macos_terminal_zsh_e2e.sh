#!/usr/bin/env bash
set -euo pipefail

# Automated macOS Terminal.app AppleScript E2E Test Suite (Zsh Multi-Agent Lifecycle).
# Launches 3 real Terminal.app windows via `osascript` in an isolated temporary repository
# and exercises initialization, shell hooks, concurrent same-branch 3-way AST merges,
# path-scoped stashes, Copy-on-Write branch sandboxes (`tes exec`), branch renames,
# cross-branch range diffs, AST cherry-picks, octopus merges with verification gates,
# conflict resolution, oplog undo, bare remote push/pull, dynamic completions, and MCP stdio.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TES_BIN="${TES_BIN:-$REPO_ROOT/src/target/release/tes}"

if [[ ! -x "$TES_BIN" ]]; then
  echo "==> Building release binary at $TES_BIN..."
  cargo build --release -p cli --manifest-path "$REPO_ROOT/src/Cargo.toml"
fi

TES_DIR="$(cd "$(dirname "$TES_BIN")" && pwd)"
TES_BIN="$TES_DIR/$(basename "$TES_BIN")"

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/tes-zsh-e2e.XXXXXX")"
REMOTE_DIR="$TEST_ROOT/remote.git"
APP_DIR="$TEST_ROOT/acme-platform"
LOG_DIR="$TEST_ROOT/logs"

mkdir -p "$REMOTE_DIR" "$APP_DIR" "$LOG_DIR"

echo "==> [Setup] Creating fresh bare Git remote at $REMOTE_DIR"
git init --bare -b main "$REMOTE_DIR" >/dev/null

echo "==> [Setup] Creating fresh real-world project at $APP_DIR"
cd "$APP_DIR"
git init -b main >/dev/null
git config user.name "Donald Pakkies"
git config user.email "donald@example.com"
git remote add origin "$REMOTE_DIR"

mkdir -p src test migrations

cat > .gitignore << 'EOF'
node_modules/
dist/
.DS_Store
EOF

cat > package.json << 'EOF'
{
  "name": "acme-platform",
  "version": "1.0.0",
  "private": true,
  "scripts": {
    "test": "node test/run_tests.js"
  },
  "dependencies": {
    "zod": "^3.22.0"
  }
}
EOF

cat > src/pricing.ts << 'EOF'
import { strict as assert } from "assert";

export function calculateSubtotal(unitPriceCents: number, quantity: number): number {
  assert(quantity >= 0, "quantity must be non-negative");
  return unitPriceCents * quantity;
}

export function formatCurrency(cents: number): string {
  return `$${(cents / 100).toFixed(2)}`;
}
EOF

cat > src/auth.ts << 'EOF'
export interface SessionToken {
  readonly userId: string;
  readonly role: string;
}

export function verifyToken(raw: string): SessionToken | null {
  if (!raw.startsWith("tok_")) {
    return null;
  }
  return { userId: raw.slice(4), role: "member" };
}
EOF

cat > test/run_tests.js << 'EOF'
const fs = require("fs");
const assert = require("assert");

const pricing = fs.readFileSync("src/pricing.ts", "utf8");
const auth = fs.readFileSync("src/auth.ts", "utf8");

assert(pricing.includes("calculateSubtotal"), "calculateSubtotal missing");
assert(auth.includes("verifyToken"), "verifyToken missing");
console.log("All project tests passed in", process.cwd(), "PORT=" + (process.env.PORT || "unset"));
EOF

cat > migrations/0001_init.sql << 'EOF'
CREATE TABLE users (id SERIAL PRIMARY KEY, email TEXT NOT NULL);
EOF

mkdir -p node_modules/fake-dep
echo "module.exports = () => 'warm-cache-ok';" > node_modules/fake-dep/index.js

git add .
git commit -m "chore: initial project bootstrap" >/dev/null
git push -u origin main >/dev/null

echo "==> [AppleScript] Launching 3 real Terminal.app windows via osascript in $APP_DIR..."

# Executes a Zsh script inside a target Terminal.app window/tab via `osascript`
# and blocks until its exit status is written to disk.
run_in_terminal_tab() {
  local window_id="$1"
  local tab_index="$2"
  local step_name="$3"
  local cmd_file="$4"
  local wrapper_file="$LOG_DIR/${step_name}.wrapper.zsh"
  local status_file="$LOG_DIR/${step_name}.status"
  local out_file="$LOG_DIR/${step_name}.out"

  rm -f "$status_file" "$out_file" "$wrapper_file"
  sed -i '' "s|__APP_DIR__|$APP_DIR|g" "$cmd_file"
  sed -i '' "s|__TES_DIR__|$TES_DIR|g" "$cmd_file"

  cat > "$wrapper_file" <<WRAP
_tes_step_fn() {
  setopt localoptions errexit
  source "$cmd_file"
}
_tes_step_fn > "$out_file" 2>&1
echo \$? > "$status_file"
WRAP

  osascript <<EOF >/dev/null
tell application "Terminal"
  do script "source '$wrapper_file'" in tab $tab_index of window id $window_id
end tell
EOF

  local waited=0
  while [ ! -f "$status_file" ]; do
    sleep 0.2
    waited=$((waited + 1))
    if [ "$waited" -gt 150 ]; then
      echo "❌ TIMEOUT waiting for step '$step_name' in window $window_id tab $tab_index"
      cat "$out_file" 2>/dev/null || true
      exit 1
    fi
  done

  local exit_code
  exit_code="$(cat "$status_file" | tr -d '[:space:]')"
  echo "--- [$step_name] (Window $window_id, Tab $tab_index, Exit=$exit_code) ---"
  cat "$out_file"
  if [ "$exit_code" != "0" ]; then
    echo "❌ Step '$step_name' failed with exit code $exit_code"
    exit "$exit_code"
  fi
}

WINDOWS_RAW=$(osascript <<EOF
tell application "Terminal"
  activate
  do script "unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID; cd '$APP_DIR'"
  set win1 to id of front window
  do script "unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID; cd '$APP_DIR'"
  set win2 to id of front window
  do script "unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID; cd '$APP_DIR'"
  set win3 to id of front window
  return (win1 as string) & " " & (win2 as string) & " " & (win3 as string)
end tell
EOF
)

read -r WIN1 WIN2 WIN3 <<< "$WINDOWS_RAW"
echo "==> Opened 3 dedicated Terminal.app windows via AppleScript: WIN1=$WIN1, WIN2=$WIN2, WIN3=$WIN3"

# Closes all spawned Terminal.app windows and removes temporary test directories on exit.
cleanup_window() {
  osascript <<EOF >/dev/null 2>&1 || true
tell application "Terminal"
  repeat with wid in {$WIN1, $WIN2, $WIN3}
    if exists (window id wid) then
      close window id wid saving no
    end if
  end repeat
end tell
EOF
  rm -rf "$TEST_ROOT"
}
trap cleanup_window EXIT

sleep 1

cat > "$LOG_DIR/init_env_config.zsh" << 'EOF'
export PATH="__TES_DIR__:$HOME/.cargo/bin:$PATH"
unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID
cd __APP_DIR__
tes --version
tes init
eval "$(tes env --zsh)"
_tes_precmd
echo "TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT"
tes config --verify-cmd "node test/run_tests.js"
tes config
tes status
tes status --json | head -n 20
EOF
run_in_terminal_tab "$WIN1" 1 "init_env_config" "$LOG_DIR/init_env_config.zsh"

cat > "$LOG_DIR/tab2_setup.zsh" << 'EOF'
export PATH="__TES_DIR__:$HOME/.cargo/bin:$PATH"
unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID
cd __APP_DIR__
eval "$(tes env --zsh)"
_tes_precmd
echo "Terminal 2 TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT"
EOF
run_in_terminal_tab "$WIN2" 1 "tab2_env" "$LOG_DIR/tab2_setup.zsh"

cat > "$LOG_DIR/tab3_setup.zsh" << 'EOF'
export PATH="__TES_DIR__:$HOME/.cargo/bin:$PATH"
unset ANTIGRAVITY_AGENT ANTIGRAVITY_CONVERSATION_ID
cd __APP_DIR__
eval "$(tes env --zsh)"
_tes_precmd
echo "Terminal 3 TES_SESSION_ID=$TES_SESSION_ID TES_BRANCH=$TES_BRANCH TES_PORT=$TES_PORT"
EOF
run_in_terminal_tab "$WIN3" 1 "tab3_env" "$LOG_DIR/tab3_setup.zsh"

cat > "$LOG_DIR/tab1_claim_edit.zsh" << 'EOF'
set -e
cd __APP_DIR__
tes claim --branch main --intent "Add applyDiscount to pricing.ts" src/pricing.ts
tes hook pre-tool --path src/pricing.ts
cat >> src/pricing.ts << 'INNER'

export function applyDiscount(cents: number, discountPercent: number): number {
  return Math.round(cents * (100 - discountPercent) / 100);
}
INNER
tes hook post-tool --path src/pricing.ts
tes status
tes diff
EOF
run_in_terminal_tab "$WIN1" 1 "tab1_same_branch_edit" "$LOG_DIR/tab1_claim_edit.zsh"

cat > "$LOG_DIR/tab2_claim_edit.zsh" << 'EOF'
set -e
cd __APP_DIR__
tes claim --branch main --intent "Add calculateTax to pricing.ts" src/pricing.ts
tes hook pre-tool --path src/pricing.ts
cat >> src/pricing.ts << 'INNER'

export function calculateTax(cents: number, taxRatePercent: number): number {
  return Math.round(cents * taxRatePercent / 100);
}
INNER
tes hook post-tool --path src/pricing.ts
tes status
echo "--- Tab 2 isolated diff (should only show calculateTax): ---"
tes diff
echo "--- Tab 2 --all diff (should show both applyDiscount and calculateTax): ---"
tes diff --all
tes radar
EOF
run_in_terminal_tab "$WIN2" 1 "tab2_same_branch_concurrent_merge" "$LOG_DIR/tab2_claim_edit.zsh"

cat > "$LOG_DIR/tab2_stash.zsh" << 'EOF'
set -e
cd __APP_DIR__
tes claim --branch main --intent "Add calculateTax and update auth" src/pricing.ts src/auth.ts
cat >> src/auth.ts << 'INNER'

export function isAdmin(token: SessionToken): boolean {
  return token.role === "admin";
}
INNER
tes hook post-tool --path src/auth.ts

echo "--- Path-scoped stash of only src/auth.ts ---"
tes stash push -m "WIP: admin helper in auth.ts" src/auth.ts
tes stash list
grep -q "calculateTax" src/pricing.ts && echo "✓ calculateTax still on disk"
grep -q "applyDiscount" src/pricing.ts && echo "✓ peer applyDiscount still on disk"
! grep -q "isAdmin" src/auth.ts && echo "✓ stashed isAdmin removed from working tree"

echo "--- Stash apply (keeps stash entry) & Stash drop ---"
tes stash apply 0
grep -q "isAdmin" src/auth.ts && echo "✓ isAdmin restored via stash apply"
tes stash list
tes stash drop 0
tes stash list

echo "--- Stash push & pop ---"
tes stash push -m "WIP: auth admin" src/auth.ts
tes stash pop 0
grep -q "isAdmin" src/auth.ts && echo "✓ isAdmin restored via stash pop"

echo "--- Commit Tab 2 changes on main ---"
tes commit -m "feat(pricing,auth): add calculateTax and isAdmin"
EOF
run_in_terminal_tab "$WIN2" 1 "tab2_stash_and_commit" "$LOG_DIR/tab2_stash.zsh"

cat > "$LOG_DIR/tab1_commit.zsh" << 'EOF'
set -e
cd __APP_DIR__
tes status
grep -q "applyDiscount" src/pricing.ts && echo "✓ Tab 1 applyDiscount still present"
grep -q "calculateTax" src/pricing.ts && echo "✓ Tab 2 committed calculateTax also present"
tes commit -m "feat(pricing): add applyDiscount"
tes log -n 4 --oneline
EOF
run_in_terminal_tab "$WIN1" 1 "tab1_commit_after_peer" "$LOG_DIR/tab1_commit.zsh"

cat > "$LOG_DIR/tab2_branch_sandbox.zsh" << 'EOF'
set -e
cd __APP_DIR__
tes switch feat/tier-pricing
_tes_precmd
echo "Tab 2 switched to: $TES_BRANCH (Workspace: $TES_WORKSPACE)"

tes exec -- node -e '
  const dep = require("./node_modules/fake-dep");
  const { execSync } = require("child_process");
  const rev = execSync("git rev-parse --short HEAD").toString().trim();
  console.log("Sandbox check OK:", dep(), "Git HEAD:", rev, "PORT:", process.env.PORT);
'

tes claim --branch feat/tier-pricing --intent "Add enterprise tier pricing and migration" src/pricing.ts migrations/0002_tiers.sql
cat >> src/pricing.ts << 'INNER'

export function enterpriseTierPrice(seats: number): number {
  return seats * 4900;
}
INNER
cat > migrations/0002_tiers.sql << 'INNER'
CREATE TABLE pricing_tiers (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
INNER
tes hook post-tool --path src/pricing.ts
tes hook post-tool --path migrations/0002_tiers.sql
tes commit -m "feat(tiers): add enterpriseTierPrice and 0002 migration"

tes branch -m feat/tier-pricing feat/enterprise-pricing
_tes_precmd
echo "Tab 2 branch after rename: $TES_BRANCH"
tes branch
EOF
run_in_terminal_tab "$WIN2" 1 "tab2_branch_sandbox_exec_rename" "$LOG_DIR/tab2_branch_sandbox.zsh"

cat > "$LOG_DIR/tab3_parallel_branch.zsh" << 'EOF'
set -e
cd __APP_DIR__
tes switch feat/audit-log
_tes_precmd
echo "Tab 3 switched to: $TES_BRANCH"
tes claim --branch feat/audit-log --intent "Add audit log migration and helper" src/auth.ts migrations/0002_audit.sql
cat >> src/auth.ts << 'INNER'

export function formatAuditActor(token: SessionToken): string {
  return `${token.role}:${token.userId}`;
}
INNER
cat > migrations/0002_audit.sql << 'INNER'
CREATE TABLE audit_logs (id SERIAL PRIMARY KEY, actor TEXT NOT NULL);
INNER
tes hook post-tool --path src/auth.ts
tes hook post-tool --path migrations/0002_audit.sql
tes commit -m "feat(audit): add formatAuditActor and 0002_audit migration"

echo "--- Cross-branch range diff main..feat/audit-log: ---"
tes diff main..feat/audit-log src/auth.ts
EOF
run_in_terminal_tab "$WIN3" 1 "tab3_parallel_branch_and_range_diff" "$LOG_DIR/tab3_parallel_branch.zsh"

cat > "$LOG_DIR/tab1_merge_and_cherry_pick.zsh" << 'EOF'
set -e
cd __APP_DIR__
! grep -q "enterpriseTierPrice" src/pricing.ts && echo "✓ main working tree stayed clean while Tab 2 & Tab 3 committed to feature branches"

tes switch -b feat/hotfix-currency
tes claim --branch feat/hotfix-currency --intent "Add parseCents helper" src/pricing.ts
cat >> src/pricing.ts << 'INNER'

export function parseCents(dollars: number): number {
  return Math.round(dollars * 100);
}
INNER
tes hook post-tool --path src/pricing.ts
tes commit -m "feat(pricing): add parseCents hotfix"
HOTFIX_SHA=$(tes log -b feat/hotfix-currency -n 1 --json | node -e 'const d=JSON.parse(require("fs").readFileSync(0,"utf8")); console.log(d[0].short_sha);')
echo "Hotfix SHA: $HOTFIX_SHA"

tes switch -e main
_tes_precmd
echo "Tab 1 back on: $TES_BRANCH"

tes cherry-pick "$HOTFIX_SHA"
grep -q "parseCents" src/pricing.ts && echo "✓ AST cherry-pick applied parseCents onto main"

tes branch -D feat/hotfix-currency

tes merge feat/enterprise-pricing feat/audit-log --into main -m "feat: merge enterprise pricing and audit log branches" --verify
ls -1 migrations/
grep -q "enterpriseTierPrice" src/pricing.ts && echo "✓ enterpriseTierPrice merged into main"
grep -q "formatAuditActor" src/auth.ts && echo "✓ formatAuditActor merged into main"
EOF
run_in_terminal_tab "$WIN1" 1 "tab1_cherry_pick_and_octopus_merge" "$LOG_DIR/tab1_merge_and_cherry_pick.zsh"

cat > "$LOG_DIR/checkpoints_conflicts_restore_remote_mcp.zsh" << 'EOF'
set -e
cd __APP_DIR__

echo "--- Checkpoint & Restore ---"
tes checkpoint before-experiment
echo "// experimental change" >> src/pricing.ts
tes hook post-tool --path src/pricing.ts
tes restore src/pricing.ts
if grep -q "experimental change" src/pricing.ts; then
  echo "FAIL: experimental change still present after tes restore" >&2
  exit 1
fi
echo "✓ tes restore <path> discarded file change"

echo "--- Overlapping Conflict Detection & Resolution ---"
tes claim --branch main --intent "Agent A changes formatCurrency prefix" --agent agent-conflict-a src/pricing.ts
tes hook pre-tool --path src/pricing.ts --agent agent-conflict-a
tes claim --branch main --intent "Agent B changes formatCurrency to EUR" --agent agent-conflict-b src/pricing.ts
tes hook pre-tool --path src/pricing.ts --agent agent-conflict-b

node -e '
  const fs = require("fs");
  const s = fs.readFileSync("src/pricing.ts", "utf8").replace("return `$${(cents / 100).toFixed(2)}`;", "return `USD ${(cents / 100).toFixed(2)}`;");
  fs.writeFileSync("src/pricing.ts", s);
'
tes hook post-tool --path src/pricing.ts --agent agent-conflict-a

node -e '
  const fs = require("fs");
  const s = fs.readFileSync("src/pricing.ts", "utf8").replace("return `USD ${(cents / 100).toFixed(2)}`;", "return `EUR ${(cents / 100).toFixed(2)}`;");
  fs.writeFileSync("src/pricing.ts", s);
'
tes hook post-tool --path src/pricing.ts --agent agent-conflict-b || true

tes conflicts
tes resolve src/pricing.ts --strategy ours
tes conflicts
tes commit --agent agent-conflict-a -m "feat(pricing): use USD prefix in formatCurrency"
if ! grep -q "USD " src/pricing.ts; then
  echo "FAIL: USD prefix missing after commit" >&2
  exit 1
fi
echo "✓ committed USD resolution onto main"

echo "--- Operation Log & Undo ---"
tes oplog -n 5
tes undo
if grep -q "USD " src/pricing.ts; then
  echo "FAIL: USD prefix still present after tes undo" >&2
  exit 1
fi
echo "✓ tes undo rolled back commit cleanly"
tes agent close agent-conflict-b || true

echo "--- Push & Pull with Bare Remote ---"
tes push origin main
tes pull origin main

echo "--- Dynamic Tab Completion (tes complete) ---"
tes complete switch ""
tes complete restore ""
tes complete cherry-pick ""

echo "--- MCP Server JSON-RPC 2.0 over stdio ---"
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}\n{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status","arguments":{}}}\n{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"radar","arguments":{}}}\n' | tes mcp | head -n 10

echo "--- Session Clear, Watch, GC, & Git Passthrough ---"
tes session clear
tes watch --once
tes gc
tes tag v1.0.0
git fsck --full --no-dangling
EOF
run_in_terminal_tab "$WIN1" 1 "full_lifecycle_conflicts_undo_remote_mcp_gc" "$LOG_DIR/checkpoints_conflicts_restore_remote_mcp.zsh"

osascript <<EOF
tell application "Terminal"
  set t1 to contents of tab 1 of window id $WIN1
  return "=== Terminal 1 Tail ===\n" & (text -400 thru -1 of t1)
end tell
EOF

echo "✅ ALL REAL-WORLD APPLESCRIPT TERMINAL (ZSH) TESTS PASSED!"
