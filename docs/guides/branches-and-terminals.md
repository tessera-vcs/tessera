# Branches

In standard Git, all open terminals and editor windows share a single `.git/HEAD`. Running `git checkout feat/pricing` in one terminal switches the branch underneath every other open terminal and AI agent.

Tessera separates **per-terminal branches**, **per-agent branches**, and your **editor branch** so they never step on each other.

<BranchesAndTerminalsIllustration />

## Switching Branches

When shell integration is enabled (`tes env --install`), each terminal tab has its own session and can switch branches independently:

```bash:no-line-numbers
# Terminal 1
tes switch feat/pricing

# Terminal 2
tes switch feat/faq
```

If the target branch does not exist yet, Tessera creates it automatically from `HEAD` (or from your session's current branch if that tab is already on a feature branch). To branch off a specific parent branch explicitly, pass `--from <base-branch>` or use `tes switch -b <new-branch> <base-branch>`:

```bash:no-line-numbers
tes switch -b feat/pricing-tiers feat/pricing
# or
tes switch feat/pricing-tiers --from feat/pricing
```

In a switched terminal tab:
- Your shell prompt displays `(tes:feat/pricing)` so you always see which branch that tab is on.
- Commands you run in that terminal (`npm test`, `npm run build`, `npm run dev`, `cargo test`) execute inside that branch's Copy-on-Write sandbox while your terminal prompt stays in your project folder.
- Other open terminals and your code editor remain untouched.

## Switching the Editor Branch

When you want your IDE file explorer and language server to display a different branch's files in your main repository folder, switch the editor branch:

- **In VS Code or any VS Code-based IDE**: Click the branch name in the bottom-left status bar and select the branch from the dropdown.
- **From the CLI**: Pass `--editor` (or `-e`) to `tes switch`:

```bash:no-line-numbers
tes switch feat/pricing --editor
```

Switching the editor branch updates your repository working tree and `.git/HEAD` while keeping any open terminal tabs pinned to their own branches.

## Assigning an Agent to a Branch

To have an AI agent build a feature on a specific branch without switching your editor away from `main`, name the branch in your prompt:

> *"On branch `feat/faq`, create `src/components/TestimonialsFaq.vue` and add it to `src/App.vue`."*

The agent claims the branch automatically:

```bash:no-line-numbers
tes claim --branch feat/faq --intent "Add Testimonials and FAQ section" src/components/TestimonialsFaq.vue src/App.vue
```

To have an agent create a new branch stacked on top of an existing feature branch, specify the parent branch in your prompt (for example, *"Branch off `feat/pricing` into `feat/pricing-tiers`..."*). The agent passes `--from feat/pricing` when claiming or switching:

```bash:no-line-numbers
tes claim --branch feat/pricing-tiers --from feat/pricing --intent "Add enterprise pricing tier" src/components/PricingCalculator.vue
```

When the agent finishes and commits onto `feat/faq`, your `main` working tree remains clean.

## Listing Branches

View all branches in the repository and see which branch your current terminal session is using:

```bash:no-line-numbers
tes branch
```

Pass `--json` to inspect commit hashes, assigned ports, active sessions, and modified files ahead of `main`:

```bash:no-line-numbers
tes branch --json
```

## Merging Branches

Combine one or more feature branches into `main` (or any target branch) using `tes merge --into`:

```bash:no-line-numbers
# Merge a single branch into main
tes merge feat/pricing --into main -m "feat: add pricing calculator"

# Merge multiple branches into main at once
tes merge feat/pricing feat/faq --into main -m "feat: add pricing and faq"
```

During `tes merge`, Tessera merges unchanged directories without reading file blobs, detects renamed files across directories, renumbers colliding sequential SQL migrations, merges `package.json`, `Cargo.toml`, and `Cargo.lock` dependencies structurally, and combines concurrent AST changes with Tree-sitter.

## Pushing to Remotes

Commit any pending session changes and push a branch to your Git remote (`origin`):

```bash:no-line-numbers
tes push
tes push -u origin feat/pricing
```

## See Also

- [Sandboxes](/guides/running-tests-and-servers) - Running builds and dev servers across branches
- [Conflicts](/guides/conflict-resolution) - Handling conflicts and rolling back merges
- [Editor Support](/guides/ide-extension) - Switching and merging branches in VS Code
