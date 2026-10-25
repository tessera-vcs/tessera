---
aside: false
outline: false
---

# TL;DR

Tessera coordinates multiple AI agents and terminal sessions in the same Git checkout on separate branches or the same branch. Each session gets its own branch state, Copy-on-Write sandbox, and port slot inside one repository folder. `tes init` writes the instruction files (`AGENTS.md`, `CLAUDE.md`, `.cursor/rules`) into your project so your agents use `tes` automatically.

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
