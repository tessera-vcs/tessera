/**
 * Entry point for the VS Code / Antigravity / Cursor / Windsurf extension.
 * Registers tree views, status bar items, editor decorations, and command handlers.
 */

import { execFile } from "child_process";
import * as fs from "fs";
import * as path from "path";
import { promisify } from "util";
import * as vscode from "vscode";
import {
  CliClient,
  isSyntheticConflictPath,
  isValidRefName,
  isValidSafeIdentifier,
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

const execFileAsync = promisify(execFile);

/**
 * Collects `startPid` and up to 6 ancestor PIDs asynchronously (stopping before `stopPid`
 * or PID <= 1) for integrated terminal matching without blocking the extension host thread.
 *
 * @param startPid The starting process ID (typically the shell PID).
 * @param stopPid Optional ancestor PID at which traversal should stop.
 * @returns A set of collected ancestor process IDs.
 */
async function collectAncestorPids(
  startPid: unknown,
  stopPid?: unknown
): Promise<Set<number>> {
  const pids = new Set<number>();

  if (
    typeof startPid !== "number" ||
    !Number.isSafeInteger(startPid) ||
    startPid <= 1
  ) {
    return pids;
  }

  const validStopPid =
    typeof stopPid === "number" &&
    Number.isSafeInteger(stopPid) &&
    stopPid > 1
      ? stopPid
      : undefined;

  let curr = startPid;

  for (let i = 0; i < 6; i++) {
    if (
      !Number.isSafeInteger(curr) ||
      curr <= 1 ||
      (validStopPid !== undefined && curr === validStopPid)
    ) {
      break;
    }

    pids.add(curr);

    try {
      let stdout: string;

      if (process.platform === "win32") {
        const res = await execFileAsync(
          "powershell",
          [
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            `(Get-CimInstance Win32_Process -Filter 'ProcessId = ${curr}').ParentProcessId`,
          ],
          { encoding: "utf8", timeout: 800, windowsHide: true }
        );
        stdout = res.stdout.trim();
      } else {
        const res = await execFileAsync("ps", ["-o", "ppid=", "-p", String(curr)], {
          encoding: "utf8",
          timeout: 500,
        });
        stdout = res.stdout.trim();
      }

      if (!/^\d+$/.test(stdout)) {
        break;
      }

      const next = Number.parseInt(stdout, 10);

      if (!Number.isSafeInteger(next) || next <= 1 || next === curr) {
        break;
      }

      curr = next;
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
  /(?<![A-Za-z0-9_-])(?:antigravity|claude|codex|cursor|windsurf|kiro|opencode|copilot|gemini|aider|amp|goose|augment|term|session|mcp)-(?:\d{2,10}|(?=[0-9a-f_-]*\d)[0-9a-f][0-9a-f_-]{3,63})(?![A-Za-z0-9_-])/g;

/**
 * Returns true if `ch` is a session/identifier character (`[A-Za-z0-9_-]`).
 *
 * @param ch A single character from the terminal line, or `undefined` at string boundaries.
 * @returns True if `ch` is part of an identifier token.
 */
function isIdentifierChar(ch: string | undefined): boolean {
  return ch !== undefined && /[A-Za-z0-9_-]/.test(ch);
}

/**
 * Returns true if `text` contains any standard or leftover Git/diff3 conflict markers
 * (`<<<<<<<`, `=======`, `>>>>>>>`, `|||||||`).
 *
 * @param text The document text content to inspect.
 * @returns True if any conflict marker line is present.
 */
function hasGitConflictMarkers(text: string): boolean {
  for (const line of text.split(/\r?\n/)) {
    if (
      /^<{7}(?:\s|$)/.test(line) ||
      /^={7}$/.test(line) ||
      /^>{7}(?:\s|$)/.test(line) ||
      /^\|{7}(?:\s|$)/.test(line)
    ) {
      return true;
    }
  }

  return false;
}

/**
 * Formats a shell command to invoke `tes switch -- <branch>` cleanly across
 * Windows `cmd.exe`, PowerShell, Fish, and POSIX shells.
 *
 * @param binPath The resolved `tes` executable path or binary name.
 * @param branch The validated Git branch name.
 * @returns A shell-safe command line string for `Terminal.sendText`.
 */
function formatSwitchTerminalCommand(binPath: string, branch: string): string {
  const shellLower = (vscode.env.shell || "").toLowerCase();

  if (process.platform === "win32") {
    if (shellLower.endsWith("cmd.exe") || shellLower.endsWith("cmd")) {
      const cmdBin =
        binPath === "tes" ? "tes" : `"${binPath.replace(/"/g, '""')}"`;
      return `${cmdBin} switch -- "${branch}"`;
    }

    if (
      shellLower.includes("bash") ||
      shellLower.includes("zsh") ||
      shellLower.endsWith("sh.exe")
    ) {
      const posixBin =
        binPath === "tes"
          ? "tes"
          : `'${binPath.replace(/'/g, "'\\''")}'`;
      const posixBranch = `'${branch.replace(/'/g, "'\\''")}'`;
      return `${posixBin} switch -- ${posixBranch}`;
    }

    const psBin =
      binPath === "tes" ? "tes" : `& '${binPath.replace(/'/g, "''")}'`;
    const psBranch = `'${branch.replace(/'/g, "''")}'`;
    return `${psBin} switch -- ${psBranch}`;
  }

  if (shellLower.endsWith("fish")) {
    const fishBin =
      binPath === "tes"
        ? "tes"
        : `'${binPath.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'`;
    const fishBranch = `'${branch.replace(/\\/g, "\\\\").replace(/'/g, "\\'")}'`;
    return `${fishBin} switch -- ${fishBranch}`;
  }

  const posixBin =
    binPath === "tes" ? "tes" : `'${binPath.replace(/'/g, "'\\''")}'`;
  const posixBranch = `'${branch.replace(/'/g, "'\\''")}'`;
  return `${posixBin} switch -- ${posixBranch}`;
}

/**
 * Formats an unknown error value into a concise single-line message and detailed log string.
 *
 * @param err The caught error object.
 * @returns Tuple of `[conciseMessage, detailedLogMessage]`.
 */
function formatErrorDetails(err: unknown): readonly [string, string] {
  if (typeof err === "object" && err !== null) {
    const rec = err as { readonly message?: unknown; readonly stderr?: unknown };
    const stderr =
      typeof rec.stderr === "string" ? rec.stderr.trim() : "";
    const msg =
      typeof rec.message === "string" ? rec.message.trim() : String(err);
    const concise = stderr || msg;
    const detail = stderr && stderr !== msg ? `${msg}\n${stderr}` : msg;
    return [concise, detail] as const;
  }

  const text = String(err);
  return [text, text] as const;
}

/**
 * Selects the primary workspace folder to bind, preferring the first folder initialized with Tessera.
 *
 * @returns The selected `WorkspaceFolder`, or `undefined` when no folder is open.
 */
function pickPrimaryWorkspaceFolder(): vscode.WorkspaceFolder | undefined {
  const folders = vscode.workspace.workspaceFolders;

  if (!folders || folders.length === 0) {
    return undefined;
  }

  for (const folder of folders) {
    if (new CliClient(folder.uri.fsPath).isInitialized()) {
      return folder;
    }
  }

  return folders[0];
}

let activeWorkspaceCleanup: (() => void) | undefined;

/**
 * Activates the multi-agent and branch control extension.
 *
 * @param context The VS Code extension context used to register disposables.
 */
export function activate(context: vscode.ExtensionContext): void {
  const outputChannel = vscode.window.createOutputChannel("Tessera");
  const statusBar = new StatusBarController();
  const branchesProvider = new BranchesTreeProvider();
  const sessionsProvider = new SessionsTreeProvider();
  const conflictsProvider = new ConflictsAndCheckpointsTreeProvider();

  context.subscriptions.push(
    outputChannel,
    statusBar,
    branchesProvider,
    sessionsProvider,
    conflictsProvider,
    vscode.window.registerTreeDataProvider("branches", branchesProvider),
    vscode.window.registerTreeDataProvider("scmBranches", branchesProvider),
    vscode.window.registerTreeDataProvider("sessions", sessionsProvider),
    vscode.window.registerTreeDataProvider("conflicts", conflictsProvider)
  );

  /**
   * Logs an error with timestamp and stderr details to the Tessera output channel.
   *
   * @param scope The operation or component where the error occurred.
   * @param err The caught error object.
   * @returns A concise user-facing error message.
   */
  const logError = (scope: string, err: unknown): string => {
    const [concise, detail] = formatErrorDetails(err);
    outputChannel.appendLine(`[${new Date().toISOString()}] ${scope}: ${detail}`);
    return concise;
  };

  let client: CliClient | undefined;
  let repoRoot = "";
  let contentProvider: BranchContentProvider | undefined;
  let decorationController: AgentDecorationController | undefined;
  let latestSnapshot: WorkspaceStateSnapshot | undefined;
  const revealedConflicts = new Set<string>();
  const workspaceDisposables: vscode.Disposable[] = [];
  let isRefreshing = false;
  let refreshQueued = false;

  /**
   * Clears all UI surfaces when the workspace is untrusted, closed, or uninitialized.
   */
  const clearAllViews = (): void => {
    latestSnapshot = undefined;
    revealedConflicts.clear();
    branchesProvider.setSnapshot(undefined);
    sessionsProvider.setSnapshot(undefined);
    conflictsProvider.setSnapshot(undefined);
    decorationController?.setSnapshot(undefined);
    statusBar.hide();
  };

  /**
   * Disposes all workspace-scoped watchers, providers, and open webview panels.
   */
  const disposeWorkspaceBindings = (): void => {
    VisualGraphPanel.disposeCurrent();
    ConflictStudioPanel.disposeCurrent();

    while (workspaceDisposables.length > 0) {
      const d = workspaceDisposables.pop();

      if (d) {
        d.dispose();
      }
    }

    contentProvider = undefined;
    decorationController = undefined;
    client = undefined;
    repoRoot = "";
    clearAllViews();
  };

  activeWorkspaceCleanup = disposeWorkspaceBindings;
  context.subscriptions.push({ dispose: disposeWorkspaceBindings });

  /**
   * Opens a conflicted file in the editor and positions the cursor at the first `<<<<<<<` marker.
   *
   * @param relPath The repository-relative path of the conflicted file.
   * @returns True if the file was found and opened in the editor.
   */
  const openConflictedFileInEditor = async (
    relPath: string
  ): Promise<boolean> => {
    if (!repoRoot || !relPath || isSyntheticConflictPath(relPath)) {
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

      return true;
    } catch (err: unknown) {
      logError(`openConflictedFileInEditor(${relPath})`, err);
      return false;
    }
  };

  /**
   * Fetches the latest repository state snapshot and refreshes all views, decorations,
   * open virtual diffs, and webviews. Coalesces concurrent triggers.
   */
  const refreshAll = async (): Promise<void> => {
    if (!vscode.workspace.isTrusted || !client) {
      clearAllViews();
      return;
    }

    if (isRefreshing) {
      refreshQueued = true;
      return;
    }

    isRefreshing = true;

    try {
      do {
        refreshQueued = false;
        const activeClient: CliClient | undefined = client;

        if (!activeClient || !activeClient.isInitialized()) {
          clearAllViews();
          return;
        }

        try {
          const snapshot = await activeClient.fetchSnapshot();

          if (client !== activeClient) {
            return;
          }

          latestSnapshot = snapshot;

          branchesProvider.setSnapshot(snapshot);
          sessionsProvider.setSnapshot(snapshot);
          conflictsProvider.setSnapshot(snapshot);
          decorationController?.setSnapshot(snapshot);
          statusBar.update(snapshot);
          contentProvider?.notifyDidChange();
          VisualGraphPanel.updateIfVisible(snapshot);
          ConflictStudioPanel.updateIfVisible(snapshot);

          const currentConflictIds = new Set(
            snapshot.conflicts.map((c) => c.id)
          );

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
        } catch (err: unknown) {
          logError("refreshAll", err);

          if (!activeClient.isInitialized()) {
            clearAllViews();
          }
        }
      } while (refreshQueued);
    } finally {
      isRefreshing = false;
    }
  };

  /**
   * Binds watchers, decoration controllers, and virtual content providers to the active workspace folder.
   */
  const initializeWorkspace = (): void => {
    disposeWorkspaceBindings();

    if (!vscode.workspace.isTrusted) {
      outputChannel.appendLine(
        `[${new Date().toISOString()}] Tessera paused in Restricted Mode until workspace trust is granted.`
      );
      return;
    }

    const folder = pickPrimaryWorkspaceFolder();

    if (!folder) {
      return;
    }

    const nextClient = new CliClient(folder.uri.fsPath);
    const nextRepoRoot = nextClient.getRepoRoot();
    const nextWatcher = new StateWatcher(nextRepoRoot);
    const nextContentProvider = new BranchContentProvider(nextClient, logError);
    const nextDecorationController = new AgentDecorationController(nextRepoRoot);

    client = nextClient;
    repoRoot = nextRepoRoot;
    contentProvider = nextContentProvider;
    decorationController = nextDecorationController;

    workspaceDisposables.push(
      nextWatcher,
      nextContentProvider,
      nextDecorationController,
      nextWatcher.onDidChange(() => {
        void refreshAll();
      }),
      vscode.languages.registerCodeLensProvider(
        { scheme: "file" },
        nextDecorationController
      ),
      vscode.workspace.registerTextDocumentContentProvider(
        BranchContentProvider.scheme,
        nextContentProvider
      )
    );

    void refreshAll();
  };

  /**
   * Ensures the workspace is trusted and bound to a `CliClient` before running a command.
   *
   * @returns The active `CliClient`, or `undefined` after showing a user notification.
   */
  const requireTrustedClient = (): CliClient | undefined => {
    if (!vscode.workspace.isTrusted) {
      void vscode.window.showWarningMessage(
        "Tessera is disabled in Restricted Mode. Trust this workspace to run Tessera commands."
      );
      return undefined;
    }

    if (!client) {
      void vscode.window.showWarningMessage(
        "Open a workspace folder to use Tessera."
      );
      return undefined;
    }

    return client;
  };

  /**
   * Switches the active editor working tree to `branch` and refreshes all UI surfaces.
   *
   * @param branch The target branch name.
   */
  const switchEditorBranch = async (branch: string): Promise<void> => {
    const activeClient = requireTrustedClient();

    if (!activeClient) {
      return;
    }

    await activeClient.switchEditorBranch(branch);
    await refreshAll();
  };

  /**
   * Registers a command handler wrapped in standardized error logging and user notification.
   *
   * @param commandId The VS Code command identifier.
   * @param handler The async or sync command callback.
   * @returns A disposable for the command registration.
   */
  const registerSafeCommand = <TArgs extends unknown[]>(
    commandId: string,
    handler: (...args: TArgs) => Promise<void> | void
  ): vscode.Disposable =>
    vscode.commands.registerCommand(commandId, async (...args: TArgs) => {
      try {
        await handler(...args);
      } catch (err: unknown) {
        const message = logError(commandId, err);
        void vscode.window.showErrorMessage(`Tessera: ${message}`);
      }
    });

  context.subscriptions.push(
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      initializeWorkspace();
    }),
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      initializeWorkspace();
    }),
    vscode.workspace.onDidSaveTextDocument(async (doc) => {
      if (
        !vscode.workspace.isTrusted ||
        !client ||
        !repoRoot ||
        !latestSnapshot ||
        latestSnapshot.conflicts.length === 0 ||
        doc.uri.scheme !== "file"
      ) {
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
        } catch (err: unknown) {
          logError(`onDidSaveTextDocument.resolveConflict(${relPath})`, err);
        }
      }
    })
  );

  context.subscriptions.push(
    registerSafeCommand("tes.refresh", async () => {
      if (!requireTrustedClient()) {
        return;
      }

      await refreshAll();
    }),

    registerSafeCommand(
      "tes.switchEditorBranch",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

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

        const snap = await activeClient.fetchSnapshot();
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

    registerSafeCommand("tes.createBranch", async () => {
      const activeClient = requireTrustedClient();

      if (!activeClient) {
        return;
      }

      const name = await vscode.window.showInputBox({
        prompt: "New branch name",
        placeHolder: "feature/my-branch",
        validateInput: (val) => {
          const trimmed = val.trim();

          if (!trimmed) {
            return "Branch name cannot be empty.";
          }

          if (!isValidRefName(trimmed)) {
            return "Invalid Git branch name.";
          }

          return undefined;
        },
      });

      if (!name || !name.trim()) {
        return;
      }

      await activeClient.createBranch(name.trim());
      await refreshAll();
    }),

    registerSafeCommand(
      "tes.mergeBranch",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
        const trunk = snap.branchReport.trunk_branch || "main";
        let sourceBranch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.branchName
              : undefined;

        if (!sourceBranch) {
          const candidates = snap.branchReport.branches.filter(
            (b) => b !== trunk
          );

          if (candidates.length === 0) {
            void vscode.window.showInformationMessage(
              "No non-trunk branches available to merge."
            );
            return;
          }

          sourceBranch = await vscode.window.showQuickPick(candidates, {
            placeHolder: "Select branch to merge",
          });
        }

        if (!sourceBranch) {
          return;
        }

        const targetCandidates = snap.branchReport.branches.filter(
          (b) => b !== sourceBranch
        );

        if (targetCandidates.length === 0) {
          void vscode.window.showInformationMessage(
            `No other branch available as a merge target for '${sourceBranch}'.`
          );
          return;
        }

        const orderedTargets = [...targetCandidates].sort((a, b) => {
          if (a === trunk) {
            return -1;
          }

          if (b === trunk) {
            return 1;
          }

          return a.localeCompare(b);
        });

        const targetBranch = await vscode.window.showQuickPick(orderedTargets, {
          placeHolder: `Merge '${sourceBranch}' into target branch`,
        });

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

        await activeClient.mergeInto([sourceBranch], targetBranch, msg);
        await refreshAll();
      }
    ),

    registerSafeCommand(
      "tes.restoreBranch",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
        let targetBranch = snap.branchReport.editor_branch;
        let selectedRev: string | undefined;

        if (typeof arg === "string" && arg.trim().length > 0) {
          const trimmedArg = arg.trim();

          if (snap.checkpoints.some((cp) => cp.label === trimmedArg)) {
            selectedRev = trimmedArg;
          } else if (snap.branchReport.branches.includes(trimmedArg)) {
            targetBranch = trimmedArg;
          } else {
            selectedRev = trimmedArg;
          }
        } else if (arg instanceof SidebarNode) {
          if (arg.branchName) {
            targetBranch = arg.branchName;
          }

          if (arg.checkpointLabel) {
            selectedRev = arg.checkpointLabel;
          }
        }

        if (!selectedRev) {
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

          if (!picked) {
            return;
          }

          selectedRev = picked.label;
        }

        const choice = await vscode.window.showWarningMessage(
          `Restore branch '${targetBranch}' to '${selectedRev}'? Uncommitted changes may be overwritten.`,
          { modal: true },
          "Restore"
        );

        if (choice !== "Restore") {
          return;
        }

        await activeClient.restoreBranch(targetBranch, selectedRev);
        await refreshAll();
      }
    ),

    registerSafeCommand("tes.createCheckpoint", async () => {
      const activeClient = requireTrustedClient();

      if (!activeClient) {
        return;
      }

      const label = await vscode.window.showInputBox({
        prompt: "Checkpoint name",
        placeHolder: "before-refactor",
        validateInput: (val) => {
          const trimmed = val.trim();

          if (!trimmed) {
            return "Checkpoint name cannot be empty.";
          }

          if (!isValidRefName(trimmed)) {
            return "Invalid checkpoint name.";
          }

          return undefined;
        },
      });

      if (!label || !label.trim()) {
        return;
      }

      await activeClient.createCheckpoint(label.trim());
      await refreshAll();
    }),

    registerSafeCommand(
      "tes.pushBranch",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
        const branch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode && arg.branchName
              ? arg.branchName
              : snap.branchReport.editor_branch;

        await vscode.window.withProgress(
          {
            location: vscode.ProgressLocation.Notification,
            title: `Pushing branch '${branch}' to origin...`,
            cancellable: false,
          },
          async () => {
            await activeClient.pushBranch(branch);
          }
        );

        void vscode.window.showInformationMessage(
          `Pushed branch '${branch}' to origin.`
        );
        await refreshAll();
      }
    ),

    registerSafeCommand(
      "tes.openBranchTerminal",
      (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const trunk = latestSnapshot?.branchReport.trunk_branch || "main";
        const rawBranch =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.branchName
              : (latestSnapshot?.branchReport.editor_branch ?? trunk);
        const branch =
          rawBranch && isValidRefName(rawBranch) ? rawBranch : trunk;
        const binPath = activeClient.resolveBinPath();
        const commandText = formatSwitchTerminalCommand(binPath, branch);
        const term = vscode.window.createTerminal({
          name: branch,
          cwd: activeClient.getRepoRoot(),
        });
        term.show();
        term.sendText(commandText);
      }
    ),

    registerSafeCommand(
      "tes.openBranchPort",
      async (arg?: number | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        let port: number | undefined;

        if (
          typeof arg === "number" &&
          Number.isInteger(arg) &&
          arg > 0 &&
          arg <= 65535
        ) {
          port = arg;
        } else if (
          arg instanceof SidebarNode &&
          arg.port !== undefined &&
          Number.isInteger(arg.port) &&
          arg.port > 0 &&
          arg.port <= 65535
        ) {
          port = arg.port;
        } else {
          const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
          const activeDetail =
            snap.branchReport.branch_details.find((b) => b.is_editor_branch) ??
            snap.branchReport.branch_details.find(
              (b) => b.name === snap.branchReport.editor_branch
            );
          port = activeDetail?.port;
        }

        if (!port || !Number.isInteger(port) || port <= 0 || port > 65535) {
          void vscode.window.showWarningMessage(
            "No valid dev server port is configured for the active branch."
          );
          return;
        }

        await vscode.env.openExternal(
          vscode.Uri.parse(`http://localhost:${port}`)
        );
      }
    ),

    registerSafeCommand(
      "tes.diffBranchFile",
      async (branch?: string, relPath?: string) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
        const trunkBranch = snap.branchReport.trunk_branch || "main";
        const editorBranch = snap.branchReport.editor_branch || trunkBranch;
        const targetBranch =
          typeof branch === "string" && branch.trim().length > 0
            ? branch.trim()
            : editorBranch;

        let targetRelPath =
          typeof relPath === "string" && relPath.trim().length > 0
            ? relPath
            : undefined;

        if (!targetRelPath && vscode.window.activeTextEditor) {
          const editorUri = vscode.window.activeTextEditor.document.uri;

          if (editorUri.scheme === "file") {
            const rel = path
              .relative(activeClient.getRepoRoot(), editorUri.fsPath)
              .split(path.sep)
              .join("/");

            if (rel && !rel.startsWith("..") && !path.isAbsolute(rel)) {
              targetRelPath = rel;
            }
          }
        }

        if (!targetRelPath) {
          void vscode.window.showInformationMessage(
            "Select a modified file in the Tessera sidebar or open a repository file to diff."
          );
          return;
        }

        const rawClean = (
          targetRelPath.split("::")[0]?.trim() || targetRelPath
        ).replace(/^\/+/, "");
        const confined = resolveConfinedPath(
          activeClient.getRepoRoot(),
          rawClean
        );

        if (!confined) {
          throw new Error(`Invalid repository-relative file path '${rawClean}'`);
        }

        const [cleanPath, absPath] = confined;

        if (targetBranch === editorBranch) {
          const leftUri = BranchContentProvider.buildUri("HEAD", cleanPath);
          const rightUri = vscode.Uri.file(absPath);
          await vscode.commands.executeCommand(
            "vscode.diff",
            leftUri,
            rightUri,
            `${cleanPath} (${editorBranch} HEAD <-> Working Tree)`
          );
          return;
        }

        if (targetBranch === trunkBranch) {
          const leftUri = BranchContentProvider.buildUri(
            `refs/heads/${trunkBranch}`,
            cleanPath
          );
          const rightUri = BranchContentProvider.buildUri(
            targetBranch,
            cleanPath
          );
          await vscode.commands.executeCommand(
            "vscode.diff",
            leftUri,
            rightUri,
            `${cleanPath} (${trunkBranch} HEAD <-> Working Tree)`
          );
          return;
        }

        const leftUri = BranchContentProvider.buildUri(trunkBranch, cleanPath);
        const rightUri = BranchContentProvider.buildUri(
          targetBranch,
          cleanPath
        );
        await vscode.commands.executeCommand(
          "vscode.diff",
          leftUri,
          rightUri,
          `${cleanPath} (${trunkBranch} <-> ${targetBranch})`
        );
      }
    ),

    registerSafeCommand(
      "tes.openBranchDiff",
      async (branch?: string, relPath?: string) => {
        await vscode.commands.executeCommand(
          "tes.diffBranchFile",
          branch,
          relPath
        );
      }
    ),

    registerSafeCommand("tes.dismissFileHighlights", (relPath?: string) => {
      decorationController?.dismissForFile(relPath);
    }),

    registerSafeCommand(
      "tes.deleteBranch",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
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

          if (candidates.length === 0) {
            void vscode.window.showInformationMessage(
              "No non-trunk branches available to delete."
            );
            return;
          }

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
          await activeClient.deleteBranch(
            targetBranch,
            choice === "Force Delete"
          );
          await refreshAll();
        } catch (err: unknown) {
          const message = logError(`tes.deleteBranch(${targetBranch})`, err);
          const forceChoice = await vscode.window.showWarningMessage(
            `Could not safely delete '${targetBranch}': ${message}`,
            { modal: true },
            "Force Delete (-D)"
          );

          if (forceChoice === "Force Delete (-D)") {
            await activeClient.deleteBranch(targetBranch, true);
            await refreshAll();
          }
        }
      }
    ),

    registerSafeCommand(
      "tes.discardWorkingChanges",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
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

        await activeClient.discardWorkingChanges(targetBranch);
        await refreshAll();
      }
    ),

    registerSafeCommand(
      "tes.focusSession",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
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

        const ancestorPids = await collectAncestorPids(
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
          const report = await activeClient.focusSession(targetId);
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
          const message = logError(`tes.focusSession(${targetId})`, err);
          void vscode.window.showWarningMessage(
            `Could not focus session '${targetId}': ${message}`
          );
        }
      }
    ),

    registerSafeCommand(
      "tes.closeSession",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        let sessionId =
          typeof arg === "string"
            ? arg
            : arg instanceof SidebarNode
              ? arg.sessionId
              : undefined;

        if (!sessionId) {
          const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
          const activeAgents = snap.radarReport.active_agents.filter(
            (a) => a.agent_id !== "editor"
          );

          if (activeAgents.length === 0) {
            void vscode.window.showInformationMessage(
              "No active sessions to close."
            );
            return;
          }

          const picked = await vscode.window.showQuickPick(
            activeAgents.map((a) => ({
              label: a.agent_id,
              description: a.target_branch ?? snap.branchReport.editor_branch,
              detail: a.intent,
            })),
            { placeHolder: "Select a session to close" }
          );

          if (!picked) {
            return;
          }

          sessionId = picked.label;
        }

        await activeClient.closeSession(sessionId);
        await refreshAll();
      }
    ),

    registerSafeCommand("tes.clearSessions", async () => {
      const activeClient = requireTrustedClient();

      if (!activeClient) {
        return;
      }

      await activeClient.clearSessions(false);
      await refreshAll();
    }),

    registerSafeCommand(
      "tes.popStash",
      async (arg?: number | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
        let index: number | undefined;

        if (typeof arg === "number" && Number.isInteger(arg) && arg >= 0) {
          index = arg;
        } else if (
          arg instanceof SidebarNode &&
          arg.stashIndex !== undefined
        ) {
          index = arg.stashIndex;
        } else {
          if (snap.stashes.length === 0) {
            void vscode.window.showInformationMessage("No stashes available.");
            return;
          }

          const picked = await vscode.window.showQuickPick(
            snap.stashes.map((s, idx) => ({
              label: `stash@{${idx}}: ${s.message}`,
              description: `on ${s.branch}`,
              index: idx,
            })),
            { placeHolder: "Select a stash to pop (3-way AST merge)" }
          );

          if (!picked) {
            return;
          }

          index = picked.index;
        }

        const stashEntry = snap.stashes[index];
        const stashDesc = stashEntry
          ? `stash@{${index}} (${stashEntry.message})`
          : `stash@{${index}}`;
        const choice = await vscode.window.showWarningMessage(
          `Pop ${stashDesc} onto the editor session? This modifies working tree files and removes the stash entry.`,
          { modal: true },
          "Pop Stash"
        );

        if (choice !== "Pop Stash") {
          return;
        }

        await activeClient.popStash(index);
        await refreshAll();
      }
    ),

    registerSafeCommand(
      "tes.dropStash",
      async (arg?: number | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
        let index: number | undefined;

        if (typeof arg === "number" && Number.isInteger(arg) && arg >= 0) {
          index = arg;
        } else if (
          arg instanceof SidebarNode &&
          arg.stashIndex !== undefined
        ) {
          index = arg.stashIndex;
        } else {
          if (snap.stashes.length === 0) {
            void vscode.window.showInformationMessage("No stashes available.");
            return;
          }

          const picked = await vscode.window.showQuickPick(
            snap.stashes.map((s, idx) => ({
              label: `stash@{${idx}}: ${s.message}`,
              description: `on ${s.branch}`,
              index: idx,
            })),
            { placeHolder: "Select a stash to permanently drop" }
          );

          if (!picked) {
            return;
          }

          index = picked.index;
        }

        const stashEntry = snap.stashes[index];
        const stashDesc = stashEntry
          ? `stash@{${index}} (${stashEntry.message})`
          : `stash@{${index}}`;
        const choice = await vscode.window.showWarningMessage(
          `Permanently drop ${stashDesc}?`,
          { modal: true },
          "Drop Stash"
        );

        if (choice !== "Drop Stash") {
          return;
        }

        await activeClient.dropStash(index);
        await refreshAll();
      }
    ),

    registerSafeCommand("tes.openGraph", async () => {
      const activeClient = requireTrustedClient();

      if (!activeClient) {
        return;
      }

      const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
      VisualGraphPanel.show(activeClient, snap, switchEditorBranch, refreshAll);
    }),

    registerSafeCommand(
      "tes.openConflictStudio",
      async (arg?: string | SidebarNode) => {
        const activeClient = requireTrustedClient();

        if (!activeClient) {
          return;
        }

        const snap = latestSnapshot ?? (await activeClient.fetchSnapshot());
        const focusPath =
          typeof arg === "string" && arg.length > 0
            ? arg
            : arg instanceof SidebarNode
              ? (arg.filePath ?? arg.conflict?.path)
              : undefined;

        ConflictStudioPanel.show(activeClient, snap, refreshAll, focusPath);
      }
    ),

    vscode.window.registerUriHandler({
      handleUri(uri: vscode.Uri): vscode.ProviderResult<void> {
        if (!vscode.workspace.isTrusted) {
          return;
        }

        const params = new URLSearchParams(uri.query);
        const sessionId = params.get("session")?.trim();

        if (
          (uri.path === "/focus" || uri.path === "/session") &&
          sessionId &&
          isValidSafeIdentifier(sessionId)
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
          (latestSnapshot?.radarReport.active_agents ?? [])
            .map((a) => a.agent_id)
            .filter((id) => id.length > 0 && id !== "editor")
        );

        for (const activeId of activeIds) {
          let searchFrom = 0;

          while (searchFrom < line.length) {
            const idx = line.indexOf(activeId, searchFrom);

            if (idx === -1) {
              break;
            }

            const prevCh = idx > 0 ? line[idx - 1] : undefined;
            const nextCh = line[idx + activeId.length];

            if (!isIdentifierChar(prevCh) && !isIdentifierChar(nextCh)) {
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
        if (
          vscode.workspace.isTrusted &&
          isValidSafeIdentifier(link.sessionId)
        ) {
          void vscode.commands.executeCommand(
            "tes.focusSession",
            link.sessionId
          );
        }
      },
    })
  );

  initializeWorkspace();
}

/**
 * Deactivates the extension and disposes open webview panels and workspace-scoped listeners.
 */
export function deactivate(): void {
  if (activeWorkspaceCleanup) {
    activeWorkspaceCleanup();
    activeWorkspaceCleanup = undefined;
  }

  VisualGraphPanel.disposeCurrent();
  ConflictStudioPanel.disposeCurrent();
}
