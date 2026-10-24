# tes init

Initialize Tessera in a project directory and install AI editor rules.

## Usage

```bash:no-line-numbers
tes init [options]
```

## Options

- `-b, --branch <BRANCH>` - Default trunk branch name (default: `main`).
- `--harness <LIST>` - Comma-separated list of AI editors/harnesses to configure (`antigravity`, `claude`, `cursor`, `windsurf`, `kiro`, `opencode`, `codex`, `vscode`, or `all`).

## Description

The `init` command sets up Tessera inside your project by:
1. Initializing `.git` and creating an initial commit if the directory is not already a Git repository.
2. Creating the `.git/tessera` store and default configuration.
3. Installing workspace rules (`AGENTS.md`, `.agents/rules/tessera.md`, `.codex/config.toml`, `.codex/hooks.json`, and detected editor rules) and MCP configuration so AI coding agents coordinate through Tessera automatically.
4. Installing lightweight Git `post-commit` and `post-merge` hooks to keep `.git/tessera` synchronized with native `git` commands.

## Examples

```bash:no-line-numbers
# initialize in current directory (auto-detects AI editors)
tes init

# initialize with a specific default branch
tes init --branch main

# install rules for specific AI editors
tes init --harness antigravity,claude,cursor

# install rules for all supported AI editors
tes init --harness all
```
