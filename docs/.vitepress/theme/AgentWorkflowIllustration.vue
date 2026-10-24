<script setup lang="ts">
import { ref, onMounted, onUnmounted } from 'vue'
import { useData } from 'vitepress'

const { isDark } = useData()
const activeStep = ref(0)
const isPaused = ref(false)
let timer: ReturnType<typeof setInterval> | null = null

/**
 * Selects the active step panel in the autonomous agent workflow diagram.
 */
function setStep(step: number) {
  activeStep.value = step
}

onMounted(() => {
  timer = setInterval(() => {
    if (!isPaused.value) {
      activeStep.value = (activeStep.value + 1) % 4
    }
  }, 3200)
})

onUnmounted(() => {
  if (timer) {
    clearInterval(timer)
  }
})
</script>

<template>
  <div
    class="fig-wrapper"
    :class="{ 'is-dark': isDark }"
    @mouseenter="isPaused = true"
    @mouseleave="isPaused = false"
  >
    <svg width="0" height="0" style="position: absolute; pointer-events: none;" aria-hidden="true">
      <defs>
        <filter id="ink-sketch-agent" x="-5%" y="-5%" width="110%" height="110%">
          <feTurbulence type="fractalNoise" baseFrequency="0.035" numOctaves="2" result="noise" seed="5" />
          <feDisplacementMap in="SourceGraphic" in2="noise" scale="1.25" xChannelSelector="R" yChannelSelector="G" />
        </filter>
      </defs>
    </svg>

    <div class="fig-card">
      <div class="fig-title">How Agents Coordinate Automatically</div>

      <div class="fig-row">
        <div
          class="fig-panel"
          :class="{ active: activeStep === 0 }"
          @mouseenter="setStep(0)"
          @click="setStep(0)"
        >
          <div class="fig-panel-header">1. Claim Task</div>
          <div class="fig-scene">
            <div class="fig-bubble bubble-blue" style="top: 7px; left: 7px;">
              tes claim --branch
            </div>

            <svg viewBox="0 0 220 138" class="fig-svg">
              <g transform="translate(10, 34)" filter="url(#ink-sketch-agent)">
                <line x1="16" y1="2" x2="16" y2="7" stroke="var(--accent-blue)" stroke-width="1.6" stroke-linecap="round" />
                <circle cx="16" cy="2" r="2" fill="var(--accent-blue)" />
                <rect x="4" y="7" width="24" height="21" rx="5" fill="var(--svg-blue-card-bg)" stroke="var(--accent-blue)" stroke-width="1.6" />
                <circle cx="11.5" cy="15.5" r="2" fill="var(--accent-blue)" />
                <circle cx="20.5" cy="15.5" r="2" fill="var(--accent-blue)" />
                <path d="M 11 21.5 Q 16 24.5, 21 21.5" fill="none" stroke="var(--accent-blue)" stroke-width="1.4" stroke-linecap="round" />
                <rect x="34" y="3" width="166" height="29" rx="6" fill="var(--svg-blue-card-bg)" stroke="var(--accent-blue)" stroke-width="1.5" />
              </g>
              <g transform="translate(44, 37)">
                <circle cx="10" cy="14.5" r="3.8" fill="var(--accent-blue)" class="svg-pulse-dot" />
                <text x="19" y="13" class="svg-mono-branch" fill="var(--svg-blue-text-bold)">Agent 1 → feat/pricing</text>
                <text x="19" y="24" class="svg-mono-sub" fill="var(--svg-blue-text)">Pricing.vue · App.vue</text>
              </g>

              <g transform="translate(10, 74)" filter="url(#ink-sketch-agent)">
                <line x1="16" y1="2" x2="16" y2="7" stroke="var(--accent-purple)" stroke-width="1.6" stroke-linecap="round" />
                <circle cx="16" cy="2" r="2" fill="var(--accent-purple)" />
                <rect x="4" y="7" width="24" height="21" rx="5" fill="var(--svg-purple-card-bg)" stroke="var(--accent-purple)" stroke-width="1.6" />
                <circle cx="11.5" cy="15.5" r="2" fill="var(--accent-purple)" />
                <circle cx="20.5" cy="15.5" r="2" fill="var(--accent-purple)" />
                <path d="M 11 21.5 Q 16 24.5, 21 21.5" fill="none" stroke="var(--accent-purple)" stroke-width="1.4" stroke-linecap="round" />
                <rect x="34" y="3" width="166" height="29" rx="6" fill="var(--svg-purple-card-bg)" stroke="var(--accent-purple)" stroke-width="1.5" />
              </g>
              <g transform="translate(44, 77)">
                <circle cx="10" cy="14.5" r="3.8" fill="var(--accent-purple)" class="svg-pulse-dot" />
                <text x="19" y="13" class="svg-mono-branch" fill="var(--svg-purple-text)">Agent 2 → feat/faq</text>
                <text x="19" y="24" class="svg-mono-sub" fill="var(--svg-purple-line)">Faq.vue · App.vue</text>
              </g>
            </svg>

            <div class="fig-badge badge-info">✓ Scopes Claimed</div>
          </div>
        </div>

        <div class="fig-arrow fig-arrow-1" :class="{ pulse: activeStep === 1 }">
          <svg viewBox="0 0 28 20" width="24" height="18">
            <path
              d="M 2 10 Q 13 8, 23 10 M 17 5 Q 22 8, 25 10 Q 21 12, 17 15"
              fill="none"
              stroke="currentColor"
              stroke-width="2.2"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </div>

        <div
          class="fig-panel"
          :class="{ active: activeStep === 1 }"
          @mouseenter="setStep(1)"
          @click="setStep(1)"
        >
          <div class="fig-panel-header">2. Reconcile Edits</div>
          <div class="fig-scene">
            <div class="fig-bubble bubble-neutral" style="top: 7px; left: 7px;">
              tes hook post-tool
            </div>

            <svg viewBox="0 0 220 138" class="fig-svg">
              <g filter="url(#ink-sketch-agent)">
                <rect x="10" y="38" width="56" height="24" rx="5" fill="var(--svg-blue-card-bg)" stroke="var(--accent-blue)" stroke-width="1.4" />
                <rect x="10" y="74" width="56" height="24" rx="5" fill="var(--svg-purple-card-bg)" stroke="var(--accent-purple)" stroke-width="1.4" />

                <path d="M 66 50 C 80 50, 82 65, 95 65" fill="none" stroke="var(--accent-blue)" stroke-width="2" stroke-linecap="round" />
                <path d="M 66 86 C 80 86, 82 69, 95 69" fill="none" stroke="var(--accent-purple)" stroke-width="2" stroke-linecap="round" />

                <rect x="95" y="52" width="40" height="30" rx="6" fill="var(--svg-win-bg)" stroke="var(--accent-green)" stroke-width="1.6" />
                <line x1="135" y1="67" x2="146" y2="67" stroke="var(--accent-green)" stroke-width="2" stroke-linecap="round" />

                <rect x="146" y="36" width="64" height="62" rx="6" fill="var(--svg-win-bg)" stroke="var(--svg-win-stroke)" stroke-width="1.5" />
                <rect x="152" y="52" width="52" height="17" rx="3" fill="var(--svg-blue-card-bg)" stroke="var(--accent-blue)" stroke-width="1.1" />
                <rect x="152" y="73" width="52" height="17" rx="3" fill="var(--svg-purple-card-bg)" stroke="var(--accent-purple)" stroke-width="1.1" />
              </g>

              <text x="38" y="53" text-anchor="middle" class="svg-card-txt-bold" fill="var(--svg-blue-text-bold)">Agent 1</text>
              <text x="38" y="89" text-anchor="middle" class="svg-card-txt-bold" fill="var(--svg-purple-text)">Agent 2</text>
              <text x="115" y="65" text-anchor="middle" class="svg-card-txt-bold" fill="var(--svg-green-text)">AST</text>
              <text x="115" y="76" text-anchor="middle" class="svg-card-txt" fill="var(--svg-text-muted)">3-Way</text>
              <text x="178" y="47" text-anchor="middle" class="svg-card-txt-bold" fill="var(--svg-text-neutral)">App.vue</text>
              <text x="178" y="63.5" text-anchor="middle" class="svg-card-txt" fill="var(--svg-blue-text)">+Pricing</text>
              <text x="178" y="84.5" text-anchor="middle" class="svg-card-txt" fill="var(--svg-purple-text)">+FAQ</text>
            </svg>

            <div class="fig-badge badge-success">✓ Auto-Merged</div>
          </div>
        </div>

        <div class="fig-arrow fig-arrow-2" :class="{ pulse: activeStep === 2 }">
          <svg viewBox="0 0 28 20" width="24" height="18">
            <path
              d="M 2 10 Q 13 8, 23 10 M 17 5 Q 22 8, 25 10 Q 21 12, 17 15"
              fill="none"
              stroke="currentColor"
              stroke-width="2.2"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </div>

        <div
          class="fig-panel"
          :class="{ active: activeStep === 2 }"
          @mouseenter="setStep(2)"
          @click="setStep(2)"
        >
          <div class="fig-panel-header">3. Verify in Sandbox</div>
          <div class="fig-scene">
            <svg viewBox="0 0 220 138" class="fig-svg">
              <g transform="translate(12, 9)" filter="url(#ink-sketch-agent)">
                <rect x="0" y="0" width="196" height="47" rx="6" fill="var(--svg-win-bg)" stroke="var(--accent-blue)" stroke-width="1.6" />
                <path d="M0 6 C0 3, 3 0, 6 0 L190 0 C193 0, 196 3, 196 6 L196 14 L0 14 Z" fill="var(--svg-win-bar-blue)" />
                <circle cx="8" cy="7" r="2" fill="#f87171" />
                <circle cx="14.5" cy="7" r="2" fill="#fbbf24" />
                <circle cx="21" cy="7" r="2" fill="#4ade80" />
                <rect x="8" y="18" width="180" height="23" rx="3" fill="var(--svg-blue-card-bg)" stroke="var(--accent-blue)" stroke-width="1.1" />
              </g>
              <g transform="translate(12, 9)">
                <text x="30" y="10.5" class="svg-browser-url" fill="var(--svg-blue-text-bold)">Slot 1 · :4100 (feat/pricing)</text>
                <text x="14" y="33" class="svg-mono-sub" fill="var(--svg-blue-text)">❯ tes exec -- npm run build  ✓</text>
              </g>

              <g transform="translate(12, 62)" filter="url(#ink-sketch-agent)">
                <rect x="0" y="0" width="196" height="47" rx="6" fill="var(--svg-win-bg)" stroke="var(--accent-purple)" stroke-width="1.6" />
                <path d="M0 6 C0 3, 3 0, 6 0 L190 0 C193 0, 196 3, 196 6 L196 14 L0 14 Z" fill="var(--svg-win-bar-purple)" />
                <circle cx="8" cy="7" r="2" fill="#f87171" />
                <circle cx="14.5" cy="7" r="2" fill="#fbbf24" />
                <circle cx="21" cy="7" r="2" fill="#4ade80" />
                <rect x="8" y="18" width="180" height="23" rx="3" fill="var(--svg-purple-card-bg)" stroke="var(--accent-purple)" stroke-width="1.1" />
              </g>
              <g transform="translate(12, 62)">
                <text x="30" y="10.5" class="svg-browser-url" fill="var(--svg-purple-text)">Slot 2 · :4200 (feat/faq)</text>
                <text x="14" y="33" class="svg-mono-sub" fill="var(--svg-purple-line)">❯ tes exec -- npm run build  ✓</text>
              </g>
            </svg>

            <div class="fig-badge badge-success">✓ CoW Sandboxes</div>
          </div>
        </div>

        <div class="fig-arrow fig-arrow-3" :class="{ pulse: activeStep === 3 }">
          <svg viewBox="0 0 28 20" width="24" height="18">
            <path
              d="M 2 10 Q 13 8, 23 10 M 17 5 Q 22 8, 25 10 Q 21 12, 17 15"
              fill="none"
              stroke="currentColor"
              stroke-width="2.2"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </div>

        <div
          class="fig-panel"
          :class="{ active: activeStep === 3 }"
          @mouseenter="setStep(3)"
          @click="setStep(3)"
        >
          <div class="fig-panel-header">4. Commit to Git</div>
          <div class="fig-scene">
            <div class="fig-bubble bubble-updated" style="top: 6px; right: 7px;">
              Committed
            </div>

            <svg viewBox="0 0 220 138" class="fig-svg">
              <g filter="url(#ink-sketch-agent)">
                <path d="M 14 10 C 30 10, 34 19, 48 19" fill="none" stroke="var(--accent-blue)" stroke-width="2.2" stroke-linecap="round" />
                <path d="M 14 28 C 30 28, 34 19, 48 19" fill="none" stroke="var(--accent-purple)" stroke-width="2.2" stroke-linecap="round" />
                <line x1="48" y1="19" x2="74" y2="19" stroke="var(--accent-green)" stroke-width="2.4" stroke-linecap="round" />
                <circle cx="48" cy="19" r="3.8" fill="var(--accent-green)" stroke="#15803d" stroke-width="1.4" />
              </g>
              <text x="80" y="23" class="svg-mono-branch" fill="var(--svg-green-text)">tes commit / merge</text>

              <g transform="translate(12, 33)" filter="url(#ink-sketch-agent)">
                <rect x="0" y="0" width="196" height="74" rx="6" fill="var(--svg-win-bg)" stroke="var(--accent-green)" stroke-width="1.6" />
                <path d="M0 6 C0 3, 3 0, 6 0 L190 0 C193 0, 196 3, 196 6 L196 14 L0 14 Z" fill="var(--svg-win-bar-green)" />
                <circle cx="8" cy="7" r="2" fill="#f87171" />
                <circle cx="14.5" cy="7" r="2" fill="#fbbf24" />
                <circle cx="21" cy="7" r="2" fill="#4ade80" />
                <rect x="8" y="19" width="180" height="22" rx="3" fill="var(--svg-blue-card-bg)" stroke="var(--accent-blue)" stroke-width="1.2" />
                <rect x="8" y="45" width="180" height="22" rx="3" fill="var(--svg-purple-card-bg)" stroke="var(--accent-purple)" stroke-width="1.2" />
              </g>
              <g transform="translate(12, 33)">
                <text x="30" y="10.5" class="svg-browser-url" fill="var(--svg-green-text)">.git/refs/heads/main</text>
                <text x="15" y="33.5" class="svg-mono-sub" fill="var(--svg-blue-text-bold)">✓ feat(pricing): add pricing calc</text>
                <text x="15" y="59.5" class="svg-mono-sub" fill="var(--svg-purple-text)">✓ feat(faq): add faq section</text>
              </g>
            </svg>

            <div class="fig-badge badge-success">✓ Standard Git</div>
          </div>
        </div>
      </div>

      <div class="fig-summary-bar">
        <span :class="{ highlight: activeStep === 0 }" @click="setStep(0)">1. tes claim</span>
        <span class="fig-dot">·</span>
        <span :class="{ highlight: activeStep === 1 }" @click="setStep(1)">2. tes hook post-tool</span>
        <span class="fig-dot">·</span>
        <span :class="{ highlight: activeStep === 2 }" @click="setStep(2)">3. tes exec</span>
        <span class="fig-dot">·</span>
        <span :class="{ highlight: activeStep === 3 }" @click="setStep(3)">4. tes commit / merge</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
.fig-wrapper {
  --hand-font: 'Kalam', 'Patrick Hand', 'Caveat', 'Comic Sans MS', cursive;
  --accent-blue: #3b82f6;
  --accent-purple: #9333ea;
  --accent-green: #16a34a;
  --fig-card-bg: #ffffff;
  --fig-card-border: #cbd5e1;
  --fig-card-shadow: 0 12px 32px rgba(15, 23, 42, 0.08);
  --fig-title-color: #0f172a;
  --fig-panel-bg: #ffffff;
  --fig-panel-border: #93b4e8;
  --fig-panel-active-border: #2563eb;
  --fig-panel-active-shadow: 0 10px 24px rgba(37, 99, 235, 0.14);
  --fig-header-bg: #f8fafc;
  --fig-header-text: #0f172a;
  --fig-header-active-bg: #eff6ff;
  --fig-header-active-text: #1d4ed8;
  --fig-scene-bg: linear-gradient(180deg, #e8f3fe 0%, #f3f8fc 68%, #e6f4ea 100%);
  --svg-win-bg: #ffffff;
  --svg-win-stroke: #64748b;
  --svg-win-bar-neutral: #e2e8f0;
  --svg-win-bar-blue: #dbeafe;
  --svg-win-bar-purple: #f3e8ff;
  --svg-win-bar-green: #dcfce7;
  --svg-hero-bg: #f1f5f9;
  --svg-hero-stroke: #94a3b8;
  --svg-empty-bg: #f8fafc;
  --svg-empty-stroke: #cbd5e1;
  --svg-text-neutral: #334155;
  --svg-text-muted: #475569;
  --svg-blue-card-bg: #eff6ff;
  --svg-blue-card-feat-bg: #dbeafe;
  --svg-blue-text: #1d4ed8;
  --svg-blue-text-bold: #1e3a8a;
  --svg-purple-card-bg: #faf5ff;
  --svg-purple-text: #6b21a8;
  --svg-purple-line: #7e22ce;
  --svg-green-text: #15803d;
  --bubble-bg: #ffffff;
  --bubble-blue-text: #1d4ed8;
  --bubble-purple-text: #6b21a8;
  --bubble-neutral-text: #334155;
  --bubble-updated-bg: #fef3c7;
  --bubble-updated-text: #92400e;
  --badge-info-bg: #eff6ff;
  --badge-info-text: #1d4ed8;
  --badge-success-bg: #dcfce7;
  --badge-success-text: #15803d;
  --summary-bg: #bfd7f6;
  --summary-border: #7ea8e0;
  --summary-text: #0f172a;
  --summary-highlight: #1d4ed8;
  --arrow-color: #3b82f6;
  --arrow-active-color: #1d4ed8;
  container-type: inline-size;
  margin: 2rem 0 2.5rem;
}

.fig-wrapper.is-dark,
:global(.dark) .fig-wrapper {
  --accent-blue: #3794ff;
  --accent-purple: #b180d7;
  --accent-green: #3fb950;
  --fig-card-bg: #161618;
  --fig-card-border: #2e2e32;
  --fig-card-shadow: 0 10px 28px rgba(0, 0, 0, 0.28);
  --fig-title-color: #e6edf3;
  --fig-panel-bg: #1e1e1e;
  --fig-panel-border: #333333;
  --fig-panel-active-border: #3794ff;
  --fig-panel-active-shadow: 0 8px 22px rgba(0, 0, 0, 0.35);
  --fig-header-bg: #252526;
  --fig-header-text: #cccccc;
  --fig-header-active-bg: #1f2d3d;
  --fig-header-active-text: #ffffff;
  --fig-scene-bg: #1e1e1e;
  --svg-win-bg: #252526;
  --svg-win-stroke: #454545;
  --svg-win-bar-neutral: #2d2d2d;
  --svg-win-bar-blue: #1f2d3d;
  --svg-win-bar-purple: #2b2133;
  --svg-win-bar-green: #1b2e22;
  --svg-hero-bg: #2d2d2d;
  --svg-hero-stroke: #454545;
  --svg-empty-bg: #252526;
  --svg-empty-stroke: #3c3c3c;
  --svg-text-neutral: #d4d4d4;
  --svg-text-muted: #858585;
  --svg-blue-card-bg: #1f2d3d;
  --svg-blue-card-feat-bg: #26384d;
  --svg-blue-text: #9cdcfe;
  --svg-blue-text-bold: #ffffff;
  --svg-purple-card-bg: #2b2133;
  --svg-purple-text: #d9b8f3;
  --svg-purple-line: #b180d7;
  --svg-green-text: #7ee787;
  --bubble-bg: #252526;
  --bubble-blue-text: #9cdcfe;
  --bubble-purple-text: #d9b8f3;
  --bubble-neutral-text: #d4d4d4;
  --bubble-updated-bg: #2d2618;
  --bubble-updated-text: #dcdcaa;
  --badge-info-bg: #1f2d3d;
  --badge-info-text: #9cdcfe;
  --badge-success-bg: #1b2e22;
  --badge-success-text: #7ee787;
  --summary-bg: #202127;
  --summary-border: #2e2e32;
  --summary-text: #d4d4d4;
  --summary-highlight: #3794ff;
  --arrow-color: #6e7681;
  --arrow-active-color: #3794ff;
}

.fig-card {
  background: transparent;
  border: none;
  padding: 0;
  box-shadow: none;
  color: var(--fig-title-color);
  font-family: var(--hand-font);
}

.fig-title {
  text-align: center;
  font-family: var(--hand-font);
  font-size: 1.38rem;
  font-weight: 700;
  color: var(--fig-title-color);
  margin-bottom: 1.05rem;
  letter-spacing: 0.01em;
}

.fig-row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr) auto minmax(0, 1fr) auto minmax(0, 1fr);
  align-items: center;
  gap: 0.35rem;
}

.fig-panel {
  min-width: 0;
  border: 1.5px solid var(--fig-panel-border);
  border-radius: 10px;
  background: var(--fig-panel-bg);
  overflow: hidden;
  cursor: pointer;
  transition: transform 0.25s ease, border-color 0.25s ease, box-shadow 0.25s ease, background 0.2s ease;
}

.fig-panel.active {
  border-color: var(--fig-panel-active-border);
  transform: translateY(-2px);
  box-shadow: var(--fig-panel-active-shadow);
}

.fig-panel-header {
  text-align: center;
  font-family: var(--hand-font);
  font-size: 1.02rem;
  font-weight: 700;
  color: var(--fig-header-text);
  padding: 0.42rem 0.4rem 0.35rem;
  border-bottom: 1px solid var(--fig-panel-border);
  background: var(--fig-header-bg);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.fig-panel.active .fig-panel-header {
  background: var(--fig-header-active-bg);
  border-bottom-color: var(--fig-panel-active-border);
  color: var(--fig-header-active-text);
}

.fig-scene {
  position: relative;
  height: 164px;
  background: var(--fig-scene-bg);
  margin: 0.45rem;
  border-radius: 8px;
  overflow: hidden;
  display: flex;
  align-items: center;
  justify-content: center;
}

.fig-svg {
  width: 100%;
  height: 100%;
  display: block;
}

.svg-mono-sub {
  font-family: var(--hand-font);
  font-size: 9.8px;
  font-weight: 700;
}

.svg-mono-branch {
  font-family: var(--hand-font);
  font-size: 11px;
  font-weight: 700;
}

.svg-browser-url {
  font-family: var(--hand-font);
  font-size: 9.5px;
  font-weight: 700;
}

.svg-card-txt {
  font-family: var(--hand-font);
  font-size: 9px;
  font-weight: 700;
}

.svg-card-txt-bold {
  font-family: var(--hand-font);
  font-size: 9.5px;
  font-weight: 700;
}

.fig-bubble {
  position: absolute;
  z-index: 2;
  max-width: calc(100% - 14px);
  overflow: hidden;
  text-overflow: ellipsis;
  font-family: var(--hand-font);
  font-size: 0.78rem;
  font-weight: 700;
  padding: 0.1rem 0.5rem;
  border-radius: 999px;
  background: var(--bubble-bg);
  box-shadow: 0 2px 5px rgba(0, 0, 0, 0.12);
  line-height: 1.25;
  white-space: nowrap;
}

.bubble-blue {
  color: var(--bubble-blue-text);
  border: 1.5px solid var(--accent-blue);
}

.bubble-purple {
  color: var(--bubble-purple-text);
  border: 1.5px solid var(--accent-purple);
}

.bubble-neutral {
  color: var(--bubble-neutral-text);
  border: 1.5px solid var(--svg-win-stroke);
}

.bubble-updated {
  color: var(--bubble-updated-text);
  background: var(--bubble-updated-bg);
  border: 1.5px solid #d97706;
}

.fig-badge {
  position: absolute;
  right: 6px;
  bottom: 6px;
  z-index: 2;
  max-width: calc(100% - 12px);
  overflow: hidden;
  text-overflow: ellipsis;
  font-family: var(--hand-font);
  font-size: 0.76rem;
  font-weight: 700;
  padding: 0.12rem 0.5rem;
  border-radius: 6px;
  line-height: 1.2;
  white-space: nowrap;
}

.badge-info {
  background: var(--badge-info-bg);
  color: var(--badge-info-text);
  border: 1.5px solid var(--accent-blue);
}

.badge-success {
  background: var(--badge-success-bg);
  color: var(--badge-success-text);
  border: 1.5px solid var(--accent-green);
}

.fig-arrow {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  color: var(--arrow-color);
  user-select: none;
  transition: transform 0.2s ease, color 0.2s ease;
}

.fig-arrow svg {
  transition: transform 0.2s ease;
}

.fig-arrow.pulse {
  color: var(--arrow-active-color);
  transform: scale(1.2);
}

.svg-pulse-dot {
  animation: figPulse 1.6s ease-in-out infinite;
}

@keyframes figPulse {
  0%, 100% {
    r: 3.5;
    opacity: 0.85;
  }
  50% {
    r: 5.2;
    opacity: 1;
  }
}

.fig-summary-bar {
  margin-top: 0.95rem;
  background: var(--summary-bg);
  border: 1.5px solid var(--summary-border);
  border-radius: 8px;
  padding: 0.5rem 0.85rem;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-wrap: wrap;
  gap: 0.55rem;
  font-family: var(--hand-font);
  font-size: 1.02rem;
  font-weight: 700;
  color: var(--summary-text);
}

.fig-summary-bar span {
  cursor: pointer;
  transition: color 0.2s ease;
}

.fig-summary-bar span.highlight {
  color: var(--summary-highlight);
  text-decoration: underline;
  text-decoration-style: wavy;
  text-underline-offset: 3px;
}

.fig-dot {
  color: var(--svg-text-muted);
  cursor: default !important;
}

@container (max-width: 880px) {
  .fig-row {
    grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr);
    row-gap: 0.35rem;
    column-gap: 0.45rem;
  }

  .fig-arrow-2 {
    display: inline-flex;
    grid-column: 1 / -1;
    justify-self: center;
  }

  .fig-arrow-2 svg {
    transform: rotate(135deg);
  }
}

@container (max-width: 540px) {
  .fig-row {
    grid-template-columns: minmax(0, 1fr);
    gap: 0.35rem;
  }

  .fig-arrow {
    justify-self: center;
    padding: 0.1rem 0;
  }

  .fig-arrow svg,
  .fig-arrow-2 svg {
    transform: rotate(90deg);
  }
}

@media (max-width: 960px) {
  .fig-row {
    grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr);
    row-gap: 0.35rem;
    column-gap: 0.45rem;
  }

  .fig-arrow-2 {
    display: inline-flex;
    grid-column: 1 / -1;
    justify-self: center;
  }

  .fig-arrow-2 svg {
    transform: rotate(135deg);
  }
}

@media (max-width: 560px) {
  .fig-row {
    grid-template-columns: minmax(0, 1fr);
    gap: 0.35rem;
  }

  .fig-arrow {
    justify-self: center;
    padding: 0.1rem 0;
  }

  .fig-arrow svg,
  .fig-arrow-2 svg {
    transform: rotate(90deg);
  }
}
</style>
