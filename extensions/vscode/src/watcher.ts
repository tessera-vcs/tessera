/**
 * Hybrid filesystem watcher and lightweight signature poller for `.git/tessera` and `.git/refs`.
 */

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

/**
 * Watches `.git/tessera/op_heads`, `.git/tessera/conflicts`, `.git/HEAD`, and `.git/refs/heads`
 * using both VS Code file watchers and a lightweight `fs` stat check (since VS Code's default
 * `files.watcherExclude` ignores `.git/**` in many workspaces).
 */
export class StateWatcher implements vscode.Disposable {
  private readonly watchers: vscode.FileSystemWatcher[] = [];
  private debounceTimer: NodeJS.Timeout | undefined;
  private pollTimer: NodeJS.Timeout | undefined;
  private lastSignature = "";
  private readonly onDidChangeEmitter = new vscode.EventEmitter<void>();

  /**
   * Fires whenever repository or multi-agent state changes on disk.
   */
  public readonly onDidChange: vscode.Event<void> = this.onDidChangeEmitter.event;

  /**
   * Initializes file watchers and an 800ms fallback signature check for `repoRoot`.
   *
   * @param repoRoot The absolute path to the Git repository root.
   */
  public constructor(repoRoot: string) {
    const patterns = [
      new vscode.RelativePattern(repoRoot, ".git/tessera/op_heads/**"),
      new vscode.RelativePattern(repoRoot, ".git/tessera/conflicts/**"),
      new vscode.RelativePattern(repoRoot, ".git/HEAD"),
      new vscode.RelativePattern(repoRoot, ".git/refs/heads/**"),
    ];

    for (const pattern of patterns) {
      const watcher = vscode.workspace.createFileSystemWatcher(pattern);
      watcher.onDidCreate(() => this.scheduleFire());
      watcher.onDidChange(() => this.scheduleFire());
      watcher.onDidDelete(() => this.scheduleFire());
      this.watchers.push(watcher);
    }

    this.lastSignature = this.computeSignature(repoRoot);
    this.pollTimer = setInterval(() => {
      const nextSig = this.computeSignature(repoRoot);

      if (nextSig !== this.lastSignature) {
        this.lastSignature = nextSig;
        this.scheduleFire();
      }
    }, 800);
  }

  /**
   * Recursively hashes the ref filenames and commit SHAs under `.git/refs/heads`.
   *
   * @param dir The directory path to scan for Git ref files.
   * @returns A sorted comma-separated signature of ref names and SHAs.
   */
  private collectRefsSignature(dir: string): string {
    try {
      const entries = fs.readdirSync(dir, { withFileTypes: true });
      const parts: string[] = [];

      for (const entry of entries) {
        const full = path.join(dir, entry.name);

        if (entry.isDirectory()) {
          parts.push(`${entry.name}:[${this.collectRefsSignature(full)}]`);
        } else if (entry.isFile()) {
          const sha = fs.readFileSync(full, "utf8").trim();
          parts.push(`${entry.name}:${sha}`);
        }
      }

      return parts.sort().join(",");
    } catch {
      return "";
    }
  }

  /**
   * Computes a composite state signature of `.git/HEAD`, `.git/refs/heads`, `op_heads`, and `conflicts`.
   *
   * @param repoRoot The absolute path to the Git repository root.
   * @returns A deterministic pipe-delimited state signature string.
   */
  private computeSignature(repoRoot: string): string {
    const gitDir = path.join(repoRoot, ".git");
    const stateDir = path.join(gitDir, "tessera");
    const isInit = fs.existsSync(stateDir) ? "1" : "0";
    const headPath = path.join(gitDir, "HEAD");
    const refsHeadsDir = path.join(gitDir, "refs", "heads");
    const opHeadsDir = path.join(stateDir, "op_heads");
    const conflictsDir = path.join(stateDir, "conflicts");

    let headContent = "";
    try {
      headContent = fs.readFileSync(headPath, "utf8").trim();
    } catch {
      // Ignore missing HEAD file if repository is not yet initialized
    }

    const refsSig = this.collectRefsSignature(refsHeadsDir);

    let opHeads = "";
    try {
      opHeads = fs.readdirSync(opHeadsDir).sort().join(",");
    } catch {
      // Ignore missing op_heads directory
    }

    let conflicts = "";
    try {
      conflicts = fs.readdirSync(conflictsDir).sort().join(",");
    } catch {
      // Ignore missing conflicts directory
    }

    return `${isInit}|${headContent}|${refsSig}|${opHeads}|${conflicts}`;
  }

  /**
   * Debounces and emits an `onDidChange` notification within 60ms.
   */
  private scheduleFire(): void {
    if (this.debounceTimer !== undefined) {
      clearTimeout(this.debounceTimer);
    }

    this.debounceTimer = setTimeout(() => {
      this.debounceTimer = undefined;
      this.onDidChangeEmitter.fire();
    }, 60);
  }

  /**
   * Disposes all file watchers, timers, and event emitters.
   */
  public dispose(): void {
    if (this.debounceTimer !== undefined) {
      clearTimeout(this.debounceTimer);
    }

    if (this.pollTimer !== undefined) {
      clearInterval(this.pollTimer);
    }

    for (const w of this.watchers) {
      w.dispose();
    }

    this.onDidChangeEmitter.dispose();
  }
}
