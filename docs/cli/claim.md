# tes claim

Register a task intent, target branch, and file paths for an AI agent session.

## Usage

```bash:no-line-numbers
tes claim --intent <TEXT> [options] <files...>
```

## Options

- `--intent <TEXT>` - Concise description of the task being performed.
- `-b, --branch <BRANCH>` - Target branch for the session (created from `--from`, the session's current branch, or `HEAD` if it does not exist).
- `--from, --base <BRANCH>` - Base branch or commit to fork `--branch` from if the target branch does not exist yet. Fails if the specified base revision does not exist.
- `--agent <ID>` - Explicit session ID (auto-detected from environment by default).

## Description

AI agents run `tes claim` before modifying files. The command records the starting baseline of each claimed file, binds the session to `--branch` if specified, prepares the branch's Copy-on-Write sandbox, and starts the background file watcher.

If `--branch` does not exist yet and `--from <base-branch>` is passed, Tessera forks the new branch from `<base-branch>` and stages the base branch's baseline for claimed files so IDE tools read and edit the parent branch's state.

## Examples

```bash:no-line-numbers
# claim files on the current branch
tes claim --intent "Add currency formatter" src/currency.ts

# claim files on an isolated feature branch
tes claim --branch feat/pricing --intent "Build pricing calculator" src/components/PricingCalculator.vue src/App.vue

# branch off an existing feature branch
tes claim --branch feat/pricing-tiers --from feat/pricing --intent "Add enterprise tier" src/components/PricingCalculator.vue
```
