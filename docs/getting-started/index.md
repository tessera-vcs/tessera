# Getting Started

Tessera coordinates multiple AI coding agents and terminal sessions inside an existing Git repository. This guide covers installing the `tes` CLI, initializing a repository, and running a multi-branch workflow.

## Prerequisites

- **OS**: macOS, Linux, Windows, FreeBSD, or OpenBSD
- **Git**: Standard Git 2.23+

## Installation

Install the `tes` CLI on **macOS, Linux, or BSD**:

```bash:no-line-numbers
curl -fsSL https://tessera.getlunaris.dev/install.sh | sh
tes --version
```

Or on **Windows (PowerShell)**:

```powershell:no-line-numbers
irm https://tessera.getlunaris.dev/install.ps1 | iex
tes --version
```

For shell integration, prebuilt binaries, and editor setup details, see the [Installation Guide](/getting-started/installation).

## Initializing Your Project

Navigate to any project directory and run `tes init`:

```bash:no-line-numbers
tes init
```

This creates `.git/tessera`, installs Git sync hooks (`post-commit`, `post-merge`, and `post-checkout`), and installs workspace rules for your AI coding editors (Antigravity by default, plus auto-detected Claude Code, Cursor, Windsurf, Kiro, OpenCode, Codex, and VS Code, or specify `--harness all|<list>`). Pass `tes init --local` on mixed teams to exclude installed rule files via `.git/info/exclude`.

> **Note:** If the directory is not yet a Git repository, `tes init` runs `git init` and creates an initial commit from your existing files before setting up Tessera.

## Your First Workflow

Switch your current terminal tab to a feature branch, run your tests, and merge your work:

```bash:no-line-numbers
tes switch feat/pricing
npm test
tes status
tes merge feat/pricing --into main -m "feat: add pricing calculator"
```

## What Each Command Does

```bash:no-line-numbers
tes init
```

- Sets up `.git/tessera` and Git sync hooks (`.git/hooks/post-commit`, `post-merge`, and `post-checkout`) inside your existing Git repository
- Installs instruction rules for Antigravity (`.agents/rules/tessera.md`) by default plus auto-detected AI editors (`CLAUDE.md`, `.claude/settings.json`, `.cursor/rules/tessera.mdc`, `AGENTS.md`, and more; use `--harness all` to install all, or `--local` to add them to `.git/info/exclude`)

```bash:no-line-numbers
tes switch feat/pricing
```

- Binds your current terminal tab to the `feat/pricing` branch (creating it from `HEAD` if it does not exist yet)
- Prepares a Copy-on-Write sandbox for `feat/pricing` that shares your existing `node_modules`, `target`, or `.venv`

```bash:no-line-numbers
tes merge feat/pricing --into main -m "feat: add pricing calculator"
```

- Merges `feat/pricing` into `main` using Tessera's 3-way AST merge engine
- Updates your working directory on `main`

## Working in a Single Folder

In a standard Git checkout, every terminal tab and AI agent shares `.git/HEAD` and the working tree on disk. Tessera isolates each session inside a **single repository folder** with its own branch state and `TES_PORT` slot. When running concurrent dev servers on separate branches, enable `port_isolation` (defaults to `false`) with `tes config set port_isolation true` or `TES_PORT_ISOLATION=1`:

```bash:no-line-numbers
# Enable automatic dev-server port isolation (defaults to false)
tes config set port_isolation true

# Terminal 1 works on feat/pricing (assigned port 3100)
tes switch feat/pricing
npm run dev

# Terminal 2 works on feat/faq in the same folder (assigned port 3200)
tes switch feat/faq
npm run dev
```

When you prompt an AI agent in your editor, you do not need to mention `tes` in your prompt:

```text
On branch feat/pricing, create PricingCalculator.vue and add it to src/App.vue.
```

The workspace rules installed by `tes init` instruct the agent to claim the branch, isolate its edits, verify the build in its branch sandbox, and commit.

## Next Steps

- [Why Tessera?](/getting-started/why-tessera) - Understand the core problems Tessera solves for parallel AI agents
- [Installation](/getting-started/installation) - Set up `(tes:<branch>)` in your terminal prompt and tab completion
- [Your First Workflow](/getting-started/first-workflow) - Walk through a multi-agent example step by step
- [Guides](/guides/) - Learn about multi-agent workflows, dev servers, conflicts, and editor integration
