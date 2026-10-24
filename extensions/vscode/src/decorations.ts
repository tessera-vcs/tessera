/**
 * Editor line-range decorations and CodeLens provider for live multi-agent attribution.
 */

import * as path from "path";
import * as vscode from "vscode";
import { FileLineAttribution, WorkspaceStateSnapshot } from "./client";

/**
 * Renders live multi-agent line-range decorations and CodeLenses directly inside open
 * text editors for active sessions working on the current editor branch.
 */
export class AgentDecorationController
  implements vscode.CodeLensProvider, vscode.Disposable
{
  private readonly repoRoot: string;
  private snapshot: WorkspaceStateSnapshot | undefined;
  private readonly slotDecorations: readonly vscode.TextEditorDecorationType[];
  private readonly collisionDecoration: vscode.TextEditorDecorationType;
  private readonly dismissedSignatures = new Map<string, string>();
  private readonly onDidChangeCodeLensesEmitter =
    new vscode.EventEmitter<void>();
  public readonly onDidChangeCodeLenses =
    this.onDidChangeCodeLensesEmitter.event;
  private readonly disposables: vscode.Disposable[] = [];

  /**
   * Initializes per-slot decoration styles and subscribes to editor visibility and document events.
   *
   * @param repoRoot The absolute path to the Git repository root.
   */
  public constructor(repoRoot: string) {
    this.repoRoot = repoRoot;

    const palette: readonly {
      readonly border: string;
      readonly bg: string;
    }[] = [
      {
        border: "charts.blue",
        bg: "diffEditor.insertedLineBackground",
      },
      {
        border: "charts.purple",
        bg: "editor.selectionHighlightBackground",
      },
      {
        border: "charts.green",
        bg: "diffEditor.insertedLineBackground",
      },
      {
        border: "charts.orange",
        bg: "editor.wordHighlightBackground",
      },
    ];

    this.slotDecorations = palette.map((colors) =>
      vscode.window.createTextEditorDecorationType({
        isWholeLine: true,
        borderWidth: "0 0 0 3px",
        borderStyle: "solid",
        borderColor: new vscode.ThemeColor(colors.border),
        backgroundColor: new vscode.ThemeColor(colors.bg),
        overviewRulerColor: new vscode.ThemeColor(colors.border),
        overviewRulerLane: vscode.OverviewRulerLane.Full,
      })
    );

    this.collisionDecoration = vscode.window.createTextEditorDecorationType({
      isWholeLine: true,
      borderWidth: "0 0 0 3px",
      borderStyle: "solid",
      borderColor: new vscode.ThemeColor("inputValidation.errorBorder"),
      backgroundColor: new vscode.ThemeColor(
        "diffEditor.removedLineBackground"
      ),
      overviewRulerColor: new vscode.ThemeColor("errorForeground"),
      overviewRulerLane: vscode.OverviewRulerLane.Full,
    });

    this.disposables.push(
      vscode.window.onDidChangeVisibleTextEditors(() => {
        this.applyToVisibleEditors();
      }),
      vscode.workspace.onDidChangeTextDocument(() => {
        this.applyToVisibleEditors();
      })
    );
  }

  /**
   * Updates the active repository snapshot and refreshes all visible editor decorations.
   *
   * @param snapshot The latest repository state snapshot, or `undefined` when uninitialized.
   */
  public setSnapshot(snapshot: WorkspaceStateSnapshot | undefined): void {
    this.snapshot = snapshot;
    this.applyToVisibleEditors();
    this.onDidChangeCodeLensesEmitter.fire();
  }

  /**
   * Dismisses the currently displayed highlights for `relPath` until new agent edits arrive.
   *
   * @param relPath Optional repository-relative path to dismiss highlights for.
   */
  public dismissForFile(relPath?: string): void {
    const targetRel =
      relPath ??
      (vscode.window.activeTextEditor
        ? this.resolveRelativePath(
            vscode.window.activeTextEditor.document.uri.fsPath
          )
        : undefined);

    if (!targetRel) {
      return;
    }

    const rawAttrs = this.getRawAttributionsForFile(targetRel);
    const sig = this.computeSignature(rawAttrs);
    this.dismissedSignatures.set(targetRel, sig);
    this.applyToVisibleEditors();
    this.onDidChangeCodeLensesEmitter.fire();
  }

  /**
   * Computes CodeLens badges at the top of files and hunks touched by active agents on the current branch.
   *
   * @param document The text document for which to provide CodeLenses.
   * @returns An array of CodeLens items for active agent attributions.
   */
  public provideCodeLenses(
    document: vscode.TextDocument
  ): vscode.CodeLens[] {
    if (!this.snapshot) {
      return [];
    }

    const relPath = this.resolveRelativePath(document.uri.fsPath);

    if (!relPath) {
      return [];
    }

    const attributions = this.getAttributionsForFile(relPath);

    if (attributions.length === 0) {
      return [];
    }

    const lenses: vscode.CodeLens[] = [];
    const byAgent = new Map<string, FileLineAttribution[]>();

    for (const attr of attributions) {
      const existing = byAgent.get(attr.agent_id) ?? [];
      existing.push(attr);
      byAgent.set(attr.agent_id, existing);
    }

    const topRange = new vscode.Range(0, 0, 0, 0);

    if (byAgent.size > 1) {
      const summaryParts: string[] = [];

      for (const [agentId] of byAgent) {
        summaryParts.push(agentId);
      }

      lenses.push(
        new vscode.CodeLens(topRange, {
          title: `${byAgent.size} agents concurrently editing this file (${this.getEditorBranch()}): ${summaryParts.join(" · ")}`,
          command: "tes.openGraph",
        })
      );
    }

    lenses.push(
      new vscode.CodeLens(topRange, {
        title: "Dismiss Highlights",
        command: "tes.dismissFileHighlights",
        arguments: [relPath],
      })
    );

    const maxDocLine = Math.max(document.lineCount - 1, 0);

    for (const attr of attributions) {
      const startLine0 = Math.min(
        Math.max(attr.start_line - 1, 0),
        maxDocLine
      );
      const range = new vscode.Range(startLine0, 0, startLine0, 0);
      const canJump = this.isTerminalNavigable(attr.agent_id);

      lenses.push(
        new vscode.CodeLens(
          range,
          canJump
            ? {
                title: `${attr.agent_id} · L${attr.start_line}-L${attr.end_line}: ${attr.intent} · Jump to Terminal`,
                command: "tes.focusSession",
                arguments: [attr.agent_id],
              }
            : {
                title: `${attr.agent_id} · L${attr.start_line}-L${attr.end_line}: ${attr.intent}`,
                command: "",
              }
        )
      );
    }

    return lenses;
  }

  /**
   * Applies gutter bars, background highlights, and inline labels to all visible editors.
   */
  public applyToVisibleEditors(): void {
    for (const editor of vscode.window.visibleTextEditors) {
      this.applyToEditor(editor);
    }
  }

  /**
   * Returns true if `agentId` corresponds to a CLI or terminal session with a navigable terminal waypoint.
   *
   * @param agentId The session identifier to inspect.
   * @returns True if the session has a terminal waypoint that can be focused.
   */
  private isTerminalNavigable(agentId: string): boolean {
    const activeAgents = this.snapshot?.radarReport.active_agents ?? [];
    const agent = activeAgents.find((a) => a.agent_id === agentId);
    const wp = agent?.waypoint;

    if (wp?.ide_pid !== undefined && wp?.ide_pid !== null && !wp?.tty) {
      return false;
    }

    return Boolean(
      wp?.tty || wp?.term_session_id || agentId.startsWith("term-")
    );
  }

  /**
   * Applies per-agent slot and collision line decorations to a single `TextEditor`.
   *
   * @param editor The visible text editor to decorate.
   */
  private applyToEditor(editor: vscode.TextEditor): void {
    const clearAll = (): void => {
      for (const dec of this.slotDecorations) {
        editor.setDecorations(dec, []);
      }

      editor.setDecorations(this.collisionDecoration, []);
    };

    if (!this.snapshot) {
      clearAll();
      return;
    }

    const relPath = this.resolveRelativePath(editor.document.uri.fsPath);

    if (!relPath) {
      clearAll();
      return;
    }

    const attributions = this.getAttributionsForFile(relPath);

    if (attributions.length === 0) {
      clearAll();
      return;
    }

    const maxLine0 = Math.max(editor.document.lineCount - 1, 0);
    const lineOwners = new Map<number, FileLineAttribution[]>();

    for (const attr of attributions) {
      const start0 = Math.min(Math.max(attr.start_line - 1, 0), maxLine0);
      const end0 = Math.min(Math.max(attr.end_line - 1, start0), maxLine0);

      for (let line = start0; line <= end0; line++) {
        const list = lineOwners.get(line) ?? [];
        const alreadyListed = list.some((a) => a.agent_id === attr.agent_id);

        if (!alreadyListed) {
          list.push(attr);
          lineOwners.set(line, list);
        }
      }
    }

    const slotBuckets: vscode.DecorationOptions[][] =
      this.slotDecorations.map(() => []);
    const collisionOptions: vscode.DecorationOptions[] = [];
    const hunkHeaderLines = new Map<number, FileLineAttribution[]>();

    for (const attr of attributions) {
      const start0 = Math.min(Math.max(attr.start_line - 1, 0), maxLine0);
      const existing = hunkHeaderLines.get(start0) ?? [];
      existing.push(attr);
      hunkHeaderLines.set(start0, existing);
    }

    for (const [line0, owners] of lineOwners.entries()) {
      const lineRange = editor.document.lineAt(line0).range;
      const hoverMd = new vscode.MarkdownString();
      hoverMd.isTrusted = {
        enabledCommands: ["tes.focusSession", "tes.openGraph"],
      };
      hoverMd.supportThemeIcons = true;

      for (const owner of owners) {
        const encodedArgs = encodeURIComponent(
          JSON.stringify([owner.agent_id])
        );
        const jumpLink = this.isTerminalNavigable(owner.agent_id)
          ? `[$(terminal) Jump to Terminal](command:tes.focusSession?${encodedArgs}) · `
          : "";
        const safeAgent = owner.agent_id.replace(/[\\`*_{}[\]()#+\-.!|<>]/g, "\\$&");
        const safeBranch = owner.branch.replace(/[`\\<>]/g, "\\$&");
        const safeIntent = owner.intent
          .replace(/[\\`*_{}[\]()#+\-.!|<>]/g, "\\$&")
          .replace(/\r?\n/g, " ");
        hoverMd.appendMarkdown(
          `**${safeAgent}** (\`${safeBranch}\` · lines ${owner.start_line}-${owner.end_line})\n\n${safeIntent}\n\n${jumpLink}[$(git-commit) Branch Graph](command:tes.openGraph)\n\n---\n\n`
        );
      }

      const headerAttrs = hunkHeaderLines.get(line0);
      let afterRender:
        | vscode.ThemableDecorationAttachmentRenderOptions
        | undefined;

      if (headerAttrs && headerAttrs.length > 0) {
        const isCollision = owners.length > 1;
        const labels = headerAttrs.map((a) => `${a.agent_id}: ${a.intent}`);

        afterRender = {
          contentText: `    ${labels.join("  |  ")}`,
          color: new vscode.ThemeColor(
            isCollision ? "errorForeground" : "editorCodeLens.foreground"
          ),
          fontStyle: "italic",
        };
      }

      const option: vscode.DecorationOptions = {
        range: lineRange,
        hoverMessage: hoverMd,
        ...(afterRender ? { renderOptions: { after: afterRender } } : {}),
      };

      if (owners.length > 1) {
        collisionOptions.push(option);
        continue;
      }

      const primary = owners[0];

      if (!primary) {
        continue;
      }

      const slotIdx =
        Math.abs((primary.slot || 1) - 1) % this.slotDecorations.length;
      slotBuckets[slotIdx]?.push(option);
    }

    for (let i = 0; i < this.slotDecorations.length; i++) {
      const dec = this.slotDecorations[i];

      if (!dec) {
        continue;
      }

      editor.setDecorations(dec, slotBuckets[i] ?? []);
    }

    editor.setDecorations(this.collisionDecoration, collisionOptions);
  }

  /**
   * Returns the active editor branch name from the latest snapshot.
   *
   * @returns The active editor branch name, defaulting to `"main"`.
   */
  private getEditorBranch(): string {
    return (
      this.snapshot?.branchReport.editor_branch ||
      this.snapshot?.branchReport.trunk_branch ||
      "main"
    );
  }

  /**
   * Computes a deterministic signature string for a set of line attributions to track dismissals.
   *
   * @param attrs The line attributions for a single file.
   * @returns A pipe-delimited signature string.
   */
  private computeSignature(attrs: readonly FileLineAttribution[]): string {
    return attrs
      .map((a) => `${a.agent_id}:${a.branch}:${a.start_line}-${a.end_line}`)
      .join("|");
  }

  /**
   * Filters raw line attributions matching `relPath` on the active editor branch.
   *
   * @param relPath The repository-relative file path.
   * @returns Matching line attributions on the active editor branch.
   */
  private getRawAttributionsForFile(
    relPath: string
  ): readonly FileLineAttribution[] {
    const editorBranch = this.getEditorBranch();
    const all = this.snapshot?.radarReport.file_attributions ?? [];

    return all.filter(
      (a) =>
        a.path === relPath &&
        a.branch === editorBranch &&
        a.in_working_tree
    );
  }

  /**
   * Returns active line attributions for `relPath`, respecting any user dismissal for the current signature.
   *
   * @param relPath The repository-relative file path.
   * @returns Active non-dismissed line attributions for the file.
   */
  private getAttributionsForFile(
    relPath: string
  ): readonly FileLineAttribution[] {
    const raw = this.getRawAttributionsForFile(relPath);

    if (raw.length === 0) {
      this.dismissedSignatures.delete(relPath);
      return [];
    }

    const sig = this.computeSignature(raw);

    if (this.dismissedSignatures.get(relPath) === sig) {
      return [];
    }

    return raw;
  }

  /**
   * Converts an absolute filesystem path into a normalized repository-relative slash path.
   *
   * @param fsPath The absolute filesystem path to convert.
   * @returns The normalized repository-relative path, or `undefined` if outside the working tree.
   */
  private resolveRelativePath(fsPath: string): string | undefined {
    const normalizedRoot = path.resolve(this.repoRoot);
    const normalizedFile = path.resolve(fsPath);

    if (normalizedFile.startsWith(normalizedRoot + path.sep)) {
      const rel = path
        .relative(normalizedRoot, normalizedFile)
        .split(path.sep)
        .join("/");

      if (!rel.startsWith(".git/") && !rel.startsWith(".tessera/")) {
        return rel;
      }
    }

    return undefined;
  }

  /**
   * Disposes all editor decoration types, event emitters, and listener subscriptions.
   */
  public dispose(): void {
    for (const dec of this.slotDecorations) {
      dec.dispose();
    }

    this.collisionDecoration.dispose();
    this.onDidChangeCodeLensesEmitter.dispose();

    while (this.disposables.length > 0) {
      const d = this.disposables.pop();

      if (d) {
        d.dispose();
      }
    }
  }
}
