# tes radar

Display all active AI agent and terminal sessions in real time.

## Usage

```bash:no-line-numbers
tes radar [options]
```

## Options

- `--json` - Emit structured JSON output.

## Description

`tes radar` shows every active session in the repository along with its assigned slot number, target branch, task intent, claimed AST symbols, and modified files. Any structural overlaps between sessions on the same branch are highlighted.

## Examples

```bash:no-line-numbers
# inspect live sessions
tes radar

# output live sessions as JSON
tes radar --json
```
