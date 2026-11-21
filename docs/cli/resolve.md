# tes resolve

Resolve a merge conflict in a file using a specified strategy.

## Usage

```bash:no-line-numbers
tes resolve <path> --strategy <strategy> [options]
```

## Options

- `--strategy <STRATEGY>` - Resolution strategy (`ast-union`, `ours`, `theirs`, `working`, or `content`; defaults to `working`).
- `--content <TEXT>` - Inline resolved file content when `--strategy content` is selected.
- `--content-file <PATH>` - Read resolved file content from `<PATH>` when `--strategy content` is selected (useful for multi-line or large files).

## Description

The `resolve` command clears the active conflict record for `<path>` and writes the resolved content to the active branch's working tree and sandbox:

- When no active sessions hold uncommitted changes on `<path>` for the active branch, Tessera records a new resolution commit (`Resolve conflict in <path>`) on top of the active branch tip and updates its ref, Git index (if checked out), and branch sandbox.
- When active sessions hold uncommitted edits on `<path>`, Tessera updates the working file, sandbox, and active session baselines so those sessions can continue editing and commit cleanly.

Supported strategies:

- `ast-union` - Combine both sides' AST declarations and statements in order (rejected on binary files).
- `ours` - Keep the first session or branch's version.
- `theirs` - Keep the second session or branch's version.
- `working` - Use the file as currently edited and saved on disk (fails if Git conflict markers `<<<<<<<` / `=======` / `>>>>>>>` still remain).
- `content` - Write explicit resolved content supplied via `--content <TEXT>` or `--content-file <PATH>`.

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

# resolve with explicit content from a file
tes resolve src/App.vue --strategy content --content-file resolved-app.vue
```
