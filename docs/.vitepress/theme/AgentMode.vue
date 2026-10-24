<script lang="ts">
import { ref } from 'vue'

/**
 * Shared reactive state across navbar switcher and document sheet instances.
 */
const isAgentMode = ref(false)
const activeTab = ref<'page' | 'agents' | 'llms' | 'llms-full'>('page')
const cachedBundles = ref<Record<string, string>>({})
const isLoadingBundle = ref(false)
let initialized = false
</script>

<script setup lang="ts">
import { computed, onMounted, watch } from 'vue'
import { useData, useRoute } from 'vitepress'

const props = defineProps<{
  slotMode: 'nav' | 'banner'
}>()

const { frontmatter } = useData()
const route = useRoute()
const copiedContent = ref(false)
const copiedPrompt = ref(false)

const agentBootstrapPrompt =
  'Read https://tessera.getlunaris.dev/agents.txt and https://tessera.getlunaris.dev/llms.txt to learn Tessera (tes) multi-agent source control, then use tes commands in this repository.'

/**
 * Synchronizes DOM classes, URL query parameter, and localStorage with the active Human | Agent view state.
 */
function syncDomState() {
  if (typeof document === 'undefined') {
    return
  }

  document.documentElement.classList.toggle('agent-mode', isAgentMode.value)

  try {
    localStorage.setItem('docs-agent-mode', isAgentMode.value ? '1' : '0')
  } catch {
  }

  if (typeof window !== 'undefined') {
    const url = new URL(window.location.href)

    if (isAgentMode.value) {
      url.searchParams.set('agent', '1')
    } else {
      url.searchParams.delete('agent')
      url.searchParams.delete('mode')
    }

    window.history.replaceState({}, '', url.toString())
  }
}

/**
 * Sets the active mode to Human (`false`) or Agent (`true`).
 */
function setAgentMode(nextMode: boolean) {
  if (isAgentMode.value === nextMode) {
    return
  }

  isAgentMode.value = nextMode
  syncDomState()
}

/**
 * Switches the active document tab inside Agent View and fetches static `.txt` bundles on demand.
 */
async function selectTab(tab: 'page' | 'agents' | 'llms' | 'llms-full') {
  activeTab.value = tab

  if (tab === 'agents' || tab === 'llms' || tab === 'llms-full') {
    const pathMap: Record<'agents' | 'llms' | 'llms-full', string> = {
      agents: '/agents.txt',
      llms: '/llms.txt',
      'llms-full': '/llms-full.txt'
    }
    const targetUrl = pathMap[tab]

    if (!cachedBundles.value[tab]) {
      isLoadingBundle.value = true

      try {
        const res = await fetch(targetUrl, { cache: 'no-store' })
        cachedBundles.value[tab] = await res.text()
      } catch {
        cachedBundles.value[tab] = `# Failed to load ${targetUrl}`
      } finally {
        isLoadingBundle.value = false
      }
    }
  }
}

/**
 * Decodes the current page's base64-encoded agent Markdown from VitePress frontmatter.
 */
const decodedPageMarkdown = computed(() => {
  const b64 = frontmatter.value.agentMarkdownB64 as string | undefined

  if (!b64) {
    return ''
  }

  try {
    if (typeof atob === 'function') {
      const bin = atob(b64)
      const bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0))
      return new TextDecoder().decode(bytes)
    }
  } catch {
    return ''
  }

  return ''
})

const currentRawText = computed(() => {
  if (activeTab.value === 'agents') {
    return cachedBundles.value.agents || 'Loading /agents.txt...'
  }

  if (activeTab.value === 'llms') {
    return cachedBundles.value.llms || 'Loading /llms.txt...'
  }

  if (activeTab.value === 'llms-full') {
    return cachedBundles.value['llms-full'] || 'Loading /llms-full.txt...'
  }

  return decodedPageMarkdown.value
})

const estimatedTokens = computed(() => {
  const text = currentRawText.value || ''
  return Math.max(1, Math.round(text.length / 4)).toLocaleString()
})

const activeSourceLabel = computed(() => {
  if (activeTab.value === 'agents') {
    return '/agents.txt'
  }

  if (activeTab.value === 'llms') {
    return '/llms.txt'
  }

  if (activeTab.value === 'llms-full') {
    return '/llms-full.txt'
  }

  return (frontmatter.value.sourcePath as string | undefined) || 'page.md'
})

const activeRawEndpoint = computed(() => {
  if (activeTab.value === 'agents') {
    return '/agents.txt'
  }

  if (activeTab.value === 'llms') {
    return '/llms.txt'
  }

  if (activeTab.value === 'llms-full') {
    return '/llms-full.txt'
  }

  return null
})

/**
 * Copies the active page Markdown or selected `.txt` bundle to the clipboard.
 */
async function copyCurrentContent() {
  const textToCopy = currentRawText.value

  if (!textToCopy || typeof navigator === 'undefined' || !navigator.clipboard) {
    return
  }

  await navigator.clipboard.writeText(textToCopy)
  copiedContent.value = true

  setTimeout(() => {
    copiedContent.value = false
  }, 1800)
}

/**
 * Copies the ready-to-use agent bootstrap prompt to the clipboard.
 */
async function copyAgentPrompt() {
  if (typeof navigator === 'undefined' || !navigator.clipboard) {
    return
  }

  await navigator.clipboard.writeText(agentBootstrapPrompt)
  copiedPrompt.value = true

  setTimeout(() => {
    copiedPrompt.value = false
  }, 1800)
}

onMounted(() => {
  if (!initialized && typeof window !== 'undefined') {
    initialized = true
    const params = new URLSearchParams(window.location.search)
    const fromQuery = params.get('agent') === '1' || params.get('mode') === 'agent'
    let fromStorage = false

    try {
      fromStorage = localStorage.getItem('docs-agent-mode') === '1'
    } catch {
      fromStorage = false
    }

    if (fromQuery || fromStorage) {
      isAgentMode.value = true
    }
  }

  syncDomState()
})

watch(
  () => route.path,
  () => {
    syncDomState()
  }
)
</script>

<template>
  <div v-if="props.slotMode === 'nav'" class="agent-nav-wrap">
    <div class="mode-switch" role="group" aria-label="Documentation view mode">
      <button
        type="button"
        class="mode-btn"
        :class="{ active: !isAgentMode }"
        :aria-pressed="!isAgentMode"
        title="Standard human documentation view"
        @click="setAgentMode(false)"
      >
        Human
      </button>
      <button
        type="button"
        class="mode-btn"
        :class="{ active: isAgentMode }"
        :aria-pressed="isAgentMode"
        title="Clean raw Markdown & LLM context view for AI coding agents"
        @click="setAgentMode(true)"
      >
        <span class="mode-prompt">&gt;_</span>
        <span>Agent</span>
      </button>
    </div>
  </div>

  <div v-else-if="props.slotMode === 'banner' && isAgentMode" class="agent-banner-container">
    <div class="agent-prompt-card">
      <div class="agent-prompt-top">
        <span class="agent-prompt-badge">PROMPT YOUR AGENT</span>
        <span class="agent-prompt-hint">
          Paste into Claude Code, Cursor, Antigravity, Codex, or Windsurf
        </span>
      </div>
      <div class="agent-prompt-row">
        <code class="agent-prompt-code">{{ agentBootstrapPrompt }}</code>
        <button type="button" class="agent-copy-prompt-btn" @click="copyAgentPrompt">
          {{ copiedPrompt ? '✓ Copied' : 'Copy Prompt' }}
        </button>
      </div>
    </div>

    <div class="agent-sheet">
      <div class="agent-bar">
        <div class="agent-bar-tabs" role="tablist" aria-label="Agent Markdown Sources">
          <button
            type="button"
            class="agent-tab"
            :class="{ active: activeTab === 'page' }"
            @click="selectTab('page')"
          >
            This Page (.md)
          </button>
          <button
            type="button"
            class="agent-tab"
            :class="{ active: activeTab === 'agents' }"
            @click="selectTab('agents')"
          >
            /agents.txt
          </button>
          <button
            type="button"
            class="agent-tab"
            :class="{ active: activeTab === 'llms' }"
            @click="selectTab('llms')"
          >
            /llms.txt
          </button>
          <button
            type="button"
            class="agent-tab"
            :class="{ active: activeTab === 'llms-full' }"
            @click="selectTab('llms-full')"
          >
            /llms-full.txt
          </button>
        </div>

        <div class="agent-bar-actions">
          <span class="agent-token-badge" title="Approximate LLM token count">
            ~{{ estimatedTokens }} tokens
          </span>
          <a
            v-if="activeRawEndpoint"
            :href="activeRawEndpoint"
            target="_blank"
            rel="noopener"
            class="agent-action-btn"
          >
            Raw ↗
          </a>
          <button type="button" class="agent-action-btn primary" @click="copyCurrentContent">
            {{ copiedContent ? '✓ Copied' : 'Copy Markdown' }}
          </button>
        </div>
      </div>

      <div class="agent-bar-sub">
        <span class="agent-source-path">{{ activeSourceLabel }}</span>
        <span class="agent-endpoints">
          Static endpoints:
          <a href="/agents.txt" target="_blank">/agents.txt</a> ·
          <a href="/llms.txt" target="_blank">/llms.txt</a> ·
          <a href="/llms-full.txt" target="_blank">/llms-full.txt</a>
        </span>
      </div>

      <pre class="agent-raw-pre"><code>{{ isLoadingBundle ? 'Loading...' : currentRawText }}</code></pre>
    </div>
  </div>
</template>
