---
pageClass: home
aside: false
---

<div class="hero-section">
  <div class="hero-content">
  <img src="/branding/tessera-logo-wordmark.svg" alt="Tessera" class="logo hero-wordmark" style="max-width: 450px; width: 100%; height: auto; display: block; margin: 0 auto 2rem;" />

  <p style="font-size: 2rem; font-weight: 500; margin-bottom: 1rem; line-height: 1.3;">
    Run multiple agents in the<br>same Git repository.
  </p>

  <p style="font-size: 1.2rem; opacity: 0.85; margin-bottom: 2.5rem; line-height: 1.6;">
    Tessera works on top of your existing Git repository so concurrent AI agents and terminal sessions can share one checkout, work on independent branches, and merge edits structurally without lockfile collisions, port conflicts, or overwritten files.
  </p>

  <div style="display: flex; gap: 1rem; justify-content: center; flex-wrap: wrap;">
    <a href="/getting-started/" style="display: inline-block; padding: 0.75rem 2rem; background: #000; color: #fff; text-decoration: none; border-radius: 6px; font-weight: 600; width: 180px; align-content: center;">Get Started</a>
    <a href="/guides/" style="display: inline-block; padding: 0.75rem 2rem; background: transparent; text-decoration: none; border-radius: 6px; font-weight: 600; border: 1px solid currentColor; width: 180px; align-content: center;">Documentation</a>
  </div>
  </div>

  <EditorPreview />
</div>

## Why Tessera?

In a standard Git checkout, every open terminal and AI agent shares a single `.git/HEAD` and working directory on disk. Running multiple agents at once overwrites uncommitted files, switches branches underneath active processes, and collides on local ports and databases. Tessera coordinates sessions inside `.git/tessera` and writes standard Git commits so you stay in one folder while each session owns its own branch, sandbox environment, and structurally merged changes. [Read Why Tessera? →](/getting-started/why-tessera)

<HowTesseraWorks />

### **Semantic Merging**
Line-based Git merges break when two agents edit the same file. Tessera parses your code with Tree-sitter to merge functions, imports, class methods, and manifest files structurally.

```ts
// Agent 1 adds `locale`, Agent 2 adds `decimals` to the same signature:
export function formatCurrency(
  cents: number,
  currency: string = "USD",
  locale: string = "en-US",
  decimals: number = 2
): string {
  const amount = (cents / 100).toFixed(decimals);
  return `[${locale}] ${currency} ${amount}`;
}
```

### **Session Isolation**
Each terminal tab and agent session can switch to its own branch without changing `.git/HEAD` for the rest of your workspace. Edits on the same branch combine automatically on disk, while edits on separate branches stay completely isolated.

```bash:no-line-numbers
# Terminal 1 works on feat/pricing
tes switch feat/pricing

# Terminal 2 works on feat/faq at the same time
tes switch feat/faq
```

### **Instant Sandboxes**
Run builds, tests, and dev servers on any branch without reinstalling dependencies. Tessera clones `node_modules`, `target`, and `.venv` using filesystem copy-on-write and assigns each session its own port and test database.

```bash:no-line-numbers
# Run builds or dev servers in isolated Copy-on-Write branch sandboxes
tes exec -b feat/pricing -- npm run build
tes exec -b feat/faq -- npm test
```

<div class="build-anything-section">

<div class="build-header" style="border-top: 1px solid var(--vp-c-gutter);">
<h3 class="build-heading">Work in Parallel</h3>

<p class="build-intro">
Run multiple agents on the same branch, split features across branches, or run two dev servers side by side from a single checkout.
</p>
</div>

<div class="build-container">

<div class="build-tabs">
<button class="build-tab active" data-tab="agents">
<strong>Multiple Agents</strong>
<span>Point two or more agents at the same branch or separate branches without manual coordination.</span>
</button>
<button class="build-tab" data-tab="terminals">
<strong>Per-Terminal Branches</strong>
<span>Switch branches in a terminal tab and run a dev server or test suite without affecting your editor.</span>
</button>
<button class="build-tab" data-tab="merge">
<strong>Branch Merging</strong>
<span>Merge multiple feature branches into main in a single command.</span>
</button>
</div>

<div class="build-code-window">

<div class="build-code-content active" data-content="agents">

```bash:no-line-numbers
# Initialize Tessera and install AI editor rules
tes init

# Prompt two AI agents in parallel:
# Tab 1: "On branch feat/pricing, build PricingCalculator.vue and add it to App.vue"
# Tab 2: "On branch feat/faq, build TestimonialsFaq.vue and add it to App.vue"

# Check live agent sessions and claimed files
tes radar
```

</div>

<div class="build-code-content" data-content="terminals">

```bash:no-line-numbers
# Enable (tes:<branch>) in your shell prompt
tes env --install

# Terminal 1: run feat/pricing on port 3100
tes switch feat/pricing
npm run dev

# Terminal 2: run feat/faq on port 3200
tes switch feat/faq
npm run dev
```

</div>

<div class="build-code-content" data-content="merge">

```bash:no-line-numbers
# Inspect branch diffs against main
tes diff -b feat/pricing
tes diff -b feat/faq

# Merge both branches into main using 3-way AST merge
tes merge feat/pricing feat/faq --into main -m "feat: add pricing and faq"

# Push to remote
tes push
```

</div>

</div>
<div class="build-code-mobile">

<div class="build-code-content active" data-content="agents">

```bash:no-line-numbers
# Initialize Tessera and install AI editor rules
tes init

# Prompt two AI agents in parallel:
# Tab 1: "On branch feat/pricing, build PricingCalculator.vue and add it to App.vue"
# Tab 2: "On branch feat/faq, build TestimonialsFaq.vue and add it to App.vue"

# Check live agent sessions and claimed files
tes radar
```

</div>

<div class="build-code-content" data-content="terminals">

```bash:no-line-numbers
# Enable (tes:<branch>) in your shell prompt
tes env --install

# Terminal 1: run feat/pricing on port 3100
tes switch feat/pricing
npm run dev

# Terminal 2: run feat/faq on port 3200
tes switch feat/faq
npm run dev
```

</div>

<div class="build-code-content" data-content="merge">

```bash:no-line-numbers
# Inspect branch diffs against main
tes diff -b feat/pricing
tes diff -b feat/faq

# Merge both branches into main using 3-way AST merge
tes merge feat/pricing feat/faq --into main -m "feat: add pricing and faq"

# Push to remote
tes push
```

</div>

</div>

</div>

</div>
