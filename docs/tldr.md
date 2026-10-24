---
aside: false
outline: false
---

# TL;DR

Tessera works on top of your existing Git repository so multiple AI agents and terminal sessions can work in the same checkout on separate branches or the same branch without overwriting files, colliding on `.git/HEAD`, or conflicting on local ports and databases. `tes init` writes the instruction files (`AGENTS.md`, `CLAUDE.md`, `.cursor/rules`) into your project so your agents use `tes` automatically.

## Install

**macOS, Linux, and BSD:**

```bash:no-line-numbers
curl -fsSL https://tessera.getlunaris.dev/install.sh | sh
tes init
```

**Windows (PowerShell):**

```powershell:no-line-numbers
irm https://tessera.getlunaris.dev/install.ps1 | iex
tes init
```
