---
trigger: always_on
---

# Tessera Multi-Agent Source Control (Autonomous Mode)

This repository is version-controlled by **Tessera** (`tes`).
Multiple AI agents may be editing this codebase concurrently in separate chat tabs or terminals, either on the same branch or on separate per-session branches.
**The user will give you normal coding prompts without mentioning `tes`. You MUST automatically run Tessera CLI commands on every coding task without waiting to be asked:**

## Critical Rules
- **NEVER run raw `git` commands** (`git status`, `git diff`, `git log`, `git branch`, `git checkout`, `git switch`, `git stash`, `git reset`, `git add`, `git commit`, `git cherry-pick`). Tessera manages branches and virtual working trees in `.git/tessera` without altering `.git/HEAD` for other concurrent agents. Always use `tes status`, `tes diff`, `tes log`, `tes branch`, `tes switch`, `tes stash`, `tes restore`, `tes cherry-pick`, `tes pull`, `tes push`, `tes commit`, and `tes merge`.
- **Never Revert Concurrent Peer Edits**: When multiple agents edit the same file on the same branch, `tes hook post-tool` automatically 3-way AST-merges their edits on disk. `tes status` and `tes diff` show only your session's own changes (`Staged / Session Changes`) and list concurrent `Peer Session Changes` separately. Never delete or revert a peer agent's code simply because you did not author it.
- **Use `tes` CLI commands in terminal tool calls** rather than duplicating with MCP calls so your per-tab session ID (`ANTIGRAVITY_CONVERSATION_ID`) is automatically isolated.

## Mandatory Autonomous Workflow for Every Coding Task
1. **Before Editing Files**:
   - Claim your task intent, target branch, and target files via CLI:
     `tes claim [--branch <branch>] [--from <base-branch>] --intent "<concise summary of your task>" <target-file-paths...>`
   - (`tes claim --branch <branch>` automatically creates and binds your session to `<branch>`; pass `--from <base-branch>` or run `tes switch -b <branch> <base-branch>` when branching off a specific parent branch. If a claimed file already has uncommitted changes from the user or a peer and you omit `--branch`, Tessera auto-isolates your session onto `agent/<session_id>`; pass `--branch <branch>` explicitly when same-branch collaboration is intended.)
2. **Immediately After Editing Each File**:
   - Reconcile your edits with any concurrent agent changes via CLI:
     `tes hook post-tool --path <edited-file>`
   - Tessera automatically 3-way AST-merges non-overlapping changes on disk while preserving per-branch session isolation.
3. **Running Builds, Tests, or Servers on a Branch**:
   - Use `tes exec [-b <branch>] -- <command>` to run builds, tests, or dev servers in an isolated Copy-on-Write branch sandbox with cloned dependencies (`node_modules`, `target`, `.venv`, `vendor`, etc.) and isolated `TES_PORT` / `DATABASE_URL` (plus full `PORT` and dev-server port shims when `tes config set port_isolation true` is enabled).
4. **If a Conflict Is Reported**:
   - Standard Git conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) are written directly into the affected file on disk.
   - Edit the file to resolve the conflict markers and run `tes hook post-tool --path <path>` (or run `tes resolve <path> --strategy <working|ast-union|ours|theirs>`) to clear the conflict automatically. Never manually delete files inside `.git/tessera/`.
5. **Branch Lifecycle, Stash, Cherry-Pick & Restore**:
   - Delete or rename branches via `tes branch -d <branch>`, `tes branch -D <branch>`, or `tes branch -m <old> <new>`.
   - Stash, cherry-pick, or restore changes via `tes stash [push|pop|apply|list|drop]`, `tes cherry-pick <commit>`, or `tes restore [<rev>] [<paths...>]`.
6. **When Your Coding Task Is Complete**:
   - Commit your session's isolated changes cleanly onto its branch via CLI:
     `tes commit -m "<clean conventional commit message>"` (or `tes commit --amend -m "..."` / `tes merge -m "<clean conventional commit message>"`)
   - To AST-merge multiple branches into a target branch, run:
     `tes merge <branch1> <branch2> --into <target-branch> -m "<commit message>"`
