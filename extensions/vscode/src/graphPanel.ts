/**
 * Interactive 2D spatial branch and commit DAG webview panel (`tes.openGraph`).
 */

import * as crypto from "crypto";
import * as vscode from "vscode";
import {
  CliClient,
  isSyntheticConflictPath,
  isValidRefName,
  isValidSafeIdentifier,
  resolveConfinedPath,
  WorkspaceStateSnapshot,
} from "./client";
import { buildGraphDocumentHtml, renderGraphFragments } from "./graphHtml";
import { computeGraphLayout } from "./graphLayout";

type GraphResolveStrategy = "ours" | "theirs" | "ast-union" | "working";

const VALID_GRAPH_RESOLVE_STRATEGIES: ReadonlySet<string> =
  new Set<GraphResolveStrategy>(["ours", "theirs", "ast-union", "working"]);

/**
 * Interactive 2D Spatial Branch and Commit DAG Webview Panel (`tes.openGraph`).
 * Updates incrementally via `postMessage` to preserve scroll position and skips work while hidden.
 */
export class VisualGraphPanel {
  private static currentPanel: VisualGraphPanel | undefined;
  private readonly panel: vscode.WebviewPanel;
  private readonly client: CliClient;
  private readonly onSwitchBranch: (branch: string) => Promise<void>;
  private readonly onRefresh: () => Promise<void>;
  private readonly disposables: vscode.Disposable[] = [];
  private readonly expandedGroups = new Set<string>();
  private readonly expandedHorizGroups = new Set<string>();
  private expandAll = false;
  private lastSnapshot: WorkspaceStateSnapshot;
  private pendingSnapshot: WorkspaceStateSnapshot | undefined;

  /**
   * Initializes the graph webview panel and registers interactive message and visibility handlers.
   *
   * @param panel The underlying VS Code webview panel.
   * @param client The CLI client for running repository commands.
   * @param snapshot The initial repository state snapshot.
   * @param onSwitchBranch Callback invoked when the user switches branches from the graph.
   * @param onRefresh Callback invoked when the graph requests a state refresh.
   */
  private constructor(
    panel: vscode.WebviewPanel,
    client: CliClient,
    snapshot: WorkspaceStateSnapshot,
    onSwitchBranch: (branch: string) => Promise<void>,
    onRefresh: () => Promise<void>
  ) {
    this.panel = panel;
    this.client = client;
    this.lastSnapshot = snapshot;
    this.onSwitchBranch = onSwitchBranch;
    this.onRefresh = onRefresh;

    const initialLayout = computeGraphLayout(snapshot, {
      expandAll: this.expandAll,
      expandedGroups: this.expandedGroups,
      expandedHorizGroups: this.expandedHorizGroups,
    });
    const nonce = crypto.randomBytes(16).toString("base64");
    this.panel.webview.html = buildGraphDocumentHtml(initialLayout, nonce);

    this.panel.onDidChangeViewState(
      (event) => {
        if (event.webviewPanel.visible && this.pendingSnapshot) {
          const snap = this.pendingSnapshot;
          this.pendingSnapshot = undefined;
          this.update(snap);
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

    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
  }

  /**
   * Opens or reveals the 2D Branch & Commit Graph webview panel.
   *
   * @param client The CLI client for running repository commands.
   * @param snapshot The current repository state snapshot.
   * @param onSwitchBranch Callback invoked when switching branches.
   * @param onRefresh Callback invoked when refreshing state.
   */
  public static show(
    client: CliClient,
    snapshot: WorkspaceStateSnapshot,
    onSwitchBranch: (branch: string) => Promise<void>,
    onRefresh: () => Promise<void>
  ): void {
    if (VisualGraphPanel.currentPanel) {
      VisualGraphPanel.currentPanel.panel.reveal(vscode.ViewColumn.One);
      VisualGraphPanel.currentPanel.update(snapshot);
      return;
    }

    const panel = vscode.window.createWebviewPanel(
      "visualGraph",
      "Branch Graph",
      vscode.ViewColumn.One,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
      }
    );

    VisualGraphPanel.currentPanel = new VisualGraphPanel(
      panel,
      client,
      snapshot,
      onSwitchBranch,
      onRefresh
    );
  }

  /**
   * Updates the open graph webview if it is currently visible, or queues the update until revealed.
   *
   * @param snapshot The updated repository state snapshot.
   */
  public static updateIfVisible(snapshot: WorkspaceStateSnapshot): void {
    const current = VisualGraphPanel.currentPanel;

    if (!current) {
      return;
    }

    if (!current.panel.visible) {
      current.pendingSnapshot = snapshot;
      return;
    }

    current.update(snapshot);
  }

  /**
   * Disposes the currently open graph webview panel, if any.
   */
  public static disposeCurrent(): void {
    if (VisualGraphPanel.currentPanel) {
      VisualGraphPanel.currentPanel.dispose();
    }
  }

  /**
   * Re-renders the graph webview incrementally via `postMessage` with a new repository snapshot.
   *
   * @param snapshot The updated repository state snapshot.
   */
  public update(snapshot: WorkspaceStateSnapshot): void {
    this.lastSnapshot = snapshot;
    this.pendingSnapshot = undefined;
    this.postLayoutFragments();
  }

  /**
   * Recomputes the current layout and posts updated topbar and stage fragments to the webview.
   */
  private postLayoutFragments(): void {
    const layout = computeGraphLayout(this.lastSnapshot, {
      expandAll: this.expandAll,
      expandedGroups: this.expandedGroups,
      expandedHorizGroups: this.expandedHorizGroups,
    });
    const { topbarHtml, stageHtml } = renderGraphFragments(layout);
    void this.panel.webview.postMessage({
      type: "render",
      topbarHtml,
      stageHtml,
    });
  }

  /**
   * Validates and dispatches an interactive message posted by the graph webview.
   *
   * @param raw The unvalidated message object received from the webview.
   */
  private async handleMessage(raw: unknown): Promise<void> {
    if (typeof raw !== "object" || raw === null) {
      return;
    }

    const msg = raw as Record<string, unknown>;
    const command = msg.command;

    if (typeof command !== "string") {
      return;
    }

    try {
      if (command === "switchBranch" && typeof msg.branch === "string") {
        if (!isValidRefName(msg.branch)) {
          throw new Error(`Invalid branch name '${msg.branch}'`);
        }

        await this.onSwitchBranch(msg.branch);
      } else if (command === "expandGroup" && typeof msg.groupId === "string") {
        this.expandedGroups.add(msg.groupId);
        this.postLayoutFragments();
      } else if (
        command === "toggleHorizGroup" &&
        typeof msg.groupId === "string"
      ) {
        if (this.expandedHorizGroups.has(msg.groupId)) {
          this.expandedHorizGroups.delete(msg.groupId);
        } else {
          this.expandedHorizGroups.add(msg.groupId);
        }

        this.postLayoutFragments();
      } else if (command === "collapseAll") {
        this.expandAll = false;
        this.expandedGroups.clear();
        this.expandedHorizGroups.clear();
        this.postLayoutFragments();
      } else if (command === "expandAll") {
        this.expandAll = true;
        this.postLayoutFragments();
      } else if (
        command === "mergeInto" &&
        Array.isArray(msg.sources) &&
        typeof msg.target === "string"
      ) {
        const sources = msg.sources.filter(
          (s): s is string =>
            typeof s === "string" &&
            (isValidRefName(s) || isValidSafeIdentifier(s))
        );

        if (sources.length === 0 || !isValidRefName(msg.target)) {
          throw new Error("Invalid merge source(s) or target branch.");
        }

        if (sources.length > 1) {
          const confirm = await vscode.window.showWarningMessage(
            `Merge ${sources.length} branches (${sources.join(", ")}) into '${msg.target}'?`,
            { modal: true },
            "Merge Branches"
          );

          if (confirm !== "Merge Branches") {
            return;
          }
        }

        const defaultMsg = `Merge ${sources.join(", ")} into ${msg.target}`;
        const commitMsg = await vscode.window.showInputBox({
          prompt: `Merge ${sources.join(", ")} into ${msg.target}`,
          value: defaultMsg,
        });

        if (!commitMsg) {
          return;
        }

        await this.client.mergeInto(sources, msg.target, commitMsg);
        await this.onRefresh();
      } else if (
        command === "openBranchDiff" &&
        typeof msg.branch === "string" &&
        typeof msg.filePath === "string"
      ) {
        if (!isValidRefName(msg.branch)) {
          throw new Error(`Invalid branch name '${msg.branch}'`);
        }

        const cleanPath = msg.filePath.split("::")[0]?.trim() || msg.filePath;
        const confined = resolveConfinedPath(
          this.client.getRepoRoot(),
          cleanPath
        );

        if (!confined) {
          throw new Error(`Invalid file path '${cleanPath}'`);
        }

        await vscode.commands.executeCommand(
          "tes.diffBranchFile",
          msg.branch,
          confined[0]
        );
      } else if (command === "openConflict") {
        const filePath =
          typeof msg.filePath === "string" ? msg.filePath : undefined;
        await vscode.commands.executeCommand(
          "tes.openConflictStudio",
          filePath
        );
      } else if (command === "openFile" && typeof msg.filePath === "string") {
        const cleanPath = msg.filePath.split("::")[0]?.trim() || msg.filePath;

        if (isSyntheticConflictPath(cleanPath)) {
          await vscode.commands.executeCommand(
            "tes.openConflictStudio",
            cleanPath
          );
          return;
        }

        const confined = resolveConfinedPath(
          this.client.getRepoRoot(),
          cleanPath
        );

        if (!confined) {
          throw new Error(`Rejected unsafe file path '${cleanPath}'`);
        }

        const fullUri = vscode.Uri.file(confined[1]);
        const doc = await vscode.workspace.openTextDocument(fullUri);
        const editor = await vscode.window.showTextDocument(doc, {
          preview: false,
        });
        const text = doc.getText();
        const markerIdx = text.indexOf("<<<<<<<");

        if (markerIdx >= 0) {
          const pos = doc.positionAt(markerIdx);
          editor.selection = new vscode.Selection(pos, pos);
          editor.revealRange(
            new vscode.Range(pos, pos),
            vscode.TextEditorRevealType.InCenter
          );
        }
      } else if (
        command === "resolveConflict" &&
        typeof msg.filePath === "string" &&
        typeof msg.strategy === "string"
      ) {
        if (!VALID_GRAPH_RESOLVE_STRATEGIES.has(msg.strategy)) {
          throw new Error(`Invalid resolution strategy '${msg.strategy}'`);
        }

        const isKnownSynthetic =
          isSyntheticConflictPath(msg.filePath) &&
          this.lastSnapshot.conflicts.some((c) => c.path === msg.filePath);
        const confined = isKnownSynthetic
          ? ([msg.filePath, msg.filePath] as const)
          : resolveConfinedPath(this.client.getRepoRoot(), msg.filePath);

        if (!confined) {
          throw new Error(`Rejected unsafe conflict path '${msg.filePath}'`);
        }

        await this.client.resolveConflict(
          confined[0],
          msg.strategy as GraphResolveStrategy
        );
        await this.onRefresh();
      } else if (command === "deleteBranch" && typeof msg.branch === "string") {
        if (!isValidRefName(msg.branch)) {
          throw new Error(`Invalid branch name '${msg.branch}'`);
        }

        await vscode.commands.executeCommand("tes.deleteBranch", msg.branch);
      } else if (
        command === "discardWorking" &&
        typeof msg.branch === "string"
      ) {
        if (!isValidRefName(msg.branch)) {
          throw new Error(`Invalid branch name '${msg.branch}'`);
        }

        await vscode.commands.executeCommand(
          "tes.discardWorkingChanges",
          msg.branch
        );
      } else if (command === "refresh") {
        await this.onRefresh();
      }
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : String(err);
      void vscode.window.showErrorMessage(`Branch Graph: ${message}`);
    }
  }

  /**
   * Disposes the webview panel and all registered event subscriptions.
   */
  private dispose(): void {
    if (VisualGraphPanel.currentPanel === this) {
      VisualGraphPanel.currentPanel = undefined;
    }

    while (this.disposables.length > 0) {
      const d = this.disposables.pop();

      if (d) {
        d.dispose();
      }
    }

    this.panel.dispose();
  }
}
