# tes restore

Restore a branch or specific files to a checkpoint, commit, or branch.

## Usage

```bash:no-line-numbers
tes restore [options] [target_or_paths...]
```

Alias: `tes reset`

## Options

- `-b, --branch <BRANCH>` - Restore a specific branch without switching your working tree to it first.
- `-s, --source <REF>` - Restore specific files from `<REF>` (checkpoint name, commit SHA, `HEAD~N`, or branch name).

## Description

When given a checkpoint name, commit SHA, or `HEAD~N` (with no file paths), `tes restore` resets the target branch to that point in time and synchronizes `.git/HEAD`, `.git/index`, active agent baselines, and the branch's Copy-on-Write sandbox.

When given file paths, `tes restore` restores only those files from `--source` (or from `HEAD` if `--source` is omitted).

## Examples

```bash:no-line-numbers
# restore the current branch to a named checkpoint or previous commit
tes restore before-refactor
tes restore HEAD~1

# restore a specific branch to its previous commit
tes restore -b feat/pricing HEAD~1

# discard uncommitted changes in a file
tes restore src/App.vue

# restore a file from main
tes restore --source main src/App.vue
```
