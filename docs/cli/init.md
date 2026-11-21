# tes init

Initialize Tessera in a project directory and install AI editor rules.

## Usage

```bash:no-line-numbers
tes init [path] [options]
```

## Options

- `-b, --branch <BRANCH>` - Default trunk branch name (defaults to the active Git branch or `init.defaultBranch`, falling back to `main`).
- `--harness <LIST>` - Comma-separated list of AI editors/harnesses to configure (`antigravity`, `claude`, `cursor`, `windsurf`, `kiro`, `opencode`, `codex`, `vscode`, or `all`).
- `--local` - Add installed AI agent rule and harness files to `.git/info/exclude` so they stay ignored locally without modifying `.gitignore` or polluting `git status` on mixed teams.

## Description

The `init` command sets up Tessera inside your project by:
1. Initializing `.git` and creating an initial commit if the directory is not already a Git repository.
2. Creating the `.git/tessera` store and default configuration (and excluding `.tessera/` in `.git/info/exclude`).
3. Installing workspace rules and MCP configuration for AI coding editors: by default, `tes init` installs Antigravity (`.agents/rules/tessera.md`) plus any auto-detected editors (`claude`, `cursor`, `windsurf`, `kiro`, `opencode`, `codex`, `vscode`) whose repository files or environment variables are present. Pass `--harness <list>` or `--harness all` to install specific or all supported harnesses (`.agents/rules/tessera.md`, `.claude/settings.json`, `.mcp.json`, `CLAUDE.md`, `.cursor/mcp.json`, `.cursor/rules/tessera.mdc`, `.windsurf/rules/tessera.md`, `.kiro/steering/tessera.md`, `.kiro/settings/mcp.json`, `opencode.json`, `.codex/config.toml`, `.codex/hooks.json`, `.vscode/mcp.json`, `AGENTS.md`).
4. Installing lightweight Git `post-commit`, `post-merge`, and `post-checkout` hooks (`.git/hooks/post-commit`, `.git/hooks/post-merge`, `.git/hooks/post-checkout`) to keep `.git/tessera` synchronized with native `git` commands.

## Examples

```bash:no-line-numbers
# initialize in current directory (Antigravity + auto-detected AI editors)
tes init

# initialize locally on a mixed team (adds installed rule files to .git/info/exclude)
tes init --local

# initialize with a specific default branch
tes init --branch main

# install rules for specific AI editors
tes init --harness antigravity,claude,cursor

# install rules for all supported AI editors
tes init --harness all
```
