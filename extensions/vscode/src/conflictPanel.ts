/**
 * Webview panel providing an interactive 3-way AST conflict resolution studio.
 */

import * as crypto from "crypto";
import * as fs from "fs";
import * as vscode from "vscode";
import {
  CliClient,
  ConflictItem,
  isSyntheticConflictPath,
  resolveConfinedPath,
  WorkspaceStateSnapshot,
} from "./client";

type ResolveStrategy = "ast-union" | "ours" | "theirs" | "working" | "content";

const VALID_STRATEGIES: ReadonlySet<string> = new Set<ResolveStrategy>([
  "ast-union",
  "ours",
  "theirs",
  "working",
  "content",
]);

/**
 * Serializes a value as JSON with HTML-sensitive characters (`<`, `>`, `&`, U+2028, U+2029)
 * escaped as Unicode sequences so it cannot break out of an inline `<script>` tag.
 *
 * @param value The value to serialize.
 * @returns A script-safe JSON string.
 */
function serializeJsonForHtmlScript(value: unknown): string {
  return JSON.stringify(value)
    .replace(/</g, "\\u003c")
    .replace(/>/g, "\\u003e")
    .replace(/&/g, "\\u0026")
    .replace(/\u2028/g, "\\u2028")
    .replace(/\u2029/g, "\\u2029");
}

/**
 * Interactive Visual Conflict Resolution Studio Webview.
 * Displays competing AI agent intents, Base/Ours/Theirs snippets, verification diagnostics,
 * and an editable live preview of `AST-Union` with 1-click resolution buttons.
 * Preserves unsaved custom textarea edits across background refreshes via `postMessage`.
 */
export class ConflictStudioPanel {
  private static currentPanel: ConflictStudioPanel | undefined;
  private readonly panel: vscode.WebviewPanel;
  private readonly client: CliClient;
  private readonly onResolved: () => Promise<void>;
  private readonly disposables: vscode.Disposable[] = [];
  private lastConflicts: readonly ConflictItem[] = [];
  private pendingSnapshot: WorkspaceStateSnapshot | undefined;
  private pendingFocusPath: string | undefined;

  /**
   * Initializes the conflict studio webview and registers message and visibility handlers.
   *
   * @param panel The underlying VS Code webview panel.
   * @param client The CLI client used to execute conflict resolution commands.
   * @param snapshot The initial repository state snapshot.
   * @param onResolved Callback invoked after a conflict is successfully resolved.
   * @param focusPath Optional relative file path to highlight and scroll into view.
   */
  private constructor(
    panel: vscode.WebviewPanel,
    client: CliClient,
    snapshot: WorkspaceStateSnapshot,
    onResolved: () => Promise<void>,
    focusPath?: string
  ) {
    this.panel = panel;
    this.client = client;
    this.onResolved = onResolved;
    this.lastConflicts = snapshot.conflicts;

    this.panel.webview.html = this.buildInitialHtml(
      snapshot.conflicts,
      focusPath
    );

    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);

    this.panel.onDidChangeViewState(
      (event) => {
        if (event.webviewPanel.visible && this.pendingSnapshot) {
          const snap = this.pendingSnapshot;
          const focus = this.pendingFocusPath;
          this.pendingSnapshot = undefined;
          this.pendingFocusPath = undefined;
          this.pushUpdate(snap, focus);
        }
      },
      null,
      this.disposables
    );

    this.panel.webview.onDidReceiveMessage(
      (raw: unknown) => {
        void this.handleMessage(raw);
      },
      null,
      this.disposables
    );
  }

  /**
   * Opens or reveals the Visual Conflict Resolution Studio.
   *
   * @param client The CLI client for running resolution commands.
   * @param snapshot The current repository state snapshot.
   * @param onResolved Callback invoked when a conflict is resolved.
   * @param focusPath Optional relative file path to focus in the conflict view.
   */
  public static show(
    client: CliClient,
    snapshot: WorkspaceStateSnapshot,
    onResolved: () => Promise<void>,
    focusPath?: string
  ): void {
    if (ConflictStudioPanel.currentPanel) {
      ConflictStudioPanel.currentPanel.panel.reveal(vscode.ViewColumn.One);
      ConflictStudioPanel.currentPanel.pushUpdate(snapshot, focusPath);
      return;
    }

    const panel = vscode.window.createWebviewPanel(
      "conflictStudio",
      "Conflict Resolver",
      vscode.ViewColumn.One,
      { enableScripts: true, retainContextWhenHidden: true }
    );

    ConflictStudioPanel.currentPanel = new ConflictStudioPanel(
      panel,
      client,
      snapshot,
      onResolved,
      focusPath
    );
  }

  /**
   * Pushes an incremental snapshot update if the studio panel is currently visible,
   * or queues it until the panel becomes visible again.
   *
   * @param snapshot The updated repository state snapshot.
   */
  public static updateIfVisible(snapshot: WorkspaceStateSnapshot): void {
    const current = ConflictStudioPanel.currentPanel;

    if (!current) {
      return;
    }

    if (!current.panel.visible) {
      current.pendingSnapshot = snapshot;
      return;
    }

    current.pushUpdate(snapshot);
  }

  /**
   * Disposes the currently open conflict studio panel, if any.
   */
  public static disposeCurrent(): void {
    if (ConflictStudioPanel.currentPanel) {
      ConflictStudioPanel.currentPanel.dispose();
    }
  }

  /**
   * Validates and dispatches messages posted from the webview script.
   *
   * @param raw The unvalidated message object received from the webview.
   */
  private async handleMessage(raw: unknown): Promise<void> {
    if (typeof raw !== "object" || raw === null) {
      return;
    }

    const record = raw as Record<string, unknown>;
    const msgType = record.type;

    if (msgType === "openFile") {
      const filePath = record.path;

      if (typeof filePath !== "string" || isSyntheticConflictPath(filePath)) {
        return;
      }

      const confined = resolveConfinedPath(this.client.getRepoRoot(), filePath);

      if (!confined || !fs.existsSync(confined[1])) {
        void vscode.window.showErrorMessage(
          `Cannot open conflicted file '${filePath}'.`
        );
        return;
      }

      try {
        const doc = await vscode.workspace.openTextDocument(
          vscode.Uri.file(confined[1])
        );
        let conflictLine = 0;

        for (let i = 0; i < doc.lineCount; i++) {
          if (doc.lineAt(i).text.startsWith("<<<<<<<")) {
            conflictLine = i;
            break;
          }
        }

        const pos = new vscode.Position(conflictLine, 0);
        await vscode.window.showTextDocument(doc, {
          preview: false,
          selection: new vscode.Range(pos, pos),
        });
      } catch (err: unknown) {
        const message = err instanceof Error ? err.message : String(err);
        void vscode.window.showErrorMessage(`Open file failed: ${message}`);
      }

      return;
    }

    if (msgType === "resolve") {
      const filePath = record.path;
      const strategy = record.strategy;
      const customContent =
        typeof record.customContent === "string"
          ? record.customContent
          : undefined;

      if (
        typeof filePath !== "string" ||
        typeof strategy !== "string" ||
        !VALID_STRATEGIES.has(strategy) ||
        !this.lastConflicts.some((c) => c.path === filePath) ||
        (!isSyntheticConflictPath(filePath) &&
          !resolveConfinedPath(this.client.getRepoRoot(), filePath))
      ) {
        void vscode.window.showErrorMessage(
          "Resolve rejected: invalid conflict path or strategy."
        );
        return;
      }

      try {
        await this.client.resolveConflict(
          filePath,
          strategy as ResolveStrategy,
          customContent
        );
        void vscode.window.showInformationMessage(
          `Resolved conflict in ${filePath} (${strategy}).`
        );
        await this.onResolved();
      } catch (err: unknown) {
        const message = err instanceof Error ? err.message : String(err);
        void vscode.window.showErrorMessage(`Resolve failed: ${message}`);
      }
    }
  }

  /**
   * Sends updated conflict state to the webview via `postMessage` without overwriting HTML.
   *
   * @param snapshot The repository state snapshot containing active conflicts.
   * @param focusPath Optional relative path to highlight and scroll into view.
   */
  private pushUpdate(
    snapshot: WorkspaceStateSnapshot,
    focusPath?: string
  ): void {
    this.lastConflicts = snapshot.conflicts;
    this.pendingSnapshot = undefined;
    this.pendingFocusPath = undefined;
    void this.panel.webview.postMessage({
      type: "update",
      conflicts: snapshot.conflicts,
      focusPath,
    });
  }

  /**
   * Disposes the webview panel and all registered event subscriptions.
   */
  private dispose(): void {
    if (ConflictStudioPanel.currentPanel === this) {
      ConflictStudioPanel.currentPanel = undefined;
    }

    while (this.disposables.length > 0) {
      const d = this.disposables.pop();

      if (d) {
        d.dispose();
      }
    }

    this.panel.dispose();
  }

  /**
   * Generates the initial self-contained HTML shell for the conflict studio webview.
   * Subsequent updates arrive via `postMessage` so unsaved custom textarea edits are preserved.
   *
   * @param conflicts The initial list of unresolved conflict items.
   * @param focusPath Optional relative file path to highlight.
   * @returns Complete HTML markup for the webview.
   */
  private buildInitialHtml(
    conflicts: readonly ConflictItem[],
    focusPath?: string
  ): string {
    const nonce = crypto.randomBytes(16).toString("base64");
    const serialized = serializeJsonForHtmlScript({ conflicts, focusPath });

    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';" />
<style>
  * { box-sizing: border-box; }
  body {
    font-family: var(--vscode-font-family);
    font-size: var(--vscode-font-size, 13px);
    background-color: var(--vscode-editor-background);
    color: var(--vscode-foreground);
    margin: 0;
    padding: 16px 20px;
  }
  h1 { font-size: 13px; font-weight: 600; margin: 0 0 12px 0; }
  .conflict-card {
    background-color: var(--vscode-editorWidget-background, var(--vscode-sideBar-background));
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: 2px;
    padding: 12px;
    margin-bottom: 12px;
  }
  .conflict-card.focused {
    border-color: var(--vscode-focusBorder);
    box-shadow: 0 0 0 1px var(--vscode-focusBorder);
  }
  .conflict-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    border-bottom: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    padding-bottom: 8px;
    margin-bottom: 10px;
  }
  .header-meta {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .file-title { font-weight: 600; font-size: 12px; font-family: var(--vscode-editor-font-family); }
  .symbol-badge {
    background-color: var(--vscode-badge-background);
    color: var(--vscode-badge-foreground);
    padding: 1px 6px;
    border-radius: 2px;
    font-size: 11px;
  }
  .intents-row {
    display: flex;
    gap: 8px;
    margin-bottom: 10px;
    flex-wrap: wrap;
  }
  .intent-pill {
    flex: 1;
    min-width: 200px;
    background-color: var(--vscode-textCodeBlock-background);
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    padding: 6px 8px;
    border-radius: 2px;
    font-size: 12px;
  }
  .diagnostics-box {
    background-color: var(--vscode-inputValidation-errorBackground, var(--vscode-editor-background));
    border: 1px solid var(--vscode-inputValidation-errorBorder, var(--vscode-errorForeground));
    border-radius: 2px;
    padding: 8px;
    margin-bottom: 10px;
    overflow-x: auto;
  }
  .grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
    margin-bottom: 10px;
  }
  .grid.three-col {
    grid-template-columns: 1fr 1fr 1fr;
  }
  .code-box {
    background-color: var(--vscode-editor-background);
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: 2px;
    padding: 8px;
    overflow-x: auto;
  }
  .code-title {
    font-size: 11px;
    font-weight: 600;
    margin-bottom: 6px;
    color: var(--vscode-descriptionForeground);
  }
  pre, textarea {
    font-family: var(--vscode-editor-font-family);
    font-size: 12px;
    margin: 0;
    white-space: pre-wrap;
    color: var(--vscode-foreground);
  }
  textarea {
    width: 100%;
    min-height: 110px;
    background-color: var(--vscode-input-background);
    color: var(--vscode-input-foreground);
    border: 1px solid var(--vscode-input-border, var(--vscode-panel-border));
    border-radius: 2px;
    padding: 8px;
  }
  .actions {
    display: flex;
    gap: 6px;
    margin-top: 10px;
    flex-wrap: wrap;
  }
  button {
    background-color: var(--vscode-button-background);
    color: var(--vscode-button-foreground);
    border: 1px solid var(--vscode-contrastBorder, transparent);
    border-radius: 2px;
    padding: 4px 10px;
    cursor: pointer;
    font-family: var(--vscode-font-family);
    font-size: 12px;
  }
  button:hover {
    background-color: var(--vscode-button-hoverBackground);
  }
  button.secondary {
    background-color: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
  }
  button.secondary:hover {
    background-color: var(--vscode-button-secondaryHoverBackground);
  }
  .empty {
    padding: 20px;
    text-align: center;
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: 2px;
    color: var(--vscode-descriptionForeground);
    font-size: 12px;
  }
</style>
</head>
<body>
  <h1>Conflict Resolver</h1>
  <div id="root"></div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const root = document.getElementById('root');
    const dirtyDrafts = new Map();
    let currentState = ${serialized};
    let lastRenderedSignature = '';

    function escapeHtml(str) {
      return String(str || '')
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
    }

    function isSyntheticPath(p) {
      return p === '<speculative-verify>' || p === '<build>';
    }

    function computeSignature(conflicts) {
      return JSON.stringify((conflicts || []).map((c) => [
        c.id,
        c.path,
        c.symbol,
        c.kind,
        c.base_snippet,
        c.left_snippet,
        c.right_snippet,
        c.ast_union_preview,
        c.diagnostics,
        c.agent_intents,
      ]));
    }

    function applyFocusHighlight(focusPath) {
      const cards = root.querySelectorAll('.conflict-card');
      let targetCard = null;
      cards.forEach((card) => {
        const matches = Boolean(focusPath && card.getAttribute('data-path') === focusPath);
        card.classList.toggle('focused', matches);
        if (matches && !targetCard) {
          targetCard = card;
        }
      });
      if (targetCard) {
        targetCard.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
      }
    }

    function renderConflicts(conflicts, focusPath, forceRender) {
      const nextSig = computeSignature(conflicts);
      if (!forceRender && nextSig === lastRenderedSignature) {
        if (focusPath) {
          applyFocusHighlight(focusPath);
        }
        return;
      }

      const activeEl = document.activeElement;
      const activeConflictId =
        activeEl && activeEl.tagName === 'TEXTAREA'
          ? activeEl.getAttribute('data-conflict-id')
          : null;
      const selStart = activeConflictId ? activeEl.selectionStart : null;
      const selEnd = activeConflictId ? activeEl.selectionEnd : null;

      const activeIds = new Set((conflicts || []).map((c) => c.id));
      for (const draftId of Array.from(dirtyDrafts.keys())) {
        if (!activeIds.has(draftId)) {
          dirtyDrafts.delete(draftId);
        }
      }

      lastRenderedSignature = nextSig;

      if (!conflicts || conflicts.length === 0) {
        root.innerHTML = '<div class="empty">No active conflicts</div>';
        return;
      }

      root.innerHTML = conflicts.map((c, idx) => {
        const intentsHtml = Object.entries(c.agent_intents || {}).map(([aid, intent]) =>
          '<div class="intent-pill"><strong>' + escapeHtml(aid) + '</strong>: ' + escapeHtml(intent) + '</div>'
        ).join('');

        const hasDiagnostics = Boolean(c.diagnostics && String(c.diagnostics).trim().length > 0);
        const diagnosticsHtml = hasDiagnostics
          ? '<div class="diagnostics-box">' +
              '<div class="code-title">Verification Diagnostics</div>' +
              '<pre>' + escapeHtml(c.diagnostics) + '</pre>' +
            '</div>'
          : '';

        const hasBase = Boolean(c.base_snippet && String(c.base_snippet).length > 0);
        const baseBoxHtml = hasBase
          ? '<div class="code-box">' +
              '<div class="code-title">Base</div>' +
              '<pre>' + escapeHtml(c.base_snippet) + '</pre>' +
            '</div>'
          : '';

        const openEditorBtn = isSyntheticPath(c.path)
          ? ''
          : '<button class="secondary" data-action="openFile" data-idx="' + idx + '">Open in Editor</button>';

        const textareaValue = dirtyDrafts.has(c.id) ? dirtyDrafts.get(c.id) : c.ast_union_preview;
        const isFocused = Boolean(focusPath && c.path === focusPath);

        return '<div class="conflict-card' + (isFocused ? ' focused' : '') + '" data-path="' + escapeHtml(c.path) + '">' +
          '<div class="conflict-header">' +
            '<span class="file-title">' + escapeHtml(c.path) + '</span>' +
            '<div class="header-meta">' +
              '<span class="symbol-badge">' + escapeHtml(c.symbol || c.kind) + '</span>' +
              openEditorBtn +
            '</div>' +
          '</div>' +
          (intentsHtml ? '<div class="intents-row">' + intentsHtml + '</div>' : '') +
          diagnosticsHtml +
          '<div class="grid' + (hasBase ? ' three-col' : '') + '">' +
            baseBoxHtml +
            '<div class="code-box">' +
              '<div class="code-title">Ours</div>' +
              '<pre>' + escapeHtml(c.left_snippet) + '</pre>' +
            '</div>' +
            '<div class="code-box">' +
              '<div class="code-title">Theirs</div>' +
              '<pre>' + escapeHtml(c.right_snippet) + '</pre>' +
            '</div>' +
          '</div>' +
          '<div class="code-box">' +
            '<div class="code-title">AST-Union Preview</div>' +
            '<textarea id="custom-' + idx + '" data-conflict-id="' + escapeHtml(c.id) + '">' + escapeHtml(textareaValue) + '</textarea>' +
          '</div>' +
          '<div class="actions">' +
            '<button data-action="resolve" data-idx="' + idx + '" data-strategy="ast-union">Accept AST-Union</button>' +
            '<button class="secondary" data-action="resolve" data-idx="' + idx + '" data-strategy="ours">Accept Ours</button>' +
            '<button class="secondary" data-action="resolve" data-idx="' + idx + '" data-strategy="theirs">Accept Theirs</button>' +
            '<button class="secondary" data-action="resolve" data-idx="' + idx + '" data-strategy="working">Keep Working File</button>' +
            '<button class="secondary" data-action="resolveCustom" data-idx="' + idx + '">Save Custom</button>' +
          '</div>' +
        '</div>';
      }).join('');

      if (activeConflictId) {
        const restored = root.querySelector('textarea[data-conflict-id="' + CSS.escape(activeConflictId) + '"]');
        if (restored) {
          restored.focus();
          if (selStart !== null && selEnd !== null) {
            restored.setSelectionRange(selStart, selEnd);
          }
        }
      }

      if (focusPath) {
        applyFocusHighlight(focusPath);
      }
    }

    renderConflicts(currentState.conflicts || [], currentState.focusPath, true);

    window.addEventListener('message', (event) => {
      const msg = event.data;
      if (!msg || msg.type !== 'update') {
        return;
      }

      currentState = {
        conflicts: msg.conflicts || [],
        focusPath: msg.focusPath,
      };
      renderConflicts(currentState.conflicts, currentState.focusPath, false);
    });

    root.addEventListener('input', (ev) => {
      const target = ev.target;
      if (!target || target.tagName !== 'TEXTAREA') {
        return;
      }

      const cid = target.getAttribute('data-conflict-id');
      if (cid) {
        dirtyDrafts.set(cid, target.value);
      }
    });

    document.addEventListener('click', (ev) => {
      const btn = ev.target.closest('button[data-action]');
      if (!btn) {
        return;
      }

      const idx = Number(btn.getAttribute('data-idx'));
      const c = currentState.conflicts && currentState.conflicts[idx];
      if (!c) {
        return;
      }

      const action = btn.getAttribute('data-action');
      if (action === 'openFile') {
        vscode.postMessage({ type: 'openFile', path: c.path });
      } else if (action === 'resolve') {
        const strategy = btn.getAttribute('data-strategy');
        dirtyDrafts.delete(c.id);
        vscode.postMessage({ type: 'resolve', path: c.path, strategy });
      } else if (action === 'resolveCustom') {
        const textarea = document.getElementById('custom-' + idx);
        const content = textarea ? textarea.value : c.ast_union_preview;
        dirtyDrafts.delete(c.id);
        vscode.postMessage({
          type: 'resolve',
          path: c.path,
          strategy: 'content',
          customContent: content,
        });
      }
    });
  </script>
</body>
</html>`;
  }
}
