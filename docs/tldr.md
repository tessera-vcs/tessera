---
aside: false
outline: false
---

# TL;DR

Tessera coordinates multiple AI agents and terminal sessions in the same Git checkout on separate branches or the same branch. Each session gets its own branch state, Copy-on-Write sandbox, and `TES_PORT` slot inside one repository folder (enable automatic dev-server port isolation with `tes config set port_isolation true` or `TES_PORT_ISOLATION=1`; it defaults to `false`). `tes init` writes instruction and hook files (Antigravity by default, plus auto-detected Claude Code, Cursor, Windsurf, Kiro, OpenCode, Codex, and VS Code, or `--harness all`) so your agents use `tes` automatically. Pass `tes init --local` on mixed teams to add installed rule files to `.git/info/exclude`.

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
