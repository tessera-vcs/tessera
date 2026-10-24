# tes claim

Register a task intent, target branch, and file paths for an AI agent session.

## Usage

```bash:no-line-numbers
tes claim --intent <TEXT> [options] <files...>
```

## Options

- `-i, --intent <TEXT>` - Concise description of the task being performed.
- `-b, --branch <BRANCH>` - Target branch for the session (created from `HEAD` if it does not exist).
- `-a, --agent <ID>` - Explicit session ID (auto-detected from environment by default).

## Description

AI agents run `tes claim` before modifying files. The command records the starting baseline of each claimed file, binds the session to `--branch` if specified, prepares the branch's Copy-on-Write sandbox, and starts the background file watcher.

## Examples

```bash:no-line-numbers
# claim files on the current branch
tes claim --intent "Add currency formatter" src/currency.ts

# claim files on an isolated feature branch
tes claim --branch feat/pricing --intent "Build pricing calculator" src/components/PricingCalculator.vue src/App.vue
```
