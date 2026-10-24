# Getting Started

Tessera works on top of your existing Git repository so multiple AI coding agents and terminal sessions can work in the same checkout without overwriting files or colliding on `.git/HEAD`. This guide covers installing the `tes` CLI, initializing a repository, and running your first multi-branch workflow.

## Prerequisites

- **OS**: macOS, Linux, Windows, FreeBSD, OpenBSD, or NetBSD
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

This creates `.git/tessera` and installs workspace rules for your AI coding editors.

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

- Sets up `.git/tessera` inside your existing Git repository
- Installs instruction rules (`AGENTS.md`, `.agents/rules/tessera.md`, `.cursor/rules`, `CLAUDE.md`) so your AI coding agents coordinate through Tessera

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

In standard Git, running two AI agents on two different branches requires creating separate `git worktree` folders, reinstalling dependencies in each folder, and configuring different ports by hand.

With Tessera, you work inside a **single repository folder**:

```bash:no-line-numbers
# Terminal 1 works on feat/pricing (assigned port 4100)
tes switch feat/pricing
npm run dev

# Terminal 2 works on feat/faq in the same folder (assigned port 4200)
tes switch feat/faq
npm run dev
```

When you prompt an AI agent in your editor, you do not need to mention `tes` in your prompt:

```text
On branch feat/pricing, create PricingCalculator.vue and add it to src/App.vue.
```

The workspace rules installed by `tes init` instruct the agent to claim the branch, isolate its edits, verify the build in its branch sandbox, and commit.

## Next Steps

- [Why Tessera?](/getting-started/why-tessera) - See how Tessera compares to standard Git and `git worktree`
- [Installation](/getting-started/installation) - Set up `(tes:<branch>)` in your terminal prompt and tab completion
- [Your First Workflow](/getting-started/first-workflow) - Walk through a multi-agent example step by step
- [Guides](/guides/) - Learn about multi-agent workflows, dev servers, conflicts, and editor integration
