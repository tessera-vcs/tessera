# Agents

Once a repository is initialized with `tes init`, editors and CLI harnesses like Antigravity, Claude Code, Codex, Cursor, Windsurf, Kiro, and OpenCode can work in the same checkout simultaneously without overwriting each other's files.

## Automatic Coordination

You do not need to change how you prompt your AI agents or manually manage locks. The workspace rules installed by `tes init` instruct each agent to run four commands:

<AgentWorkflowIllustration />

1. **Claim the task (`tes claim`)**: Before editing files, the agent declares what it is working on, which files it will touch, and (optionally) which branch it is targeting:
   ```bash:no-line-numbers
   tes claim --branch feat/pricing --intent "Add pricing calculator" src/components/PricingCalculator.vue src/App.vue
   ```
2. **Reconcile edits (`tes hook post-tool`)**: Immediately after modifying a file, the agent reconciles its edit:
   ```bash:no-line-numbers
   tes hook post-tool --path src/App.vue
   ```
3. **Verify in a sandbox (`tes exec`)**: To run a compiler, type-checker, or test suite, the agent executes inside its branch's Copy-on-Write sandbox:
   ```bash:no-line-numbers
   tes exec -- npm run build
   ```
4. **Commit (`tes commit` / `tes merge`)**: When finished, the agent commits its changes cleanly onto its target branch:
   ```bash:no-line-numbers
   tes commit -m "feat(pricing): add pricing calculator"
   ```

## Same Branch vs. Separate Branches

### Same Branch

When two agents work on the **same branch** (for example, both editing on `main`), Tessera combines their edits automatically using 3-way AST merging:

- **Functions and Methods**: Two agents can add or edit different functions, or add methods to the same `class` or `impl` block, in the same file.
- **Imports and Parameters**: Both agents can add imports to the top of a file or add parameters to the same function signature.
- **Vue and Svelte Components**: Concurrent edits to `<script setup>`, `<template>`, and `<style scoped>` blocks merge cleanly.
- **Manifests and Lockfiles**: Concurrent additions to `package.json`, `Cargo.toml`, and `Cargo.lock` are merged structurally.

### Separate Branches

When you ask Agent 1 to work on `feat/pricing` and Agent 2 to work on `feat/faq`, both agents may touch shared files like `src/App.vue` or `index.html` at the same time.

Tessera keeps each branch isolated:
- Changes made on `feat/pricing` never leak into `feat/faq`'s sandbox or commit tree.
- Two agents on different branches will never trigger a conflict against each other while building their features.
- When both agents finish, you can inspect each branch independently or merge both into `main` with `tes merge feat/pricing feat/faq --into main`.

## Status and Diffs

Check modified files in your current session or branch with `tes status`:

```bash:no-line-numbers
tes status
```

`tes status` groups files into three sections:
- **Staged Changes**: Changes recorded in the active session (`M`, `A`, `D`)
- **Unstaged Changes**: Working tree edits on disk not yet reconciled
- **Committed on Branch**: Files committed on the current feature branch ahead of `main`

Use `tes diff` to view line-by-line diffs:

```bash:no-line-numbers
# View all changes in the current session or working tree
tes diff

# View only staged session changes
tes diff --staged

# View changes for a single file
tes diff src/App.vue

# Compare any branch against main
tes diff -b feat/pricing
```

## Live Sessions

To see all active agents and terminal sessions in real time, run:

```bash:no-line-numbers
tes radar
```

`tes radar` shows each session's ID, assigned port slot, target branch, task intent, and modified files.

## See Also

- [Branches](/guides/branches-and-terminals) - Switching and merging branches
- [Sandboxes](/guides/running-tests-and-servers) - Running builds and dev servers in branch sandboxes
- [Examples](/examples/) - Step-by-step walkthroughs
