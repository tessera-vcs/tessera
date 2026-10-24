<script setup lang="ts">
/**
 * Shared VS Code / Antigravity IDE editor preview showing live multi-agent attribution,
 * CodeLens headers, inline labels, gutter borders, overview ruler marks, and status bar items.
 */
</script>

<template>
  <div class="editor-frame">
    <div class="editor-tabbar">
      <div class="editor-tab active">
        <span class="editor-tab-ts">TS</span>
        <span>currency.ts</span>
        <span class="editor-tab-close">×</span>
      </div>
      <div class="editor-tab">
        <span class="editor-tab-ts">TS</span>
        <span>index.ts</span>
      </div>
    </div>
    <div class="editor-breadcrumbs">
      <span>src</span>
      <span>›</span>
      <span>currency.ts</span>
    </div>
    <div class="editor-body">
      <div class="editor-scroll">
        <div class="editor-canvas">
          <div class="codelens-row"><span class="codelens-gutter"></span><span class="codelens-text">2 agents concurrently editing this file (main): antigravity-a1b2 · claude-c3d4</span></div>
          <div class="code-line"><span class="line-num">1</span><div class="line-content"><span class="code-text"><span class="tok-kw">export</span> <span class="tok-decl">function</span> <span class="tok-fn">formatCurrency</span>(</span></div></div>
          <div class="code-line"><span class="line-num">2</span><div class="line-content"><span class="code-text indent-1"><span class="tok-param">cents</span>: <span class="tok-type">number</span>,</span></div></div>
          <div class="codelens-row"><span class="codelens-gutter"></span><span class="codelens-text">antigravity-a1b2 · L3-L6: Add locale and decimals</span></div>
          <div class="code-line slot-blue"><span class="line-num">3</span><div class="line-content"><span class="code-text indent-1"><span class="tok-param">currency</span> = <span class="tok-str">"USD"</span>,</span><span class="inline-after">antigravity-a1b2: Add locale and decimals</span></div></div>
          <div class="code-line slot-blue"><span class="line-num">4</span><div class="line-content"><span class="code-text indent-1"><span class="tok-param">locale</span> = <span class="tok-str">"en-US"</span>,</span></div></div>
          <div class="code-line slot-blue"><span class="line-num">5</span><div class="line-content"><span class="code-text indent-1"><span class="tok-param">decimals</span> = <span class="tok-num">2</span></span></div></div>
          <div class="code-line slot-blue"><span class="line-num">6</span><div class="line-content"><span class="code-text">): <span class="tok-type">string</span> {</span></div></div>
          <div class="code-line"><span class="line-num">7</span><div class="line-content"><span class="code-text indent-1"><span class="tok-decl">const</span> <span class="tok-param">amount</span> = (<span class="tok-param">cents</span> / <span class="tok-num">100</span>).<span class="tok-fn">toFixed</span>(<span class="tok-param">decimals</span>);</span></div></div>
          <div class="code-line"><span class="line-num">8</span><div class="line-content"><span class="code-text indent-1"><span class="tok-kw">return</span> <span class="tok-str">`[${</span><span class="tok-param">locale</span><span class="tok-str">}] ${</span><span class="tok-param">currency</span><span class="tok-str">} ${</span><span class="tok-param">amount</span><span class="tok-str">}`</span>;</span></div></div>
          <div class="code-line"><span class="line-num">9</span><div class="line-content"><span class="code-text">}</span></div></div>
          <div class="codelens-row"><span class="codelens-gutter"></span><span class="codelens-text">claude-c3d4 · L10-L14: Add formatCompact helper</span></div>
          <div class="code-line slot-purple"><span class="line-num">10</span><div class="line-content"><span class="code-text"><span class="tok-kw">export</span> <span class="tok-decl">function</span> <span class="tok-fn">formatCompact</span>(<span class="tok-param">cents</span>: <span class="tok-type">number</span>) {</span><span class="inline-after">claude-c3d4: Add formatCompact helper</span></div></div>
          <div class="code-line slot-purple"><span class="line-num">11</span><div class="line-content"><span class="code-text indent-1"><span class="tok-kw">return</span> <span class="tok-decl">new</span> <span class="tok-type">Intl</span>.<span class="tok-type">NumberFormat</span>(<span class="tok-str">"en-US"</span>, {</span></div></div>
          <div class="code-line slot-purple"><span class="line-num">12</span><div class="line-content"><span class="code-text indent-2"><span class="tok-param">style</span>: <span class="tok-str">"currency"</span>, <span class="tok-param">currency</span>: <span class="tok-str">"USD"</span>, <span class="tok-param">notation</span>: <span class="tok-str">"compact"</span></span></div></div>
          <div class="code-line slot-purple"><span class="line-num">13</span><div class="line-content"><span class="code-text indent-1">}).<span class="tok-fn">format</span>(<span class="tok-param">cents</span> / <span class="tok-num">100</span>);</span></div></div>
          <div class="code-line slot-purple"><span class="line-num">14</span><div class="line-content"><span class="code-text">}</span></div></div>
        </div>
      </div>
      <div class="overview-ruler">
        <div class="ruler-mark slot-blue"></div>
        <div class="ruler-mark slot-purple"></div>
      </div>
    </div>
    <div class="editor-statusbar">
      <div class="statusbar-left">
        <span class="statusbar-item">
          <svg viewBox="0 0 16 16"><path d="M11.75 2.5a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5zm-2.25.75a2.25 2.25 0 1 1 3 2.122V6A2.5 2.5 0 0 1 10 8.5H6a1 1 0 0 0-1 1v1.128a2.251 2.251 0 1 1-1.5 0V5.372a2.25 2.25 0 1 1 1.5 0v1.836A2.492 2.492 0 0 1 6 7h4a1 1 0 0 0 1-1v-.628A2.25 2.25 0 0 1 9.5 3.25zM4.25 12a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5zM3.5 3.25a.75.75 0 1 1 1.5 0 .75.75 0 0 1-1.5 0z"/></svg>
          main
        </span>
        <span class="statusbar-item">
          <svg viewBox="0 0 16 16"><path d="M10.5 7.75a2.5 2.5 0 1 1-5 0 2.5 2.5 0 0 1 5 0zm1.43.75a4.002 4.002 0 0 1-7.86 0H.75a.75.75 0 0 1 0-1.5h3.32a4.002 4.002 0 0 1 7.86 0h3.32a.75.75 0 0 1 0 1.5h-3.32z"/></svg>
          Graph
        </span>
      </div>
      <div class="statusbar-right">
        <span class="statusbar-item statusbar-hide-mobile">Ln 4, Col 28</span>
        <span class="statusbar-item statusbar-hide-mobile">Spaces: 2</span>
        <span class="statusbar-item statusbar-hide-mobile">UTF-8</span>
        <span class="statusbar-item">TypeScript</span>
      </div>
    </div>
  </div>
</template>
