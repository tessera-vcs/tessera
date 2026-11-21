# Changelog

## [0.0.3] - 2026-11-21

### Features

- *(core,merge,workspace,agent,cli)* Harden invariants, fail-safe merges, doctor/dry-run/mcp, and modularize crates
- *(cli)* Add self-updating tes upgrade/update command with SHA-256 verification and HTTP E2E tests

### Bug Fixes

- *(vscode)* Repair broken CLI actions, security, conflict studio, and async watcher
- *(workspace,agent,cli)* Gate unix/macos-only helpers and muts for Linux and Windows clippy
- *(agent)* Gate auto-watcher daemon spawn to Unix to avoid Windows 90s pipe hang
- *(install,docs)* Guard curl.exe stderr on PowerShell 5.1 and document tes upgrade in agents.txt
- *(ci,release)* Include CHANGELOG.md release commits in extension git-cliff path filter to preserve v0.0.1/v0.0.2 tags
- *(agent)* Prevent watcher from recording HEAD or stale pre-commit blobs into observed_blobs
- Harden core, merge, agent, CLI, VS Code, release, and docs for v0.0.3
- *(core)* Allow Windows readonly clear and refine .tessera/tmp fallback boundary
- *(agent)* Pass Windows --verify command via cmd /S /C raw_arg and use batch script in test

### Documentation

- Align guides, CLI references, MCP schemas, and README with hardened implementation
- Update footer copyright to Lunaris
- *(vscode)* Simplify extension README title and add extension-only CHANGELOG.md
- *(vscode)* Simplify README title, auto-generate extension CHANGELOG.md, and fix VSIX packaging (#11)
- *(readme)* Add CI, release, docs, Rust, and license badges

### Miscellaneous

- *(release,install)* Harden CI gates, release workflow, atomic installers, and multi-shell E2E
- *(release)* Restore automatic CHANGELOG.md generation and push on main
- *(release,vscode)* Auto-generate extension CHANGELOG.md via git-cliff and align @types/vscode with ^1.85.0
- Execute install.sh, install.ps1 (pwsh + PS 5.1), git-cliff, and vsce package in CI

## [0.0.2] - 2026-10-25

### Features

- *(merge)* Support indentation, keyword-end, and section structural blocks
- *(workspace)* Preserve and sync gitignored files and monorepo symlinks
- Universal structural block merging, CoW ignore preservation, and docs
- *(branch)* Support branching off specific parent branches in switch and claim
- *(agent,cli,workspace)* Persistent per-branch slots, active port lease detection, TTY session scoping, and multi-path post-tool batching

### Bug Fixes

- *(workspace)* Preserve file and remote database URLs during slot isolation
- *(core)* Strip existing timestamp suffix when preserving author on commit amend
- *(vscode)* Lock Branch Graph topbar and make branch headers sticky on scroll (v0.0.2)
- *(agent,cli)* Preserve watcher-observed in-flight edits across peer commits and support chained tes switch
- *(agent)* Restore HEAD baseline in repo_root when cross-branch peers await edits
- *(cli)* Prevent oplog reconciliation storm and sandbox PWD stranding
- Eliminate IDE refresh subprocess spawns and tighten documentation copy

### Documentation

- Clarify core problem statement and remove external tool comparisons
- Remove unreleased NetBSD references from docs and install.sh

### Miscellaneous

- *(github)* Add pull request template
- *(ci)* Trigger docs deployment only on manual workflow_dispatch

## [0.0.1] - 2026-10-24

### Features

- Initial commit
- *(install)* Show download progress bar and verify installers in release CI
- *(install)* Adopt stepped installer UI with inline download progress
- *(agent)* Resolve remote-tracking refs when pulling or switching branches

### Bug Fixes

- *(ci)* Replace npm ci with npm i
- *(workspace)* Gate prune_unsafe_symlinks_in_dir with macos cfg
- *(ci)* Track scripts, resolve cross-platform compiler warnings, and drop -D warnings
- *(ci)* Resolve default branch, pwsh env hook, and bsd matrix failures
- *(agent)* Verify shell_pid before pruning passive sessions and skip Windows CIM subprocesses
- *(cli)* Run main on 16 MiB stack for Windows debug builds and shorten csh unsetenv
- *(ci)* Seed .zshrc in fake_home test and gate background watcher daemon to unix
- *(workspace)* Use libc::Ioctl type for FICLONE on Linux musl targets
- *(install)* Align download arrow width and shorten progress label
- *(workspace)* Support multi-wildcard and recursive globs in .gitignore matching
- *(agent)* Avoid creating duplicate AGENTS.md and .mcp.json during init

### Documentation

- Add uninstall guide, release download links, and CLI examples
- Add contributor guide and update init reference
- Clarify extension support for VS Code and VS Code-based editors

### Testing

- *(cli)* Resolve native sandbox path and gate POSIX chmod assertions on Windows
- *(cli)* Gate bin.sh chmod assertion in stash roundtrip test on Windows
- *(cli)* Sample best of 3 runs for tes env latency check in debug builds

### Miscellaneous

- *(actions)* Consolidate e2e jobs into rust-matrix and reuse debug binary
- *(release)* Set initial version to 0.0.1 and verify passed CI before release
- *(deploy)* Deploy docs to Vercel when docs/ changes on main or release
- *(deploy)* Simplify Vercel deploy step name and output
- *(docs)* Extract Vercel deployment into dedicated workflow
- Track npm lockfiles and auto-generate CHANGELOG.md during release
- *(repo)* Add .editorconfig, .gitattributes, and autonomous agent rules
- *(actions)* Filter CI jobs by modified paths

