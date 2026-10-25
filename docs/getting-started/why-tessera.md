---
aside: false
outline: false
pageClass: why-tessera
---

# Why Tessera?

A Git checkout has a single `.git/HEAD` pointer and a single working tree on disk. Every terminal tab, background process, and AI coding agent opened in a project directory reads and writes that shared state:

1. **Working tree and branch overwrites**: If two agents edit `src/App.vue` concurrently, the second save overwrites the first agent's lines, or an agent sees unfamiliar edits on disk and reverts them. Switching branches to test a task changes `.git/HEAD` underneath your editor, running dev servers, and every other open terminal.
2. **Line-based merge conflicts**: Concurrent tasks frequently modify adjacent lines in shared files, such as adding imports at the top of a module, registering routes or components, or updating dependencies in `package.json` and lockfiles. Line-based diffing flags these compatible additions as conflicts.
3. **Port and cache collisions**: Running test suites or dev servers for two tasks at the same time collides on hardcoded ports (`localhost:3000`), build caches (`target/`, `node_modules/`), and local test databases.

## One Folder, Isolated Sessions

Tessera lets multiple AI agents and terminal tabs work in the **same repository folder** at the same time, either on separate branches or on the same branch.

<HowTesseraWorks />

When an agent claims a branch like `feat/pricing`, Tessera isolates that session inside `.git/tessera/` without changing `.git/HEAD` for the rest of your workspace. Concurrent agents can edit `src/App.vue` on their own branches in `~/acme-web` at the same time without touching each other's uncommitted files.

When you want to build, test, or preview a branch, `tes exec -b feat/pricing -- npm run dev` executes inside a Copy-on-Write sandbox. It clones your existing `node_modules`, `target`, or `.venv` directories (including monorepo workspace symlinks), preserves your untracked `.env*` secrets and `.gitignore`d local files (such as local SQLite databases or `.dev.vars`), and assigns each branch its own port (`localhost:3100`, `localhost:3200`) and local database slot so you can run multiple branches side by side.

## Structural Merging Across Code, Configs, and Lockfiles

When you run `tes merge feat/pricing feat/faq --into main`, Tessera merges changes at the syntax-tree level rather than line by line:

- **Source code**: Parses modules with Tree-sitter to merge functions, class methods, imports, and JSX/Vue templates.
- **Manifests and lockfiles**: Merges JSON, TOML, `Cargo.lock`, `pnpm-lock.yaml`, `yarn.lock`, and `poetry.lock` blocks by key and stanza.
- **Universal block structure**: Recognizes braced blocks (`{ ... }`), HTML/XML tags, indentation-scoped blocks (YAML, Python), keyword-delimited blocks (`def ... end`, `if ... fi`), and `[section]` stanzas so compatible additions merge across any language or configuration format.

## Editing on the Same Branch

When two agents work directly on `main` at the same time, Tessera runs a 3-way structural merge on disk whenever either agent saves a file.

<SameBranchAstIllustration />

If one agent updates `formatCurrency()` while another adds `formatCompact()` in `src/currency.ts`, both edits are preserved on disk. In VS Code and Cursor, the Tessera extension highlights each agent's lines so you can see which session owns each change.

## Git Compatibility

Tessera stores session state locally in `.git/tessera/` with no external server. Every `tes commit` or `tes merge` writes standard Git blobs, trees, and commits directly into `.git/objects/` and `refs/heads/*`, so `git push`, GitHub pull requests, and CI pipelines work without changes.

---

[Install Tessera →](/getting-started/installation)
