# tes conflicts

List unresolved merge conflicts in the repository.

## Usage

```bash:no-line-numbers
tes conflicts [options]
```

## Options

- `--json` - Emit structured JSON output with `base`, `ours`, and `theirs` code snippets and participating session IDs.

## Description

`tes conflicts` lists all files that currently have an unresolved structural merge conflict.

## Examples

```bash:no-line-numbers
# list unresolved conflicts
tes conflicts

# inspect full conflict snippets as JSON
tes conflicts --json
```
