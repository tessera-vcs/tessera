# Conflict Resolution

This example shows what happens when two branches modify the exact same line in incompatible ways, and how to resolve the conflict in your editor or from the CLI.

## 1. Creating a Conflict

Suppose `index.html` on `main` has:

```html
<title>My App</title>
```

Now suppose:
- Branch `feat/pricing` changes `<title>` to `<title>Pricing | My App</title>`.
- Branch `feat/faq` changes `<title>` to `<title>FAQ | My App</title>`.

When you merge both branches into `main`:

```bash:no-line-numbers
tes merge feat/pricing feat/faq --into main -m "feat: merge pricing and faq"
```

All non-overlapping files and components merge automatically, and Tessera reports `1` conflict on `index.html`.

## 2. Resolving in Your Editor

If you have the VS Code / Antigravity IDE extension installed:

1. `index.html` opens in your editor at the `<<<<<<<` line:
   ```html
   <<<<<<< ours
   <title>Pricing | My App</title>
   =======
   <title>FAQ | My App</title>
   >>>>>>> theirs
   ```
2. Edit the line to what you want:
   ```html
   <title>My App | Pricing and FAQ</title>
   ```
3. Save the file (`Cmd+S` / `Ctrl+S`). The extension detects that the conflict markers have been removed and resolves the conflict automatically.

## 3. Resolving from the CLI

You can also inspect and resolve conflicts directly from your terminal:

```bash:no-line-numbers
# List active conflicts
tes conflicts

# Resolve using the edited file on disk
tes resolve index.html --strategy working

# Or pick one side directly
tes resolve index.html --strategy ours
tes resolve index.html --strategy theirs
```

## Next Steps

- [Conflicts](/guides/conflict-resolution) - Learn about checkpoints, `tes restore`, and `tes undo`
- [Editor Support](/guides/ide-extension) - Explore the Conflict Resolver panel
