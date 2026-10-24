# tes diff

Display line-by-line unified diffs, diffstats, or changed file lists for the session, working tree, branch, or any commit range.

## Usage

```bash:no-line-numbers
tes diff [options] [<revA>..<revB> | <revA> <revB>] [--] [<paths...>]
```

## Options

- `--staged, --cached` - Show only staged session changes.
- `--all` - Show all combined working tree edits on disk across all concurrent sessions on the branch.
- `--stat` - Show a diffstat summary of changed files and line counts.
- `--shortstat` - Show only the aggregate line count summary (files changed, insertions, deletions).
- `--numstat` - Show machine-friendly per-file added and deleted line counts.
- `--name-only` - Show only the names of changed files.
- `--name-status` - Show names and status letters (`A`, `M`, `D`) of changed files.
- `-U, --unified <LINES>` - Generate diffs with `<LINES>` lines of context instead of the default `3`.
- `-w, --ignore-all-space` - Ignore all whitespace when comparing lines.
- `--ignore-space-change` - Ignore changes in the amount of whitespace.
- `-b, --branch <BRANCH>` - Compare `<BRANCH>` against `main` without switching branches.
- `-a, --agent <ID>` - Show changes for a specific session ID.
- `-- <paths...>` - Restrict the diff to specific files or directories.

## Description

By default, when your session has claimed files, `tes diff` shows only your session's isolated diff (`base_commit -> own_blobs`) so concurrent peer edits do not pollute your diff output. You can pass `--all` to inspect the combined working tree diff, `-b <branch>` to diff a branch against `main`, or `<revA>..<revB>` (or `<revA> <revB>`) to diff any two revisions, branches, or commits.

## Examples

```bash:no-line-numbers
# diff changes in the current session (or working tree if unclaimed)
tes diff

# show diffstat or changed file names for the current session
tes diff --stat
tes diff --name-status

# diff all combined working tree changes across concurrent sessions
tes diff --all

# diff specific paths or directories
tes diff -- crates/cli/src/main.rs docs/

# compare a feature branch against main
tes diff -b feat/pricing

# diff two revisions or branches (optionally scoped to paths)
tes diff HEAD~1..HEAD --stat
tes diff main..feat/pricing -- src/App.vue
```
