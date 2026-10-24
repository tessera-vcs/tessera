---
aside: false
outline: false
pageClass: why-tessera
---

# Why Tessera?

Git assumes one person is editing one branch in one folder at a time. Every terminal tab and editor window in your project reads from the same `.git/HEAD` and writes to the same working tree on disk.

That works fine when you write code by hand in a single editor, but it breaks down when you run two or three AI coding agents in parallel.

If two agents edit `src/App.vue` at the same time, the second agent to save overwrites the first agent's lines, or sees unfamiliar code and reverts it. If one agent runs `git checkout feat/pricing` to test its work, it switches the branch underneath your editor, your running dev server, and every other agent in the folder.

The usual workaround is `git worktree`: creating a separate copy of the repository in another folder for every agent. That requires duplicating `node_modules` or `target` directories, waiting on fresh builds, configuring different ports by hand so dev servers do not collide on `localhost:3000`, and jumping between separate editor windows.

## One Folder, Multiple Branches and Dev Servers

Tessera lets multiple agents and terminal tabs work in the **same repository folder** at the same time, either on separate branches or on the same branch.

<HowTesseraWorks />

When an agent claims a branch like `feat/pricing`, Tessera isolates that session inside `.git/tessera/` without changing `.git/HEAD` for the rest of your workspace. Both agents can edit `src/App.vue` on their own branches in `~/acme-web` at the same time without touching each other's uncommitted files.

When you want to preview or test a branch, `tes exec -b feat/pricing -- npm run dev` starts the dev server inside a copy-on-write sandbox. It clones your existing `node_modules`, `target`, or `.venv` directory in milliseconds using filesystem reflinks and assigns each branch its own port (`localhost:4100`, `localhost:4200`) so you can open both branches side by side in your browser.

## Merging by Syntax Tree

Line-based Git merges fail whenever two branches touch adjacent lines, like when two agents add imports at the top of `src/App.vue` or register two new components in the same template.

When you run `tes merge feat/pricing feat/faq --into main`, Tessera parses each file into a syntax tree with Tree-sitter and merges functions, imports, JSX/Vue templates, and lockfiles structurally. Compatible edits merge without manual conflict resolution.

## Editing on the Same Branch

You do not always want separate branches. When two agents work directly on `main` at the same time, Tessera runs a 3-way AST merge on disk every time an agent saves a file.

<SameBranchAstIllustration />

If one agent updates `formatCurrency()` while another adds `formatCompact()` in `src/currency.ts`, both edits are preserved on disk. In VS Code and Cursor, the Tessera extension highlights each agent's lines so you can see which session owns each change.

## Built on Top of Git, Not a Replacement

Tessera is not a replacement for Git and does not use a hosted server. Instead, it gives each AI agent and terminal session its own isolated state in `.git/tessera/` so concurrent sessions can use Git branches, diffs, and commits safely in the same working directory.

Every `tes commit` or `tes merge` writes standard Git blobs, trees, and commits directly into `.git/objects/` and `refs/heads/*`. Your existing `git push`, GitHub pull requests, and CI pipelines work without changes.

---

[Install Tessera →](/getting-started/installation)
