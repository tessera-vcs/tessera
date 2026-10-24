# tes resolve

Resolve a merge conflict in a file using a specified strategy.

## Usage

```bash:no-line-numbers
tes resolve <path> --strategy <strategy>
```

## Options

- `--strategy <STRATEGY>` - Resolution strategy (`ast-union`, `ours`, `theirs`, `working`, or `content`).

## Description

The `resolve` command clears the conflict record for `<path>`, writes the chosen resolution to disk and the target branch, and replaces any intermediate conflict commit cleanly:

- `ast-union` - Combine both sides' AST declarations and statements in order.
- `ours` - Keep the first session or branch's version.
- `theirs` - Keep the second session or branch's version.
- `working` - Use the file as currently edited and saved on disk.

## Examples

```bash:no-line-numbers
# combine both sides using AST union
tes resolve src/App.vue --strategy ast-union

# keep the first branch's version
tes resolve src/App.vue --strategy ours

# keep the second branch's version
tes resolve src/App.vue --strategy theirs

# resolve using the manually edited file on disk
tes resolve src/App.vue --strategy working
```
