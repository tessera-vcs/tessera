# tes status

Reconcile working tree edits and display current session and branch status.

## Usage

```bash:no-line-numbers
tes status [options]
```

## Options

- `-s, --short, --porcelain` - Give the output in compact `XY <path>` short-format.
- `-b, --branch` - Show the branch and session header (`## <branch> [session: <id>]`) in short-format.
- `-a, --agent <ID>` - Inspect status for a specific session ID.
- `--json` - Emit structured JSON output.

## Description

The `status` command snapshots any modified files in the current workspace, reconciles concurrent edits, and reports:
1. **Staged / Session Changes**: Files recorded in the current session (`M`, `A`, `D`).
2. **Peer Session Changes (concurrent on `<branch>`)**: Files modified by other concurrent AI agent or terminal sessions on the same branch, kept strictly isolated from your session's staged list.
3. **Unstaged Changes**: Working tree edits on disk not yet claimed by any session.
4. **Committed on Branch**: Files committed on the active feature branch ahead of `main`.

## Examples

```bash:no-line-numbers
# show current session and branch status
tes status

# show compact short-format status with branch header
tes status -sb

# output status as JSON
tes status --json
```
