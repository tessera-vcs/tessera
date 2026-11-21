# Editor Support

The Tessera extension (`tessera-vscode.vsix`) works in **VS Code** and all **VS Code-based editors** (including **Antigravity**, **Cursor**, **Windsurf**, **Kiro**, and **VSCodium**), providing real-time branch switching, a 2D commit graph, active session monitoring, and one-click conflict resolution.

## Status Bar

When you open a repository initialized with `tes init`, two items appear on the left side of the status bar:

1. **Current Branch (`main`)**: Displays the branch currently checked out in your editor working tree. Click it at any time to switch your editor to another branch.
2. **`Graph` / `N Conflicts`**: Opens the Branch Graph. If any unresolved conflicts exist, this item highlights in red (`N Conflicts`) and opens the Conflict Resolver when clicked.

> **Note:** In workspaces that have not been initialized with `tes init`, the Tessera status bar items stay hidden automatically.

## Live Attribution

Whenever any active session has uncommitted edits in a file on your active editor branch (whether a single session or multiple concurrent agents), the extension highlights each session's lines in real time:

1. **Top-of-File CodeLenses**: Shows a **`Dismiss Highlights`** CodeLens whenever highlights are active, plus a file summary CodeLens at line 1 whenever two or more sessions are concurrently editing the file (`2 agents concurrently editing this file (main): antigravity-a1b2 · claude-c3d4`).
2. **Hunk CodeLens**: Appears directly above the start line of each session's modified section (`<agent_id> · L<start>-L<end>: <intent>`). For terminal and CLI sessions with a navigable terminal waypoint (`term-*`, TTY, `tmux`, `WezTerm`, `kitty`, `Terminal.app`, or `iTerm2`), the CodeLens appends a clickable **`· Jump to Terminal`** link that focuses that terminal tab or pane.
3. **Gutter Borders and Line Highlights**: Each session slot receives a distinct 3px left border and line highlight along with a matching marker on the scrollbar overview ruler (overlapping lines from multiple sessions use an error highlight).
4. **Inline Labels and Hover Cards**: The first line of each session's section displays an italic inline label (`<agent_id>: <intent>`), and hovering any highlighted line shows a card with the session ID, branch, line range, task intent, a **`Jump to Terminal`** link (when navigable), and a **`Branch Graph`** link.

<EditorPreview />

## Branch Graph

Click **`Graph`** in the status bar (or run **`Tessera: Open Branch Graph`** from the Command Palette) to open the 2D commit graph:

- **Branch Column Headers**: Sticky headers at the top of each branch lane show the branch name, **`Active`** / **`Switch`** status badge (clicking the header switches the editor working tree to that branch), a delete button (`×` on non-trunk branches), a **`Default`** or **`Branch`** badge, and the branch's assigned port (`:<port>`).
- **Topbar Actions**: Includes one-click buttons to **`Resolve Conflict`** (when conflicts exist), **`Merge into <trunk>`**, **`Collapse Linear`** / **`Expand All`**, and **`Refresh`**.
- **Newest-First Commit History**: Renders commit and merge cards newest-first so **`Working`** and **`HEAD`** nodes stay immediately visible beneath the sticky column headers. Consecutive commits with matching conventional topics group horizontally, long linear chains collapse into expandable pills, file chips open diffs or conflicted files, **`Working`** cards include a **`Discard`** button, and conflicted nodes are highlighted with a red border.

## Sidebar Views

Clicking the Tessera icon in the Activity Bar opens three sidebar views (plus a **Branches** view inside the Source Control sidebar):

### Branches
Lists every branch in the repository with its short commit SHA, active marker, port (`:<port>`), and modified file count ahead of `main`. Expanding a branch lets you:
- Switch your editor to that branch
- Open the branch's assigned dev server (`localhost:<port>`)
- Click any modified file (`M`) to open a side-by-side diff against `main`

### Sessions
Displays all active AI agent and terminal sessions (`antigravity-*`, `claude-*`, `codex-*`, `cursor-*`, `term-*`), their target branch, and their current task intent. Expanding a terminal session provides a **`Jump to Terminal`** action (and the inline **Jump to Session** button or `tes session focus <session-id>` focuses the session's terminal tab, multiplexer pane, or IDE window), along with branch switching, claimed symbols, and modified files.

### Conflicts & Checkpoints
Lists active conflicts, saved checkpoints (`tes checkpoint`), and stashes (`stash@{0}`, expandable to view stashed files with inline **Pop Stash** and **Drop Stash** actions), with toolbar actions to create a new checkpoint or open the Conflict Resolver.

## Conflict Resolution

When a merge conflict occurs:
1. The extension opens the conflicted file in your editor at the `<<<<<<<` conflict marker.
2. Resolve the conflict by editing the file and saving (`Cmd+S` / `Ctrl+S` resolves once markers are removed) or by opening the **Conflict Resolver** panel (click **`N Conflicts`** in the status bar or run **`Tessera: Open Conflict Resolver`**).
3. Each conflict card displays competing agent intents, **`Verification Diagnostics`** (when `diagnostics` is present, such as from a failed `--verify` check), **`Base`** (when `base_snippet` is present), **`Ours`**, and **`Theirs`** snippets, an editable **`AST-Union Preview`** box, an **`Open in Editor`** button, and one-click actions: **`Accept AST-Union`**, **`Accept Ours`**, **`Accept Theirs`**, **`Keep Working File`**, and **`Save Custom`**.

## See Also

- [Branches](/guides/branches-and-terminals) - How editor branches and terminal branches interact
- [Conflicts](/guides/conflict-resolution) - Resolving conflicts and rolling back changes
