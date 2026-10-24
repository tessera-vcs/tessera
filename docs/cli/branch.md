# tes branch

List, create, rename, or delete branches and their Copy-on-Write sandboxes.

## Usage

```bash:no-line-numbers
tes branch [branch] [options]
```

Alias: `tes branches`

## Options

- `-l, --list` - List branches.
- `-v, --verbose` - Show the tip commit SHA and subject line for each branch in list mode.
- `-d, --delete` - Safely delete `<branch>` and its Copy-on-Write sandbox (refuses if unmerged or dirty).
- `-D, --force-delete` - Force-delete `<branch>` and its Copy-on-Write sandbox even if unmerged or dirty.
- `-m, --move <NEW_BRANCH>` - Rename `<branch>` (or the current session's branch if omitted) to `<NEW_BRANCH>`, migrating active sessions and its Copy-on-Write sandbox.
- `-e, --editor` - When switching to `<branch>`, also switch the primary repository working tree (`repo_root` and `.git/HEAD`) and bind the calling terminal session.
- `--json` - Output detailed branch metadata including commit SHAs, ports, active sessions, modified files, and the commit graph.

## Description

When run with no arguments (or with `-l` / `-v`), `tes branch` lists all local Git and Tessera branches and highlights the current session's active branch with `*`.

## Examples

```bash:no-line-numbers
# list all branches
tes branch

# list branches with short commit SHAs and commit subjects
tes branch -v

# safely delete a merged branch and its sandbox
tes branch -d feat/old-experiment

# force-delete an unmerged branch
tes branch -D feat/abandoned

# rename a branch and migrate active sessions
tes branch -m feat/old-name feat/new-name

# inspect detailed branch state as JSON
tes branch --json
```
