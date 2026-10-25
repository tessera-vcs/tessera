/**
 * VitePress site configuration, navigation hierarchy, sidebar layout, and LLM/Agent text bundle generation.
 */

import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vitepress'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const docsRoot = path.resolve(__dirname, '..')

const HOW_TESSERA_WORKS_ASCII = [
  '```text',
  '[Workflow: Parallel Agents in One Repository]',
  '1. Prompt in One Repo (~/acme-web)',
  '   ├─ Agent 1: "Add Pricing"',
  '   └─ Agent 2: "Add FAQ"',
  '       │',
  '       ▼',
  '2. Own Branches (Isolated in .git/tessera while .git/HEAD stays on main)',
  '   ├─ feat/pricing (Agent 1 · +Pricing)',
  '   └─ feat/faq     (Agent 2 · +FAQ)',
  '       │',
  '       ▼',
  '3. Branch Dev Servers (Copy-on-Write node_modules / target)',
  '   ├─ http://localhost:3100 (feat/pricing)',
  '   └─ http://localhost:3200 (feat/faq)',
  '       │',
  '       ▼',
  '4. Merge into main (3-Way Tree-sitter AST Merge)',
  '   └─ tes merge feat/pricing feat/faq --into main -m "feat: add pricing and faq"',
  '```'
].join('\n')

const AGENT_WORKFLOW_ASCII = [
  '```text',
  '[Workflow: How Agents Coordinate Automatically]',
  '1. Claim Task        -> tes claim [--branch <branch>] [--from <base>] --intent "<goal>" <files...>',
  '2. Reconcile Edits   -> tes hook post-tool --path <files...> (3-way AST merge on save)',
  '3. Verify in Sandbox -> tes exec -- npm run build            (CoW sandbox on isolated port)',
  '4. Commit to Git     -> tes commit -m "..." / tes merge      (standard Git commit on branch)',
  '```'
].join('\n')

const SAME_BRANCH_AST_ASCII = [
  '```text',
  '[Workflow: Same-Branch AST Merging on main]',
  '1. Prompt on main   -> Agent 1 ("Add locale") & Agent 2 ("Add decimals") target src/currency.ts',
  '2. Concurrent Edits -> Both agents save directly on main (no file locks)',
  '3. Tree-sitter AST  -> Parses FunctionDecl -> merges ParamNode[1] (locale) + ParamNode[2] (decimals)',
  '4. Unified on Disk  -> Both edits preserved in src/currency.ts with zero overwrites',
  '```'
].join('\n')

const BRANCHES_AND_TERMINALS_ASCII = [
  '```text',
  '[Architecture: Editor, Terminal & Agent Branches in One Folder]',
  '- Editor Working Tree : .git/HEAD stays on main in VS Code and VS Code-based IDEs',
  '- Terminal Tabs       : tes switch feat/pricing (:3100) | tes switch feat/faq (:3200)',
  '- Agent Branches      : tes claim --branch isolates agent edits inside .git/tessera/',
  '- Switch or Merge     : tes switch <branch> --editor  OR  tes merge ... --into main',
  '```'
].join('\n')

const EDITOR_ATTRIBUTION_ASCII = [
  '```ts',
  '// Live Multi-Agent Attribution in src/currency.ts (main):',
  '// [CodeLens] 2 agents concurrently editing this file (main): antigravity-a1b2 · claude-c3d4',
  'export function formatCurrency(',
  '  cents: number,',
  '  // [antigravity-a1b2 · L3-L6: Add locale and decimals]',
  '  currency = "USD",',
  '  locale = "en-US",',
  '  decimals = 2',
  '): string {',
  '  const amount = (cents / 100).toFixed(decimals);',
  '  return `[${locale}] ${currency} ${amount}`;',
  '}',
  '// [claude-c3d4 · L10-L14: Add formatCompact helper]',
  'export function formatCompact(cents: number) {',
  '  return new Intl.NumberFormat("en-US", {',
  '    style: "currency", currency: "USD", notation: "compact"',
  '  }).format(cents / 100);',
  '}',
  '```'
].join('\n')

const HOME_HERO_MARKDOWN = [
  '# Tessera: Multi-Agent Coordination for Git',
  '',
  '> Run multiple agents in the same Git repository.',
  '',
  'Tessera coordinates concurrent AI agents and terminal sessions inside your existing Git repository. Sessions share one checkout, work on independent branches, and merge edits at the AST level.',
  '',
  EDITOR_ATTRIBUTION_ASCII
].join('\n')

const HOME_WORK_IN_PARALLEL_MARKDOWN = [
  '## Work in Parallel',
  '',
  'Run multiple agents on the same branch, split features across branches, or run two dev servers side by side from a single checkout.',
  '',
  '### 1. Multiple Agents',
  '',
  '```bash',
  '# Initialize Tessera and install AI editor rules',
  'tes init',
  '',
  '# Prompt two AI agents in parallel:',
  '# Tab 1: "On branch feat/pricing, build PricingCalculator.vue and add it to App.vue"',
  '# Tab 2: "On branch feat/faq, build TestimonialsFaq.vue and add it to App.vue"',
  '',
  '# Check live agent sessions and claimed files',
  'tes radar',
  '```',
  '',
  '### 2. Per-Terminal Branches',
  '',
  '```bash',
  '# Enable (tes:<branch>) in your shell prompt',
  'tes env --install',
  '',
  '# Terminal 1: run feat/pricing on port 3100',
  'tes switch feat/pricing',
  'npm run dev',
  '',
  '# Terminal 2: run feat/faq on port 3200',
  'tes switch feat/faq',
  'npm run dev',
  '```',
  '',
  '### 3. Branch Merging',
  '',
  '```bash',
  '# Inspect branch diffs against main',
  'tes diff -b feat/pricing',
  'tes diff -b feat/faq',
  '',
  '# Merge both branches into main using 3-way AST merge',
  'tes merge feat/pricing feat/faq --into main -m "feat: add pricing and faq"',
  '',
  '# Push to remote',
  'tes push',
  '```'
].join('\n')

/**
 * Converts a documentation page's raw Markdown into a clean, agent-optimized Markdown document
 * with decorative HTML/Vue components replaced by structured ASCII/Markdown blocks.
 */
function toAgentMarkdown(rawMd: string, relativePath: string): string {
  let content = rawMd.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n*/, '')

  content = content.replace(/<HowTesseraWorks\s*\/>/g, HOW_TESSERA_WORKS_ASCII)
  content = content.replace(/<AgentWorkflowIllustration\s*\/>/g, AGENT_WORKFLOW_ASCII)
  content = content.replace(/<SameBranchAstIllustration\s*\/>/g, SAME_BRANCH_AST_ASCII)
  content = content.replace(/<BranchesAndTerminalsIllustration\s*\/>/g, BRANCHES_AND_TERMINALS_ASCII)
  content = content.replace(/```([a-zA-Z0-9_-]+):no-line-numbers/g, '```$1')

  if (relativePath === 'index.md') {
    content = content.replace(/<div class="hero-section">[\s\S]*?(?=\r?\n## Why Tessera\?)/, HOME_HERO_MARKDOWN + '\n')
    content = content.replace(/<div class="build-anything-section">[\s\S]*$/, HOME_WORK_IN_PARALLEL_MARKDOWN)
  } else {
    content = content.replace(/<EditorPreview\s*\/>/g, EDITOR_ATTRIBUTION_ASCII)
  }

  return content.trim()
}

const ORDERED_DOC_PATHS = [
  'index.md',
  'getting-started/index.md',
  'getting-started/why-tessera.md',
  'getting-started/installation.md',
  'getting-started/first-workflow.md',
  'guides/index.md',
  'guides/multi-agent-workflow.md',
  'guides/branches-and-terminals.md',
  'guides/running-tests-and-servers.md',
  'guides/conflict-resolution.md',
  'guides/ide-extension.md',
  'guides/configuration.md',
  'examples/index.md',
  'examples/same-branch.md',
  'examples/parallel-branches.md',
  'examples/conflict-resolution.md',
  'cli/index.md',
  'cli/init.md',
  'cli/env.md',
  'cli/switch.md',
  'cli/branch.md',
  'cli/claim.md',
  'cli/status.md',
  'cli/diff.md',
  'cli/stash.md',
  'cli/radar.md',
  'cli/exec.md',
  'cli/merge.md',
  'cli/cherry-pick.md',
  'cli/commit.md',
  'cli/pull.md',
  'cli/push.md',
  'cli/log.md',
  'cli/conflicts.md',
  'cli/resolve.md',
  'cli/checkpoint.md',
  'cli/restore.md',
  'cli/undo.md',
  'cli/config.md'
]

/**
 * Generates `docs/public/llms-full.txt` by concatenating all documentation pages in reading order.
 */
function generateLlmsFullTxt(): void {
  const sections: string[] = [
    '# Tessera: Complete Documentation (llms-full.txt)',
    '',
    '> URL: https://tessera.getlunaris.dev/llms-full.txt | Index: https://tessera.getlunaris.dev/llms.txt | Agent Protocol: https://tessera.getlunaris.dev/agents.txt'
  ]

  for (const relPath of ORDERED_DOC_PATHS) {
    const absPath = path.join(docsRoot, relPath)

    if (!fs.existsSync(absPath)) {
      continue
    }

    const raw = fs.readFileSync(absPath, 'utf-8')
    const cleaned = toAgentMarkdown(raw, relPath)

    sections.push('')
    sections.push('---')
    sections.push('')
    sections.push(cleaned)
  }

  sections.push('')

  const outPath = path.join(docsRoot, 'public', 'llms-full.txt')
  const nextContent = sections.join('\n')
  const existing = fs.existsSync(outPath) ? fs.readFileSync(outPath, 'utf-8') : ''

  if (existing !== nextContent) {
    fs.writeFileSync(outPath, nextContent, 'utf-8')
  }
}

generateLlmsFullTxt()

export default defineConfig({
  title: 'TESSERA',
  titleTemplate: ':title | Tessera',
  description: 'Run multiple AI agents and terminal sessions in the same Git repository',
  cleanUrls: true,
  vite: {
    plugins: [
      {
        name: 'raw-static-files',
        configureServer(server) {
          server.middlewares.use((req, res, next) => {
            if (!req.url || (req.method !== 'GET' && req.method !== 'HEAD')) {
              next()
              return
            }

            const pathname = decodeURIComponent(req.url.split('?')[0])

            if (/\.(txt|sh|ps1)$/i.test(pathname)) {
              const publicFile = path.join(docsRoot, 'public', pathname.replace(/^\/+/, ''))

              if (fs.existsSync(publicFile) && fs.statSync(publicFile).isFile()) {
                const body = fs.readFileSync(publicFile, 'utf-8')
                res.statusCode = 200
                res.setHeader('Content-Type', 'text/plain; charset=utf-8')
                res.setHeader('Cache-Control', 'no-cache')
                res.end(req.method === 'HEAD' ? undefined : body)
                return
              }
            }

            next()
          })
        }
      }
    ]
  },
  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg' }],
    ['meta', { name: 'apple-mobile-web-app-title', content: 'Tessera' }],
    ['link', { rel: 'alternate', type: 'text/plain', href: '/llms.txt', title: 'LLM Documentation Index' }],
    ['link', { rel: 'alternate', type: 'text/plain', href: '/llms-full.txt', title: 'LLM Full Documentation' }],
    ['link', { rel: 'alternate', type: 'text/plain', href: '/agents.txt', title: 'Autonomous Agent Protocol' }],
    ['link', { rel: 'preconnect', href: 'https://fonts.googleapis.com' }],
    ['link', { rel: 'preconnect', href: 'https://fonts.gstatic.com', crossorigin: '' }],
    [
      'link',
      {
        rel: 'stylesheet',
        href: 'https://fonts.googleapis.com/css2?family=Caveat:wght@600;700&family=Kalam:wght@400;700&family=Patrick+Hand&display=swap'
      }
    ]
  ],

  transformPageData(pageData) {
    const absPath = path.join(docsRoot, pageData.relativePath)

    if (fs.existsSync(absPath)) {
      const raw = fs.readFileSync(absPath, 'utf-8')
      const cleaned = toAgentMarkdown(raw, pageData.relativePath)
      pageData.frontmatter.agentMarkdownB64 = Buffer.from(cleaned, 'utf-8').toString('base64')
      pageData.frontmatter.sourcePath = `docs/${pageData.relativePath}`
    }
  },

  buildEnd(siteConfig) {
    for (const relPath of [...ORDERED_DOC_PATHS, 'tldr.md']) {
      const absPath = path.join(docsRoot, relPath)

      if (!fs.existsSync(absPath)) {
        continue
      }

      const raw = fs.readFileSync(absPath, 'utf-8')
      const cleaned = toAgentMarkdown(raw, relPath) + '\n'
      const targetFile = path.join(siteConfig.outDir, relPath)

      fs.mkdirSync(path.dirname(targetFile), { recursive: true })
      fs.writeFileSync(targetFile, cleaned, 'utf-8')

      if (relPath.endsWith('/index.md')) {
        const flatMd = path.join(siteConfig.outDir, relPath.replace(/\/index\.md$/, '.md'))
        fs.writeFileSync(flatMd, cleaned, 'utf-8')
      }
    }
  },

  markdown: {
    theme: {
      dark: 'dark-plus',
      light: 'light-plus'
    },
    lineNumbers: true
  },

  themeConfig: {
    logo: '/branding/tessera-icon.svg',
    nav: [
      { text: 'Home', link: '/' },
      { text: 'TL;DR', link: '/tldr' },
      {
        text: 'Documentation',
        items: [
          { text: 'Why Tessera?', link: '/getting-started/why-tessera' },
          { text: 'Getting Started', link: '/getting-started/' },
          { text: 'Guides', link: '/guides/' },
          { text: 'Examples', link: '/examples/' },
          { text: 'CLI Commands', link: '/cli/' }
        ]
      }
    ],

    sidebar: {
      '/getting-started/': [
        {
          text: 'Getting Started',
          items: [
            { text: 'Introduction', link: '/getting-started/' },
            { text: 'Why Tessera?', link: '/getting-started/why-tessera' },
            { text: 'Installation', link: '/getting-started/installation' },
            { text: 'Your First Workflow', link: '/getting-started/first-workflow' }
          ]
        }
      ],
      '/guides/': [
        {
          text: 'Guides',
          items: [
            { text: 'Overview', link: '/guides/' },
            { text: 'Configuration', link: '/guides/configuration' },
            { text: 'Editor Support', link: '/guides/ide-extension' },
            { text: 'Agents', link: '/guides/multi-agent-workflow' },
            { text: 'Branches', link: '/guides/branches-and-terminals' },
            { text: 'Sandboxes', link: '/guides/running-tests-and-servers' },
            { text: 'Conflicts', link: '/guides/conflict-resolution' }
          ]
        }
      ],
      '/cli/': [
        {
          text: 'CLI Commands',
          items: [
            { text: 'Overview', link: '/cli/' },
            { text: 'tes init', link: '/cli/init' },
            { text: 'tes env', link: '/cli/env' },
            { text: 'tes switch', link: '/cli/switch' },
            { text: 'tes branch', link: '/cli/branch' },
            { text: 'tes claim', link: '/cli/claim' },
            { text: 'tes status', link: '/cli/status' },
            { text: 'tes diff', link: '/cli/diff' },
            { text: 'tes stash', link: '/cli/stash' },
            { text: 'tes radar', link: '/cli/radar' },
            { text: 'tes exec', link: '/cli/exec' },
            { text: 'tes merge', link: '/cli/merge' },
            { text: 'tes cherry-pick', link: '/cli/cherry-pick' },
            { text: 'tes commit', link: '/cli/commit' },
            { text: 'tes pull', link: '/cli/pull' },
            { text: 'tes push', link: '/cli/push' },
            { text: 'tes log', link: '/cli/log' },
            { text: 'tes conflicts', link: '/cli/conflicts' },
            { text: 'tes resolve', link: '/cli/resolve' },
            { text: 'tes checkpoint', link: '/cli/checkpoint' },
            { text: 'tes restore', link: '/cli/restore' },
            { text: 'tes undo', link: '/cli/undo' },
            { text: 'tes config', link: '/cli/config' }
          ]
        }
      ],
      '/examples/': [
        {
          text: 'Examples',
          items: [
            { text: 'Overview', link: '/examples/' },
            { text: 'Same-Branch Editing', link: '/examples/same-branch' },
            { text: 'Feature Branches', link: '/examples/parallel-branches' },
            { text: 'Conflict Resolution', link: '/examples/conflict-resolution' }
          ]
        }
      ]
    },

    footer: {
      message:
        '<a href="/agents.txt" target="_blank">agents.txt</a> · <a href="/llms.txt" target="_blank">llms.txt</a> · <a href="/llms-full.txt" target="_blank">llms-full.txt</a> · Released under the MIT License.',
      copyright: 'Copyright © 2026 Donald Pakkies'
    },

    search: {
      provider: 'local'
    }
  }
})
