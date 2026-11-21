# tes push

Synchronize pending session changes and push a branch to a Git remote.

## Usage

```bash:no-line-numbers
tes push [remote] [branch] [options]
```

## Options

- `-u, --set-upstream` - Set the upstream tracking reference (`git push -u`).
- `-m, --message <MSG>` - Commit message if uncommitted session changes need to be committed before pushing.
- `-f, --force` - Force push using raw `git push --force` (unconditionally overwrites the remote branch; conflicts with `--force-with-lease`).
- `--force-with-lease` - Safely force push with `git push --force-with-lease`, refusing to overwrite remote commits you have not fetched (conflicts with `--force`).
- `--tags` - Push all tags (`git push --tags`).
- `-d, --delete` - Delete the specified branch or ref on the remote (`git push --delete`).
- `-n, --dry-run` - Simulate the push without updating remote refs (`git push --dry-run`).
- `--no-auto-commit` - Skip auto-committing uncommitted session changes before pushing.
- `-a, --agent <ID>` - Explicit session ID.

## Description

`tes push` commits any uncommitted changes in the current session onto the active branch (unless `--no-auto-commit` is passed) and runs `git push` for `<remote>` and `refs/heads/<branch>:refs/heads/<branch>`.

## Examples

```bash:no-line-numbers
# push the active branch to origin and set upstream tracking
tes push -u

# push a specific branch to origin
tes push origin feat/pricing

# safely force-push a rewritten branch
tes push --force-with-lease origin feat/pricing

# preview a push without mutating remote refs or auto-committing local edits
tes push --dry-run --no-auto-commit

# push tags or delete a remote branch
tes push --tags
tes push -d origin feat/old-experiment
```
