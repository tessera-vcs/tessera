/**
 * Status bar controller displaying the active editor branch, graph launcher, and conflict badge.
 */

import * as vscode from "vscode";
import { WorkspaceStateSnapshot } from "./client";

/**
 * Renders the Status Bar controls:
 * - Current Branch (`main`): click to switch the active branch.
 * - Branch Graph / Conflict Resolver (`Graph` / `N Conflicts`).
 */
export class StatusBarController implements vscode.Disposable {
  private readonly editorItem: vscode.StatusBarItem;
  private readonly graphItem: vscode.StatusBarItem;

  /**
   * Creates and configures the branch switcher and graph/conflict status bar items.
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
  }

  /**
   * Updates and shows status bar items from the latest snapshot.
   *
   * @param snapshot The latest repository state snapshot.
   */
  public update(snapshot: WorkspaceStateSnapshot): void {
    const editorBranch = snapshot.branchReport.editor_branch || "main";
    this.editorItem.text = `$(source-control) ${editorBranch}`;
    this.editorItem.tooltip = `Current branch: ${editorBranch}. Click to switch.`;

    const conflictCount = snapshot.conflicts.length;

    if (conflictCount > 0) {
      this.graphItem.text = `$(warning) ${conflictCount} Conflicts`;
      this.graphItem.command = "tes.openConflictStudio";
      this.graphItem.backgroundColor = new vscode.ThemeColor(
        "statusBarItem.errorBackground"
      );
      this.graphItem.tooltip = "Open Conflict Resolver";
    } else {
      this.graphItem.text = "$(git-commit) Graph";
      this.graphItem.command = "tes.openGraph";
      this.graphItem.backgroundColor = undefined;
      this.graphItem.tooltip = "Open Branch Graph";
    }

    this.editorItem.show();
    this.graphItem.show();
  }

  /**
   * Hides status bar items when the workspace is not initialized with `tes`.
   */
  public hide(): void {
    this.editorItem.hide();
    this.graphItem.hide();
  }

  /**
   * Disposes both status bar items.
   */
  public dispose(): void {
    this.editorItem.dispose();
    this.graphItem.dispose();
  }
}
