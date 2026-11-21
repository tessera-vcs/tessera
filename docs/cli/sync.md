# tes sync

Auto-rebase all active agent sessions onto the latest trunk commit using 3-way Tree-sitter AST merge.

## Usage

```bash:no-line-numbers
tes sync [options]
```

## Options

- `--json` - Emit structured JSON output listing the rebased sessions.

## Description

`tes sync` performs a non-blocking auto-rebase of active sessions onto the latest branch/trunk commit using Tessera's 3-way Tree-sitter AST merge engine, preserving each session's stable reverse-hex `ChangeId` and recording any overlapping edits as first-class conflicts rather than aborting other sessions.

## Examples

```bash:no-line-numbers
# rebase all active sessions onto the latest commits
tes sync

# emit sync results as JSON
tes sync --json
```
