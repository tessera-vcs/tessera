/**
 * Custom VitePress theme extension registering interactive components, Agent Mode, and tab switchers.
 */

import { h, onMounted, watch, nextTick } from 'vue'
import { useRoute, type EnhanceAppContext } from 'vitepress'
import DefaultTheme from 'vitepress/theme'
import HowTesseraWorks from './HowTesseraWorks.vue'
import AgentWorkflowIllustration from './AgentWorkflowIllustration.vue'
import SameBranchAstIllustration from './SameBranchAstIllustration.vue'
import BranchesAndTerminalsIllustration from './BranchesAndTerminalsIllustration.vue'
import EditorPreview from './EditorPreview.vue'
import AgentMode from './AgentMode.vue'
import './branding.css'

export default {
  ...DefaultTheme,
  /**
   * Extends the default VitePress layout with the "I'm an agent" navbar toggle and top Agent Mode banner.
   */
  Layout() {
    return h(DefaultTheme.Layout, null, {
      'nav-bar-content-after': () => h(AgentMode, { slotMode: 'nav' }),
      'nav-screen-content-after': () => h(AgentMode, { slotMode: 'nav' }),
      'doc-before': () => h(AgentMode, { slotMode: 'banner' })
    })
  },
  /**
   * Registers global Vue components and intercepts raw file navigations before the SPA router handles them.
   */
  enhanceApp(ctx: EnhanceAppContext) {
    DefaultTheme.enhanceApp?.(ctx)
    ctx.app.component('HowTesseraWorks', HowTesseraWorks)
    ctx.app.component('AgentWorkflowIllustration', AgentWorkflowIllustration)
    ctx.app.component('SameBranchAstIllustration', SameBranchAstIllustration)
    ctx.app.component('BranchesAndTerminalsIllustration', BranchesAndTerminalsIllustration)
    ctx.app.component('EditorPreview', EditorPreview)
    ctx.app.component('AgentMode', AgentMode)

    if (typeof window !== 'undefined') {
      const prevBeforeRouteChange = ctx.router.onBeforeRouteChange

      ctx.router.onBeforeRouteChange = async (to: string) => {
        const parsed = new URL(to, window.location.origin)

        if (/\.(txt|sh|ps1)$/i.test(parsed.pathname)) {
          window.location.assign(parsed.href)
          return false
        }

        if (prevBeforeRouteChange) {
          return await prevBeforeRouteChange(to)
        }

        return true
      }
    }
  },
  /**
   * Initializes interactive code-tab switchers and re-binds them on route changes.
   */
  setup() {
    const route = useRoute()

    /**
     * Attaches click handlers to `.build-tab` elements to toggle `.build-code-content` panels.
     */
    const setupTabs = () => {
      if (typeof document === 'undefined') {
        return
      }

      const tabs = document.querySelectorAll<HTMLElement>('.build-tab')
      const contents = document.querySelectorAll<HTMLElement>('.build-code-content')

      if (tabs.length === 0) {
        return
      }

      tabs.forEach((tab) => {
        tab.addEventListener('click', () => {
          const targetTab = tab.dataset.tab

          tabs.forEach((t) => t.classList.remove('active'))
          tab.classList.add('active')

          contents.forEach((c) => {
            if (c.dataset.content === targetTab) {
              c.classList.add('active')
            } else {
              c.classList.remove('active')
            }
          })
        })
      })
    }

    onMounted(() => {
      setupTabs()

      watch(
        () => route.path,
        () => {
          void nextTick(() => setupTabs())
        }
      )
    })
  }
}
