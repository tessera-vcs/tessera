/**
 * HTML, SVG, and client-script renderer for the 2D Branch Graph webview.
 * Supports incremental `postMessage` updates that preserve viewport scroll position.
 */

import {
  colSpineCenterX,
  GraphLayoutResult,
  nodeCardHeight,
  nodeCenterX,
  rowCenterY,
} from "./graphLayout";

/**
 * Rendered HTML fragments pushed into the Branch Graph shell on initial load and updates.
 */
export interface GraphFragments {
  readonly topbarHtml: string;
  readonly stageHtml: string;
}

/**
 * Escapes HTML special characters for safe interpolation into webview markup and attributes.
 *
 * @param raw The unescaped input string.
 * @returns The HTML-escaped string.
 */
export function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/**
 * Extracts the trailing filename component from a slash-delimited relative path.
 *
 * @param filePath The slash-delimited relative file path.
 * @returns The basename of the path.
 */
export function basename(filePath: string): string {
  const parts = filePath.split("/");
  return parts[parts.length - 1] ?? filePath;
}

/**
 * Abbreviates long session identifiers so they fit cleanly inside compact node badges.
 *
 * @param id The full session or agent identifier.
 * @returns An abbreviated display name.
 */
export function shortAgentName(id: string): string {
  if (id.startsWith("antigravity-")) {
    return `agy-${id.slice("antigravity-".length, "antigravity-".length + 6)}`;
  }

  return id.length > 14 ? id.slice(0, 14) : id;
}

/**
 * Coerces a numeric port value to a safe non-negative integer for HTML interpolation.
 *
 * @param rawPort The raw port number from the branch report.
 * @returns A finite integer port number.
 */
function sanitizePort(rawPort: number): number {
  const num = Number(rawPort);
  return Number.isFinite(num) && num >= 0 ? Math.trunc(num) : 0;
}

/**
 * Renders the topbar and stage HTML/SVG fragments for a computed `GraphLayoutResult`.
 *
 * @param layout The computed 2D graph layout geometry and node metadata.
 * @returns Topbar and stage HTML markup fragments for initial load or `postMessage` patching.
 */
export function renderGraphFragments(layout: GraphLayoutResult): GraphFragments {
  const svgElements: string[] = [];
  const headerBottomY = layout.headerTop + layout.headerH;

  for (let col = 0; col < layout.sortedBranches.length; col++) {
    const colNodes = layout.canvasNodes.filter(
      (n) => n.col === col && n.horizIndex === 0
    );

    if (colNodes.length === 0) {
      continue;
    }

    const firstNode = colNodes.reduce((a, b) => (a.row < b.row ? a : b));
    const spineX = colSpineCenterX(layout, col);
    const firstNodeTopY =
      rowCenterY(layout, firstNode.row) - nodeCardHeight(layout, firstNode) / 2;

    svgElements.push(
      `<line class="track-line" x1="${spineX}" y1="${headerBottomY}" x2="${spineX}" y2="${firstNodeTopY}" />`
    );
  }

  for (const node of layout.canvasNodes) {
    const childX = nodeCenterX(layout, node);
    const childY = rowCenterY(layout, node.row);

    if (node.horizIndex > 0 && node.parents.length === 1) {
      const prevId = node.parents[0];
      const prevNode = prevId ? layout.nodeById.get(prevId) : undefined;

      if (prevNode && prevNode.row === node.row) {
        const x1 = nodeCenterX(layout, prevNode) + layout.cardW / 2;
        const x2 = childX - layout.cardW / 2;
        svgElements.push(
          `<path class="edge-line horiz" d="M ${x1} ${childY} L ${x2} ${childY}" />`
        );
        continue;
      }
    }

    for (let pIdx = 0; pIdx < node.parents.length; pIdx++) {
      const parentId = node.parents[pIdx];

      if (!parentId) {
        continue;
      }

      const parentNode = layout.nodeById.get(parentId);

      if (!parentNode) {
        continue;
      }

      const topNode = node.row <= parentNode.row ? node : parentNode;
      const bottomNode = node.row <= parentNode.row ? parentNode : node;
      const topX = nodeCenterX(layout, topNode);
      const topBottomY =
        rowCenterY(layout, topNode.row) + nodeCardHeight(layout, topNode) / 2;
      const bottomX = nodeCenterX(layout, bottomNode);
      const bottomTopY =
        rowCenterY(layout, bottomNode.row) -
        nodeCardHeight(layout, bottomNode) / 2;

      const edgeClass =
        node.kind === "working"
          ? "edge-line dashed"
          : pIdx > 0 || node.col !== parentNode.col
            ? "edge-line branch"
            : "edge-line";

      if (topX === bottomX) {
        svgElements.push(
          `<path class="${edgeClass}" d="M ${topX} ${topBottomY} L ${bottomX} ${bottomTopY}" />`
        );
      } else {
        const dy = Math.max(Math.abs(bottomTopY - topBottomY) * 0.5, 16);
        const d = `M ${topX} ${topBottomY} C ${topX} ${topBottomY + dy}, ${bottomX} ${bottomTopY - dy}, ${bottomX} ${bottomTopY}`;
        svgElements.push(`<path class="${edgeClass}" d="${d}" />`);
      }
    }
  }

  const colHeaderHtml = layout.sortedBranches
    .map((b, col) => {
      const cx = colSpineCenterX(layout, col);
      const left = cx - layout.cardW / 2;
      const isViewing = b.name === layout.editorBranch;
      const canDelete = b.name !== layout.trunkBranch;
      const safePort = sanitizePort(b.port);

      return `
        <div class="col-header ${isViewing ? "viewing" : ""}" style="left: ${left}px; width: ${layout.cardW}px; height: ${layout.headerH}px;" data-action="switchBranch" data-branch="${escapeHtml(b.name)}" title="Switch editor to '${escapeHtml(b.name)}'">
          <div class="col-header-top">
            <span class="col-name">${escapeHtml(b.name)}</span>
            <div class="col-header-actions">
              ${
                isViewing
                  ? '<span class="badge">Active</span>'
                  : '<span class="badge secondary">Switch</span>'
              }
              ${
                canDelete
                  ? `<button class="icon-btn" data-action="deleteBranch" data-branch="${escapeHtml(b.name)}" title="Delete branch '${escapeHtml(b.name)}'">×</button>`
                  : ""
              }
            </div>
          </div>
          <div class="col-header-sub">
            <span>${b.name === layout.trunkBranch ? "Default" : "Branch"}</span>
            <span>:${safePort}</span>
          </div>
        </div>`;
    })
    .join("");

  const agentIconSvg = `<svg class="agent-icon" width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="10" height="8" rx="2"/><path d="M8 2v3"/><circle cx="6" cy="9" r="0.75" fill="currentColor"/><circle cx="10" cy="9" r="0.75" fill="currentColor"/></svg>`;

  const nodesHtml = layout.canvasNodes
    .map((n) => {
      const cx = nodeCenterX(layout, n);
      const cy = rowCenterY(layout, n.row);
      const cardH = nodeCardHeight(layout, n);
      const left = cx - layout.cardW / 2;
      const top = cy - cardH / 2;

      if (n.kind === "collapsed" && n.groupId) {
        return `
          <button class="graph-node kind-collapsed"
                  style="left: ${left}px; top: ${top}px; width: ${layout.cardW}px; height: ${cardH}px;"
                  data-action="expandGroup"
                  data-group-id="${escapeHtml(n.groupId)}"
                  title="Expand ${escapeHtml(n.shortHash)} (${escapeHtml(n.title)})">
            <span class="collapsed-pill-badge">${escapeHtml(n.shortHash)}</span>
            <span class="collapsed-pill-range">${escapeHtml(n.title)}</span>
            <span class="collapsed-pill-action">Expand</span>
          </button>`;
      }

      const fileAction = n.hasConflict ? "openFile" : "openBranchDiff";
      const fileChips = n.files
        .slice(0, 3)
        .map(
          (f) => `
            <button class="file-chip" data-action="${fileAction}" data-path="${escapeHtml(f)}" data-branch="${escapeHtml(n.branchName)}" title="${escapeHtml(f)}">
              ${escapeHtml(basename(f))}
            </button>`
        )
        .join("");

      let sessionBadges = "";

      if (n.sessions.length > 0) {
        const singleShortName = shortAgentName(n.sessions[0].id);
        const maxSingleChars = n.isHead ? 10 : 15;
        const hasOverflowRisk =
          n.sessions.length > 1 || singleShortName.length > maxSingleChars;
        const badgeLabel = hasOverflowRisk
          ? `+${n.sessions.length}`
          : escapeHtml(singleShortName);
        const fallbackTitle = n.sessions
          .map(
            (s) =>
              `${shortAgentName(s.id)}: ${s.intent || "Active session"}`
          )
          .join("\n");

        sessionBadges = `
            <span class="badge secondary agent-badge" title="${escapeHtml(fallbackTitle)}">
              ${agentIconSvg}
              <span class="agent-badge-text">${badgeLabel}</span>
            </span>`;
      }

      const kindLabel =
        n.kind === "merge"
          ? `<span class="badge">Merge</span>`
          : n.kind === "working"
            ? `<span class="badge">${escapeHtml(n.shortHash)}</span>`
            : `<span class="node-hash">${escapeHtml(n.shortHash)}</span>`;

      const horizToggleBtn =
        n.horizIndex === 0 && n.horizTotalCount > 1 && n.horizGroupId
          ? `<button class="file-chip horiz-toggle" data-action="toggleHorizGroup" data-group-id="${escapeHtml(n.horizGroupId)}" title="${n.horizExpanded ? "Collapse" : "Expand"} ${n.horizTotalCount - 1} related ${escapeHtml(n.horizTopic ?? "")} commit(s)">${
              n.horizExpanded
                ? `◂ Hide ${n.horizTotalCount - 1}`
                : `+${n.horizTotalCount - 1} ${escapeHtml(n.horizTopic ?? "related")} ▸`
            }</button>`
          : n.horizIndex > 0 && n.horizTopic
            ? `<span class="badge secondary">${escapeHtml(n.horizTopic)} ${n.horizIndex + 1}/${n.horizTotalCount}</span>`
            : "";

      const discardBtn =
        n.kind === "working"
          ? `<button class="file-chip" data-action="discardWorking" data-branch="${escapeHtml(n.branchName)}" title="Discard uncommitted working changes on '${escapeHtml(n.branchName)}'">Discard</button>`
          : "";

      return `
        <div class="graph-node kind-${n.kind} ${n.isHead ? "is-head" : ""} ${n.hasConflict ? "is-conflict" : ""}"
             style="left: ${left}px; top: ${top}px; width: ${layout.cardW}px; height: ${cardH}px;">
          <div class="node-top-bar">
            <div class="node-id-group">
              ${kindLabel}
              ${n.isHead ? '<span class="badge">HEAD</span>' : ""}
              ${horizToggleBtn}
            </div>
            <div class="node-agents">${sessionBadges}</div>
          </div>
          <div class="node-title" title="${escapeHtml(n.title)}">${escapeHtml(n.title)}</div>
          <div class="node-footer">
            <div class="node-files">${fileChips || `<span class="node-meta">${escapeHtml(n.subtitle)}</span>`}</div>
            ${discardBtn}
          </div>
        </div>`;
    })
    .join("");

  const mergeSourcesJson = escapeHtml(JSON.stringify(layout.mergeableBranches));

  const topbarHtml = `
    <div class="topbar-left">
      <span class="topbar-title">Branch Graph</span>
      <span class="topbar-sub">Active: ${escapeHtml(layout.editorBranch)}</span>
    </div>
    <div class="topbar-actions">
      ${
        layout.firstConflict
          ? `<button class="btn conflict" data-action="openConflict" data-path="${escapeHtml(layout.firstConflict.path)}">Resolve Conflict (${escapeHtml(basename(layout.firstConflict.path))})</button>`
          : ""
      }
      ${
        layout.mergeableBranches.length > 0
          ? `<button class="btn primary" data-action="mergeInto" data-sources="${mergeSourcesJson}" data-target="${escapeHtml(layout.trunkBranch)}">Merge into ${escapeHtml(layout.trunkBranch)}</button>`
          : ""
      }
      ${
        layout.hasAnyExpanded
          ? `<button class="btn" data-action="collapseAll">Collapse Linear</button>`
          : ""
      }
      ${
        layout.hasAnyCollapsed
          ? `<button class="btn" data-action="expandAll">Expand All</button>`
          : ""
      }
      <button class="btn" data-action="refresh">Refresh</button>
    </div>`;

  const stageHtml = `
    <div class="graph-stage" style="width: ${layout.canvasWidth}px; height: ${layout.canvasHeight}px;">
      <svg class="graph-svg" width="${layout.canvasWidth}" height="${layout.canvasHeight}" viewBox="0 0 ${layout.canvasWidth} ${layout.canvasHeight}">
        ${svgElements.join("\n        ")}
      </svg>
      <div class="col-header-bar" style="width: ${layout.canvasWidth}px;">
        ${colHeaderHtml}
      </div>
      ${nodesHtml}
    </div>`;

  return { topbarHtml, stageHtml };
}

/**
 * Generates the initial complete HTML shell document for the Branch Graph webview.
 * Subsequent renders patch `#topbar` and `#stage-host` via `postMessage` while preserving scroll offsets.
 *
 * @param layout The initial computed graph layout.
 * @param nonce Cryptographic CSP script nonce.
 * @returns Complete HTML markup for the webview.
 */
export function buildGraphDocumentHtml(
  layout: GraphLayoutResult,
  nonce: string
): string {
  const { topbarHtml, stageHtml } = renderGraphFragments(layout);

  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src 'nonce-${nonce}';">
<style>
  * { box-sizing: border-box; }
  html, body {
    margin: 0;
    padding: 0;
    width: 100%;
    height: 100vh;
    overflow: hidden;
    display: flex;
    flex-direction: column;
    background-color: var(--vscode-editor-background);
    color: var(--vscode-foreground);
    font-family: var(--vscode-font-family);
    font-size: var(--vscode-font-size, 13px);
  }
  .topbar {
    flex-shrink: 0;
    width: 100%;
    z-index: 20;
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 20px;
    background-color: var(--vscode-editor-background);
    border-bottom: 1px solid var(--vscode-panel-border, var(--vscode-widget-border));
  }
  .topbar-left {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .topbar-title {
    font-size: 13px;
    font-weight: 600;
  }
  .topbar-sub {
    font-size: 12px;
    color: var(--vscode-descriptionForeground);
  }
  .topbar-actions {
    display: flex;
    gap: 6px;
  }
  .btn {
    background-color: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: 1px solid var(--vscode-contrastBorder, transparent);
    border-radius: 2px;
    padding: 4px 10px;
    font-family: var(--vscode-font-family);
    font-size: 12px;
    cursor: pointer;
  }
  .btn:hover {
    background-color: var(--vscode-button-secondaryHoverBackground);
  }
  .btn.primary {
    background-color: var(--vscode-button-background);
    color: var(--vscode-button-foreground);
  }
  .btn.primary:hover {
    background-color: var(--vscode-button-hoverBackground);
  }
  .btn.conflict {
    background-color: var(--vscode-inputValidation-errorBackground, var(--vscode-button-secondaryBackground));
    color: var(--vscode-errorForeground, var(--vscode-foreground));
    border: 1px solid var(--vscode-inputValidation-errorBorder, var(--vscode-errorForeground));
  }
  .graph-viewport {
    flex: 1;
    width: 100%;
    min-height: 0;
    overflow: auto;
    position: relative;
  }
  .graph-stage {
    position: relative;
    margin: 0 auto 24px;
  }
  .col-header-bar {
    position: sticky;
    top: 0;
    z-index: 10;
    height: ${layout.headerHeight}px;
    background-color: var(--vscode-editor-background);
  }
  .graph-svg {
    position: absolute;
    top: 0;
    left: 0;
    pointer-events: none;
    z-index: 1;
  }
  .track-line {
    stroke: var(--vscode-tree-indentGuidesStroke, var(--vscode-panel-border));
    stroke-width: 1;
    stroke-dasharray: 4 4;
  }
  .edge-line {
    fill: none;
    stroke: var(--vscode-descriptionForeground);
    stroke-width: 1.5;
  }
  .edge-line.branch {
    stroke: var(--vscode-descriptionForeground);
  }
  .edge-line.dashed {
    stroke-dasharray: 4 4;
  }
  .col-header {
    position: absolute;
    top: ${layout.headerTop}px;
    z-index: 5;
    background-color: var(--vscode-sideBar-background, var(--vscode-editorWidget-background));
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: 2px;
    padding: 7px 10px;
    cursor: pointer;
  }
  .col-header:hover {
    background-color: var(--vscode-list-hoverBackground);
  }
  .col-header.viewing {
    border-color: var(--vscode-focusBorder);
  }
  .col-header-top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
  }
  .col-header-actions {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    flex-shrink: 0;
  }
  .icon-btn {
    background: transparent;
    color: var(--vscode-descriptionForeground);
    border: 1px solid transparent;
    border-radius: 2px;
    padding: 0 4px;
    font-size: 12px;
    line-height: 15px;
    cursor: pointer;
  }
  .icon-btn:hover {
    background-color: var(--vscode-toolbar-hoverBackground, var(--vscode-button-secondaryHoverBackground));
    color: var(--vscode-foreground);
  }
  .col-name {
    font-size: 12px;
    font-weight: 600;
    font-family: var(--vscode-editor-font-family);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    flex: 1;
  }
  .col-header-sub {
    display: flex;
    justify-content: space-between;
    margin-top: 3px;
    font-size: 11px;
    color: var(--vscode-descriptionForeground);
  }
  .badge {
    font-size: 10px;
    font-weight: 500;
    padding: 1px 5px;
    border-radius: 2px;
    background-color: var(--vscode-badge-background);
    color: var(--vscode-badge-foreground);
    white-space: nowrap;
  }
  .badge.secondary {
    background-color: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
  }
  .graph-node {
    position: absolute;
    z-index: 4;
    background-color: var(--vscode-editorWidget-background, var(--vscode-sideBar-background));
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: 2px;
    padding: 6px 10px;
    display: flex;
    flex-direction: column;
    justify-content: space-between;
  }
  .graph-node:hover {
    background-color: var(--vscode-list-hoverBackground);
  }
  .graph-node.is-head {
    border-color: var(--vscode-focusBorder);
  }
  .graph-node.kind-working {
    border-style: dashed;
  }
  .graph-node.is-conflict {
    border-color: var(--vscode-inputValidation-errorBorder, var(--vscode-errorForeground));
  }
  .graph-node.kind-collapsed {
    flex-direction: row;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
    padding: 4px 10px;
    border-style: dashed;
    cursor: pointer;
    font-family: var(--vscode-font-family);
    color: var(--vscode-foreground);
    text-align: left;
  }
  .collapsed-pill-badge {
    font-family: var(--vscode-editor-font-family);
    font-size: 11px;
    font-weight: 600;
    color: var(--vscode-badge-foreground);
    background-color: var(--vscode-badge-background);
    padding: 1px 6px;
    border-radius: 2px;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .collapsed-pill-range {
    font-family: var(--vscode-editor-font-family);
    font-size: 11px;
    color: var(--vscode-descriptionForeground);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
  }
  .collapsed-pill-action {
    font-size: 11px;
    color: var(--vscode-textLink-foreground, var(--vscode-descriptionForeground));
    white-space: nowrap;
    flex-shrink: 0;
  }
  .node-top-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
    min-width: 0;
  }
  .node-id-group {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-shrink: 0;
  }
  .node-agents {
    display: flex;
    align-items: center;
    gap: 4px;
    min-width: 0;
    flex-shrink: 1;
  }
  .agent-badge {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    max-width: 100%;
    overflow: hidden;
    cursor: default;
  }
  .agent-icon {
    flex-shrink: 0;
    display: block;
  }
  .agent-badge-text {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .node-hash {
    font-family: var(--vscode-editor-font-family);
    font-size: 11px;
    color: var(--vscode-descriptionForeground);
    background-color: var(--vscode-textCodeBlock-background);
    padding: 1px 5px;
    border-radius: 2px;
  }
  .node-title {
    font-size: 12px;
    font-weight: 500;
    line-height: 1.28;
    color: var(--vscode-foreground);
    display: -webkit-box;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .node-footer {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 4px;
    min-width: 0;
  }
  .node-files {
    display: flex;
    gap: 4px;
    flex-wrap: nowrap;
    overflow: hidden;
    min-width: 0;
    max-width: 100%;
  }
  .file-chip {
    background-color: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: 1px solid var(--vscode-contrastBorder, transparent);
    border-radius: 2px;
    padding: 1px 6px;
    font-size: 11px;
    font-family: var(--vscode-editor-font-family);
    cursor: pointer;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .file-chip:hover {
    background-color: var(--vscode-button-secondaryHoverBackground);
  }
  .node-meta {
    font-size: 11px;
    color: var(--vscode-descriptionForeground);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
</head>
<body>
  <div id="topbar" class="topbar">${topbarHtml}</div>

  <div id="viewport" class="graph-viewport">
    <div id="stage-host">${stageHtml}</div>
  </div>

  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const topbarEl = document.getElementById('topbar');
    const viewportEl = document.getElementById('viewport');
    const stageHostEl = document.getElementById('stage-host');

    window.addEventListener('message', (event) => {
      const msg = event.data;
      if (!msg || msg.type !== 'render') {
        return;
      }

      const prevScrollLeft = viewportEl.scrollLeft;
      const prevScrollTop = viewportEl.scrollTop;
      topbarEl.innerHTML = msg.topbarHtml || '';
      stageHostEl.innerHTML = msg.stageHtml || '';
      viewportEl.scrollLeft = prevScrollLeft;
      viewportEl.scrollTop = prevScrollTop;
    });

    document.addEventListener('click', (ev) => {
      const target = ev.target.closest('[data-action]');
      if (!target) {
        return;
      }

      ev.stopPropagation();
      const action = target.getAttribute('data-action');

      if (action === 'switchBranch') {
        vscode.postMessage({
          command: 'switchBranch',
          branch: target.getAttribute('data-branch'),
        });
      } else if (action === 'expandGroup') {
        vscode.postMessage({
          command: 'expandGroup',
          groupId: target.getAttribute('data-group-id'),
        });
      } else if (action === 'toggleHorizGroup') {
        vscode.postMessage({
          command: 'toggleHorizGroup',
          groupId: target.getAttribute('data-group-id'),
        });
      } else if (action === 'collapseAll') {
        vscode.postMessage({ command: 'collapseAll' });
      } else if (action === 'expandAll') {
        vscode.postMessage({ command: 'expandAll' });
      } else if (action === 'mergeInto') {
        const raw = target.getAttribute('data-sources') || '[]';
        let sources = [];
        try {
          const parsed = JSON.parse(raw);
          if (Array.isArray(parsed)) {
            sources = parsed.filter((s) => typeof s === 'string' && s.length > 0);
          }
        } catch {
          sources = [];
        }
        const targetBranch = target.getAttribute('data-target');
        if (sources.length > 0 && targetBranch) {
          vscode.postMessage({
            command: 'mergeInto',
            sources,
            target: targetBranch,
          });
        }
      } else if (action === 'openBranchDiff') {
        vscode.postMessage({
          command: 'openBranchDiff',
          filePath: target.getAttribute('data-path'),
          branch: target.getAttribute('data-branch'),
        });
      } else if (action === 'openConflict') {
        vscode.postMessage({
          command: 'openConflict',
          filePath: target.getAttribute('data-path'),
        });
      } else if (action === 'openFile') {
        vscode.postMessage({
          command: 'openFile',
          filePath: target.getAttribute('data-path'),
        });
      } else if (action === 'resolveConflict') {
        vscode.postMessage({
          command: 'resolveConflict',
          filePath: target.getAttribute('data-path'),
          strategy: target.getAttribute('data-strategy'),
        });
      } else if (action === 'deleteBranch') {
        vscode.postMessage({
          command: 'deleteBranch',
          branch: target.getAttribute('data-branch'),
        });
      } else if (action === 'discardWorking') {
        vscode.postMessage({
          command: 'discardWorking',
          branch: target.getAttribute('data-branch'),
        });
      } else if (action === 'refresh') {
        vscode.postMessage({ command: 'refresh' });
      }
    });
  </script>
</body>
</html>`;
}
