# tes switch

Switch the current terminal session or the editor working tree to a branch.

## Usage

```bash:no-line-numbers
tes switch <branch> [options]
```

Alias: `tes checkout <branch>`

## Options

- `-e, --editor` - Switch the primary repository working tree (`repo_root` and `.git/HEAD`) so your IDE displays `<branch>`, and also bind the calling terminal session to `<branch>`.
- `-a, --agent <ID>` - Bind a specific agent session ID to `<branch>`.

## Description

By default, `tes switch <branch>` binds your current terminal session to `<branch>` (creating the branch at `HEAD` if it does not exist yet) and materializes its Copy-on-Write sandbox without changing `.git/HEAD` for other open terminals or AI agents.

When `--editor` is passed, `tes switch` updates the primary repository working tree (`repo_root`), `.git/HEAD`, and `.git/index` to `<branch>` and binds the calling terminal session to `<branch>` while preserving isolated sandboxes for all other background terminal sessions.

## Examples

```bash:no-line-numbers
# switch the current terminal tab to feat/pricing
tes switch feat/pricing

# switch both the IDE working tree (repo_root) and current terminal to feat/pricing
tes switch feat/pricing --editor

# bind a specific agent session to feat/pricing
tes switch feat/pricing --agent antigravity-1
```
