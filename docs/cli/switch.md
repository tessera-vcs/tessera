# tes switch

Switch the current terminal session or the editor working tree to a branch.

## Usage

```bash:no-line-numbers
tes switch <branch> [start-point] [options]
tes switch -b <new-branch> [start-point] [options]
```

Alias: `tes checkout <branch>`

## Options

- `-b, -c, --create <BRANCH>` - Create `<BRANCH>` (compatibility with `git checkout -b` and `git switch -c`).
- `--from, --base <BRANCH>` - Base branch or commit to fork `<branch>` from when creating a new branch. Fails if the specified base revision does not exist.
- `-e, --editor` - Switch the primary repository working tree (`repo_root` and `.git/HEAD`) so your IDE displays `<branch>`, and also bind the calling terminal session to `<branch>`.
- `-a, --agent <ID>` - Bind a specific agent session to `<branch>`.

## Description

By default, `tes switch <branch>` binds your current terminal session to `<branch>` and materializes its Copy-on-Write sandbox without changing `.git/HEAD` for other open terminals or AI agents.

When `<branch>` does not exist yet:
- If `--from <base-branch>` or a positional `[start-point]` is provided (`tes switch -b <new-branch> <start-point>`), Tessera resolves that revision (or fails fast if it does not exist) and creates `<branch>` from it.
- Otherwise, if the calling session is already bound to a feature branch (`tes switch feat/parent` followed by `tes switch feat/child`), `<branch>` forks from the session's current branch commit; if not, it forks from `HEAD`.

When `--editor` is passed, `tes switch` updates the primary repository working tree (`repo_root`), `.git/HEAD`, and `.git/index` to `<branch>` and binds the calling terminal session to `<branch>` while preserving isolated sandboxes for all other background terminal sessions.

## Examples

```bash:no-line-numbers
# switch the current terminal tab to feat/pricing
tes switch feat/pricing

# create feat/pricing-tiers branched off feat/pricing
tes switch -b feat/pricing-tiers feat/pricing
tes switch feat/pricing-tiers --from feat/pricing

# switch both the IDE working tree (repo_root) and current terminal to feat/pricing
tes switch feat/pricing --editor

# bind a specific agent session to feat/pricing
tes switch feat/pricing --agent antigravity-1
```
