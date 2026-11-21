# Changelog

All notable changes to the Tessera extension for VS Code and VS Code-based editors are documented in this file.

## [0.0.3] - 2026-11-21

### Fixed

- Repaired CLI invocations for **Close Session** (`agent close`), **Clear Idle Sessions** (`agent clear`), and **Pop Stash** (`stash -a editor pop`).
- Made the **Conflict Resolver** panel directly openable and updated it via `postMessage` instead of rewriting full webview HTML so unsaved edits and scroll position are preserved.
- Switched custom conflict resolution saves to `tes resolve --content-file` to avoid CLI argument length and quoting issues.
- Enforced VS Code Workspace Trust before invoking workspace-local `tes` binaries and added confirmation dialogs for destructive checkpoint and stash actions.
- Hardened Windows session focusing against PowerShell PID interpolation and moved process lookups off the synchronous extension host thread.
- Fixed Command Palette argument handling for branch file diffs and stash drop actions, and removed hardcoded `"main"` trunk assumptions.

### Improved

- Replaced synchronous `.git` polling with an async watcher supporting linked Git worktrees and `packed-refs`, debounced live editor decorations, and added a dedicated **Tessera** Output Channel.
- Lowered `engines.vscode` to `^1.85.0` for broader compatibility across VS Code and VS Code-based editors, and set `tes.binPath` scope to `machine-overridable` for Remote-SSH and WSL workspaces.
- Modularized the 2D Branch Graph panel into dedicated layout and HTML renderer modules.

## [0.0.2] - 2026-10-25

### Fixed

- Locked the 2D Branch Graph topbar and made branch headers sticky while scrolling.
- Eliminated redundant subprocess spawns during IDE refresh cycles and prevented operation-log refresh storms.

## [0.0.1] - 2026-10-24

### Added

- Initial release of the Tessera extension.
- Added Status Bar branch switcher, live multi-agent line attribution with gutter markers and **Jump to Session** CodeLenses, interactive 2D Branch & Commit Graph, 3-Way AST Conflict Resolver, and Sidebar tree views (**Branches**, **Sessions**, and **Conflicts & Checkpoints**).
