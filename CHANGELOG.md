## [0.0.1] - 2026-10-24

### 🚀 Features

- Initial commit
- *(install)* Show download progress bar and verify installers in release CI
- *(install)* Adopt stepped installer UI with inline download progress
- *(agent)* Resolve remote-tracking refs when pulling or switching branches

### 🐛 Bug Fixes

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

### 📚 Documentation

- Add uninstall guide, release download links, and CLI examples
- Add contributor guide and update init reference
- Clarify extension support for VS Code and VS Code-based editors

### 🧪 Testing

- *(cli)* Resolve native sandbox path and gate POSIX chmod assertions on Windows
- *(cli)* Gate bin.sh chmod assertion in stash roundtrip test on Windows
- *(cli)* Sample best of 3 runs for tes env latency check in debug builds

### ⚙️ Miscellaneous Tasks

- *(actions)* Consolidate e2e jobs into rust-matrix and reuse debug binary
- *(release)* Set initial version to 0.0.1 and verify passed CI before release
- *(deploy)* Deploy docs to Vercel when docs/ changes on main or release
- *(deploy)* Simplify Vercel deploy step name and output
- *(docs)* Extract Vercel deployment into dedicated workflow
- Track npm lockfiles and auto-generate CHANGELOG.md during release
- *(repo)* Add .editorconfig, .gitattributes, and autonomous agent rules
- *(actions)* Filter CI jobs by modified paths
