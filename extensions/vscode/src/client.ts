/**
 * Typed client for invoking the `tes` CLI binary and deserializing repository snapshots.
 */

import { execFile } from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { promisify } from "util";
import * as vscode from "vscode";

const execFileAsync = promisify(execFile);

/**
 * Metadata for a single Git commit in the visual commit graph.
 */
export interface CommitGraphNode {
  readonly sha: string;
  readonly short_sha: string;
  readonly parents: readonly string[];
  readonly author: string;
  readonly timestamp: number;
  readonly subject: string;
  readonly refs: readonly string[];
}

/**
 * Detailed status for a branch and its Copy-on-Write sandbox.
 */
export interface BranchDetail {
  readonly name: string;
  readonly commit: string;
  readonly short_commit: string;
  readonly is_editor_branch: boolean;
  readonly workspace_path: string;
  readonly port: number;
  readonly sessions: readonly string[];
  readonly modified_files: readonly string[];
}

/**
 * Output of `tes branch --json`.
 */
export interface BranchReport {
  readonly session_id: string;
  readonly trunk_branch?: string;
  readonly editor_branch: string;
  readonly active_branch: string;
  readonly branches: readonly string[];
  readonly branch_details: readonly BranchDetail[];
  readonly commit_graph: readonly CommitGraphNode[];
}

/**
 * Navigation waypoint metadata linking a session to its originating terminal or IDE harness.
 */
export interface SessionWaypoint {
  readonly harness?: string | null;
  readonly conversation_id?: string | null;
  readonly term_program?: string | null;
  readonly term_session_id?: string | null;
  readonly tty?: string | null;
  readonly shell_pid?: number | null;
  readonly ide_pid?: number | null;
}

/**
 * Report returned by `tes session focus <session-id> --json`.
 */
export interface FocusSessionReport {
  readonly session_id: string;
  readonly harness?: string;
  readonly focused: boolean;
  readonly mechanism: string;
  readonly detail: string;
  readonly waypoint?: SessionWaypoint | null;
}

/**
 * Active AI agent or terminal session entry from `tes radar --json`.
 */
export interface AgentEntry {
  readonly agent_id: string;
  readonly change_id: string;
  readonly intent: string;
  readonly slot: number;
  readonly base_commit: string;
  readonly head_commit: string;
  readonly target_branch: string | null;
  readonly workspace_path: string | null;
  readonly claimed_symbols: readonly string[];
  readonly own_blobs?: Readonly<Record<string, string>>;
  readonly modified_files?: readonly string[];
  readonly checkpoints?: Readonly<Record<string, string>>;
  readonly waypoint?: SessionWaypoint | null;
  readonly updated_at_ms?: number;
}

/**
 * Enriched algebraic conflict record from `tes conflicts --json`.
 */
export interface ConflictItem {
  readonly id: string;
  readonly path: string;
  readonly symbol: string | null;
  readonly kind: string;
  readonly agent_intents: Readonly<Record<string, string>>;
  readonly base_snippet: string;
  readonly left_snippet: string;
  readonly right_snippet: string;
  readonly ast_union_preview: string;
  readonly created_at_ms: number;
}

/**
 * Live per-file line-range attribution for an active agent or unmerged branch.
 */
export interface FileLineAttribution {
  readonly path: string;
  readonly agent_id: string;
  readonly branch: string;
  readonly intent: string;
  readonly slot: number;
  readonly start_line: number;
  readonly end_line: number;
  readonly in_working_tree: boolean;
  readonly symbol: string | null;
}

/**
 * Radar report returned by `tes radar --json`.
 */
export interface RadarReport {
  readonly active_agents: readonly AgentEntry[];
  readonly collisions: readonly {
    readonly target: string;
    readonly competing_agents: Readonly<Record<string, string>>;
  }[];
  readonly active_conflicts: readonly ConflictItem[];
  readonly file_attributions?: readonly FileLineAttribution[];
}

/**
 * Operation Log entry returned by `tes oplog --json`.
 */
export interface OpLogEntry {
  readonly id: string;
  readonly agent_id: string;
  readonly description: string;
  readonly timestamp_ms: number;
  readonly is_ephemeral: boolean;
  readonly view?: {
    readonly agents?: Readonly<
      Record<
        string,
        {
          readonly checkpoints?: Readonly<Record<string, string>>;
        }
      >
    >;
  };
}

/**
 * Stash entry returned by `tes stash list --json`.
 */
export interface StashEntry {
  readonly id: string;
  readonly session_id: string;
  readonly branch: string;
  readonly base_commit: string;
  readonly message: string;
  readonly own_blobs: Readonly<Record<string, string>>;
  readonly claimed_symbols: readonly string[];
  readonly created_at_ms: number;
}

/**
 * Combined repository snapshot used by all visual views and webviews.
 */
export interface WorkspaceStateSnapshot {
  readonly branchReport: BranchReport;
  readonly radarReport: RadarReport;
  readonly conflicts: readonly ConflictItem[];
  readonly oplog: readonly OpLogEntry[];
  readonly stashes: readonly StashEntry[];
  readonly checkpoints: readonly {
    readonly label: string;
    readonly opId: string;
    readonly sessionId: string;
  }[];
}

/**
 * Validates that a Git branch or reference name contains only safe characters and no shell metacharacters.
 *
 * @param name The branch or reference name to validate.
 * @returns True if the reference name is safe.
 */
export function isValidRefName(name: string): boolean {
  if (
    !name ||
    name.length > 255 ||
    name.startsWith("-") ||
    name.startsWith("/") ||
    name.endsWith("/") ||
    name.endsWith(".") ||
    name.endsWith(".lock") ||
    name.includes("..") ||
    name.includes("@{") ||
    name.includes("//") ||
    name.includes("\\")
  ) {
    return false;
  }

  if (/[\0-\x20\x7f~^:?*\[\\;&|$`'"()<>!{}]/.test(name)) {
    return false;
  }

  for (const seg of name.split("/")) {
    const lower = seg.toLowerCase();

    if (
      !seg ||
      seg.startsWith(".") ||
      seg.endsWith(".lock") ||
      lower === ".git" ||
      lower === ".tessera"
    ) {
      return false;
    }
  }

  return true;
}

/**
 * Resolves a repository-relative path inside `rootDir` while rejecting path traversal (`..`),
 * reserved `.git`/`.tessera` directories, and symbolic links along the path.
 *
 * @param rootDir The base directory to confine the path within.
 * @param relPath The repository-relative path to validate and resolve.
 * @returns The validated `[cleanRelPath, fullPath]` tuple, or `undefined` if unsafe.
 */
export function resolveConfinedPath(
  rootDir: string,
  relPath: string
): readonly [string, string] | undefined {
  if (!relPath || relPath.includes("\0")) {
    return undefined;
  }

  let trimmed = relPath.replace(/\\/g, "/").trim();

  while (trimmed.startsWith("./")) {
    trimmed = trimmed.slice(2);
  }

  if (
    !trimmed ||
    trimmed.startsWith("/") ||
    trimmed.startsWith("-") ||
    trimmed.includes(":")
  ) {
    return undefined;
  }

  const segments: string[] = [];

  for (const seg of trimmed.split("/")) {
    if (!seg || seg === ".") {
      continue;
    }

    const lower = seg.toLowerCase();

    if (
      seg === ".." ||
      lower === ".git" ||
      lower === ".tessera" ||
      lower === "git~1"
    ) {
      return undefined;
    }

    segments.push(seg);
  }

  if (segments.length === 0) {
    return undefined;
  }

  let current = path.resolve(rootDir);

  for (const seg of segments) {
    current = path.join(current, seg);

    try {
      const st = fs.lstatSync(current);

      if (st.isSymbolicLink()) {
        return undefined;
      }
    } catch {
      // Non-existent leaf or intermediate component is permitted prior to creation.
    }
  }

  const relCheck = path.relative(path.resolve(rootDir), current);

  if (relCheck.startsWith("..") || path.isAbsolute(relCheck)) {
    return undefined;
  }

  return [segments.join("/"), current] as const;
}

/**
 * Resolves the underlying Git repository root even if the IDE window was opened
 * inside a `.tessera` Copy-on-Write branch sandbox (`.../branches/<branch>`).
 *
 * @param workspaceFolderPath The workspace folder path opened in the editor.
 * @returns The resolved Git repository root path.
 */
export function resolveRepoRoot(workspaceFolderPath: string): string {
  let curr = workspaceFolderPath;

  for (let depth = 0; depth < 10; depth++) {
    const dotGit = path.join(curr, ".git");

    if (fs.existsSync(dotGit)) {
      return curr;
    }

    const dotTessera = path.join(curr, ".tessera");
    const pointerPath = path.join(dotTessera, "repo_root");

    try {
      if (
        fs.existsSync(pointerPath) &&
        !fs.lstatSync(dotTessera).isSymbolicLink() &&
        !fs.lstatSync(pointerPath).isSymbolicLink()
      ) {
        const target = fs.readFileSync(pointerPath, "utf8").trim();
        const normalizedCurr = curr.replace(/\\/g, "/");
        const isSandboxPath =
          normalizedCurr.includes("/tessera/workspaces/") ||
          normalizedCurr.includes("/tessera-workspaces") ||
          normalizedCurr.includes("/.git/tessera/") ||
          normalizedCurr.includes("/.tessera/workspaces/");

        if (
          isSandboxPath &&
          target &&
          path.isAbsolute(target) &&
          fs.existsSync(path.join(target, ".git", "tessera"))
        ) {
          return target;
        }
      }
    } catch {
      // Fall through to parent traversal
    }

    const parent = path.dirname(curr);

    if (parent === curr) {
      break;
    }

    curr = parent;
  }

  return workspaceFolderPath;
}

/**
 * Typed client for invoking the `tes` CLI binary against a workspace root.
 *
 * Preconditions: `repoRoot` must be a valid directory on disk.
 * Error conditions: Rejects with a descriptive `Error` if `tes` exits non-zero or returns malformed JSON.
 */
export class CliClient {
  private readonly repoRoot: string;

  /**
   * Creates a new CLI client rooted at the resolved Git repository for `workspaceFolderPath`.
   *
   * @param workspaceFolderPath The workspace folder path opened in the editor.
   */
  public constructor(workspaceFolderPath: string) {
    this.repoRoot = resolveRepoRoot(workspaceFolderPath);
  }

  /**
   * Returns the repository root path managed by this client.
   *
   * @returns The absolute path to the repository root.
   */
  public getRepoRoot(): string {
    return this.repoRoot;
  }

  /**
   * Returns true when `.git/tessera` exists in the repository root.
   *
   * @returns True if the repository is initialized with `tes`.
   */
  public isInitialized(): boolean {
    return fs.existsSync(path.join(this.repoRoot, ".git", "tessera"));
  }

  /**
   * Resolves the configured `tes` binary path, checking `~/.cargo/bin/tes` and `/opt/homebrew/bin/tes`.
   *
   * @returns The resolved executable path or binary name.
   */
  public resolveBinPath(): string {
    const inspected = vscode.workspace
      .getConfiguration("tes")
      .inspect<string>("binPath");
    const configured = (
      inspected?.globalValue ??
      inspected?.defaultValue ??
      "tes"
    ).trim();

    if (configured !== "tes" && fs.existsSync(configured)) {
      return configured;
    }

    const candidates = [
      path.join(os.homedir(), ".cargo", "bin", "tes"),
      path.join(os.homedir(), ".local", "bin", "tes"),
      "/opt/homebrew/bin/tes",
      "/usr/local/bin/tes",
    ];

    for (const candidate of candidates) {
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    }

    return configured || "tes";
  }

  /**
   * Builds a PATH string that includes standard macOS/Linux user binary locations.
   *
   * @param envOverrides Optional environment variables to merge into the child process environment.
   * @returns The constructed process environment object.
   */
  private buildEnv(
    envOverrides?: Readonly<Record<string, string>>
  ): NodeJS.ProcessEnv {
    const extraDirs = [
      path.join(os.homedir(), ".cargo", "bin"),
      path.join(os.homedir(), ".local", "bin"),
      "/opt/homebrew/bin",
      "/usr/local/bin",
    ];
    const currentPath = process.env.PATH ?? "/usr/bin:/bin";

    return {
      ...process.env,
      PATH: `${extraDirs.join(path.delimiter)}${path.delimiter}${currentPath}`,
      ...(envOverrides ?? {}),
    };
  }

  /**
   * Executes a `tes` CLI command and returns raw `stdout`.
   *
   * Error conditions: Throws `Error` if spawning fails or exit code is non-zero.
   *
   * @param args CLI arguments to pass to `tes`.
   * @param envOverrides Optional environment variable overrides.
   * @returns The standard output string from the command.
   */
  public async runRaw(
    args: readonly string[],
    envOverrides?: Readonly<Record<string, string>>
  ): Promise<string> {
    const bin = this.resolveBinPath();
    const fullArgs = ["-C", this.repoRoot, ...args];
    const { stdout } = await execFileAsync(bin, fullArgs, {
      cwd: this.repoRoot,
      maxBuffer: 16 * 1024 * 1024,
      env: this.buildEnv(envOverrides),
    });

    return stdout;
  }

  /**
   * Executes a `tes --json` command and parses its output.
   *
   * Error conditions: Throws `Error` if the command fails or JSON parsing fails.
   *
   * @param args CLI arguments to pass after `--json`.
   * @param envOverrides Optional environment variable overrides.
   * @returns The deserialized JSON response of type `T`.
   */
  public async runJson<T>(
    args: readonly string[],
    envOverrides?: Readonly<Record<string, string>>
  ): Promise<T> {
    const stdout = await this.runRaw(["--json", ...args], envOverrides);
    return JSON.parse(stdout) as T;
  }

  /**
   * Fetches the complete state snapshot for visual panels and tree views.
   *
   * @returns A combined snapshot of branches, active sessions, conflicts, oplog, stashes, and checkpoints.
   */
  public async fetchSnapshot(): Promise<WorkspaceStateSnapshot> {
    const [branchReport, radarReport, conflicts, oplog, stashes] =
      await Promise.all([
        this.runJson<BranchReport>(["branch", "--agent", "editor"]),
        this.runJson<RadarReport>(["radar"]),
        this.runJson<ConflictItem[]>(["conflicts"]),
        this.runJson<OpLogEntry[]>(["oplog", "--limit", "30"]),
        this.runJson<StashEntry[]>(["stash", "list"]).catch(
          (): StashEntry[] => []
        ),
      ]);

    const checkpointMap = new Map<
      string,
      { label: string; opId: string; sessionId: string }
    >();

    for (const op of oplog) {
      const agentsObj = op.view?.agents;

      if (agentsObj) {
        for (const [sid, agentData] of Object.entries(agentsObj)) {
          if (agentData.checkpoints) {
            for (const [label, opId] of Object.entries(agentData.checkpoints)) {
              if (!checkpointMap.has(label)) {
                checkpointMap.set(label, { label, opId, sessionId: sid });
              }
            }
          }
        }
      }
    }

    return {
      branchReport,
      radarReport,
      conflicts,
      oplog,
      stashes,
      checkpoints: Array.from(checkpointMap.values()),
    };
  }

  /**
   * Switches the primary editor working tree (`repo_root` and `.git/HEAD`) to `branch`.
   *
   * @param branch The target branch name to check out in the editor.
   */
  public async switchEditorBranch(branch: string): Promise<void> {
    await this.runRaw([
      "switch",
      "--editor",
      branch.trim(),
      "--agent",
      "editor",
    ]);
  }

  /**
   * Switches a specific session/terminal (`sessionId`) to `branch`.
   *
   * @param sessionId The session identifier to rebind.
   * @param branch The target branch name.
   */
  public async switchSessionBranch(
    sessionId: string,
    branch: string
  ): Promise<void> {
    await this.runRaw(["switch", branch, "--agent", sessionId]);
  }

  /**
   * Closes and removes a single session (`agentId`) via `tes done --force`.
   *
   * @param agentId The session or agent identifier to close.
   */
  public async closeSession(agentId: string): Promise<void> {
    await this.runRaw(["done", "--force", "--agent", agentId]);
  }

  /**
   * Focuses the terminal tab, multiplexer pane, or IDE window for `sessionId`.
   *
   * @param sessionId The session identifier to focus.
   * @returns A report describing whether focus succeeded and which mechanism was used.
   */
  public async focusSession(sessionId: string): Promise<FocusSessionReport> {
    return this.runJson<FocusSessionReport>(["session", "focus", sessionId], {
      TES_FROM_IDE_EXTENSION: "1",
    });
  }

  /**
   * Clears all clean/idle sessions (or all sessions if `force` is true) via `tes clear`.
   *
   * @param force Whether to force-clear active sessions with uncommitted changes.
   */
  public async clearSessions(force = false): Promise<void> {
    const args = force ? ["clear", "--force"] : ["clear"];
    await this.runRaw(args);
  }

  /**
   * Pops a stash entry by index (`stash@{index}`) onto the current session/branch.
   *
   * @param index Zero-based index in the stash stack.
   */
  public async popStash(index: number): Promise<void> {
    await this.runRaw(["stash", "pop", String(index), "--agent", "editor"]);
  }

  /**
   * Drops a stash entry by index (`stash@{index}`).
   *
   * @param index Zero-based index in the stash stack.
   */
  public async dropStash(index: number): Promise<void> {
    await this.runRaw(["stash", "drop", String(index)]);
  }

  /**
   * Merges one or more branches or sessions into `targetBranch`.
   *
   * @param sources List of source branch or session names to merge.
   * @param targetBranch The destination branch to merge into.
   * @param message The commit message for the merge commit.
   */
  public async mergeInto(
    sources: readonly string[],
    targetBranch: string,
    message: string
  ): Promise<void> {
    await this.runRaw([
      "merge",
      ...sources,
      "--into",
      targetBranch,
      "-m",
      message,
    ]);
  }

  /**
   * Restores `branch` (or specific `paths`) to `revisionOrCheckpoint`.
   *
   * @param branch The target branch to restore.
   * @param revisionOrCheckpoint The checkpoint label, commit SHA, or ref to restore from.
   * @param paths Optional list of specific repository-relative file paths to restore.
   */
  public async restoreBranch(
    branch: string,
    revisionOrCheckpoint: string,
    paths: readonly string[] = []
  ): Promise<void> {
    const args = ["restore", "-b", branch, revisionOrCheckpoint, ...paths];
    await this.runRaw(args);
  }

  /**
   * Saves a named checkpoint in the Operation Log.
   *
   * @param label The human-readable checkpoint name.
   */
  public async createCheckpoint(label: string): Promise<void> {
    await this.runRaw(["checkpoint", label]);
  }

  /**
   * Undoes the most recent operation or rolls back to `opId`.
   *
   * @param opId Optional operation ID to roll back to.
   */
  public async undo(opId?: string): Promise<void> {
    const args = opId ? ["undo", "--op", opId] : ["undo"];
    await this.runRaw(args);
  }

  /**
   * Resolves a conflict at `filePath` using the specified strategy or explicit content.
   *
   * @param filePath The repository-relative path of the conflicted file.
   * @param strategy The resolution strategy (`ast-union`, `ours`, `theirs`, `working`, or `content`).
   * @param customContent Optional custom file content when `strategy` is `"content"`.
   */
  public async resolveConflict(
    filePath: string,
    strategy: "ast-union" | "ours" | "theirs" | "working" | "content",
    customContent?: string
  ): Promise<void> {
    const args = ["resolve", filePath, "--strategy", strategy];

    if (strategy === "content" && customContent !== undefined) {
      args.push("--content", customContent);
    }

    await this.runRaw(args);
  }

  /**
   * Pushes `branch` to `origin`.
   *
   * @param branch The branch name to push.
   */
  public async pushBranch(branch: string): Promise<void> {
    await this.runRaw(["push", "origin", branch]);
  }

  /**
   * Deletes `branch` safely (`force = false`) or forcefully (`force = true`).
   *
   * @param branch The branch name to delete.
   * @param force Whether to force-delete an unmerged branch (`-D`).
   */
  public async deleteBranch(branch: string, force = false): Promise<void> {
    await this.runRaw(["branch", force ? "-D" : "-d", branch]);
  }

  /**
   * Discards uncommitted working changes on `branch` by restoring it to its `HEAD` commit.
   *
   * @param branch The branch whose working changes should be discarded.
   */
  public async discardWorkingChanges(branch: string): Promise<void> {
    await this.runRaw(["restore", "-b", branch, "HEAD"]);
  }

  /**
   * Reads a file from a specific branch sandbox or Git commit tree for virtual diffing.
   *
   * @param branch The branch name or `"HEAD"`.
   * @param relPath The repository-relative file path.
   * @returns The UTF-8 contents of the file on the specified branch.
   */
  public async readBranchFile(branch: string, relPath: string): Promise<string> {
    if (branch !== "HEAD" && !isValidRefName(branch)) {
      throw new Error(`Invalid branch reference '${branch}'`);
    }

    const confinedRepo = resolveConfinedPath(this.repoRoot, relPath);

    if (!confinedRepo) {
      throw new Error(`Invalid repository-relative path '${relPath}'`);
    }

    const [cleanRelPath] = confinedRepo;

    if (branch !== "HEAD") {
      const snapshot = await this.runJson<BranchReport>(["branch"]);
      const detail = snapshot.branch_details.find((b) => b.name === branch);

      if (detail) {
        const confinedSandbox = resolveConfinedPath(
          detail.workspace_path,
          cleanRelPath
        );

        if (confinedSandbox && fs.existsSync(confinedSandbox[1])) {
          return fs.readFileSync(confinedSandbox[1], "utf8");
        }
      }
    }

    const { stdout } = await execFileAsync(
      "git",
      ["-C", this.repoRoot, "show", `${branch}:${cleanRelPath}`],
      { maxBuffer: 10 * 1024 * 1024 }
    );

    return stdout;
  }
}
