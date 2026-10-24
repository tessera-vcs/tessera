# Conflicts

Most of the time, Tessera combines concurrent edits automatically using 3-way Tree-sitter AST merging and manifest drivers.

A conflict is recorded **only** when two sessions or branches modify the exact same statement or expression in incompatible ways (for example, both rewriting the same heading or return expression to different strings).

## Resolving in the Editor

When a merge conflict occurs while using the VS Code / Antigravity IDE extension:

1. Tessera writes standard Git conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) into the file.
2. The extension opens the conflicted file in your editor and jumps to the `<<<<<<<` line.
3. Edit the file to keep the code you want, remove the conflict markers, and save (`Cmd+S` / `Ctrl+S`).
4. As soon as the file is saved without conflict markers, Tessera resolves the conflict and updates the branch commit.

You can also click **`N Conflicts`** in the bottom status bar to open the **Conflict Resolver** panel and resolve with a single click (`AST Union`, `Keep Ours`, `Keep Theirs`, or `Keep Working File`).

## Inspecting Conflicts

To list any unresolved conflicts in your repository:

```bash:no-line-numbers
tes conflicts
```

Pass `--json` to inspect the base, ours, and theirs snippets along with the participating session IDs:

```bash:no-line-numbers
tes conflicts --json
```

## Resolving from the CLI

Resolve a conflicted file using one of four strategies:

```bash:no-line-numbers
# Combine both sides' AST declarations and statements in order
tes resolve src/App.vue --strategy ast-union

# Keep the first session/branch's version
tes resolve src/App.vue --strategy ours

# Keep the second session/branch's version
tes resolve src/App.vue --strategy theirs

# Use the file as currently edited and saved on disk
tes resolve src/App.vue --strategy working
```

## Checkpoints

Before asking an AI agent to perform a large multi-file refactor, save a named checkpoint:

```bash:no-line-numbers
tes checkpoint before-refactor
```

## Restoring Files

Use `tes restore` (or `tes reset`) to restore a branch or specific files to any checkpoint, commit SHA, `HEAD~N`, or branch:

```bash:no-line-numbers
# Restore the current branch to a named checkpoint or previous commit
tes restore before-refactor
tes restore HEAD~1

# Restore a specific branch without switching to it
tes restore -b feat/pricing HEAD~1

# Discard uncommitted changes in a specific file
tes restore src/App.vue

# Restore a file from another branch or commit
tes restore --source main src/App.vue
```

## Undo

Every claim, file reconciliation, checkpoint, restore, and merge is recorded in `.git/tessera`.

View recent operations:

```bash:no-line-numbers
tes oplog
```

Undo the most recent operation (including a merge):

```bash:no-line-numbers
tes undo
```

Or roll back to a specific operation ID from `tes oplog`:

```bash:no-line-numbers
tes undo --op <operation-id>
```

## See Also

- [Editor Support](/guides/ide-extension) - Conflict resolver and branch graph
- [tes resolve](/cli/resolve) - CLI command reference
- [tes restore](/cli/restore) - CLI command reference
