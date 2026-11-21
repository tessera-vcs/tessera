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

const DEFAULT_QUERY_TIMEOUT_MS = 30_000;
const LONG_OPERATION_TIMEOUT_MS = 120_000;
const RESTORE_TIMEOUT_MS = 60_000;

/**
 * Error thrown when a file legitimately does not exist on a target branch or commit tree
 * (for example, a newly added or deleted file during a virtual diff).
 */
export class BranchFileNotFoundError extends Error {
  public readonly branch: string;
  public readonly relPath: string;

  /**
   * Constructs a `BranchFileNotFoundError` for the missing `relPath` on `branch`.
   *
   * @param branch The target branch or revision name.
   * @param relPath The repository-relative file path.
   */
  public constructor(branch: string, relPath: string) {
    super(`File '${relPath}' does not exist on '${branch}'`);
    this.name = "BranchFileNotFoundError";
    this.branch = branch;
    this.relPath = relPath;
  }
}

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
  readonly diagnostics?: string | null;
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
 * Options passed to `tes gc`.
 */
export interface GcOptions {
  readonly dryRun?: boolean;
  readonly keepMerged?: boolean;
  readonly staleDays?: number;
  readonly pruneCaches?: boolean;
  readonly allRepos?: boolean;
}

/**
 * Structured report returned by `tes gc --json`.
 */
export interface GcReport {
  readonly dry_run: boolean;
  readonly compacted_operations: number;
  readonly pruned_sandboxes: readonly string[];
  readonly pruned_merged_sandboxes?: readonly string[];
  readonly pruned_stale_sandboxes?: readonly string[];
  readonly pruned_verify_sandboxes?: readonly string[];
  readonly pruned_agent_workspaces?: readonly string[];
  readonly pruned_tmp_dirs?: readonly string[];
  readonly pruned_sidecars: readonly string[];
  readonly pruned_dead_leases: readonly string[];
  readonly pruned_stat_caches?: number;
  readonly pruned_locks?: number;
  readonly pruned_session_slots?: number;
  readonly pruned_stale_view_branches?: readonly string[];
  readonly pruned_caches?: readonly string[];
  readonly pruned_global_repos?: readonly string[];
  readonly skipped_active_sandboxes: readonly string[];
  readonly reclaimed_bytes?: number;
}

/**
 * Individual diagnostic check entry returned inside `tes doctor --json`.
 */
export interface DoctorCheck {
  readonly id: string;
  readonly name: string;
  readonly status: string;
  readonly summary: string;
  readonly details?: Readonly<Record<string, unknown>>;
}

/**
 * Structured diagnostic report returned by `tes doctor --json`.
 */
export interface DoctorReport {
  readonly ok: boolean;
  readonly summary: {
    readonly pass: number;
    readonly warn: number;
    readonly fail: number;
  };
  readonly checks: readonly DoctorCheck[];
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
 * Returns true if `pathStr` is one of the synthetic build/verification conflict paths.
 *
 * @param pathStr The conflict path string to check.
 * @returns True if `pathStr` is a synthetic verification conflict marker.
 */
export function isSyntheticConflictPath(pathStr: string): boolean {
  return pathStr === "<speculative-verify>" || pathStr === "<build>";
}

/**
 * Validates that an agent, session, or operation identifier contains only safe characters
 * (`[A-Za-z0-9._-]`) and cannot traverse directories or inject CLI flags.
 *
 * @param id The identifier to validate.
 * @returns True if the identifier is safe.
 */
export function isValidSafeIdentifier(id: string): boolean {
  if (
    !id ||
    id.length > 128 ||
    id.startsWith("-") ||
    id.startsWith(".") ||
    id.includes("..")
  ) {
    return false;
  }

  return /^[A-Za-z0-9._-]+$/.test(id);
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
 * Validates a Git revision expression (such as `HEAD~1`, a commit SHA, or a branch/checkpoint name)
 * while preventing leading `-` flag injection and shell metacharacters.
 *
 * @param rev The revision or checkpoint string to validate.
 * @returns True if the revision or checkpoint label is safe.
 */
export function isValidRevisionOrCheckpoint(rev: string): boolean {
  if (
    !rev ||
    rev.length > 255 ||
    rev.startsWith("-") ||
    rev.includes("..") ||
    rev.includes("@{") ||
    rev.includes("\\")
  ) {
    return false;
  }

  if (/[\0-\x20\x7f:?*\[\\;&|$`'"()<>!{}]/.test(rev)) {
    return false;
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
 * Resolves the actual Git directory for `repoRoot`, supporting standard `.git` directories
 * as well as Git worktrees and submodules where `.git` is a regular file containing `gitdir: <path>`.
 *
 * @param repoRoot The repository working tree root directory.
 * @returns The resolved absolute `.git` directory path, or `null` if not a Git repository.
 */
export function resolveGitDir(repoRoot: string): string | null {
  const dotGit = path.join(repoRoot, ".git");

  try {
    const stat = fs.statSync(dotGit);

    if (stat.isDirectory()) {
      return dotGit;
    }

    if (stat.isFile()) {
      const content = fs.readFileSync(dotGit, "utf8");
      const match = /^gitdir:\s*(.+)$/m.exec(content);

      if (!match || !match[1]) {
        return null;
      }

      const resolved = path.resolve(repoRoot, match[1].trim());

      if (fs.existsSync(resolved) && fs.statSync(resolved).isDirectory()) {
        return resolved;
      }
    }
  } catch {
    return null;
  }

  return null;
}

/**
 * Resolves the shared Git common directory (`commondir`) for a linked worktree, or returns
 * `gitDir` itself for a standard repository or submodule.
 *
 * @param gitDir The resolved Git directory from `resolveGitDir`.
 * @returns The common Git directory containing shared refs and metadata.
 */
export function resolveCommonGitDir(gitDir: string): string {
  const commonDirFile = path.join(gitDir, "commondir");

  try {
    if (fs.existsSync(commonDirFile) && fs.statSync(commonDirFile).isFile()) {
      const relCommon = fs.readFileSync(commonDirFile, "utf8").trim();

      if (relCommon) {
        const resolved = path.resolve(gitDir, relCommon);

        if (fs.existsSync(resolved) && fs.statSync(resolved).isDirectory()) {
          return resolved;
        }
      }
    }
  } catch {
    // Fall back to gitDir when commondir cannot be read.
  }

  return gitDir;
}

/**
 * Resolves the initialized `.git/tessera` metadata directory for `repoRoot`, supporting
 * both standard repositories and linked Git worktrees/submodules.
 *
 * @param repoRoot The repository working tree root directory.
 * @returns The absolute path to the `tessera` state directory, or `null` if uninitialized.
 */
export function resolveTesseraDir(repoRoot: string): string | null {
  const gitDir = resolveGitDir(repoRoot);

  if (!gitDir) {
    return null;
  }

  const direct = path.join(gitDir, "tessera");

  if (fs.existsSync(direct)) {
    return direct;
  }

  const commonGitDir = resolveCommonGitDir(gitDir);

  if (commonGitDir !== gitDir) {
    const shared = path.join(commonGitDir, "tessera");

    if (fs.existsSync(shared)) {
      return shared;
    }
  }

  return null;
}

/**
 * Resolves the underlying Git repository root even if the IDE window was opened
 * inside a `.tessera` Copy-on-Write branch sandbox (`.../branches/<branch>`),
 * a Git worktree, or a submodule.
 *
 * @param workspaceFolderPath The workspace folder path opened in the editor.
 * @returns The resolved Git repository root path.
 */
export function resolveRepoRoot(workspaceFolderPath: string): string {
  let curr = workspaceFolderPath;

  for (let depth = 0; depth < 10; depth++) {
    if (resolveGitDir(curr) !== null) {
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
          normalizedCurr.includes("/.tessera/workspaces/") ||
          normalizedCurr.includes("/.tessera/sandboxes/");

        if (
          isSandboxPath &&
          target &&
          path.isAbsolute(target) &&
          resolveTesseraDir(target) !== null
        ) {
          return target;
        }
      }
    } catch {
      // Fall through to parent traversal.
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
 * Expands a leading `~/` or `~\` home-directory prefix in a configured binary path.
 *
 * @param rawPath The raw path string from configuration.
 * @returns The path with a leading tilde expanded to `os.homedir()`.
 */
function expandHomePrefix(rawPath: string): string {
  if (rawPath === "~") {
    return os.homedir();
  }

  if (rawPath.startsWith("~/") || rawPath.startsWith("~\\")) {
    return path.join(os.homedir(), rawPath.slice(2));
  }

  return rawPath;
}

/**
 * Extracts a descriptive stderr string from a child process error when available.
 *
 * @param err The caught error object from `execFileAsync`.
 * @returns The lowercase stderr or message string for diagnostics matching.
 */
function extractExecErrorText(err: unknown): string {
  if (typeof err !== "object" || err === null) {
    return String(err);
  }

  const record = err as { readonly stderr?: unknown; readonly message?: unknown };
  const stderr = typeof record.stderr === "string" ? record.stderr : "";
  const message = typeof record.message === "string" ? record.message : "";

  return `${stderr}\n${message}`.toLowerCase();
}

/**
 * Typed client for invoking the `tes` CLI binary against a workspace root.
 *
 * Preconditions: `repoRoot` must be a valid directory on disk.
 * Error conditions: Rejects with a descriptive `Error` if `tes` exits non-zero, times out, or returns malformed JSON.
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
   * Returns true when `.git/tessera` exists in the repository's resolved Git directory.
   *
   * @returns True if the repository is initialized with `tes`.
   */
  public isInitialized(): boolean {
    return resolveTesseraDir(this.repoRoot) !== null;
  }

  /**
   * Resolves the configured `tes` binary path, expanding `~/` prefixes and probing platform-appropriate
   * install locations when using the default `"tes"` setting.
   *
   * @returns The resolved executable path or binary name.
   */
  public resolveBinPath(): string {
    const rawConfigured = vscode.workspace
      .getConfiguration("tes")
      .get<string>("binPath", "tes");
    const configured = expandHomePrefix((rawConfigured ?? "tes").trim());

    if (configured && configured !== "tes") {
      return configured;
    }

    const candidates =
      process.platform === "win32"
        ? [
            path.join(os.homedir(), ".cargo", "bin", "tes.exe"),
            path.join(os.homedir(), ".local", "bin", "tes.exe"),
          ]
        : [
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

    return "tes";
  }

  /**
   * Builds a process environment with `TES_FROM_IDE_EXTENSION=1` and standard user binary locations
   * prepended to `PATH` while preserving Windows `Path`/`PATH` case-insensitivity and avoiding Unix
   * directories on Windows.
   *
   * @param envOverrides Optional environment variables to merge into the child process environment.
   * @returns The constructed process environment object.
   */
  private buildEnv(
    envOverrides?: Readonly<Record<string, string>>
  ): NodeJS.ProcessEnv {
    const env: NodeJS.ProcessEnv = {
      ...process.env,
      TES_FROM_IDE_EXTENSION: "1",
    };

    if (process.platform === "win32") {
      const extraDirs = [
        path.join(os.homedir(), ".cargo", "bin"),
        path.join(os.homedir(), ".local", "bin"),
      ];
      const existingPathKey =
        Object.keys(env).find((k) => k.toLowerCase() === "path") ?? "Path";
      const currentPath = env[existingPathKey] ?? "";

      env[existingPathKey] = currentPath
        ? `${extraDirs.join(path.delimiter)}${path.delimiter}${currentPath}`
        : extraDirs.join(path.delimiter);
    } else {
      const extraDirs = [
        path.join(os.homedir(), ".cargo", "bin"),
        path.join(os.homedir(), ".local", "bin"),
        "/opt/homebrew/bin",
        "/usr/local/bin",
      ];
      const currentPath = env.PATH ?? "/usr/bin:/bin";

      env.PATH = `${extraDirs.join(path.delimiter)}${path.delimiter}${currentPath}`;
    }

    if (envOverrides) {
      Object.assign(env, envOverrides);
    }

    return env;
  }

  /**
   * Executes a `tes` CLI command and returns raw `stdout`.
   *
   * Error conditions: Throws `Error` if spawning fails, the timeout expires, or exit code is non-zero.
   *
   * @param args CLI arguments to pass to `tes`.
   * @param envOverrides Optional environment variable overrides.
   * @param timeoutMs Maximum execution duration in milliseconds before killing the child process.
   * @returns The standard output string from the command.
   */
  public async runRaw(
    args: readonly string[],
    envOverrides?: Readonly<Record<string, string>>,
    timeoutMs = DEFAULT_QUERY_TIMEOUT_MS
  ): Promise<string> {
    const bin = this.resolveBinPath();
    const fullArgs = ["-C", this.repoRoot, ...args];
    const { stdout } = await execFileAsync(bin, fullArgs, {
      cwd: this.repoRoot,
      maxBuffer: 16 * 1024 * 1024,
      env: this.buildEnv(envOverrides),
      timeout: timeoutMs,
      windowsHide: true,
    });

    return stdout;
  }

  /**
   * Executes a `tes --json` command and parses its output.
   *
   * Error conditions: Throws `Error` if the command fails, times out, or JSON parsing fails.
   *
   * @param args CLI arguments to pass after `--json`.
   * @param envOverrides Optional environment variable overrides.
   * @param timeoutMs Maximum execution duration in milliseconds.
   * @returns The deserialized JSON response of type `T`.
   */
  public async runJson<T>(
    args: readonly string[],
    envOverrides?: Readonly<Record<string, string>>,
    timeoutMs = DEFAULT_QUERY_TIMEOUT_MS
  ): Promise<T> {
    const stdout = await this.runRaw(["--json", ...args], envOverrides, timeoutMs);
    return JSON.parse(stdout) as T;
  }

  /**
   * Fetches the complete state snapshot for visual panels and tree views.
   * Reuses checkpoints already present in `radarReport.active_agents` and only queries
   * a bounded `oplog` fallback when needed.
   *
   * @returns A combined snapshot of branches, active sessions, conflicts, oplog, stashes, and checkpoints.
   */
  public async fetchSnapshot(): Promise<WorkspaceStateSnapshot> {
    const [branchReport, radarReport, conflicts, stashes] = await Promise.all([
      this.runJson<BranchReport>(["branch", "--agent", "editor"]),
      this.runJson<RadarReport>(["radar"]),
      this.runJson<ConflictItem[]>(["conflicts"]),
      this.runJson<StashEntry[]>(["stash", "list"]).catch(
        (): StashEntry[] => []
      ),
    ]);

    const checkpointMap = new Map<
      string,
      { label: string; opId: string; sessionId: string }
    >();

    for (const agent of radarReport.active_agents) {
      if (agent.checkpoints) {
        for (const [label, opId] of Object.entries(agent.checkpoints)) {
          if (!checkpointMap.has(label)) {
            checkpointMap.set(label, {
              label,
              opId,
              sessionId: agent.agent_id,
            });
          }
        }
      }
    }

    const radarCarriesCheckpoints = radarReport.active_agents.some(
      (a) => a.checkpoints !== undefined
    );
    let oplog: readonly OpLogEntry[] = [];

    if (checkpointMap.size === 0 && !radarCarriesCheckpoints) {
      oplog = await this.runJson<OpLogEntry[]>([
        "oplog",
        "--limit",
        "5",
      ]).catch((): OpLogEntry[] => []);

      for (const op of oplog) {
        const agentsObj = op.view?.agents;

        if (agentsObj) {
          for (const [sid, agentData] of Object.entries(agentsObj)) {
            if (agentData.checkpoints) {
              for (const [label, opId] of Object.entries(
                agentData.checkpoints
              )) {
                if (!checkpointMap.has(label)) {
                  checkpointMap.set(label, { label, opId, sessionId: sid });
                }
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
   * Error conditions: Throws `Error` if `branch` is not a valid Git ref name.
   *
   * @param branch The target branch name to check out in the editor.
   */
  public async switchEditorBranch(branch: string): Promise<void> {
    const trimmed = branch.trim();

    if (!isValidRefName(trimmed)) {
      throw new Error(`Invalid branch name '${branch}'`);
    }

    await this.runRaw([
      "switch",
      "--editor",
      "--agent",
      "editor",
      "--",
      trimmed,
    ]);
  }

  /**
   * Creates and switches the editor working tree to a new branch, optionally forked from `from`.
   *
   * Error conditions: Throws `Error` if `branch` or `from` is not a valid Git ref name.
   *
   * @param branch The new branch name to create.
   * @param from Optional base branch or ref to fork from.
   */
  public async createBranch(branch: string, from?: string): Promise<void> {
    const trimmed = branch.trim();

    if (!isValidRefName(trimmed)) {
      throw new Error(`Invalid branch name '${branch}'`);
    }

    const args = ["switch", "--editor", "--agent", "editor"];

    if (from !== undefined && from.trim().length > 0) {
      const trimmedFrom = from.trim();

      if (!isValidRefName(trimmedFrom)) {
        throw new Error(`Invalid base branch '${from}'`);
      }

      args.push("--from", trimmedFrom);
    }

    args.push("--", trimmed);
    await this.runRaw(args);
  }

  /**
   * Switches a specific session/terminal (`sessionId`) to `branch`.
   *
   * Error conditions: Throws `Error` if `sessionId` or `branch` is invalid.
   *
   * @param sessionId The session identifier to rebind.
   * @param branch The target branch name.
   */
  public async switchSessionBranch(
    sessionId: string,
    branch: string
  ): Promise<void> {
    const trimmedSession = sessionId.trim();
    const trimmedBranch = branch.trim();

    if (!isValidSafeIdentifier(trimmedSession)) {
      throw new Error(`Invalid session identifier '${sessionId}'`);
    }

    if (!isValidRefName(trimmedBranch)) {
      throw new Error(`Invalid branch name '${branch}'`);
    }

    await this.runRaw([
      "switch",
      "--agent",
      trimmedSession,
      "--",
      trimmedBranch,
    ]);
  }

  /**
   * Closes and removes a single session (`agentId`) via `tes agent close`.
   *
   * Error conditions: Throws `Error` if `agentId` is invalid.
   *
   * @param agentId The session or agent identifier to close.
   */
  public async closeSession(agentId: string): Promise<void> {
    const trimmed = agentId.trim();

    if (!isValidSafeIdentifier(trimmed)) {
      throw new Error(`Invalid session identifier '${agentId}'`);
    }

    await this.runRaw(["agent", "close", "--", trimmed]);
  }

  /**
   * Focuses the terminal tab, multiplexer pane, or IDE window for `sessionId`.
   *
   * Error conditions: Throws `Error` if `sessionId` is invalid.
   *
   * @param sessionId The session identifier to focus.
   * @returns A report describing whether focus succeeded and which mechanism was used.
   */
  public async focusSession(sessionId: string): Promise<FocusSessionReport> {
    const trimmed = sessionId.trim();

    if (!isValidSafeIdentifier(trimmed)) {
      throw new Error(`Invalid session identifier '${sessionId}'`);
    }

    return this.runJson<FocusSessionReport>(["session", "focus", "--", trimmed]);
  }

  /**
   * Clears all clean/idle sessions (or all sessions if `force` is true) via `tes agent clear`.
   *
   * @param force Whether to force-clear active sessions with uncommitted changes.
   */
  public async clearSessions(force = false): Promise<void> {
    await this.runRaw(["agent", "clear", ...(force ? ["--force"] : [])]);
  }

  /**
   * Pops a stash entry by index (`stash@{index}`) onto the editor session.
   *
   * Error conditions: Throws `Error` if `index` is not a non-negative integer.
   *
   * @param index Zero-based index in the stash stack.
   */
  public async popStash(index: number): Promise<void> {
    if (!Number.isInteger(index) || index < 0) {
      throw new Error(`Invalid stash index '${index}'`);
    }

    await this.runRaw(["stash", "-a", "editor", "pop", String(index)]);
  }

  /**
   * Drops a stash entry by index (`stash@{index}`).
   *
   * Error conditions: Throws `Error` if `index` is not a non-negative integer.
   *
   * @param index Zero-based index in the stash stack.
   */
  public async dropStash(index: number): Promise<void> {
    if (!Number.isInteger(index) || index < 0) {
      throw new Error(`Invalid stash index '${index}'`);
    }

    await this.runRaw(["stash", "-a", "editor", "drop", String(index)]);
  }

  /**
   * Merges one or more branches or sessions into `targetBranch`.
   *
   * Error conditions: Throws `Error` if `sources` is empty or any ref/session name is invalid.
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
    const cleanTarget = targetBranch.trim();

    if (!isValidRefName(cleanTarget)) {
      throw new Error(`Invalid target branch '${targetBranch}'`);
    }

    const cleanSources = sources.map((s) => s.trim()).filter(Boolean);

    if (cleanSources.length === 0) {
      throw new Error("At least one source branch or session is required");
    }

    for (const src of cleanSources) {
      if (!isValidRefName(src) && !isValidSafeIdentifier(src)) {
        throw new Error(`Invalid merge source '${src}'`);
      }
    }

    await this.runRaw(
      ["merge", "--into", cleanTarget, "-m", message, "--", ...cleanSources],
      undefined,
      LONG_OPERATION_TIMEOUT_MS
    );
  }

  /**
   * Restores `branch` (or specific `paths`) to `revisionOrCheckpoint`.
   *
   * Error conditions: Throws `Error` if `branch`, `revisionOrCheckpoint`, or any path is invalid.
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
    const cleanBranch = branch.trim();
    const cleanRev = revisionOrCheckpoint.trim();

    if (!isValidRefName(cleanBranch)) {
      throw new Error(`Invalid branch name '${branch}'`);
    }

    if (!isValidRevisionOrCheckpoint(cleanRev)) {
      throw new Error(`Invalid revision or checkpoint '${revisionOrCheckpoint}'`);
    }

    const cleanPaths: string[] = [];

    for (const rawPath of paths) {
      const confined = resolveConfinedPath(this.repoRoot, rawPath);

      if (!confined) {
        throw new Error(`Invalid repository-relative path '${rawPath}'`);
      }

      cleanPaths.push(confined[0]);
    }

    await this.runRaw(
      ["restore", "-b", cleanBranch, "--", cleanRev, ...cleanPaths],
      undefined,
      RESTORE_TIMEOUT_MS
    );
  }

  /**
   * Saves a named checkpoint in the Operation Log.
   *
   * Error conditions: Throws `Error` if `label` is not a safe checkpoint name.
   *
   * @param label The human-readable checkpoint name.
   */
  public async createCheckpoint(label: string): Promise<void> {
    const trimmed = label.trim();

    if (!isValidRefName(trimmed)) {
      throw new Error(`Invalid checkpoint label '${label}'`);
    }

    await this.runRaw(["checkpoint", "--", trimmed]);
  }

  /**
   * Undoes the most recent operation or rolls back to `opId`.
   *
   * Error conditions: Throws `Error` if `opId` is provided and invalid.
   *
   * @param opId Optional operation ID to roll back to.
   */
  public async undo(opId?: string): Promise<void> {
    if (opId !== undefined) {
      const trimmed = opId.trim();

      if (!isValidSafeIdentifier(trimmed)) {
        throw new Error(`Invalid operation ID '${opId}'`);
      }

      await this.runRaw(["undo", "--op", trimmed]);
      return;
    }

    await this.runRaw(["undo"]);
  }

  /**
   * Resolves a conflict at `filePath` using the specified strategy or explicit content.
   * When `strategy === "content"`, writes `customContent` to a secure temporary file
   * and passes `--content-file` to avoid OS command-line length limits (`ARG_MAX`).
   *
   * Error conditions: Throws `Error` if `filePath` escapes the repository root.
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
    let cleanPath: string;

    if (isSyntheticConflictPath(filePath)) {
      cleanPath = filePath;
    } else {
      const confined = resolveConfinedPath(this.repoRoot, filePath);

      if (!confined) {
        throw new Error(`Invalid conflict path '${filePath}'`);
      }

      cleanPath = confined[0];
    }

    if (strategy === "content") {
      const tmpDir = await fs.promises.mkdtemp(
        path.join(os.tmpdir(), "tes-resolve-")
      );
      const tmpPath = path.join(tmpDir, "content.txt");

      try {
        await fs.promises.writeFile(tmpPath, customContent ?? "", {
          encoding: "utf8",
          mode: 0o600,
        });
        await this.runRaw([
          "resolve",
          "--strategy",
          "content",
          "--content-file",
          tmpPath,
          "--",
          cleanPath,
        ]);
      } finally {
        await fs.promises
          .rm(tmpDir, { recursive: true, force: true })
          .catch(() => undefined);
      }

      return;
    }

    await this.runRaw(["resolve", "--strategy", strategy, "--", cleanPath]);
  }

  /**
   * Pushes `branch` to `origin`.
   *
   * Error conditions: Throws `Error` if `branch` is not a valid Git ref name.
   *
   * @param branch The branch name to push.
   */
  public async pushBranch(branch: string): Promise<void> {
    const trimmed = branch.trim();

    if (!isValidRefName(trimmed)) {
      throw new Error(`Invalid branch name '${branch}'`);
    }

    await this.runRaw(
      ["push", "--", "origin", trimmed],
      undefined,
      LONG_OPERATION_TIMEOUT_MS
    );
  }

  /**
   * Deletes `branch` safely (`force = false`) or forcefully (`force = true`).
   *
   * Error conditions: Throws `Error` if `branch` is not a valid Git ref name.
   *
   * @param branch The branch name to delete.
   * @param force Whether to force-delete an unmerged branch (`-D`).
   */
  public async deleteBranch(branch: string, force = false): Promise<void> {
    const trimmed = branch.trim();

    if (!isValidRefName(trimmed)) {
      throw new Error(`Invalid branch name '${branch}'`);
    }

    // Bind the branch value with `=` so Clap never interprets the value as an option flag.
    const deleteFlag = force
      ? `--force-delete=${trimmed}`
      : `--delete=${trimmed}`;
    await this.runRaw(["branch", deleteFlag]);
  }

  /**
   * Discards uncommitted working changes on `branch` by restoring it to its latest commit (`refs/heads/<branch>`).
   *
   * Error conditions: Throws `Error` if `branch` is not a valid Git ref name.
   *
   * @param branch The branch whose working changes should be discarded.
   */
  public async discardWorkingChanges(branch: string): Promise<void> {
    const trimmed = branch.trim();

    if (!isValidRefName(trimmed)) {
      throw new Error(`Invalid branch name '${branch}'`);
    }

    await this.runRaw(["restore", "-b", trimmed, "."]);
  }

  /**
   * Executes `tes gc --json` to compact ephemeral operations and prune orphaned or stale
   * sandboxes, caches, and dead port leases.
   *
   * Error conditions: Throws `Error` if `tes` exits non-zero, times out, or returns invalid JSON.
   *
   * @param options Optional garbage collection mode flags (`dryRun`, `keepMerged`, `staleDays`, `pruneCaches`, `allRepos`).
   * @returns The structured `GcReport` returned by the CLI.
   */
  public async runGc(options?: GcOptions): Promise<GcReport> {
    const args: string[] = ["gc"];

    if (options?.dryRun) {
      args.push("--dry-run");
    }

    if (options?.keepMerged) {
      args.push("--keep-merged");
    }

    if (
      options?.staleDays !== undefined &&
      Number.isInteger(options.staleDays) &&
      options.staleDays >= 0
    ) {
      args.push("--stale", String(options.staleDays));
    }

    if (options?.pruneCaches) {
      args.push("--prune-caches");
    }

    if (options?.allRepos) {
      args.push("--all");
    }

    return this.runJson<GcReport>(args, undefined, LONG_OPERATION_TIMEOUT_MS);
  }

  /**
   * Executes `tes doctor --json` and returns the structured diagnostic report, preserving
   * valid JSON output even when `tes doctor` exits with status 1 due to failing health checks.
   *
   * Error conditions: Throws `Error` if spawning fails or output is not valid diagnostic JSON.
   *
   * @returns The parsed `DoctorReport`.
   */
  public async runDoctor(): Promise<DoctorReport> {
    try {
      return await this.runJson<DoctorReport>(["doctor"]);
    } catch (err: unknown) {
      if (typeof err === "object" && err !== null && "stdout" in err) {
        const stdout = (err as { readonly stdout?: unknown }).stdout;

        if (typeof stdout === "string" && stdout.trim().startsWith("{")) {
          return JSON.parse(stdout) as DoctorReport;
        }
      }

      throw err;
    }
  }

  /**
   * Returns true if `sandboxDir` exists on disk and represents a materialized working tree.
   *
   * @param sandboxDir The workspace directory path reported by `tes branch`.
   * @returns True if the directory is materialized and authoritative for file presence.
   */
  private async isMaterializedSandbox(sandboxDir: string): Promise<boolean> {
    try {
      const stat = await fs.promises.stat(sandboxDir);

      if (!stat.isDirectory()) {
        return false;
      }

      if (path.resolve(sandboxDir) === path.resolve(this.repoRoot)) {
        return true;
      }

      const entries = await fs.promises.readdir(sandboxDir);
      return entries.length > 0;
    } catch {
      return false;
    }
  }

  /**
   * Reads a file from a specific branch sandbox or Git commit tree for virtual diffing.
   * Does not fall back to `git show` when a materialized branch sandbox reports `ENOENT`
   * so deleted uncommitted files are not silently resurrected.
   *
   * Error conditions: Throws `BranchFileNotFoundError` when the file is absent on `branch`,
   * or `Error` for invalid refs, path confinement failures, or I/O/Git failures.
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
      const snapshot = await this.runJson<BranchReport>([
        "branch",
        "--agent",
        "editor",
      ]);
      const detail = snapshot.branch_details.find((b) => b.name === branch);

      if (detail && detail.workspace_path) {
        const materialized = await this.isMaterializedSandbox(
          detail.workspace_path
        );

        if (materialized) {
          const confinedSandbox = resolveConfinedPath(
            detail.workspace_path,
            cleanRelPath
          );

          if (!confinedSandbox) {
            throw new Error(
              `Path '${cleanRelPath}' escaped sandbox for branch '${branch}'`
            );
          }

          try {
            return await fs.promises.readFile(confinedSandbox[1], "utf8");
          } catch (err: unknown) {
            const code =
              typeof err === "object" && err !== null && "code" in err
                ? (err as { readonly code?: unknown }).code
                : undefined;

            if (code === "ENOENT") {
              throw new BranchFileNotFoundError(branch, cleanRelPath);
            }

            throw err;
          }
        }
      }
    }

    try {
      const { stdout } = await execFileAsync(
        "git",
        ["-C", this.repoRoot, "show", `${branch}:${cleanRelPath}`],
        {
          maxBuffer: 10 * 1024 * 1024,
          env: this.buildEnv(),
          timeout: DEFAULT_QUERY_TIMEOUT_MS,
          windowsHide: true,
        }
      );

      return stdout;
    } catch (err: unknown) {
      const detail = extractExecErrorText(err);

      if (
        detail.includes("does not exist in") ||
        detail.includes("exists on disk, but not in") ||
        detail.includes("does not exist")
      ) {
        throw new BranchFileNotFoundError(branch, cleanRelPath);
      }

      throw err;
    }
  }
}
