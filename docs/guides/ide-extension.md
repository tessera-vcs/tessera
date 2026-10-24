# Editor Support

The Tessera extension for **VS Code** and **Antigravity IDE** provides real-time branch switching, a 2D commit graph, active session monitoring, and one-click conflict resolution.

## Status Bar

When you open a repository initialized with `tes init`, two items appear on the left side of the status bar:

1. **Current Branch (`main`)**: Displays the branch currently checked out in your editor working tree. Click it at any time to switch your editor to another branch.
2. **`Graph` / `N Conflicts`**: Opens the Branch Graph. If any unresolved conflicts exist, this item highlights in red (`N Conflicts`) and opens the Conflict Resolver when clicked.

> **Note:** In workspaces that have not been initialized with `tes init`, the Tessera status bar items stay hidden automatically.

## Live Attribution

When two or more AI agents work on the same file on your active editor branch, the extension highlights each agent's lines in real time:

1. **File Summary CodeLens**: Appears at line 1 whenever two or more agents are concurrently editing the file (`2 agents concurrently editing this file (main): antigravity-a1b2 · claude-c3d4`).
2. **Hunk CodeLens (`Jump to Session`)**: Appears directly above the start line of each agent's modified section (`<agent_id> · L<start>-L<end>: <intent> · Jump to Session`). Clicking the CodeLens focuses the originating integrated terminal tab, terminal multiplexer pane (`tmux`, `WezTerm`, `kitty`), external macOS terminal tab (`Terminal.app`, `iTerm2`), or IDE agent conversation.
3. **Gutter Borders and Line Highlights**: Each agent session receives a distinct 3px left border and line highlight along with a matching marker on the scrollbar overview ruler.
4. **Inline Labels and Hover Cards**: The first line of each agent's section displays an italic inline label (`<agent_id>: <intent>`), and hovering any highlighted line shows a card with the agent ID, branch, line range, task intent, and a clickable **`Jump to Session`** action link.

<EditorPreview />

## Branch Graph

Click **`Graph`** in the status bar (or run **`tes: Open Branch Graph`** from the Command Palette) to open the commit graph:

- **Branch Column Headers**: Shows each branch's name, current commit hash, sandbox port, modified file count, and active agent sessions, with action buttons to **Switch**, **Merge**, or **Restore**.
- **Commit History**: Displays the commit history across parallel branch lanes with merge nodes, author names, and timestamps. Conflicted merge nodes are highlighted with a red border and a `Conflict` badge.

## Sidebar Views

Clicking the Tessera icon in the Activity Bar opens three sidebar views (plus a **Branches** view inside the Source Control sidebar):

### Branches
Lists every branch in the repository with its short commit SHA and modified file count ahead of `main`. Expanding a branch lets you:
- Switch your editor to that branch
- Open the branch's assigned dev server (`localhost:<port>`)
- Click any modified file (`M`) to open a side-by-side diff against `main`

### Sessions
Displays all active AI agent and terminal sessions (`antigravity-*`, `claude-*`, `codex-*`, `cursor-*`, `term-*`), their target branch, and their current task intent. Clicking the **Jump to Session** inline button (or running `tes session focus <session-id>` from a terminal) navigates directly to that session's terminal tab, multiplexer pane, or IDE window. Expanding a session shows claimed symbols and modified files.

### Checkpoints
Lists active conflicts alongside saved checkpoints (`tes checkpoint`), with toolbar actions to create a new checkpoint or open the Conflict Resolver.

## Conflict Resolution

When a merge conflict occurs:
1. The extension opens the conflicted file in your editor at the `<<<<<<<` conflict marker.
2. Resolve the conflict by editing the file and saving (`Cmd+S` / `Ctrl+S` resolves once markers are removed) or by using the **Conflict Resolver** panel (`AST Union`, `Keep Ours`, `Keep Theirs`, `Keep Working File`).

## See Also

- [Branches](/guides/branches-and-terminals) - How editor branches and terminal branches interact
- [Conflicts](/guides/conflict-resolution) - Resolving conflicts and rolling back changes
