# tes pull

Fetch from a Git remote, fast-forward or 3-way AST-merge the remote branch into your local branch, and rebase active sessions on that branch.

## Usage

```bash:no-line-numbers
tes pull [remote] [branch]
```

## Arguments

- `[remote]` - Git remote name (defaults to `origin`).
- `[branch]` - Branch name to pull (defaults to the current session's active branch).

## Description

`tes pull` fetches the latest commits from `<remote>/<branch>`, fast-forwards your local branch ref when possible (or performs a 3-way AST merge if local commits have diverged), and rebases any active sessions and Copy-on-Write branch sandboxes onto the updated branch `HEAD`.

## Examples

```bash:no-line-numbers
# pull the current active branch from origin
tes pull

# pull a specific branch from origin
tes pull origin feat/pricing
```
