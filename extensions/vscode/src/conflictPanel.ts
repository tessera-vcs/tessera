/**
 * Webview panel providing an interactive 3-way AST conflict resolution studio.
 */

import * as crypto from "crypto";
import * as vscode from "vscode";
import {
  CliClient,
  ConflictItem,
  resolveConfinedPath,
  WorkspaceStateSnapshot,
} from "./client";

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
 * Message payload posted from the conflict resolution webview back to the extension host.
 */
interface ConflictWebviewMessage {
  readonly type: "resolve";
  readonly path: string;
  readonly strategy: "ast-union" | "ours" | "theirs" | "working" | "content";
  readonly customContent?: string;
}

/**
 * Interactive Visual Conflict Resolution Studio Webview.
 * Displays competing AI agent intents, Ours code, Theirs code, and a live
 * preview of `AST-Union` (Combined AST) with 1-click resolution buttons.
 */
export class ConflictStudioPanel {
  private static currentPanel: ConflictStudioPanel | undefined;
  private readonly panel: vscode.WebviewPanel;
  private readonly client: CliClient;
  private readonly onResolved: () => Promise<void>;
  private lastConflicts: readonly ConflictItem[] = [];

  /**
   * Initializes the conflict studio webview and registers message handlers.
   *
   * @param panel The underlying VS Code webview panel.
   * @param client The CLI client used to execute conflict resolution commands.
   * @param onResolved Callback invoked after a conflict is successfully resolved.
   */
  private constructor(
    panel: vscode.WebviewPanel,
    client: CliClient,
    onResolved: () => Promise<void>
  ) {
    this.panel = panel;
    this.client = client;
    this.onResolved = onResolved;

    this.panel.onDidDispose(() => {
      ConflictStudioPanel.currentPanel = undefined;
    });

    this.panel.webview.onDidReceiveMessage(async (raw: unknown) => {
      if (typeof raw !== "object" || raw === null || !("type" in raw)) {
        return;
      }

      const msg = raw as ConflictWebviewMessage;

      if (msg.type === "resolve") {
        const validStrategies = new Set([
          "ast-union",
          "ours",
          "theirs",
          "working",
          "content",
        ]);

        if (
          typeof msg.path !== "string" ||
          !validStrategies.has(msg.strategy) ||
          !this.lastConflicts.some((c) => c.path === msg.path) ||
          !resolveConfinedPath(this.client.getRepoRoot(), msg.path)
        ) {
          void vscode.window.showErrorMessage(
            `Resolve rejected: invalid conflict path or strategy.`
          );
          return;
        }

        try {
          await this.client.resolveConflict(
            msg.path,
            msg.strategy,
            msg.customContent
          );
          void vscode.window.showInformationMessage(
            `Resolved conflict in ${msg.path} (${msg.strategy}).`
          );
          await this.onResolved();
        } catch (err: unknown) {
          const message = err instanceof Error ? err.message : String(err);
          void vscode.window.showErrorMessage(`Resolve failed: ${message}`);
        }
      }
    });
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
      ConflictStudioPanel.currentPanel.render(snapshot, focusPath);
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
      onResolved
    );
    ConflictStudioPanel.currentPanel.render(snapshot, focusPath);
  }

  /**
   * Refreshes the open studio if it is currently visible.
   *
   * @param snapshot The updated repository state snapshot.
   */
  public static updateIfVisible(snapshot: WorkspaceStateSnapshot): void {
    if (ConflictStudioPanel.currentPanel) {
      ConflictStudioPanel.currentPanel.render(snapshot);
    }
  }

  /**
   * Renders the given snapshot's conflicts into the webview HTML.
   *
   * @param snapshot The repository state snapshot containing active conflicts.
   * @param focusPath Optional relative path to focus.
   */
  private render(snapshot: WorkspaceStateSnapshot, focusPath?: string): void {
    const conflicts = snapshot.conflicts;
    this.lastConflicts = conflicts;
    this.panel.webview.html = this.buildHtml(conflicts, focusPath);
  }

  /**
   * Generates the self-contained HTML document for the conflict studio webview.
   *
   * @param conflicts The list of unresolved conflict items.
   * @param focusPath Optional relative file path to highlight.
   * @returns Complete HTML markup for the webview.
   */
  private buildHtml(
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
  .conflict-header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    border-bottom: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    padding-bottom: 8px;
    margin-bottom: 10px;
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
  .grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
    margin-bottom: 10px;
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
    const state = ${serialized};
    const root = document.getElementById('root');

    function escapeHtml(str) {
      return String(str || '')
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
    }

    if (!state.conflicts || state.conflicts.length === 0) {
      root.innerHTML = '<div class="empty">No active conflicts</div>';
    } else {
      root.innerHTML = state.conflicts.map((c, idx) => {
        const intentsHtml = Object.entries(c.agent_intents || {}).map(([aid, intent]) =>
          '<div class="intent-pill"><strong>' + escapeHtml(aid) + '</strong>: ' + escapeHtml(intent) + '</div>'
        ).join('');

        return '<div class="conflict-card">' +
          '<div class="conflict-header">' +
            '<span class="file-title">' + escapeHtml(c.path) + '</span>' +
            '<span class="symbol-badge">' + escapeHtml(c.symbol || c.kind) + '</span>' +
          '</div>' +
          '<div class="intents-row">' + intentsHtml + '</div>' +
          '<div class="grid">' +
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
            '<textarea id="custom-' + idx + '">' + escapeHtml(c.ast_union_preview) + '</textarea>' +
          '</div>' +
          '<div class="actions">' +
            '<button data-action="resolve" data-idx="' + idx + '" data-strategy="ast-union">Accept AST-Union</button>' +
            '<button class="secondary" data-action="resolve" data-idx="' + idx + '" data-strategy="ours">Accept Ours</button>' +
            '<button class="secondary" data-action="resolve" data-idx="' + idx + '" data-strategy="theirs">Accept Theirs</button>' +
            '<button class="secondary" data-action="resolveCustom" data-idx="' + idx + '">Save Custom</button>' +
          '</div>' +
        '</div>';
      }).join('');
    }

    document.addEventListener('click', (ev) => {
      const btn = ev.target.closest('button[data-action]');
      if (!btn) return;
      const idx = Number(btn.getAttribute('data-idx'));
      const c = state.conflicts && state.conflicts[idx];
      if (!c) return;
      const action = btn.getAttribute('data-action');
      if (action === 'resolve') {
        const strategy = btn.getAttribute('data-strategy');
        vscode.postMessage({ type: 'resolve', path: c.path, strategy });
      } else if (action === 'resolveCustom') {
        const textarea = document.getElementById('custom-' + idx);
        vscode.postMessage({
          type: 'resolve',
          path: c.path,
          strategy: 'content',
          customContent: textarea ? textarea.value : c.ast_union_preview
        });
      }
    });
  </script>
</body>
</html>`;
  }
}
