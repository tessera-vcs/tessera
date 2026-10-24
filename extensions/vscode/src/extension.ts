/**
 * Entry point for the VS Code / Antigravity / Cursor / Windsurf extension.
 * Registers tree views, status bar items, editor decorations, and command handlers.
 */

import { execFileSync } from "child_process";
import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  CliClient,
  isValidRefName,
  resolveConfinedPath,
  WorkspaceStateSnapshot,
} from "./client";
import { ConflictStudioPanel } from "./conflictPanel";
import { BranchContentProvider } from "./contentProvider";
import { AgentDecorationController } from "./decorations";
import { VisualGraphPanel } from "./graphPanel";
import { StatusBarController } from "./statusBar";
import {
  BranchesTreeProvider,
  ConflictsAndCheckpointsTreeProvider,
  SessionsTreeProvider,
  SidebarNode,
} from "./views";
import { StateWatcher } from "./watcher";

/**
 * Collects `startPid` and up to 6 ancestor PIDs (stopping before `stopPid` or PID <= 1)
 * for integrated terminal matching.
 *
 * @param startPid The starting process ID (typically the shell PID).
 * @param stopPid Optional ancestor PID at which traversal should stop.
 * @returns A set of collected ancestor process IDs.
 */
function collectAncestorPids(
  startPid: number | null | undefined,
  stopPid?: number | null
): Set<number> {
  const pids = new Set<number>();

  if (startPid === undefined || startPid === null || startPid <= 1) {
    return pids;
  }

  let curr = startPid;

  for (let i = 0; i < 6; i++) {
    if (
      curr <= 1 ||
      (stopPid !== undefined && stopPid !== null && curr === stopPid)
    ) {
      break;
    }

    pids.add(curr);

    try {
      if (process.platform === "win32") {
        const out = execFileSync(
          "powershell",
          [
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            `(Get-CimInstance Win32_Process -Filter 'ProcessId = ${curr}').ParentProcessId`,
          ],
          { encoding: "utf8", timeout: 800 }
        ).trim();
        const next = Number.parseInt(out, 10);

        if (Number.isNaN(next) || next <= 1 || next === curr) {
          break;
        }

        curr = next;
      } else {
        const out = execFileSync("ps", ["-o", "ppid=", "-p", String(curr)], {
          encoding: "utf8",
          timeout: 500,
        }).trim();
        const next = Number.parseInt(out, 10);

        if (Number.isNaN(next) || next <= 1 || next === curr) {
          break;
        }

        curr = next;
      }
    } catch {
      break;
    }
  }

  return pids;
}

interface SessionTerminalLink extends vscode.TerminalLink {
  readonly sessionId: string;
}

const SESSION_ID_LINK_PATTERN =
  /\b(?:antigravity|claude|codex|cursor|windsurf|kiro|opencode|copilot|term|session|mcp)-[a-z0-9][a-z0-9_-]{2,63}\b/g;

/**
 * Returns true if `text` still contains standard Git conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`).
 *
 * @param text The document text content to inspect.
 * @returns True if a complete Git conflict marker block is present.
 */
function hasGitConflictMarkers(text: string): boolean {
  let hasOpen = false;
  let hasSep = false;

  for (const line of text.split(/\r?\n/)) {
    if (line.startsWith("<<<<<<< ")) {
      hasOpen = true;
    } else if (hasOpen && line === "=======") {
      hasSep = true;
    } else if (hasOpen && hasSep && line.startsWith(">>>>>>> ")) {
      return true;
    }
  }

  return false;
}

/**
 * Activates the multi-agent and branch control extension.
 *
 * @param context The VS Code extension context used to register disposables.
 */
export function activate(context: vscode.ExtensionContext): void {
  const statusBar = new StatusBarController();
  context.subscriptions.push(statusBar);

  const folder = vscode.workspace.workspaceFolders?.[0];

  if (!folder) {
    return;
  }

  const client = new CliClient(folder.uri.fsPath);
  const repoRoot = client.getRepoRoot();
  const watcher = new StateWatcher(repoRoot);

  const branchesProvider = new BranchesTreeProvider();
  const sessionsProvider = new SessionsTreeProvider();
  const conflictsProvider = new ConflictsAndCheckpointsTreeProvider();
  const contentProvider = new BranchContentProvider(client);
  const decorationController = new AgentDecorationController(repoRoot);

  let latestSnapshot: WorkspaceStateSnapshot | undefined;
  const revealedConflicts = new Set<string>();

  /**
   * Opens a conflicted file in the editor and positions the cursor at the first `<<<<<<< ` marker.
   *
   * @param relPath The repository-relative path of the conflicted file.
   * @returns True if the file was found and opened in the editor.
   */
  const openConflictedFileInEditor = async (
    relPath: string
  ): Promise<boolean> => {
    if (!relPath || relPath === "<speculative-verify>") {
      return false;
    }

    const confined = resolveConfinedPath(repoRoot, relPath);

    if (!confined) {
      return false;
    }

    const [, absPath] = confined;

    if (!fs.existsSync(absPath)) {
      return false;
    }

    try {
      const doc = await vscode.workspace.openTextDocument(
        vscode.Uri.file(absPath)
      );
      let conflictLine = 0;

      for (let i = 0; i < doc.lineCount; i++) {
        if (doc.lineAt(i).text.startsWith("<<<<<<< ")) {
          conflictLine = i;
          break;
        }
      }

      const pos = new vscode.Position(conflictLine, 0);
      await vscode.window.showTextDocument(doc, {
        preview: false,
        selection: new vscode.Range(pos, pos),
      });

      return true;
    } catch {
      return false;
    }
  };

  context.subscriptions.push(
    watcher,
    decorationController,
    vscode.languages.registerCodeLensProvider(
      { scheme: "file" },
      decorationController
    ),
    vscode.workspace.registerTextDocumentContentProvider(
      BranchContentProvider.scheme,
      contentProvider
    ),
    vscode.window.registerTreeDataProvider("branches", branchesProvider),
    vscode.window.registerTreeDataProvider("scmBranches", branchesProvider),
    vscode.window.registerTreeDataProvider("sessions", sessionsProvider),
    vscode.window.registerTreeDataProvider("conflicts", conflictsProvider)
  );

  /**
   * Fetches the latest repository state snapshot and refreshes all views, decorations, and webviews.
   */
  const refreshAll = async (): Promise<void> => {
    if (!client.isInitialized()) {
      latestSnapshot = undefined;
      revealedConflicts.clear();
      branchesProvider.setSnapshot(undefined);
      sessionsProvider.setSnapshot(undefined);
      conflictsProvider.setSnapshot(undefined);
      decorationController.setSnapshot(undefined);
      statusBar.hide();
      return;
    }

    try {
      const snapshot = await client.fetchSnapshot();
      latestSnapshot = snapshot;

      branchesProvider.setSnapshot(snapshot);
      sessionsProvider.setSnapshot(snapshot);
      conflictsProvider.setSnapshot(snapshot);
      decorationController.setSnapshot(snapshot);
      statusBar.update(snapshot);
      VisualGraphPanel.updateIfVisible(snapshot);
      ConflictStudioPanel.updateIfVisible(snapshot);

      const currentConflictIds = new Set(snapshot.conflicts.map((c) => c.id));

      for (const id of revealedConflicts) {
        if (!currentConflictIds.has(id)) {
          revealedConflicts.delete(id);
        }
      }

      for (const c of snapshot.conflicts) {
        if (!revealedConflicts.has(c.id)) {
          revealedConflicts.add(c.id);
          void openConflictedFileInEditor(c.path);
        }
      }
    } catch {
      if (!client.isInitialized()) {
        latestSnapshot = undefined;
        revealedConflicts.clear();
        branchesProvider.setSnapshot(undefined);
        sessionsProvider.setSnapshot(undefined);
        conflictsProvider.setSnapshot(undefined);
        decorationController.setSnapshot(undefined);
        statusBar.hide();
      }
    }
  };

  /**
   * Switches the active editor working tree to `branch` and refreshes all UI surfaces.
   *
   * @param branch The target branch name.
   */
  const switchEditorBranch = async (branch: string): Promise<void> => {
    await client.switchEditorBranch(branch);
    await refreshAll();
  };

  context.subscriptions.push(
    watcher.onDidChange(() => {
      void refreshAll();
    }),
    vscode.workspace.onDidSaveTextDocument(async (doc) => {
      if (!latestSnapshot || latestSnapshot.conflicts.length === 0) {
        return;
      }

      const relPath = path
        .relative(repoRoot, doc.uri.fsPath)
        .split(path.sep)
        .join("/");
      const matchingConflict = latestSnapshot.conflicts.find(
        (c) => c.path === relPath
      );

      if (matchingConflict && !hasGitConflictMarkers(doc.getText())) {
        try {
          await client.resolveConflict(relPath, "working");
          await refreshAll();
        } catch {
          // Ignore if already resolved concurrently
        }
      }
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("tes.refresh", async () => {
      await refreshAll();
    }),

    vscode.commands.registerCommand(
      "tes.switchEditorBranch",
      async (arg?: string | SidebarNode) => {
        const directBranch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.branchName
              : undefined;

        if (directBranch) {
          await switchEditorBranch(directBranch);
          return;
        }

        const snap = await client.fetchSnapshot();
        latestSnapshot = snap;
        statusBar.update(snap);

        const sortedBranches = [...snap.branchReport.branch_details].sort(
          (a, b) => Number(b.is_editor_branch) - Number(a.is_editor_branch)
        );
        const items: vscode.QuickPickItem[] = sortedBranches.map((b) => ({
          label: b.name,
          iconPath: new vscode.ThemeIcon(
            b.is_editor_branch ? "check" : "source-control"
          ),
          description: b.is_editor_branch
            ? `current · ${b.short_commit} · :${b.port}`
            : `${b.short_commit} · :${b.port}`,
          detail:
            b.modified_files.length > 0
              ? `${b.modified_files.length} changed: ${b.modified_files.slice(0, 4).join(", ")}`
              : "clean",
        }));

        const picked = await vscode.window.showQuickPick(items, {
          placeHolder: `Switch branch (current: ${snap.branchReport.editor_branch})`,
        });

        if (picked) {
          await switchEditorBranch(picked.label);
        }
      }
    ),

    vscode.commands.registerCommand("tes.createBranch", async () => {
      const name = await vscode.window.showInputBox({
        prompt: "New branch name",
        placeHolder: "feature/my-branch",
      });

      if (!name || !name.trim()) {
        return;
      }

      await switchEditorBranch(name.trim());
    }),

    vscode.commands.registerCommand(
      "tes.mergeBranch",
      async (arg?: string | SidebarNode) => {
        const snap = latestSnapshot ?? (await client.fetchSnapshot());
        let sourceBranch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.branchName
              : undefined;

        if (!sourceBranch) {
          const candidates = snap.branchReport.branches.filter(
            (b) => b !== "main"
          );
          sourceBranch = await vscode.window.showQuickPick(candidates, {
            placeHolder: "Select branch to merge",
          });
        }

        if (!sourceBranch) {
          return;
        }

        const targetBranch = await vscode.window.showQuickPick(
          snap.branchReport.branches,
          {
            placeHolder: `Merge '${sourceBranch}' into target branch`,
          }
        );

        if (!targetBranch) {
          return;
        }

        const msg = await vscode.window.showInputBox({
          prompt: "Commit message",
          value: `feat: merge ${sourceBranch} into ${targetBranch}`,
        });

        if (!msg) {
          return;
        }

        await client.mergeInto([sourceBranch], targetBranch, msg);
        await refreshAll();
      }
    ),

    vscode.commands.registerCommand(
      "tes.restoreBranch",
      async (arg?: string | SidebarNode) => {
        const snap = latestSnapshot ?? (await client.fetchSnapshot());
        const targetBranch =
          arg instanceof SidebarNode && arg.branchName
            ? arg.branchName
            : snap.branchReport.editor_branch;

        if (arg instanceof SidebarNode && arg.checkpointLabel) {
          await client.restoreBranch(targetBranch, arg.checkpointLabel);
          await refreshAll();
          return;
        }

        const items: vscode.QuickPickItem[] = [];

        for (const cp of snap.checkpoints) {
          items.push({
            label: cp.label,
            description: `checkpoint (${cp.opId.slice(0, 8)})`,
          });
        }

        for (const c of snap.branchReport.commit_graph) {
          items.push({
            label: c.short_sha,
            description: c.subject,
            detail: `${c.author} · ${c.refs.join(", ")}`,
          });
        }

        const picked = await vscode.window.showQuickPick(items, {
          placeHolder: `Restore '${targetBranch}' to commit or checkpoint`,
        });

        if (picked) {
          await client.restoreBranch(targetBranch, picked.label);
          await refreshAll();
        }
      }
    ),

    vscode.commands.registerCommand("tes.createCheckpoint", async () => {
      const label = await vscode.window.showInputBox({
        prompt: "Checkpoint name",
        placeHolder: "before-refactor",
      });

      if (!label || !label.trim()) {
        return;
      }

      await client.createCheckpoint(label.trim());
      await refreshAll();
    }),

    vscode.commands.registerCommand(
      "tes.pushBranch",
      async (arg?: string | SidebarNode) => {
        const snap = latestSnapshot ?? (await client.fetchSnapshot());
        const branch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode && arg.branchName
              ? arg.branchName
              : snap.branchReport.editor_branch;
        await client.pushBranch(branch);
      }
    ),

    vscode.commands.registerCommand(
      "tes.openBranchTerminal",
      (arg?: string | SidebarNode) => {
        const rawBranch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.branchName
              : latestSnapshot?.branchReport.editor_branch ?? "main";
        const branch = rawBranch && isValidRefName(rawBranch) ? rawBranch : "main";
        const quotedBranch = `'${branch.replace(/'/g, "'\\''")}'`;
        const term = vscode.window.createTerminal({
          name: branch,
          cwd: repoRoot,
        });
        term.show();
        term.sendText(`tes switch -- ${quotedBranch}`);
      }
    ),

    vscode.commands.registerCommand(
      "tes.openBranchPort",
      async (arg?: number | SidebarNode) => {
        const port =
          typeof arg === "number"
            ? arg
            : arg instanceof SidebarNode && arg.port !== undefined
              ? arg.port
              : 3000;
        await vscode.env.openExternal(
          vscode.Uri.parse(`http://localhost:${port}`)
        );
      }
    ),

    vscode.commands.registerCommand(
      "tes.diffBranchFile",
      async (branch: string, relPath: string) => {
        const cleanPath = (relPath.split("::")[0]?.trim() || relPath).replace(
          /^\/+/,
          ""
        );
        const trunkBranch =
          latestSnapshot?.branchReport.trunk_branch || "main";

        if (branch === trunkBranch) {
          const leftUri = BranchContentProvider.buildUri("HEAD", cleanPath);
          const rightUri = vscode.Uri.file(path.join(repoRoot, cleanPath));
          await vscode.commands.executeCommand(
            "vscode.diff",
            leftUri,
            rightUri,
            `${cleanPath} (${trunkBranch} HEAD <-> Working Tree)`
          );
          return;
        }

        const leftUri = BranchContentProvider.buildUri(trunkBranch, cleanPath);
        const rightUri = BranchContentProvider.buildUri(branch, cleanPath);
        await vscode.commands.executeCommand(
          "vscode.diff",
          leftUri,
          rightUri,
          `${cleanPath} (${trunkBranch} <-> ${branch})`
        );
      }
    ),

    vscode.commands.registerCommand(
      "tes.openBranchDiff",
      async (branch: string, relPath: string) => {
        await vscode.commands.executeCommand(
          "tes.diffBranchFile",
          branch,
          relPath
        );
      }
    ),

    vscode.commands.registerCommand(
      "tes.dismissFileHighlights",
      (relPath?: string) => {
        decorationController.dismissForFile(relPath);
      }
    ),

    vscode.commands.registerCommand(
      "tes.deleteBranch",
      async (arg?: string | SidebarNode) => {
        const snap = latestSnapshot ?? (await client.fetchSnapshot());
        const trunk = snap.branchReport.trunk_branch || "main";
        let targetBranch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.branchName
              : undefined;

        if (!targetBranch) {
          const candidates = snap.branchReport.branches.filter(
            (b) => b !== trunk
          );
          targetBranch = await vscode.window.showQuickPick(candidates, {
            placeHolder: "Select branch to delete",
          });
        }

        if (!targetBranch || targetBranch === trunk) {
          return;
        }

        const choice = await vscode.window.showWarningMessage(
          `Delete branch '${targetBranch}'?`,
          { modal: true },
          "Delete",
          "Force Delete"
        );

        if (!choice) {
          return;
        }

        try {
          await client.deleteBranch(targetBranch, choice === "Force Delete");
          await refreshAll();
        } catch (err: unknown) {
          const message = err instanceof Error ? err.message : String(err);
          const forceChoice = await vscode.window.showWarningMessage(
            `Could not safely delete '${targetBranch}': ${message}`,
            { modal: true },
            "Force Delete (-D)"
          );

          if (forceChoice === "Force Delete (-D)") {
            await client.deleteBranch(targetBranch, true);
            await refreshAll();
          }
        }
      }
    ),

    vscode.commands.registerCommand(
      "tes.discardWorkingChanges",
      async (arg?: string | SidebarNode) => {
        const snap = latestSnapshot ?? (await client.fetchSnapshot());
        const targetBranch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode && arg.branchName
              ? arg.branchName
              : snap.branchReport.editor_branch;

        const choice = await vscode.window.showWarningMessage(
          `Discard uncommitted working changes on '${targetBranch}'?`,
          { modal: true },
          "Discard Changes"
        );

        if (choice !== "Discard Changes") {
          return;
        }

        await client.discardWorkingChanges(targetBranch);
        await refreshAll();
      }
    ),

    vscode.commands.registerCommand(
      "tes.focusSession",
      async (arg?: string | SidebarNode) => {
        const snap = latestSnapshot ?? (await client.fetchSnapshot());
        let sessionId =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.sessionId
              : undefined;

        const activeAgents = snap.radarReport.active_agents.filter(
          (a) => a.agent_id !== "editor"
        );

        if (!sessionId) {
          if (activeAgents.length === 0) {
            void vscode.window.showInformationMessage(
              "No active agent or terminal sessions to focus."
            );
            return;
          }

          const picked = await vscode.window.showQuickPick(
            activeAgents.map((a) => ({
              label: a.agent_id,
              description: a.target_branch ?? snap.branchReport.editor_branch,
              detail: a.intent,
            })),
            {
              placeHolder: "Select an active session to focus",
            }
          );

          if (!picked) {
            return;
          }

          sessionId = picked.label;
        }

        const resolvedSessionId = sessionId;
        const matchedAgent =
          activeAgents.find((a) => a.agent_id === resolvedSessionId) ??
          activeAgents.find((a) => a.agent_id.startsWith(resolvedSessionId));
        const targetId = matchedAgent?.agent_id ?? resolvedSessionId;
        const waypoint = matchedAgent?.waypoint;

        const ancestorPids = collectAncestorPids(
          waypoint?.shell_pid,
          waypoint?.ide_pid
        );

        for (const term of vscode.window.terminals) {
          const pid = await term.processId;

          if (
            (pid !== undefined &&
              (Boolean(waypoint?.tty) || process.platform === "win32") &&
              ancestorPids.has(pid)) ||
            term.name === targetId
          ) {
            term.show(false);
            vscode.window.setStatusBarMessage(
              `Focused integrated terminal for '${targetId}'`,
              4000
            );
            return;
          }
        }

        try {
          const report = await client.focusSession(targetId);
          const conversationId = waypoint?.conversation_id ?? undefined;
          const displayId = report.session_id || targetId;

          if (report.focused) {
            vscode.window.setStatusBarMessage(
              `Focused '${displayId}' (${report.mechanism})`,
              4000
            );
          } else if (
            !waypoint?.tty &&
            (waypoint?.harness === "antigravity" ||
              targetId.startsWith("antigravity-")) &&
            conversationId
          ) {
            try {
              await vscode.commands.executeCommand(
                "workbench.action.smartFocusConversation",
                conversationId
              );
              vscode.window.setStatusBarMessage(
                `Focused Antigravity conversation for '${displayId}'`,
                4000
              );
            } catch {
              await vscode.env.clipboard.writeText(conversationId);
              vscode.window.setStatusBarMessage(
                `Copied conversation ID for '${displayId}'`,
                4000
              );
            }
          } else {
            const actions: string[] = [];

            if (conversationId) {
              actions.push("Copy Conversation ID");
            }

            if (matchedAgent?.target_branch) {
              actions.push(`Switch to ${matchedAgent.target_branch}`);
            }

            const choice = await vscode.window.showInformationMessage(
              `${displayId}: ${report.detail}`,
              ...actions
            );

            if (choice === "Copy Conversation ID" && conversationId) {
              await vscode.env.clipboard.writeText(conversationId);
            } else if (
              matchedAgent?.target_branch &&
              choice === `Switch to ${matchedAgent.target_branch}`
            ) {
              await switchEditorBranch(matchedAgent.target_branch);
            }
          }
        } catch (err: unknown) {
          const message = err instanceof Error ? err.message : String(err);
          void vscode.window.showWarningMessage(
            `Could not focus session '${targetId}': ${message}`
          );
        }
      }
    ),

    vscode.commands.registerCommand(
      "tes.closeSession",
      async (arg?: string | SidebarNode) => {
        const sessionId =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.sessionId
              : undefined;

        if (!sessionId) {
          return;
        }

        await client.closeSession(sessionId);
        await refreshAll();
      }
    ),

    vscode.commands.registerCommand("tes.clearSessions", async () => {
      await client.clearSessions(false);
      await refreshAll();
    }),

    vscode.commands.registerCommand(
      "tes.popStash",
      async (arg?: number | SidebarNode) => {
        const index =
          typeof arg === "number"
            ? arg
            : arg instanceof SidebarNode && arg.stashIndex !== undefined
              ? arg.stashIndex
              : 0;

        await client.popStash(index);
        await refreshAll();
      }
    ),

    vscode.commands.registerCommand(
      "tes.dropStash",
      async (arg?: number | SidebarNode) => {
        const index =
          typeof arg === "number"
            ? arg
            : arg instanceof SidebarNode && arg.stashIndex !== undefined
              ? arg.stashIndex
              : 0;

        await client.dropStash(index);
        await refreshAll();
      }
    ),

    vscode.commands.registerCommand("tes.openGraph", async () => {
      const snap = latestSnapshot ?? (await client.fetchSnapshot());
      VisualGraphPanel.show(client, snap, switchEditorBranch, refreshAll);
    }),

    vscode.commands.registerCommand(
      "tes.openConflictStudio",
      async (focusPath?: string) => {
        const snap = latestSnapshot ?? (await client.fetchSnapshot());
        const targetPath =
          typeof focusPath === "string" && focusPath.length > 0
            ? focusPath
            : snap.conflicts.find((c) => c.path !== "<speculative-verify>")
                ?.path;

        if (targetPath && (await openConflictedFileInEditor(targetPath))) {
          return;
        }

        ConflictStudioPanel.show(client, snap, refreshAll, focusPath);
      }
    ),

    vscode.window.registerUriHandler({
      handleUri(uri: vscode.Uri): vscode.ProviderResult<void> {
        const params = new URLSearchParams(uri.query);
        const sessionId = params.get("session")?.trim();

        if (
          (uri.path === "/focus" || uri.path === "/session") &&
          sessionId &&
          isValidRefName(sessionId)
        ) {
          void vscode.commands.executeCommand("tes.focusSession", sessionId);
        }
      },
    }),

    vscode.window.registerTerminalLinkProvider({
      provideTerminalLinks(
        linkContext: vscode.TerminalLinkContext
      ): vscode.ProviderResult<SessionTerminalLink[]> {
        const line = linkContext.line;
        const links: SessionTerminalLink[] = [];
        const seenRanges = new Set<string>();
        const activeIds = new Set<string>(
          (latestSnapshot?.radarReport.active_agents ?? []).map(
            (a) => a.agent_id
          )
        );

        for (const activeId of activeIds) {
          let searchFrom = 0;

          while (searchFrom < line.length) {
            const idx = line.indexOf(activeId, searchFrom);

            if (idx === -1) {
              break;
            }

            const key = `${idx}:${activeId.length}`;

            if (!seenRanges.has(key)) {
              seenRanges.add(key);
              links.push({
                startIndex: idx,
                length: activeId.length,
                tooltip: `Focus session '${activeId}' in Tessera`,
                sessionId: activeId,
              });
            }

            searchFrom = idx + activeId.length;
          }
        }

        SESSION_ID_LINK_PATTERN.lastIndex = 0;
        let match = SESSION_ID_LINK_PATTERN.exec(line);

        while (match !== null) {
          const sessionId = match[0];
          const idx = match.index;
          const key = `${idx}:${sessionId.length}`;

          if (!seenRanges.has(key)) {
            seenRanges.add(key);
            links.push({
              startIndex: idx,
              length: sessionId.length,
              tooltip: `Focus session '${sessionId}' in Tessera`,
              sessionId,
            });
          }

          match = SESSION_ID_LINK_PATTERN.exec(line);
        }

        return links;
      },

      handleTerminalLink(link: SessionTerminalLink): vscode.ProviderResult<void> {
        if (isValidRefName(link.sessionId)) {
          void vscode.commands.executeCommand(
            "tes.focusSession",
            link.sessionId
          );
        }
      },
    })
  );

  void refreshAll();
}

/**
 * Deactivates the extension; all registered disposables are cleaned up via `context.subscriptions`.
 */
export function deactivate(): void {
  // Disposables are cleaned up via context.subscriptions
}
