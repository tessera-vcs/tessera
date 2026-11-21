# tes watch

Continuously watch the working tree and auto-reconcile edits for hookless editors or AI harnesses.

## Usage

```bash:no-line-numbers
tes watch [options]
```

## Options

- `-a, --agent <ID>` - Explicit agent or session ID (auto-detected if omitted).
- `--interval-ms <N>` - Polling interval in milliseconds (default: `250`, minimum: `50`).
- `--once` - Run a single watch scan tick and exit (useful for scripts and tests).
- `--json` - Emit structured JSON watch outcomes.

## Description

AI harnesses that support post-tool hooks (such as Claude Code and Codex) or follow Tessera's rule instructions run `tes hook post-tool` automatically after file writes. For hookless editors or custom tools, `tes watch` polls claimed paths in the working tree, snapshots writes, and reconciles concurrent edits automatically.

## Examples

```bash:no-line-numbers
# watch the working tree every 250ms for the current session
tes watch

# run a single reconciliation pass and exit
tes watch --once --json

# watch with a custom polling interval for a specific session
tes watch --agent custom-worker --interval-ms 150
```
