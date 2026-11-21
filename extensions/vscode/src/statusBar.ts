/**
 * Status bar controller displaying the active editor branch, graph launcher, and conflict badge.
 */

import * as vscode from "vscode";
import { WorkspaceStateSnapshot } from "./client";

/**
 * Renders the Status Bar controls:
 * - Current Branch (`main`): click to switch the active branch.
 * - Branch Graph (`Graph`): always visible when initialized, opens the 2D graph.
 * - Conflict Resolver (`1 Conflict` / `N Conflicts`): visible alongside Graph whenever conflicts exist.
 */
export class StatusBarController implements vscode.Disposable {
  private readonly editorItem: vscode.StatusBarItem;
  private readonly graphItem: vscode.StatusBarItem;
  private readonly conflictItem: vscode.StatusBarItem;

  /**
   * Creates and configures the branch switcher, graph launcher, and conflict status bar items.
   */
  public constructor() {
    this.editorItem = vscode.window.createStatusBarItem(
      vscode.StatusBarAlignment.Left,
      98
    );
    this.editorItem.text = "$(source-control) main";
    this.editorItem.tooltip = "Switch branch";
    this.editorItem.command = "tes.switchEditorBranch";

    this.graphItem = vscode.window.createStatusBarItem(
      vscode.StatusBarAlignment.Left,
      97
    );
    this.graphItem.text = "$(git-commit) Graph";
    this.graphItem.tooltip = "Open Branch Graph";
    this.graphItem.command = "tes.openGraph";

    this.conflictItem = vscode.window.createStatusBarItem(
      vscode.StatusBarAlignment.Left,
      96
    );
    this.conflictItem.command = "tes.openConflictStudio";
    this.conflictItem.backgroundColor = new vscode.ThemeColor(
      "statusBarItem.errorBackground"
    );
    this.conflictItem.tooltip = "Open Conflict Resolver";
  }

  /**
   * Updates and shows status bar items from the latest snapshot.
   *
   * @param snapshot The latest repository state snapshot.
   */
  public update(snapshot: WorkspaceStateSnapshot): void {
    const editorBranch =
      snapshot.branchReport.editor_branch ||
      snapshot.branchReport.trunk_branch ||
      "main";
    this.editorItem.text = `$(source-control) ${editorBranch}`;
    this.editorItem.tooltip = `Current branch: ${editorBranch}. Click to switch.`;

    this.editorItem.show();
    this.graphItem.show();

    const conflictCount = snapshot.conflicts.length;

    if (conflictCount > 0) {
      const label =
        conflictCount === 1 ? "1 Conflict" : `${conflictCount} Conflicts`;
      this.conflictItem.text = `$(warning) ${label}`;
      this.conflictItem.show();
    } else {
      this.conflictItem.hide();
    }
  }

  /**
   * Hides status bar items when the workspace is not initialized with `tes`.
   */
  public hide(): void {
    this.editorItem.hide();
    this.graphItem.hide();
    this.conflictItem.hide();
  }

  /**
   * Disposes all status bar items.
   */
  public dispose(): void {
    this.editorItem.dispose();
    this.graphItem.dispose();
    this.conflictItem.dispose();
  }
}
