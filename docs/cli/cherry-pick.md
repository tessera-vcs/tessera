# tes cherry-pick

Apply the changes introduced by an existing commit onto the active (or specified) branch via 3-way AST merge.

## Usage

```bash:no-line-numbers
tes cherry-pick <commit> [options]
```

## Options

- `-b, --branch <BRANCH>` - Target branch to apply the commit onto (defaults to the current session's active branch).
- `-a, --agent <ID>` - Explicit session ID.

## Description

`tes cherry-pick` takes the diff introduced by `<commit>` (a commit SHA, branch name, `HEAD~N`, or checkpoint) against its parent and applies it onto the target branch using Tessera's 3-way Tree-sitter AST merge engine, rebasing active sessions on that branch automatically.

## Examples

```bash:no-line-numbers
# cherry-pick a commit onto the active branch
tes cherry-pick 4beccce

# cherry-pick the tip commit of another branch onto main
tes cherry-pick feat/pricing --branch main
```
