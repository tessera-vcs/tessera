/**
 * Hybrid filesystem watcher and async signature poller for `.git/tessera` and Git refs.
 */

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  resolveCommonGitDir,
  resolveGitDir,
  resolveTesseraDir,
} from "./client";

/**
 * Watches Tessera operation heads, conflicts, `HEAD`, `refs/heads`, and `packed-refs`
 * using VS Code file watchers paired with a lightweight async fallback poller.
 * Does not start polling in repositories that are not initialized with Tessera.
 */
export class StateWatcher implements vscode.Disposable {
  private readonly repoRoot: string;
  private readonly initWatchers: vscode.Disposable[] = [];
  private readonly activeWatchers: vscode.Disposable[] = [];
  private debounceTimer: NodeJS.Timeout | undefined;
  private pollTimer: NodeJS.Timeout | undefined;
  private lastSignature = "";
  private isPolling = false;
  private disposed = false;
  private readonly onDidChangeEmitter = new vscode.EventEmitter<void>();

  /**
   * Fires whenever repository or multi-agent state changes on disk.
   */
  public readonly onDidChange: vscode.Event<void> =
    this.onDidChangeEmitter.event;

  /**
   * Initializes file watchers for `repoRoot`. Polling starts only when Tessera is initialized.
   *
   * @param repoRoot The absolute path to the Git repository root.
   */
  public constructor(repoRoot: string) {
    this.repoRoot = repoRoot;
    this.setupInitWatcher();

    if (resolveTesseraDir(repoRoot) !== null) {
      this.activateStateMonitoring();
    }
  }

  /**
   * Watches for Tessera initialization or removal so non-Tessera workspaces incur zero polling.
   */
  private setupInitWatcher(): void {
    const gitDir = resolveGitDir(this.repoRoot);
    const watchBase = gitDir ? resolveCommonGitDir(gitDir) : this.repoRoot;
    const initPattern = gitDir
      ? new vscode.RelativePattern(watchBase, "tessera/**")
      : new vscode.RelativePattern(this.repoRoot, ".git/tessera/**");

    const watcher = vscode.workspace.createFileSystemWatcher(initPattern);
    const onInitChange = (): void => {
      if (this.disposed) {
        return;
      }

      const initialized = resolveTesseraDir(this.repoRoot) !== null;

      if (initialized && this.pollTimer === undefined) {
        this.activateStateMonitoring();
        this.scheduleFire();
      } else if (!initialized && this.pollTimer !== undefined) {
        this.deactivateStateMonitoring();
        this.scheduleFire();
      }
    };

    this.initWatchers.push(
      watcher,
      watcher.onDidCreate(onInitChange),
      watcher.onDidDelete(onInitChange)
    );
  }

  /**
   * Activates detailed file watchers and the async fallback poller once Tessera is initialized.
   */
  private activateStateMonitoring(): void {
    if (this.disposed || this.pollTimer !== undefined) {
      return;
    }

    const gitDir = resolveGitDir(this.repoRoot);
    const tesseraDir = resolveTesseraDir(this.repoRoot);

    if (!gitDir || !tesseraDir) {
      return;
    }

    const commonGitDir = resolveCommonGitDir(gitDir);
    const patterns = [
      new vscode.RelativePattern(tesseraDir, "op_heads/**"),
      new vscode.RelativePattern(tesseraDir, "conflicts/**"),
      new vscode.RelativePattern(gitDir, "HEAD"),
      new vscode.RelativePattern(commonGitDir, "refs/heads/**"),
      new vscode.RelativePattern(commonGitDir, "packed-refs"),
    ];

    const onWatcherEvent = (): void => {
      if (this.disposed) {
        return;
      }

      void this.syncSignatureAndFire();
    };

    for (const pattern of patterns) {
      const watcher = vscode.workspace.createFileSystemWatcher(pattern);
      this.activeWatchers.push(
        watcher,
        watcher.onDidCreate(onWatcherEvent),
        watcher.onDidChange(onWatcherEvent),
        watcher.onDidDelete(onWatcherEvent)
      );
    }

    void this.computeSignature().then((sig) => {
      if (!this.disposed) {
        this.lastSignature = sig;
      }
    });

    this.pollTimer = setInterval(() => {
      void this.pollOnce();
    }, 800);
  }

  /**
   * Stops active polling and detailed watchers if Tessera metadata is removed from the repository.
   */
  private deactivateStateMonitoring(): void {
    if (this.pollTimer !== undefined) {
      clearInterval(this.pollTimer);
      this.pollTimer = undefined;
    }

    while (this.activeWatchers.length > 0) {
      const d = this.activeWatchers.pop();

      if (d) {
        d.dispose();
      }
    }

    this.lastSignature = "";
  }

  /**
   * Recomputes the cached signature after a native watcher notification and schedules a single
   * debounced change event so the next poll tick does not re-trigger the same change.
   */
  private async syncSignatureAndFire(): Promise<void> {
    if (this.disposed) {
      return;
    }

    const nextSig = await this.computeSignature();

    if (this.disposed) {
      return;
    }

    this.lastSignature = nextSig;
    this.scheduleFire();
  }

  /**
   * Runs a single non-overlapping asynchronous signature check.
   */
  private async pollOnce(): Promise<void> {
    if (this.disposed || this.isPolling) {
      return;
    }

    this.isPolling = true;

    try {
      if (resolveTesseraDir(this.repoRoot) === null) {
        this.deactivateStateMonitoring();
        this.scheduleFire();
        return;
      }

      const nextSig = await this.computeSignature();

      if (!this.disposed && nextSig !== this.lastSignature) {
        this.lastSignature = nextSig;
        this.scheduleFire();
      }
    } finally {
      this.isPolling = false;
    }
  }

  /**
   * Recursively hashes the ref filenames and commit SHAs under `refs/heads`.
   *
   * @param dir The directory path to scan for Git ref files.
   * @returns A sorted comma-separated signature of ref names and SHAs.
   */
  private async collectRefsSignature(dir: string): Promise<string> {
    try {
      const entries = await fs.promises.readdir(dir, { withFileTypes: true });
      const parts: string[] = [];

      for (const entry of entries) {
        const full = path.join(dir, entry.name);

        if (entry.isDirectory()) {
          const nested = await this.collectRefsSignature(full);
          parts.push(`${entry.name}:[${nested}]`);
        } else if (entry.isFile()) {
          const sha = (await fs.promises.readFile(full, "utf8")).trim();
          parts.push(`${entry.name}:${sha}`);
        }
      }

      return parts.sort().join(",");
    } catch {
      return "";
    }
  }

  /**
   * Computes a composite state signature of `HEAD`, `refs/heads`, `packed-refs`, `op_heads`,
   * and `conflicts` using asynchronous filesystem reads.
   *
   * @returns A deterministic pipe-delimited state signature string.
   */
  private async computeSignature(): Promise<string> {
    const gitDir = resolveGitDir(this.repoRoot);

    if (!gitDir) {
      return "0|||||";
    }

    const commonGitDir = resolveCommonGitDir(gitDir);
    const stateDir = resolveTesseraDir(this.repoRoot);
    const isInit = stateDir !== null ? "1" : "0";
    const headPath = path.join(gitDir, "HEAD");
    const refsHeadsDir = path.join(commonGitDir, "refs", "heads");
    const packedRefsPath = path.join(commonGitDir, "packed-refs");
    const opHeadsDir = stateDir ? path.join(stateDir, "op_heads") : "";
    const conflictsDir = stateDir ? path.join(stateDir, "conflicts") : "";

    const [headContent, refsSig, packedSig, opHeads, conflicts] =
      await Promise.all([
        fs.promises
          .readFile(headPath, "utf8")
          .then((s) => s.trim())
          .catch(() => ""),
        this.collectRefsSignature(refsHeadsDir),
        fs.promises
          .stat(packedRefsPath)
          .then((st) => `${st.mtimeMs}:${st.size}`)
          .catch(() => ""),
        opHeadsDir
          ? fs.promises
              .readdir(opHeadsDir)
              .then((entries) => entries.sort().join(","))
              .catch(() => "")
          : Promise.resolve(""),
        conflictsDir
          ? fs.promises
              .readdir(conflictsDir)
              .then((entries) => entries.sort().join(","))
              .catch(() => "")
          : Promise.resolve(""),
      ]);

    return `${isInit}|${headContent}|${refsSig}|${packedSig}|${opHeads}|${conflicts}`;
  }

  /**
   * Debounces and emits an `onDidChange` notification within 60ms.
   */
  private scheduleFire(): void {
    if (this.disposed) {
      return;
    }

    if (this.debounceTimer !== undefined) {
      clearTimeout(this.debounceTimer);
    }

    this.debounceTimer = setTimeout(() => {
      this.debounceTimer = undefined;

      if (!this.disposed) {
        this.onDidChangeEmitter.fire();
      }
    }, 60);
  }

  /**
   * Disposes all file watchers, event listener subscriptions, timers, and emitters.
   */
  public dispose(): void {
    this.disposed = true;

    if (this.debounceTimer !== undefined) {
      clearTimeout(this.debounceTimer);
      this.debounceTimer = undefined;
    }

    this.deactivateStateMonitoring();

    while (this.initWatchers.length > 0) {
      const d = this.initWatchers.pop();

      if (d) {
        d.dispose();
      }
    }

    this.onDidChangeEmitter.dispose();
  }
}
