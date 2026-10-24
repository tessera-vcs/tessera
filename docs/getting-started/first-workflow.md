# Your First Workflow

In this guide, you will initialize Tessera in a project, work across two feature branches in parallel, and merge both branches back into `main`.

<HowTesseraWorks />

## 1. Initialize the Repository

Start by running `tes init` in your project root:

```bash:no-line-numbers
tes init
```

If you have not already enabled shell prompt integration, run:

```bash:no-line-numbers
tes env --install
```

## 2. Work on Two Branches in Parallel

You can work on multiple branches at the same time by prompting two AI agent tabs or opening two terminal tabs.

### Option A: Prompt Two AI Agents

Open two AI agent chat tabs in your editor and give each one a task on its own branch:

- **Tab 1**: *"On branch `feat/pricing`, create `src/components/PricingCalculator.vue`, add it to `src/App.vue`, and verify with `npm run build`."*
- **Tab 2**: *"On branch `feat/faq`, create `src/components/TestimonialsFaq.vue`, add it to `src/App.vue`, and verify with `npm run build`."*

### Option B: Use Two Terminal Tabs

Or open two terminal tabs in the same repository and switch each terminal to its own branch:

**Terminal 1:**

```bash:no-line-numbers
tes switch feat/pricing
```

**Terminal 2:**

```bash:no-line-numbers
tes switch feat/faq
```

> **Tip:** Each terminal tab remembers its own branch without changing `.git/HEAD` for your editor or other terminals.

## 3. Inspect Live Sessions and Changes

At any point, you can view active agent sessions, modified files, and diffs:

```bash:no-line-numbers
# See all active agent and terminal sessions
tes radar

# List all branches in the repository
tes branch

# View modified files in the current session or branch
tes status

# Compare a specific branch against main
tes diff -b feat/pricing
tes diff -b feat/faq
```

### Command Summary

**`tes radar`**
- Shows every active AI agent or terminal session
- Displays each session's target branch, assigned port (`:4100`, `:4200`), and claimed files

**`tes status`**
- **Staged / Session Changes**: Files recorded in the current session (`M`, `A`, `D`)
- **Unstaged Changes**: Working tree edits on disk not yet reconciled
- **Committed on Branch**: Files committed on the feature branch ahead of `main`

**`tes diff -b <branch>`**
- Shows a unified diff comparing `<branch>` against `main` without switching your editor branch

## 4. Merge Both Branches into `main`

When both features are ready, combine them into `main` with a single command:

```bash:no-line-numbers
tes merge feat/pricing feat/faq --into main -m "feat: add pricing calculator and faq"
```

Even if both `feat/pricing` and `feat/faq` added imports and components to `src/App.vue`, Tessera's 3-way AST merge engine combines both sets of imports and template blocks without conflict markers.

## Key Concepts

### 1. Per-Session Branches

In standard Git, `git checkout feat/pricing` switches the entire folder for every open terminal and AI agent. In Tessera:

- `tes switch feat/pricing` binds only the **current terminal session** to `feat/pricing`.
- `tes claim --branch feat/pricing ...` binds only that **AI agent session** to `feat/pricing`.
- Clicking the branch name in the IDE status bar (or running `tes switch feat/pricing --editor`) switches your **editor working tree** (`repo_root`) so your file explorer displays `feat/pricing`.

### 2. Branch Sandboxes

When you run commands in a switched terminal or via `tes exec -b <branch> -- <cmd>`, Tessera runs the command inside a Copy-on-Write sandbox for that branch:

```bash:no-line-numbers
tes exec -b feat/pricing -- npm run build
tes exec -b feat/faq -- npm run build
```

Both commands reuse your existing `node_modules` (or `target` / `.venv`).

### 3. Checkpoints and Undo

Every operation in Tessera is recorded in an operation log inside `.git/tessera`. If an agent makes an unwanted change or you want to roll back a merge, run:

```bash:no-line-numbers
tes undo
```

## Next Steps

- [Agents](/guides/multi-agent-workflow) - How same-branch and cross-branch sessions coordinate
- [Branches](/guides/branches-and-terminals) - Managing terminal branches, editor branches, and merges
- [Editor Support](/guides/ide-extension) - Using the branch graph and status bar switcher in VS Code
