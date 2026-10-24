# tes push

Synchronize pending session changes and push a branch to a Git remote.

## Usage

```bash:no-line-numbers
tes push [remote] [branch] [options]
```

## Options

- `-m, --message <MSG>` - Commit message if uncommitted session changes need to be committed before pushing.
- `-f, --force` - Push with `--force-with-lease`.
- `-a, --agent <ID>` - Explicit session ID.

## Description

`tes push` commits any uncommitted changes in the current session onto the active branch and runs `git push -u <remote> refs/heads/<branch>:refs/heads/<branch>`.

## Examples

```bash:no-line-numbers
# push the active branch to origin
tes push

# push a specific branch to origin
tes push origin feat/pricing
```
