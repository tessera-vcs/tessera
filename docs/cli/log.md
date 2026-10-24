# tes log

Show the commit history for the active session's branch, any specified branch or revision range, or specific file paths.

## Usage

```bash:no-line-numbers
tes log [options] [<revision-or-range>] [-- <paths...>]
```

## Options

- `-n, --limit, --max-count <N>` - Maximum number of commits to display (default: `20`).
- `-b, --branch <BRANCH>` - Branch or revision to inspect (defaults to the active session's branch).
- `--oneline` - Display each commit on a single compact line (`<short_sha> (<refs>) <subject>`).
- `-p, -u, --patch` - Generate a unified diff patch for each commit.
- `--stat` - Show a diffstat summary of modified files and line counts for each commit.
- `--name-only` - Show only the names of changed files for each commit.
- `--name-status` - Show the names and status letters (`A`, `M`, `D`) of changed files for each commit.
- `--graph` - Draw a text-based graphical representation of the commit history.
- `--all` - Show commits across all branches and refs.
- `--author <PATTERN>` - Filter commits by author name or email pattern.
- `--grep <PATTERN>` - Filter commits whose commit message matches `<PATTERN>`.
- `-S, --pickaxe <STRING>` - Filter commits that introduce or remove `<STRING>` in the patch.
- `--since, --after <DATE>` - Show commits more recent than a specific date.
- `--until, --before <DATE>` - Show commits older than a specific date.
- `--reverse` - Output the chosen commits in reverse chronological order.
- `-a, --agent <ID>` - Inspect commit history for a specific session's branch.
- `-- <paths...>` - Restrict commit history (and patches) to commits touching the given files or directories.
- `--json` - Emit structured JSON commit entries.

## Examples

```bash:no-line-numbers
# show recent commits on the current branch
tes log

# show compact one-line commit log
tes log --oneline -n 10

# show the last 4 commits with patches scoped to docs/
tes log -p -n 4 -- docs/

# show diffstat and commit graph across all branches
tes log --graph --stat --all -n 15

# filter commits by message or pickaxe string
tes log --grep "AST" -n 5
tes log -S "merge_trees_3way" --oneline

# inspect commit history of another branch without switching
tes log -b feat/pricing --oneline
```
