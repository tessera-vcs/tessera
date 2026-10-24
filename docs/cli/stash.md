# tes stash

Session-isolated AST stash for saving and reapplying uncommitted changes without interfering with concurrent agents or other branches.

## Usage

```bash:no-line-numbers
tes stash [subcommand] [options]
```

## Subcommands

- `tes stash [push] [-m <msg>] [-a <ID>] [paths...]` - Save the current session's (or working tree's) uncommitted changes onto the stash stack and restore the stashed files to the branch base. Optionally pass `<paths...>` to stash only specific files.
- `tes stash pop [index] [-a <ID>]` - 3-way AST-merge `stash@{index}` (default `0`) onto the current branch/session and remove it from the stash stack.
- `tes stash apply [index] [-a <ID>]` - 3-way AST-merge `stash@{index}` (default `0`) onto the current branch/session while keeping it in the stash stack.
- `tes stash list` - List all stash entries (`stash@{0}`, `stash@{1}`, ...).
- `tes stash drop [index]` - Delete a single stash entry (default `0`).
- `tes stash clear` - Remove all entries from the stash stack.

## Examples

```bash:no-line-numbers
# stash all changes in the current session or working tree
tes stash -m "WIP: refactor billing"

# stash only specific files while keeping other edits in place
tes stash push -m "partial stash" src/billing.ts

# list stashes
tes stash list

# pop the most recent stash via 3-way AST merge
tes stash pop

# apply stash@{1} without dropping it
tes stash apply 1
```
