//! Multi-agent coordinator, Zero-Command Harness Auto-Pilot, Symbol Collision Radar,
//! Speculative Verification Gate, and non-blocking auto-rebase engine for Tessera.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use serde::{Deserialize, Serialize};
pub use autopilot::{
    detect_session_id, detect_session_waypoint, focus_session_waypoint, handle_post_tool_hook,
    handle_pre_tool_hook, install_harness_integrations, install_harness_integrations_for,
    run_auto_watch_daemon, FocusSessionReport, PostToolOutcome, PreToolOutcome,
};
pub use core::SessionWaypoint;
use core::{
    atomic_write_file, is_valid_sandbox_repo_root, now_millis, resolve_confined_repo_path,
    validate_ref_name, validate_repo_rel_path, validate_safe_identifier,
    write_working_tree_file_safe, AgentMetadata, ChangeId, ConflictKind, ConflictRecord, CoreError,
    CoreResult, FileEntry, GitBackend, Merge, ObjectId, OpId, OpLogStore, Operation, StashEntry,
    View,
};
use merge::{
    attribute_agent_hunks, diff_modified_symbols, merge_ast_union, merge_file_3way,
    merge_trees_3way, MigrationResequence, RenameMatch,
};
use workspace::{
    build_agent_env_with_config, create_cow_workspace, ensure_polyglot_port_shims,
    materialize_tree_to_dir, snapshot_working_dir, sync_missing_cow_caches,
};

pub mod autopilot;

/// Per-commit authoring, amend, and co-author trailer overrides for `tes commit` and `tes merge`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitOptions {
    /// Optional explicit commit message.
    pub message: Option<String>,
    /// Optional explicit commit author override (`"Name <email>"`).
    pub author: Option<String>,
    /// Optional override for `co_author` (`Some(true)` for `--co-author`, `Some(false)` for `--no-co-author`).
    pub co_author: Option<bool>,
    /// Optional explicit `Co-authored-by` trailer identities (`"Name <email>"`).
    pub co_author_trailers: Vec<String>,
    /// Whether to amend the tip commit on the target branch instead of creating a child commit.
    #[serde(default)]
    pub amend: bool,
}

/// Rebased uncommitted state of a still-active session after a commit advanced its branch tip.
struct RebasedSession {
    /// Edited blobs re-anchored onto the new branch tip.
    own_blobs: BTreeMap<String, ObjectId>,
    /// Deletion tombstones still applicable on the new branch tip.
    deleted_paths: BTreeSet<String>,
    /// New session snapshot commit (parented on the new branch tip).
    head_commit: ObjectId,
    /// New session snapshot tree.
    head_tree: ObjectId,
}

/// Result of initializing a repository with `tes init`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitReport {
    /// Repository root path.
    pub repo_root: String,
    /// Initial trunk branch name.
    pub trunk_branch: String,
    /// Head commit SHA-1 on trunk.
    pub head_commit: ObjectId,
    /// Initial operation ID in `.git/tessera/oplog/`.
    pub root_op_id: OpId,
    /// List of harness integration files installed (`.agents/rules/tessera.md`, `.claude/settings.json`, etc.).
    pub harness_files_installed: Vec<String>,
}

/// Collision entry detected by `tes radar` when two or more active sessions touch the same
/// file or AST declaration concurrently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolCollision {
    /// Repository-relative file path or `path::symbol` identifier.
    pub target: String,
    /// Active sessions touching `target`, mapped as `agent_id -> intent`.
    pub competing_agents: BTreeMap<String, String>,
}

/// Live line-range attribution for an active agent or unmerged branch in a workspace file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLineAttribution {
    /// Relative repository path of the file (e.g. `"src/App.vue"`).
    pub path: String,
    /// Agent or session identifier (e.g. `"antigravity-a1b2"` or branch name).
    pub agent_id: String,
    /// Effective branch name for this attribution (e.g. `"feat/pomodoro-timer"` or `"main"`).
    pub branch: String,
    /// Human-readable task intent or commit summary.
    pub intent: String,
    /// Deterministic color slot (`1..=N`) for consistent visual styling.
    pub slot: u16,
    /// 1-based inclusive start line in the current working tree file.
    pub start_line: usize,
    /// 1-based inclusive end line in the current working tree file.
    pub end_line: usize,
    /// `true` when the agent's lines are materialized in the current working tree file;
    /// `false` when anchored to target context lines from another branch.
    pub in_working_tree: bool,
    /// Optional AST symbol name if this range corresponds to a claimed symbol.
    pub symbol: Option<String>,
}

/// Complete report returned by `tes radar`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadarReport {
    /// All active agent sessions in the repository.
    pub active_agents: Vec<AgentMetadata>,
    /// Overlapping file and AST symbol edits across active sessions.
    pub collisions: Vec<SymbolCollision>,
    /// Active unresolved conflicts in `.git/tessera/conflicts/`.
    pub active_conflicts: Vec<ConflictRecord>,
    /// Live per-file line-range attributions for active agents and unmerged branches.
    #[serde(default)]
    pub file_attributions: Vec<FileLineAttribution>,
}

/// Result of merging one or more agent sessions into a target branch via `tes merge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeReport {
    /// Target branch name (e.g., `"main"`).
    pub target_branch: String,
    /// Resulting Git commit [`ObjectId`] (advanced on `target_branch` if verification passed).
    pub merged_commit_id: ObjectId,
    /// Resulting Git root tree [`ObjectId`].
    pub merged_tree_id: ObjectId,
    /// Number of unchanged directory subtrees skipped in `O(1)`.
    pub skipped_subtrees: usize,
    /// First-class [`ConflictRecord`]s recorded during the merge (non-blocking).
    pub conflicts: Vec<ConflictRecord>,
    /// Cross-directory or cross-package file moves detected and routed automatically.
    pub renames_applied: Vec<RenameMatch>,
    /// Colliding sequential SQL migrations automatically renumbered.
    pub migrations_resequenced: Vec<MigrationResequence>,
    /// Packages or project roots affected by the merged paths.
    pub affected_packages: Vec<String>,
    /// Agent sessions automatically closed after a clean, verified merge.
    pub closed_agents: Vec<String>,
    /// `true` if the speculative verification gate (`--verify`) passed (or was not requested).
    pub verification_passed: bool,
    /// Compiler/test output if `--verify` was executed.
    pub verification_output: Option<String>,
    /// `false` if nothing changed, so `merged_commit_id` is the existing branch tip and no
    /// empty commit was written (an unchanged tree on a single parent without `--amend`).
    pub created_commit: bool,
}

/// Summary returned by `tes restore` when restoring a branch or specific file paths to a commit or checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    /// Branch that was restored.
    pub branch: String,
    /// Source commit [`ObjectId`] restored from.
    pub source_commit_id: ObjectId,
    /// Specific file paths restored (empty when the entire branch was restored to `source_commit_id`).
    pub restored_paths: Vec<String>,
    /// Whether the entire branch reference (`refs/heads/<branch>`) was reset to `source_commit_id`.
    pub branch_reset: bool,
}

/// Summary returned by `tes branch -d / -D` when deleting a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteBranchReport {
    /// Name of the deleted branch.
    pub deleted_branch: String,
    /// Commit [`ObjectId`] that the branch pointed to before deletion, if any.
    pub previous_commit: Option<ObjectId>,
    /// Agent or terminal sessions that were detached from the deleted branch back to trunk.
    pub detached_sessions: Vec<String>,
}

/// Summary returned by `tes branch -m` when renaming a branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameBranchReport {
    /// Previous branch name.
    pub old_branch: String,
    /// New branch name.
    pub new_branch: String,
    /// Commit [`ObjectId`] pointed to by the renamed branch.
    pub commit_id: ObjectId,
}

/// Summary returned by `tes stash pop` or `tes stash apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashPopReport {
    /// The stash entry that was applied.
    pub stash: StashEntry,
    /// Branch onto which the stash was applied.
    pub applied_branch: String,
    /// File paths restored or AST-merged onto the working tree.
    pub restored_paths: Vec<String>,
    /// Whether any file had overlapping conflict markers during 3-way AST application.
    pub has_conflicts: bool,
    /// Whether the stash entry was dropped from the stash stack (`true` for `pop`, `false` for `apply`).
    pub dropped: bool,
}

/// Summary returned by `tes pull`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullReport {
    /// Remote name (e.g. `"origin"`).
    pub remote: String,
    /// Branch name pulled.
    pub branch: String,
    /// Commit [`ObjectId`] before pulling.
    pub old_commit: ObjectId,
    /// Commit [`ObjectId`] after pulling.
    pub new_commit: ObjectId,
    /// Whether the pull was a clean fast-forward (or already up to date).
    pub fast_forward: bool,
    /// Active agent sessions rebased onto the updated branch commit.
    pub rebased_sessions: usize,
}

/// Summary returned by `tes cherry-pick` when applying a commit's delta onto a target branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CherryPickReport {
    /// Full 40-char SHA-1 of the source commit that was cherry-picked.
    pub source_commit: ObjectId,
    /// Target branch onto which the commit delta was applied.
    pub target_branch: String,
    /// Resulting Git commit [`ObjectId`] created on `target_branch`.
    pub new_commit_id: ObjectId,
    /// First-class [`ConflictRecord`]s recorded if any files had overlapping AST edits.
    pub conflicts: Vec<ConflictRecord>,
    /// File paths modified or created by the cherry-picked commit.
    pub touched_paths: Vec<String>,
}

/// Enriched commit log entry returned by `tes log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Full 40-char commit SHA-1.
    pub sha: String,
    /// Short 8-char commit SHA-1.
    pub short_sha: String,
    /// Parent commit SHA-1s.
    pub parents: Vec<String>,
    /// Author string.
    pub author: String,
    /// Commit Unix timestamp in seconds.
    pub timestamp: i64,
    /// Commit subject line.
    pub subject: String,
    /// Attached Git ref labels (`HEAD -> main`, branch names, tags).
    pub refs: Vec<String>,
}

/// Strategy for resolving a first-class conflict via `tes resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolveStrategy {
    /// Take the first side (`adds[0]`).
    Ours,
    /// Take the second side (`adds[1]`).
    Theirs,
    /// Combine both sides' AST declarations and statement additions (`--strategy ast-union`).
    AstUnion,
    /// Read the current file content on disk (after manual or agent edit) as the resolved state.
    WorkingFile,
    /// Use explicit content provided via CLI/MCP.
    ExplicitContent(String),
}

/// High-level coordinator orchestrating `.git`, `.git/tessera`, Copy-on-Write workspaces,
/// Zero-Command Auto-Pilot sessions, and semantic 3-way merges.
#[derive(Debug, Clone)]
pub struct Coordinator {
    git: GitBackend,
    oplog: OpLogStore,
    invocation_dir: Option<PathBuf>,
}

impl Coordinator {
    /// Opens an existing Tessera-enabled repository by discovering `.git` from `start_dir`.
    ///
    /// # Errors
    /// Returns [`CoreError::RepoNotFound`] if `.git` or `.git/tessera` is not initialized.
    pub fn open(start_dir: &Path) -> CoreResult<Self> {
        let git = GitBackend::discover(start_dir)?;
        let oplog = OpLogStore::new(git.git_dir());
        if !oplog.is_initialized() {
            return Err(CoreError::RepoNotFound(
                oplog.store_dir().display().to_string(),
            ));
        }
        let invocation_dir = Self::discover_sandbox_root(start_dir, git.repo_root());
        let coord = Self {
            git,
            oplog,
            invocation_dir,
        };
        coord.prune_dead_terminal_sessions();
        Ok(coord)
    }

    /// Walks upward from `start_dir` until `repo_root` to detect whether the caller is executing
    /// inside an isolated Copy-on-Write branch sandbox (`.tessera/repo_root`).
    fn discover_sandbox_root(start_dir: &Path, repo_root: &Path) -> Option<PathBuf> {
        let mut curr = start_dir.to_path_buf();
        loop {
            if curr == repo_root {
                return None;
            }

            if is_valid_sandbox_repo_root(&curr, Some(repo_root)).is_some() {
                return Some(curr);
            }

            if !curr.pop() {
                return None;
            }
        }
    }

    /// Returns the Copy-on-Write sandbox root directory if `tes` was invoked from inside a sandbox.
    #[must_use]
    pub fn invocation_dir(&self) -> Option<&Path> {
        self.invocation_dir.as_deref()
    }

    /// Automatically removes stale passive `term-<pid>` and `session-<pid>` entries whose OS process
    /// is no longer running, as well as clean idle sessions older than 30 minutes, so dead or
    /// abandoned sessions never pollute radar or conflict resolution.
    pub fn prune_dead_terminal_sessions(&self) {
        const IDLE_TTL_MS: u64 = 30 * 60 * 1000;

        let Ok(current) = self.oplog.load_current_reconciled() else {
            return;
        };

        let now = now_millis();
        let mut dead_ids = Vec::new();

        for (id, agent) in &current.view.agents {
            let pid_opt = id
                .strip_prefix("term-")
                .or_else(|| id.strip_prefix("session-"))
                .and_then(|s| s.parse::<i32>().ok());

            if let Some(pid) = pid_opt {
                let matches_shell_pid = agent
                    .waypoint
                    .as_ref()
                    .and_then(|wp| wp.shell_pid)
                    .is_some_and(|spid| spid == pid as u32);

                if pid > 1
                    && matches_shell_pid
                    && agent.intent_inferred
                    && agent.workspace_path.is_none()
                {
                    let alive = crate::autopilot::is_process_alive(pid);

                    if !alive {
                        dead_ids.push(id.clone());
                        continue;
                    }
                }
            }

            if !agent.has_own_changes()
                && agent.head_commit == agent.base_commit
                && agent.workspace_path.is_none()
                && now.saturating_sub(agent.updated_at_ms) > IDLE_TTL_MS
            {
                dead_ids.push(id.clone());
            }
        }

        if !dead_ids.is_empty() {
            let _ = self.oplog.record_tx(
                "gc",
                format!("prune dead shell sessions {:?}", dead_ids),
                true,
                |view| {
                    for id in &dead_ids {
                        view.agents.remove(id);
                    }

                    Ok(())
                },
            );

            for id in &dead_ids {
                self.git.delete_ref(&format!("refs/tessera/agents/{id}"));
            }
        }
    }

    /// Initializes `.git` (if needed), `.git/tessera`, snapshots the initial working tree if untracked
    /// files exist on a fresh repo, and installs repo-local harness integrations.
    ///
    /// # Errors
    /// Returns [`CoreError`] if repository or hook initialization fails.
    pub fn init_repo(repo_root: &Path) -> CoreResult<(Self, InitReport)> {
        Self::init_repo_with_options(repo_root, None, None)
    }

    /// Initializes `.git` (if needed), `.git/tessera`, optionally sets the initial/target branch to
    /// `branch_override`, snapshots the initial working tree, and installs harness integrations.
    ///
    /// # Errors
    /// Returns [`CoreError`] if repository or hook initialization fails.
    pub fn init_repo_with_branch(
        repo_root: &Path,
        branch_override: Option<&str>,
    ) -> CoreResult<(Self, InitReport)> {
        Self::init_repo_with_options(repo_root, branch_override, None)
    }

    /// Initializes `.git` (if needed), `.git/tessera`, optionally sets the initial/target branch to
    /// `branch_override`, and installs harness integrations filtered by `harness_filter`
    /// (`"antigravity"`, `"claude"`, `"cursor"`, `"all"`, or `None` for auto-detection).
    ///
    /// # Errors
    /// Returns [`CoreError`] if repository or hook initialization fails.
    pub fn init_repo_with_options(
        repo_root: &Path,
        branch_override: Option<&str>,
        harness_filter: Option<&str>,
    ) -> CoreResult<(Self, InitReport)> {
        let git = GitBackend::init_or_open(repo_root)?;
        let oplog = OpLogStore::new(git.git_dir());

        if let Some(b) = branch_override.map(str::trim).filter(|s| !s.is_empty()) {
            if let Ok(Some(head_oid)) = git.resolve_head_commit() {
                git.update_ref(&format!("refs/heads/{b}"), &head_oid)?;
            }
            git.set_head_branch(b)?;
        }

        // Install harness auto-discovery files first so they can be part of the workspace.
        let harness_files =
            install_harness_integrations_for(git.repo_root(), git.git_dir(), harness_filter)?;

        // Ensure stat_cache directory exists before snapshotting.
        fs::create_dir_all(git.git_dir().join("tessera/stat_cache")).map_err(|source| {
            CoreError::Io {
                path: git.git_dir().display().to_string(),
                source,
            }
        })?;

        let branch = git
            .current_branch_name()
            .unwrap_or_else(|_| "main".to_string());
        let current_head = git
            .resolve_head_commit()?
            .ok_or_else(|| CoreError::NotFound("HEAD commit".to_string()))?;
        let head_commit_obj = git.read_commit(&current_head)?;
        let head_files = git.read_tree_to_map(&head_commit_obj.tree_id)?;

        // If the initial commit was an empty tree and the directory already has files, snapshot them into the root commit.
        let final_head = if head_files.is_empty() {
            let snap = snapshot_working_dir(&git, git.repo_root(), "root", &[], None)?;
            if !snap.files.is_empty() {
                let parents: Vec<ObjectId> = if head_commit_obj.parents.is_empty() {
                    Vec::new()
                } else {
                    vec![current_head]
                };
                let author = git.resolve_git_author();
                let updated_commit = git.write_commit(
                    &snap.root_tree_id,
                    &parents,
                    &author,
                    "Initial commit",
                )?;
                git.update_ref(&format!("refs/heads/{branch}"), &updated_commit)?;
                git.sync_git_index(&updated_commit);
                updated_commit
            } else {
                current_head
            }
        } else {
            current_head
        };

        let is_fresh_init = !oplog.is_initialized();
        let mut initial_view = View::default();
        initial_view.branches.insert(branch.clone(), final_head);
        initial_view.next_agent_slot = 1;

        let root_op = oplog.init(initial_view)?;
        let mut cfg = oplog.load_config()?;
        if cfg.trunk_branch != branch {
            cfg.trunk_branch = branch.clone();
            oplog.save_config(&cfg)?;
        }
        let coord = Self {
            git,
            oplog,
            invocation_dir: None,
        };
        if is_fresh_init {
            let _ = fs::remove_dir_all(coord.sandboxes_root());
        }
        let report = InitReport {
            repo_root: coord.git.repo_root().display().to_string(),
            trunk_branch: branch,
            head_commit: final_head,
            root_op_id: root_op.id,
            harness_files_installed: harness_files,
        };
        Ok((coord, report))
    }

    /// Returns a reference to the underlying [`GitBackend`].
    pub fn git(&self) -> &GitBackend {
        &self.git
    }

    /// Returns a reference to the underlying [`OpLogStore`].
    pub fn oplog(&self) -> &OpLogStore {
        &self.oplog
    }

    /// Ensures that an AI agent session (`session_id`) is registered in `.git/tessera`.
    ///
    /// If the session is already active, updates its `intent` if `explicit_intent` is `Some`.
    /// If the session is new, allocates a unique runtime slot (`1..=N`), generates a reverse-hex
    /// [`ChangeId`], and branches from the current trunk HEAD.
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading HEAD or writing to the `OpLog` fails.
    pub fn ensure_session(
        &self,
        session_id: &str,
        explicit_intent: Option<&str>,
    ) -> CoreResult<AgentMetadata> {
        validate_safe_identifier("session_id", session_id)?;
        let head_commit = self
            .git
            .resolve_head_commit()?
            .ok_or_else(|| CoreError::NotFound("HEAD commit".to_string()))?;
        let commit_obj = self.git.read_commit(&head_commit)?;
        let head_branch = self.git.current_branch_name().unwrap_or_default();
        let foreign_baseline = if is_ai_session_id(session_id) {
            match self.oplog.load_current_reconciled() {
                Ok(cur) if !cur.view.agents.contains_key(session_id) => {
                    self.unowned_worktree_changes(&cur.view, &commit_obj.tree_id)
                }
                _ => BTreeSet::new(),
            }
        } else {
            BTreeSet::new()
        };

        let mut out_agent: Option<AgentMetadata> = None;
        self.oplog.record_tx(
            session_id,
            format!("ensure session {session_id}"),
            true,
            |view| {
                let is_ai_agent = !session_id.starts_with("term-")
                    && !session_id.starts_with("session-")
                    && !session_id.starts_with("mcp-")
                    && session_id != "editor";

                let adopted_placeholder = if is_ai_agent {
                    let existing_needs_adoption = view.agents.get(session_id).is_none_or(|a| {
                        a.targets_head_branch(&head_branch)
                            && a.intent_inferred
                            && !a.has_own_changes()
                    });
                    if existing_needs_adoption {
                        let placeholder_id = view
                            .agents
                            .iter()
                            .filter(|(id, a)| {
                                id.starts_with("mcp-")
                                    || ((id.starts_with("term-") || id.starts_with("session-"))
                                        && !a.intent_inferred
                                        && !a.has_own_changes()
                                        && (a.target_branch.is_some() || !a.claimed_symbols.is_empty()))
                            })
                            .max_by_key(|(_, a)| a.updated_at_ms)
                            .map(|(id, _)| id.clone());
                        placeholder_id.and_then(|pid| view.agents.remove(&pid))
                    } else {
                        None
                    }
                } else {
                    None
                };

                let is_term_session =
                    session_id.starts_with("term-") || session_id.starts_with("session-");
                let initial_branch = if is_term_session {
                    self.git.current_branch_name().ok()
                } else {
                    None
                };
                let detected_waypoint =
                    (session_id != "editor").then(|| detect_session_waypoint(session_id));
                let used_by_others: BTreeSet<u16> = view
                    .agents
                    .iter()
                    .filter(|(id, _)| *id != session_id)
                    .map(|(_, a)| a.slot)
                    .collect();
                let lowest_free_slot = (1u16..=u16::MAX)
                    .find(|s| !used_by_others.contains(s))
                    .unwrap_or(1);

                if let Some(existing) = view.agents.get_mut(session_id) {
                    let max_compact_slot = (used_by_others.len() as u16).saturating_add(1).max(1);
                    if existing.workspace_path.is_none()
                        && (existing.slot == 0
                            || used_by_others.contains(&existing.slot)
                            || existing.slot > max_compact_slot)
                    {
                        existing.slot = lowest_free_slot;
                    }

                    if let Some(adopted) = adopted_placeholder {
                        if existing.intent_inferred && !adopted.intent_inferred {
                            existing.intent = adopted.intent;
                            existing.intent_inferred = false;
                        }

                        if existing.targets_head_branch(&head_branch) {
                            existing.target_branch = adopted.target_branch;
                            if !existing.has_own_changes() {
                                existing.base_commit = adopted.base_commit;
                                existing.head_commit = adopted.head_commit;
                                existing.head_tree = adopted.head_tree;
                            }
                        }

                        existing.claimed_symbols.extend(adopted.claimed_symbols);
                        for (k, v) in adopted.read_blobs {
                            existing.read_blobs.entry(k).or_insert(v);
                        }
                    }

                    if existing.target_branch.is_none() && is_term_session {
                        existing.target_branch = initial_branch;
                    }

                    if existing.targets_head_branch(&head_branch)
                        && !existing.has_own_changes()
                        && existing.claimed_symbols.is_empty()
                        && existing.read_blobs.is_empty()
                        && existing.head_commit == existing.base_commit
                        && existing.base_commit != head_commit
                    {
                        existing.base_commit = head_commit;
                        existing.head_commit = head_commit;
                        existing.head_tree = commit_obj.tree_id;
                        existing.read_blobs.clear();
                        existing.integrated_peer_blobs.clear();
                    }

                    if let Some(intent_str) = explicit_intent {
                        if !intent_str.trim().is_empty() {
                            existing.intent = intent_str.trim().to_string();
                            existing.intent_inferred = false;
                        }
                    }

                    if session_id != "editor"
                        && (existing.waypoint.is_none() || detect_session_id(None) == session_id)
                    {
                        existing.waypoint = detected_waypoint.clone();
                    }

                    existing.updated_at_ms = now_millis();
                    let active_slot = existing.slot;
                    out_agent = Some(existing.clone());
                    view.next_agent_slot = used_by_others
                        .iter()
                        .copied()
                        .max()
                        .unwrap_or(0)
                        .max(active_slot)
                        .saturating_add(1);
                    return Ok(());
                }

                let slot = lowest_free_slot;
                view.next_agent_slot = used_by_others
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(0)
                    .max(slot)
                    .saturating_add(1);
                let change_id =
                    ChangeId::from_seed(&format!("{session_id}-{}", head_commit.as_str()));
                let (mut intent, mut intent_inferred) = match explicit_intent {
                    Some(s) if !s.trim().is_empty() => (s.trim().to_string(), false),
                    _ => (format!("Active session {session_id}"), true),
                };

                let mut base_commit = head_commit;
                let mut head_commit_val = head_commit;
                let mut head_tree = commit_obj.tree_id;
                let mut claimed_symbols = BTreeSet::new();
                let mut read_blobs = BTreeMap::new();
                let mut target_branch = initial_branch;
                let mut waypoint = detected_waypoint;

                if let Some(adopted) = adopted_placeholder {
                    if intent_inferred && !adopted.intent_inferred {
                        intent = adopted.intent;
                        intent_inferred = false;
                    }
                    base_commit = adopted.base_commit;
                    head_commit_val = adopted.head_commit;
                    head_tree = adopted.head_tree;
                    claimed_symbols = adopted.claimed_symbols;
                    read_blobs = adopted.read_blobs;
                    target_branch = adopted.target_branch;
                    if waypoint.is_none() {
                        waypoint = adopted.waypoint;
                    }
                }

                let meta = AgentMetadata {
                    agent_id: session_id.to_string(),
                    slot,
                    change_id: change_id.clone(),
                    base_commit,
                    head_commit: head_commit_val,
                    head_tree,
                    intent,
                    intent_inferred,
                    claimed_symbols,
                    scopes: Vec::new(),
                    workspace_path: None,
                    read_blobs,
                    own_blobs: BTreeMap::new(),
                    deleted_paths: BTreeSet::new(),
                    foreign_baseline: foreign_baseline.clone(),
                    integrated_peer_blobs: BTreeMap::new(),
                    target_branch,
                    checkpoints: BTreeMap::new(),
                    waypoint,
                    updated_at_ms: now_millis(),
                };
                view.changes.insert(change_id, head_commit_val);
                view.agents.insert(session_id.to_string(), meta.clone());
                out_agent = Some(meta);
                Ok(())
            },
        )?;

        let meta = out_agent.ok_or_else(|| CoreError::NotFound(session_id.to_string()))?;
        self.git
            .update_ref(&format!("refs/tessera/agents/{session_id}"), &meta.head_commit)?;
        self.git.update_ref(
            &format!("refs/tessera/changes/{}", meta.change_id.as_str()),
            &meta.head_commit,
        )?;
        Ok(meta)
    }

    /// Spawns an isolated Copy-on-Write workspace at `.tessera/workspaces/<agent_id>` for an agent,
    /// cloning warm build caches (`target/`, `node_modules/`) via OS COW and sanitizing `.d` stamps.
    ///
    /// # Errors
    /// Returns [`CoreError`] if workspace creation or `OpLog` recording fails.
    pub fn spawn_cow_agent(
        &self,
        agent_id: &str,
        intent: Option<&str>,
        scopes: Vec<String>,
    ) -> CoreResult<AgentMetadata> {
        validate_safe_identifier("agent_id", agent_id)?;
        let cfg = self.oplog.load_config()?;
        let ws_dir = self
            .git
            .repo_root()
            .join(".tessera")
            .join("workspaces")
            .join(agent_id);

        create_cow_workspace(
            self.git.repo_root(),
            &ws_dir,
            &scopes,
            &cfg.cow_cache_dirs,
        )?;

        let _ = self.ensure_session(agent_id, intent)?;
        let ws_path_str = ws_dir.display().to_string();
        let mut updated_meta = None;

        self.oplog.record_tx(
            agent_id,
            format!("spawn COW workspace for {agent_id}"),
            false,
            |view| {
                if let Some(agent) = view.agents.get_mut(agent_id) {
                    agent.scopes = scopes.clone();
                    agent.workspace_path = Some(ws_path_str.clone());
                    agent.updated_at_ms = now_millis();
                    updated_meta = Some(agent.clone());
                }
                Ok(())
            },
        )?;

        updated_meta.ok_or_else(|| CoreError::NotFound(agent_id.to_string()))
    }

    /// Incrementally snapshots the working directory of `agent_id` (either its isolated COW workspace
    /// or the shared repository root), extracts modified AST symbols via Tree-sitter, auto-infers
    /// semantic intent if needed, and updates the agent's Git commit and `ChangeId` ref.
    ///
    /// # Errors
    /// Returns [`CoreError`] if snapshotting or Git commit creation fails.
    pub fn snapshot_agent(
        &self,
        agent_id: &str,
        is_ephemeral: bool,
    ) -> CoreResult<AgentMetadata> {
        let meta = self.ensure_session(agent_id, None)?;
        let work_dir = meta
            .workspace_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.git.repo_root().to_path_buf());

        let base_commit_obj = self.git.read_commit(&meta.base_commit)?;
        let mut snap = snapshot_working_dir(
            &self.git,
            &work_dir,
            agent_id,
            &meta.scopes,
            Some(&base_commit_obj.tree_id),
        )?;

        let base_files = self.git.read_tree_to_map(&base_commit_obj.tree_id)?;
        let mut sandbox_own_blobs: BTreeMap<String, ObjectId> = BTreeMap::new();
        let mut snapshot_deleted_paths: BTreeSet<String> = BTreeSet::new();

        if meta.workspace_path.is_none() {
            let current = self.oplog.load_current_reconciled()?;
            let other_owned_paths: BTreeSet<String> = current
                .view
                .agents
                .values()
                .filter(|a| a.agent_id != agent_id)
                .flat_map(|a| a.owned_paths().cloned())
                .collect();
            let other_touched_paths: BTreeSet<String> = current
                .view
                .agents
                .values()
                .filter(|a| a.agent_id != agent_id)
                .flat_map(|a| {
                    a.owned_paths()
                        .cloned()
                        .chain(a.read_blobs.keys().cloned())
                        .chain(a.claimed_symbols.iter().map(|s| {
                            s.split("::").next().unwrap_or(s.as_str()).trim().to_string()
                        }))
                })
                .collect();

            let current_head_branch = self.git.current_branch_name().unwrap_or_default();
            let eff_tb = meta.effective_branch(&current_head_branch).to_string();

            if eff_tb == current_head_branch {
                // On the checked-out branch, `repo_root` is the only source of truth. The cached
                // `branches/<head>` sandbox is a mirror and must never feed stale files back into
                // this session's tree, otherwise edits, deletions, renames and `chmod` changes made
                // in `repo_root` would be shadowed or reverted.
                for rel_path in meta.own_blobs.keys().chain(base_files.keys()) {
                    if !snap.files.contains_key(rel_path)
                        && !other_owned_paths.contains(rel_path)
                        && !is_harness_config_path(rel_path)
                        && session_may_sweep(&meta, rel_path, &base_files)
                    {
                        snapshot_deleted_paths.insert(rel_path.clone());
                    }
                }

                let isolated_files = Self::isolate_head_branch_tree(
                    &meta,
                    &snap.files,
                    &base_files,
                    &other_owned_paths,
                    &other_touched_paths,
                );

                if !eff_tb.is_empty() {
                    let branch_ws = self.branch_sandbox_dir(&eff_tb);
                    if branch_ws.is_dir() {
                        for rel_path in base_files.keys().chain(meta.own_blobs.keys()) {
                            if !isolated_files.contains_key(rel_path)
                                && !snap.files.contains_key(rel_path)
                            {
                                let _ = fs::remove_file(branch_ws.join(rel_path));
                            }
                        }
                    }
                }

                snap.files = isolated_files;
                snap.root_tree_id = self.git.write_tree_from_map(&snap.files)?;
            } else {
                // If this session is bound to a non-checked-out branch and has a materialized branch
                // sandbox, also snapshot any files created, modified or deleted directly inside the
                // branch sandbox when no other shared-root session owns those paths.
                let branch_ws = self.branch_sandbox_dir(&eff_tb);
                if branch_ws.is_dir() {
                    let safe_b = Self::sanitize_branch_name(&eff_tb);
                    let cache_key = format!("{agent_id}-branch-{safe_b}");
                    if let Ok(branch_snap) = snapshot_working_dir(
                        &self.git,
                        &branch_ws,
                        &cache_key,
                        &meta.scopes,
                        Some(&base_commit_obj.tree_id),
                    ) {
                        for rel_path in base_files.keys().chain(meta.own_blobs.keys()) {
                            if !branch_snap.files.contains_key(rel_path)
                                && !other_owned_paths.contains(rel_path)
                                && !is_harness_config_path(rel_path)
                            {
                                snapshot_deleted_paths.insert(rel_path.clone());
                            }
                        }

                        if !meta.has_own_changes() {
                            for (rel_path, entry) in branch_snap.files {
                                if !is_harness_config_path(&rel_path)
                                    && base_files.get(&rel_path) != Some(&entry)
                                    && !other_owned_paths.contains(&rel_path)
                                {
                                    sandbox_own_blobs.insert(rel_path.clone(), entry.blob_id);
                                    snap.files.insert(rel_path, entry);
                                }
                            }
                        }
                    }
                }

                // Never sweep unclaimed files sitting in `repo_root` (which holds the checked-out
                // branch, not `eff_tb`) into this session's tree.
                let mut isolated_files = base_files.clone();
                for del_path in &snapshot_deleted_paths {
                    isolated_files.remove(del_path);
                }

                for (rel_path, entry) in &sandbox_own_blobs {
                    let executable = snap
                        .files
                        .get(rel_path)
                        .or_else(|| base_files.get(rel_path))
                        .is_some_and(|e| e.executable);
                    isolated_files.insert(
                        rel_path.clone(),
                        FileEntry {
                            blob_id: *entry,
                            executable,
                        },
                    );
                }

                let head_files = self.git.read_tree_to_map(&meta.head_tree).unwrap_or_default();
                for (rel_path, own_blob_id) in &meta.own_blobs {
                    if snapshot_deleted_paths.contains(rel_path) {
                        continue;
                    }

                    let executable = head_files
                        .get(rel_path)
                        .or_else(|| base_files.get(rel_path))
                        .is_some_and(|e| e.executable);
                    isolated_files.insert(
                        rel_path.clone(),
                        FileEntry {
                            blob_id: *own_blob_id,
                            executable,
                        },
                    );
                }

                for del_path in &meta.deleted_paths {
                    isolated_files.remove(del_path);
                }

                snap.files = isolated_files;
                snap.root_tree_id = self.git.write_tree_from_map(&snap.files)?;
            }
        }

        // Compute modified AST symbols compared to `base_commit` tree.
        // For shared-root sessions with `read_blobs` populated by `handle_post_tool_hook`,
        // per-edit symbol claims were already extracted prior to in-place AST merging.
        let mut detected_symbols = BTreeSet::new();
        let mut first_touched_path = None;
        let scan_whole_tree_symbols = meta.workspace_path.is_some()
            || meta.read_blobs.is_empty()
            || !sandbox_own_blobs.is_empty();

        if scan_whole_tree_symbols {
            for (path, entry) in &snap.files {
                let base_blob_id = base_files.get(path).map(|e| &e.blob_id);
                if base_blob_id != Some(&entry.blob_id) {
                    if first_touched_path.is_none() {
                        first_touched_path = Some(path.clone());
                    }

                    let base_bytes = match base_blob_id {
                        Some(bid) => self.git.read_blob(bid)?,
                        None => Vec::new(),
                    };
                    let curr_bytes = self.git.read_blob(&entry.blob_id)?;
                    let syms = diff_modified_symbols(
                        path,
                        &String::from_utf8_lossy(&base_bytes),
                        &String::from_utf8_lossy(&curr_bytes),
                    );
                    for sym in syms {
                        detected_symbols.insert(format!("{path}::{}", sym.label));
                    }
                }
            }
        }

        let mut effective_intent = meta.intent.clone();
        if meta.intent_inferred {
            if !detected_symbols.is_empty() {
                let list: Vec<&str> = detected_symbols
                    .iter()
                    .take(3)
                    .map(|s| s.rsplit("::").next().unwrap_or(s.as_str()))
                    .collect();
                if let Some(p) = &first_touched_path {
                    effective_intent = format!("Update {p} ({})", list.join(", "));
                } else {
                    effective_intent = format!("Update {}", list.join(", "));
                }
            } else if let Some(p) = &first_touched_path {
                effective_intent = format!("Update {p}");
            }
        }

        let author = self.git.resolve_git_author();
        let new_commit = if snap.root_tree_id == meta.head_tree {
            meta.head_commit
        } else {
            self.git.write_commit(
                &snap.root_tree_id,
                std::slice::from_ref(&meta.base_commit),
                &author,
                &effective_intent,
            )?
        };

        let mut updated_meta = None;
        self.oplog.record_tx(
            agent_id,
            format!("snapshot {agent_id} -> {}", new_commit.short()),
            is_ephemeral,
            |view| {
                if let Some(agent) = view.agents.get_mut(agent_id) {
                    agent.head_commit = new_commit;
                    agent.head_tree = snap.root_tree_id;
                    for del_path in &snapshot_deleted_paths {
                        agent.own_blobs.remove(del_path);
                        if base_files.contains_key(del_path) {
                            agent.deleted_paths.insert(del_path.clone());
                        } else {
                            agent.deleted_paths.remove(del_path);
                        }
                    }

                    for (rel_path, blob_id) in &sandbox_own_blobs {
                        agent.deleted_paths.remove(rel_path);
                        agent.own_blobs.insert(rel_path.clone(), *blob_id);
                    }

                    for sym in &detected_symbols {
                        agent.claimed_symbols.insert(sym.clone());
                    }

                    if agent.intent_inferred && effective_intent != agent.intent {
                        agent.intent = effective_intent.clone();
                    }
                    agent.updated_at_ms = now_millis();
                    view.changes
                        .insert(agent.change_id.clone(), new_commit);
                    updated_meta = Some(agent.clone());
                }
                Ok(())
            },
        )?;

        let out = updated_meta.ok_or_else(|| CoreError::NotFound(agent_id.to_string()))?;
        self.git
            .update_ref(&format!("refs/tessera/agents/{agent_id}"), &out.head_commit)?;
        self.git.update_ref(
            &format!("refs/tessera/changes/{}", out.change_id.as_str()),
            &out.head_commit,
        )?;
        Ok(out)
    }

    /// Builds the tree of a shared-root session that targets the branch checked out in `repo_root`.
    ///
    /// `repo_root` (`disk_files`) is authoritative for every path, so unhooked edits, deletions,
    /// renames and `chmod` changes are captured exactly as `git status` reports them, with these
    /// exceptions that preserve per-session isolation:
    ///
    /// - Paths authored only by peer sessions (`other_owned_paths`: their edits or tombstones) stay at
    ///   their `base_files` version so this session never commits or resurrects a peer's change.
    /// - Unhooked changes that are not attributable to this session (see [`session_may_sweep`]: a
    ///   user's pre-existing edits, or paths outside an AI session's explicit claims) stay at base.
    /// - Paths this session edited that another session also touched (`other_touched_paths`) use this
    ///   session's own blob, because the on-disk file may hold a 3-way merge of both sessions' edits
    ///   or a foreign-branch `pre-tool` baseline restoration.
    /// - Paths this session deleted (`meta.deleted_paths`) are always absent.
    /// - A newly created path whose content equals a base file this session removed is treated as
    ///   the destination of that move (`mv dirA dirB` run from a shell without `post-tool` on every
    ///   destination file) and is attributed to this session along with the removal.
    fn isolate_head_branch_tree(
        meta: &AgentMetadata,
        disk_files: &BTreeMap<String, FileEntry>,
        base_files: &BTreeMap<String, FileEntry>,
        other_owned_paths: &BTreeSet<String>,
        other_touched_paths: &BTreeSet<String>,
    ) -> BTreeMap<String, FileEntry> {
        let mut tree = disk_files.clone();

        let attributable = |rel_path: &String| {
            !other_owned_paths.contains(rel_path) && session_may_sweep(meta, rel_path, base_files)
        };

        let moved_away_blobs: BTreeSet<ObjectId> = base_files
            .iter()
            .filter(|(p, _)| !disk_files.contains_key(*p) && (meta.owns_path(p) || attributable(p)))
            .map(|(_, e)| e.blob_id)
            .collect();

        let changed_paths: BTreeSet<&String> = disk_files
            .iter()
            .filter(|(p, e)| base_files.get(*p) != Some(*e))
            .map(|(p, _)| p)
            .chain(base_files.keys().filter(|p| !disk_files.contains_key(*p)))
            .collect();

        for rel_path in changed_paths {
            let moved_in = !base_files.contains_key(rel_path)
                && !other_owned_paths.contains(rel_path)
                && disk_files
                    .get(rel_path)
                    .is_some_and(|e| moved_away_blobs.contains(&e.blob_id));
            if moved_in || attributable(rel_path) || meta.owns_path(rel_path) {
                continue;
            }

            match base_files.get(rel_path) {
                Some(base_entry) => {
                    tree.insert(rel_path.clone(), base_entry.clone());
                }
                None => {
                    tree.remove(rel_path);
                }
            }
        }

        for (rel_path, own_blob_id) in &meta.own_blobs {
            match disk_files.get(rel_path) {
                Some(disk_entry) if other_touched_paths.contains(rel_path) => {
                    tree.insert(
                        rel_path.clone(),
                        FileEntry {
                            blob_id: *own_blob_id,
                            executable: disk_entry.executable,
                        },
                    );
                }
                Some(disk_entry) => {
                    tree.insert(rel_path.clone(), disk_entry.clone());
                }
                None => {
                    tree.remove(rel_path);
                }
            }
        }

        for rel_path in &meta.deleted_paths {
            tree.remove(rel_path);
        }

        tree
    }

    /// Returns repository-relative paths that differ between `repo_root` and the checked-out `HEAD`
    /// tree (content, mode, addition or deletion) but are authored by no session, i.e. changes that
    /// `git status` reports and no session will commit until a human or claiming session takes them.
    ///
    /// # Errors
    /// Returns [`CoreError`] if `HEAD` or the `OpLog` cannot be read.
    pub fn unattributed_worktree_changes(&self) -> CoreResult<BTreeSet<String>> {
        let current = self.oplog.load_current_reconciled()?;
        let head_cid = self
            .git
            .resolve_head_commit()?
            .ok_or_else(|| CoreError::NotFound("HEAD commit".to_string()))?;
        let head_tree = self.git.read_commit(&head_cid)?.tree_id;
        Ok(self.unowned_worktree_changes(&current.view, &head_tree))
    }

    /// Returns repository-relative paths whose working-tree state in `repo_root` (content, executable
    /// bit, addition or deletion) differs from `head_tree` and that no session in `view` authors.
    ///
    /// These are typically a human's in-progress edits made outside any hooked agent. Harness
    /// configuration files are excluded. Snapshot failures yield an empty set.
    fn unowned_worktree_changes(&self, view: &View, head_tree: &ObjectId) -> BTreeSet<String> {
        let Ok(head_files) = self.git.read_tree_to_map(head_tree) else {
            return BTreeSet::new();
        };
        let Ok(snap) = snapshot_working_dir(
            &self.git,
            self.git.repo_root(),
            "worktree-baseline",
            &[],
            Some(head_tree),
        ) else {
            return BTreeSet::new();
        };

        let owned: BTreeSet<&String> = view.agents.values().flat_map(|a| a.owned_paths()).collect();
        let changed = snap
            .files
            .iter()
            .filter(|(p, e)| head_files.get(*p) != Some(*e))
            .map(|(p, _)| p)
            .chain(head_files.keys().filter(|p| !snap.files.contains_key(*p)));

        changed
            .filter(|p| !owned.contains(p) && !is_harness_config_path(p))
            .cloned()
            .collect()
    }

    /// Incrementally updates `agent_id`'s Git tree and commit for a single modified or deleted
    /// repository-relative path in $O(\text{depth})$ time without walking the working directory.
    ///
    /// Falls back to [`Self::snapshot_agent`] if `agent_id` uses an isolated Copy-on-Write
    /// workspace (`workspace_path.is_some()`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if updating the Git tree, commit, or `OpLog` fails.
    pub fn record_single_path_edit(
        &self,
        agent_id: &str,
        rel_path: &str,
        new_entry: Option<FileEntry>,
    ) -> CoreResult<AgentMetadata> {
        let current = self.oplog.load_current_reconciled()?;
        let meta = match current.view.agents.get(agent_id) {
            Some(a) => a.clone(),
            None => self.ensure_session(agent_id, None)?,
        };

        if meta.workspace_path.is_some() {
            return self.snapshot_agent(agent_id, true);
        }

        let norm_path = validate_repo_rel_path(rel_path)?;
        let base_commit_obj = self.git.read_commit(&meta.base_commit)?;
        let base_tree_for_update = if meta.head_commit != meta.base_commit
            && self
                .git
                .read_commit(&meta.head_commit)
                .ok()
                .is_some_and(|c| c.parents.first() == Some(&meta.base_commit))
        {
            meta.head_tree
        } else {
            base_commit_obj.tree_id
        };
        let exists_in_base = self
            .git
            .read_tree_entry_at_path(&base_commit_obj.tree_id, &norm_path)?
            .is_some();

        let head_branch = self.git.current_branch_name().unwrap_or_default();
        let targets_head = !head_branch.is_empty() && meta.targets_head_branch(&head_branch);
        let mut evicted_own_paths: BTreeSet<String> = BTreeSet::new();

        let mut updates: BTreeMap<String, Option<FileEntry>> = BTreeMap::new();
        for (own_path, own_blob_id) in &meta.own_blobs {
            if own_path == &norm_path {
                continue;
            }

            if targets_head && !self.git.repo_root().join(own_path).is_file() {
                evicted_own_paths.insert(own_path.clone());
                updates.insert(own_path.clone(), None);
                let branch_ws = self.branch_sandbox_dir(&head_branch);
                if branch_ws.is_dir() {
                    let _ = fs::remove_file(branch_ws.join(own_path));
                }
                continue;
            }

            let executable = match self
                .git
                .read_tree_entry_at_path(&meta.head_tree, own_path)?
                .or(self
                    .git
                    .read_tree_entry_at_path(&base_tree_for_update, own_path)?)
            {
                Some(entry) => entry.executable,
                None => self
                    .git
                    .read_tree_entry_at_path(&base_commit_obj.tree_id, own_path)?
                    .is_some_and(|e| e.executable),
            };
            updates.insert(
                own_path.clone(),
                Some(FileEntry {
                    blob_id: *own_blob_id,
                    executable,
                }),
            );
        }

        for deleted_path in &meta.deleted_paths {
            if deleted_path != &norm_path {
                updates.insert(deleted_path.clone(), None);
            }
        }

        let mut evicted_in_base: BTreeSet<String> = BTreeSet::new();
        for ev_path in &evicted_own_paths {
            if self
                .git
                .read_tree_entry_at_path(&base_commit_obj.tree_id, ev_path)?
                .is_some()
            {
                evicted_in_base.insert(ev_path.clone());
            }
        }

        updates.insert(norm_path.clone(), new_entry.clone());
        let new_tree_id = self
            .git
            .update_tree_entries(&base_tree_for_update, &updates)?;

        let author = self.git.resolve_git_author();
        let new_commit = if new_tree_id == meta.head_tree {
            meta.head_commit
        } else {
            self.git.write_commit(
                &new_tree_id,
                std::slice::from_ref(&meta.base_commit),
                &author,
                &meta.intent,
            )?
        };

        let mut updated_meta = None;
        self.oplog.record_tx(
            agent_id,
            format!("snapshot {agent_id} -> {}", new_commit.short()),
            true,
            |view| {
                if let Some(agent) = view.agents.get_mut(agent_id) {
                    agent.head_commit = new_commit;
                    agent.head_tree = new_tree_id;
                    for ev_path in &evicted_own_paths {
                        agent.own_blobs.remove(ev_path);
                        if evicted_in_base.contains(ev_path) {
                            agent.deleted_paths.insert(ev_path.clone());
                        } else {
                            agent.deleted_paths.remove(ev_path);
                        }
                    }

                    match &new_entry {
                        Some(entry) => {
                            agent.deleted_paths.remove(&norm_path);
                            agent.own_blobs.insert(norm_path.clone(), entry.blob_id);
                        }
                        None => {
                            agent.own_blobs.remove(&norm_path);
                            if exists_in_base {
                                agent.deleted_paths.insert(norm_path.clone());
                            } else {
                                agent.deleted_paths.remove(&norm_path);
                            }
                        }
                    }
                    agent.updated_at_ms = now_millis();
                    view.changes.insert(agent.change_id.clone(), new_commit);
                    updated_meta = Some(agent.clone());
                }
                Ok(())
            },
        )?;

        let out = updated_meta.ok_or_else(|| CoreError::NotFound(agent_id.to_string()))?;
        self.git
            .update_ref(&format!("refs/tessera/agents/{agent_id}"), &out.head_commit)?;
        self.git.update_ref(
            &format!("refs/tessera/changes/{}", out.change_id.as_str()),
            &out.head_commit,
        )?;
        Ok(out)
    }

    /// Explicitly updates the `intent` and claimed AST/file symbols for `agent_id`, and
    /// automatically records pre-edit baselines (`read_blobs`) for any target file paths.
    ///
    /// # Errors
    /// Returns [`CoreError`] if updating the `OpLog` fails.
    pub fn claim_intent_and_symbols(
        &self,
        agent_id: &str,
        intent: Option<&str>,
        symbols: &[String],
    ) -> CoreResult<AgentMetadata> {
        self.claim_intent_and_symbols_with_branch(agent_id, intent, symbols, None)
    }

    /// Explicitly updates the `intent`, claimed AST/file symbols, and optional per-session
    /// `target_branch` (`--branch <name>`) for `agent_id`, recording pre-edit baselines (`read_blobs`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if updating the `OpLog` fails.
    pub fn claim_intent_and_symbols_with_branch(
        &self,
        agent_id: &str,
        intent: Option<&str>,
        symbols: &[String],
        target_branch: Option<&str>,
    ) -> CoreResult<AgentMetadata> {
        let requested_branch = target_branch.map(str::trim).filter(|s| !s.is_empty());
        if let Some(b) = requested_branch {
            validate_ref_name(b)?;
        }

        let meta_now = self.ensure_session(agent_id, intent)?;
        let cfg = self.oplog.load_config()?;
        let head_branch = self.git.current_branch_name().unwrap_or_default();
        let pristine = !meta_now.has_own_changes() && meta_now.targets_head_branch(&head_branch);

        let head_cid = self.git.resolve_head_commit().ok().flatten();
        let head_tree = head_cid.and_then(|cid| self.git.read_commit(&cid).ok().map(|c| c.tree_id));
        let current = self.oplog.load_current_reconciled()?;
        let unowned_changes = match (&head_tree, pristine && is_ai_session_id(agent_id)) {
            (Some(tree), true) => self.unowned_worktree_changes(&current.view, tree),
            _ => BTreeSet::new(),
        };

        let auto_branch = if requested_branch.is_none()
            && cfg.auto_isolate_dirty_claims
            && pristine
            && is_ai_session_id(agent_id)
            && !head_branch.is_empty()
        {
            let mut dirty: BTreeSet<String> = unowned_changes.clone();
            for peer in current.view.agents.values() {
                if peer.agent_id != agent_id && peer.targets_head_branch(&head_branch) {
                    dirty.extend(peer.owned_paths().cloned());
                }
            }

            let claimed_targets: Vec<&str> = symbols
                .iter()
                .map(|s| s.split("::").next().unwrap_or(s).trim().trim_end_matches('/'))
                .filter(|s| !s.is_empty())
                .collect();
            let overlaps = dirty.iter().any(|d| {
                claimed_targets
                    .iter()
                    .any(|c| d == c || d.starts_with(&format!("{c}/")))
            });
            let candidate = format!("agent/{agent_id}");
            (overlaps && validate_ref_name(&candidate).is_ok()).then_some(candidate)
        } else {
            None
        };

        let clean_branch: Option<&str> = requested_branch.or(auto_branch.as_deref());
        let branch_commit_opt = if let Some(b) = clean_branch {
            let existing_ref = self
                .git
                .resolve_ref(&format!("refs/heads/{b}"))
                .ok()
                .flatten();
            let resolved_cid = match existing_ref {
                Some(cid) => Some(cid),
                None => {
                    let fork_cid = if auto_branch.is_some() {
                        head_cid
                    } else {
                        self.git
                            .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))
                            .ok()
                            .flatten()
                            .or(head_cid)
                    };
                    if let Some(tcid) = &fork_cid {
                        let _ = self.git.update_ref(&format!("refs/heads/{b}"), tcid);
                    }
                    fork_cid
                }
            };
            resolved_cid.and_then(|cid| self.git.read_commit(&cid).ok().map(|c| (cid, c.tree_id)))
        } else {
            None
        };

        self.oplog.record_tx(
            agent_id,
            format!("claim intent/symbols for {agent_id}"),
            false,
            |view| {
                if let Some((b, (b_cid, _))) = clean_branch.zip(branch_commit_opt.as_ref()) {
                    view.branches
                        .entry(b.to_string())
                        .or_insert_with(|| *b_cid);
                }

                if let Some(agent) = view.agents.get_mut(agent_id) {
                    if let Some(i) = intent {
                        if !i.trim().is_empty() {
                            agent.intent = i.trim().to_string();
                            agent.intent_inferred = false;
                        }
                    }

                    if pristine && is_ai_session_id(agent_id) {
                        agent.foreign_baseline = unowned_changes.clone();
                    }

                    if let Some(b) = clean_branch {
                        agent.target_branch = Some(b.to_string());
                        if let Some((b_cid, b_tid)) = &branch_commit_opt {
                            if !agent.has_own_changes() {
                                agent.base_commit = *b_cid;
                                agent.head_commit = *b_cid;
                                agent.head_tree = *b_tid;
                            }
                        }
                    }

                    for s in symbols {
                        agent.claimed_symbols.insert(s.clone());
                    }
                    agent.updated_at_ms = now_millis();
                }
                Ok(())
            },
        )?;

        if clean_branch.is_some() {
            let _ = self.prepare_branch_sandbox(agent_id, clean_branch);
        }

        for sym in symbols {
            let file_part = sym.split("::").next().unwrap_or(sym.as_str()).trim();
            if !file_part.is_empty() && self.git.repo_root().join(file_part).is_file() {
                let _ = handle_pre_tool_hook(self, agent_id, file_part);
            }
        }

        let refreshed = self.oplog.load_current_reconciled()?;
        refreshed
            .view
            .agents
            .get(agent_id)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(agent_id.to_string()))
    }

    /// Saves a named checkpoint in the `OpLog` for `agent_id` so the agent can instantly roll back
    /// before a risky multi-file refactor.
    ///
    /// # Errors
    /// Returns [`CoreError`] if snapshotting or recording the checkpoint fails.
    pub fn create_checkpoint(&self, agent_id: &str, label: &str) -> CoreResult<OpId> {
        let _ = self.snapshot_agent(agent_id, false)?;
        let current = self.oplog.load_current_reconciled()?;
        let cp_id = current.id.clone();

        self.oplog.record_tx(
            agent_id,
            format!("checkpoint '{label}' for {agent_id}"),
            false,
            |view| {
                if let Some(agent) = view.agents.get_mut(agent_id) {
                    agent.checkpoints.insert(label.to_string(), cp_id.clone());
                }
                Ok(())
            },
        )?;
        Ok(cp_id)
    }

    /// Executes a command inside `agent_id`'s workspace with deterministic runtime environment
    /// isolation (`PORT`, `TES_DB_NAME`, `TMPDIR`), snapshotting before and after execution.
    ///
    /// # Errors
    /// Returns [`CoreError`] if snapshotting or spawning the child process fails.
    pub fn exec_in_agent(&self, agent_id: &str, command_args: &[String]) -> CoreResult<i32> {
        if command_args.is_empty() {
            return Err(CoreError::InvalidArgument(
                "empty command arguments passed to exec".to_string(),
            ));
        }
        let cfg = self.oplog.load_config()?;
        let meta = self.snapshot_agent(agent_id, true)?;
        let work_dir = meta
            .workspace_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.git.repo_root().to_path_buf());

        let env_map = build_agent_env_with_config(&meta, &cfg, &work_dir);
        if let Some(tmp_dir) = env_map.get("TMPDIR") {
            let _ = fs::create_dir_all(tmp_dir);
        }

        let status = Command::new(&command_args[0])
            .args(&command_args[1..])
            .current_dir(&work_dir)
            .envs(&env_map)
            .status()
            .map_err(|source| CoreError::Io {
                path: command_args[0].clone(),
                source,
            })?;

        let _ = self.snapshot_agent(agent_id, false)?;
        Ok(status.code().unwrap_or(1))
    }

    /// Switches or binds the specified terminal/agent session (`session_id`) to `branch`,
    /// creating `refs/heads/<branch>` at `HEAD` if the branch does not exist yet.
    ///
    /// Unlike `git checkout`, this does not overwrite `.git/HEAD` for other concurrent terminals
    /// or sessions in the same repository.
    ///
    /// # Errors
    /// Returns [`CoreError`] if resolving `HEAD` or updating the `OpLog` fails.
    pub fn switch_branch(&self, session_id: &str, branch: &str) -> CoreResult<AgentMetadata> {
        let clean_branch = branch.trim();
        validate_ref_name(clean_branch)?;
        self.reconcile_in_flight_sessions();
        let _ = self.ensure_session(session_id, None)?;
        let ref_name = format!("refs/heads/{clean_branch}");
        let branch_cid = match self.git.resolve_ref(&ref_name)? {
            Some(cid) => cid,
            None => {
                let target_cid = match self
                    .git
                    .resolve_ref(&format!("refs/remotes/origin/{clean_branch}"))?
                {
                    Some(remote_cid) => remote_cid,
                    None => self
                        .git
                        .resolve_head_commit()?
                        .ok_or_else(|| CoreError::NotFound("HEAD commit".to_string()))?,
                };
                self.git.update_ref(&ref_name, &target_cid)?;
                target_cid
            }
        };
        let branch_commit = self.git.read_commit(&branch_cid)?;

        let cfg = self.oplog.load_config()?;
        let current = self.oplog.load_current_reconciled()?;
        let other_has_uncommitted_edits = current.view.agents.iter().any(|(id, a)| {
            let is_passive = (id.starts_with("term-")
                || id.starts_with("session-")
                || id == "editor")
                && a.intent_inferred
                && a.claimed_symbols.is_empty()
                && !a.has_own_changes();
            id != session_id && !is_passive && a.has_own_changes()
        });
        let caller_has_uncommitted_edits = current
            .view
            .agents
            .get(session_id)
            .is_some_and(|a| a.has_own_changes());
        let restore_trunk_to_repo_root = clean_branch == cfg.trunk_branch
            && !other_has_uncommitted_edits
            && !caller_has_uncommitted_edits;

        let mut out = None;
        let branch_tree_map = self
            .git
            .read_tree_to_map(&branch_commit.tree_id)
            .unwrap_or_default();

        self.oplog.record_tx(
            session_id,
            format!("switch session {session_id} to branch {clean_branch}"),
            false,
            |view| {
                view.branches
                    .entry(clean_branch.to_string())
                    .or_insert_with(|| branch_cid);
                if let Some(agent) = view.agents.get_mut(session_id) {
                    agent.target_branch = Some(clean_branch.to_string());
                    if !agent.has_own_changes() {
                        agent.base_commit = branch_cid;
                        agent.head_commit = branch_cid;
                        agent.head_tree = branch_commit.tree_id;
                        agent.read_blobs.clear();
                        agent.integrated_peer_blobs.clear();
                    }
                    agent.updated_at_ms = now_millis();
                    out = Some(agent.clone());
                }

                if restore_trunk_to_repo_root {
                    for (rel_path, entry) in &branch_tree_map {
                        let obs = view.observed_blobs.entry(rel_path.clone()).or_default();
                        obs.retain(|b| b != &entry.blob_id);
                        obs.push(entry.blob_id);
                    }
                }
                Ok(())
            },
        )?;
        let _ = self.prepare_branch_sandbox(session_id, Some(clean_branch));
        if restore_trunk_to_repo_root {
            let _ = self.git.set_symbolic_head(clean_branch);
            let _ = materialize_tree_to_dir(&self.git, &branch_commit.tree_id, self.git.repo_root());
            self.git.sync_git_index(&branch_cid);
        }
        out.ok_or_else(|| CoreError::NotFound(session_id.to_string()))
    }

    /// Switches the primary repository working tree (`repo_root`) and `.git/HEAD` to `branch`
    /// so IDE file explorers and language servers display `branch`'s files, while preserving
    /// isolated Copy-on-Write branch sandboxes (`branches/<old>` and `branches/<branch>`) for
    /// all running terminal sessions.
    ///
    /// # Errors
    /// Returns [`CoreError`] if resolving or materializing `branch` fails.
    pub fn switch_editor_branch(&self, branch: &str) -> CoreResult<String> {
        let clean_branch = branch.trim();
        validate_ref_name(clean_branch)?;
        self.reconcile_in_flight_sessions();

        let old_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| "main".to_string());
        if !old_branch.is_empty() && old_branch != clean_branch {
            let _ = self.prepare_branch_sandbox("editor", Some(&old_branch));
        }

        let ref_name = format!("refs/heads/{clean_branch}");
        let branch_cid = match self.git.resolve_ref(&ref_name)? {
            Some(cid) => cid,
            None => {
                let target_cid = match self
                    .git
                    .resolve_ref(&format!("refs/remotes/origin/{clean_branch}"))?
                {
                    Some(remote_cid) => remote_cid,
                    None => self
                        .git
                        .resolve_head_commit()?
                        .ok_or_else(|| CoreError::NotFound("HEAD commit".to_string()))?,
                };
                self.git.update_ref(&ref_name, &target_cid)?;
                target_cid
            }
        };
        let branch_commit = self.git.read_commit(&branch_cid)?;

        // Prepare `clean_branch`'s sandbox before updating `.git/HEAD` so `clean_branch` is not
        // compared against `old_branch`'s `repo_root`.
        let _ = self.prepare_branch_sandbox("editor", Some(clean_branch));

        let safe_clean = Self::sanitize_branch_name(clean_branch);
        let stamp_file = self.branches_root().join(format!("{safe_clean}.tree"));
        let effective_tree_id = fs::read_to_string(&stamp_file)
            .ok()
            .and_then(|s| {
                let trimmed = s.trim();
                let tree_hex = trimmed.rsplit(':').next().unwrap_or(trimmed);
                ObjectId::from_hex(tree_hex).ok()
            })
            .unwrap_or(branch_commit.tree_id);
        let effective_tree_map = self
            .git
            .read_tree_to_map(&effective_tree_id)
            .unwrap_or_default();

        self.git.set_symbolic_head(clean_branch)?;
        materialize_tree_to_dir(&self.git, &effective_tree_id, self.git.repo_root())?;
        self.git.sync_git_index(&branch_cid);

        let _ = self.oplog.record_tx(
            "editor",
            format!("switch editor working tree to branch {clean_branch}"),
            true,
            |view| {
                view.branches
                    .entry(clean_branch.to_string())
                    .or_insert_with(|| branch_cid);
                for (rel_path, entry) in &effective_tree_map {
                    let obs = view.observed_blobs.entry(rel_path.clone()).or_default();
                    obs.retain(|b| b != &entry.blob_id);
                    obs.push(entry.blob_id);
                }

                for agent_meta in view.agents.values_mut() {
                    let is_passive_or_editor = agent_meta.agent_id == "editor"
                        || ((agent_meta.agent_id.starts_with("term-")
                            || agent_meta.agent_id.starts_with("session-"))
                            && agent_meta.intent_inferred);

                    if is_passive_or_editor
                        && (agent_meta.agent_id == "editor" || agent_meta.target_branch.is_none())
                        && !agent_meta.has_own_changes()
                        && agent_meta.head_commit == agent_meta.base_commit
                        && agent_meta.workspace_path.is_none()
                    {
                        agent_meta.base_commit = branch_cid;
                        agent_meta.head_commit = branch_cid;
                        agent_meta.head_tree = effective_tree_id;
                        agent_meta.read_blobs.clear();
                        agent_meta.integrated_peer_blobs.clear();
                    }
                }

                if view
                    .agents
                    .get("editor")
                    .is_some_and(|a| !a.has_own_changes())
                {
                    view.agents.remove("editor");
                }
                Ok(())
            },
        );
        self.git.delete_ref("refs/tessera/agents/editor");

        if let Ok(refreshed) = self.oplog.load_current_reconciled() {
            let head_ws = self.branch_sandbox_dir(clean_branch);
            let head_ws_opt = head_ws.is_dir().then_some(head_ws.as_path());
            let mut post_obs = BTreeMap::new();
            self.reapply_remaining_head_sessions(
                &refreshed.view.agents,
                &BTreeSet::new(),
                clean_branch,
                head_ws_opt,
                &mut post_obs,
            );
        }

        Ok(clean_branch.to_string())
    }

    /// Computes a live preview of the `ast-union` resolution for `record` so visual IDE tools
    /// can display the combined AST output before the user accepts it.
    #[must_use]
    pub fn compute_ast_union_preview(&self, record: &ConflictRecord) -> String {
        let base_bytes = match record.merge.removes.first().and_then(Option::as_ref) {
            Some(bid) => self.git.read_blob(bid).unwrap_or_default(),
            None => Vec::new(),
        };
        let ours_bytes = match record.merge.adds.first().and_then(Option::as_ref) {
            Some(bid) => self.git.read_blob(bid).unwrap_or_default(),
            None => record.left_snippet.as_bytes().to_vec(),
        };
        let theirs_bytes = match record.merge.adds.get(1).and_then(Option::as_ref) {
            Some(bid) => self.git.read_blob(bid).unwrap_or_default(),
            None => record.right_snippet.as_bytes().to_vec(),
        };
        merge_ast_union(
            &record.path,
            &String::from_utf8_lossy(&base_bytes),
            &String::from_utf8_lossy(&ours_bytes),
            &String::from_utf8_lossy(&theirs_bytes),
        )
    }

    /// Sanitizes a branch name into a filesystem-safe directory slug.
    pub fn sanitize_branch_name(branch: &str) -> String {
        branch
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }

    /// Returns the external Copy-on-Write sandbox root directory for this repository.
    ///
    /// Placed outside `.git/` and without hidden dot-directory segments on macOS (`~/Library/Caches/tes/workspaces/<repo>-<hash>`)
    /// and Linux (`/var/tmp/tessera-workspaces/<repo>-<hash>`) on the same filesystem device (`st_dev`)
    /// so tools like Vite, Next.js, and ESLint never block file serving due to `**/.git/**` rules
    /// while APFS `clonefile(2)` and Linux `FICLONE` remain zero-copy.
    pub fn sandboxes_root(&self) -> PathBuf {
        let repo_root = self.git.repo_root();
        let repo_name = repo_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let full_hash = blake3::hash(repo_root.display().to_string().as_bytes()).to_hex();
        let slug = format!("{repo_name}-{}", &full_hash[..32]);

        // SAFETY: `libc::geteuid()` is a pure POSIX syscall with no arguments or memory dereferences.
        #[cfg(unix)]
        let uid = unsafe { libc::geteuid() };
        #[cfg(not(unix))]
        let uid = 0u32;

        let mut candidates = Vec::new();
        if cfg!(target_os = "macos") {
            if let Ok(home) = std::env::var("HOME") {
                candidates.push(
                    PathBuf::from(home)
                        .join("Library")
                        .join("Caches")
                        .join("tessera")
                        .join("workspaces")
                        .join(&slug),
                );
            }
        } else if cfg!(windows) {
            if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
                if !local_app_data.trim().is_empty() {
                    candidates.push(
                        PathBuf::from(local_app_data)
                            .join("tessera")
                            .join("workspaces")
                            .join(&slug),
                    );
                }
            }
            candidates.push(
                std::env::temp_dir()
                    .join("tessera-workspaces")
                    .join(&slug),
            );
        } else {
            if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
                if !xdg.trim().is_empty() {
                    candidates.push(
                        PathBuf::from(xdg)
                            .join("tessera")
                            .join("workspaces")
                            .join(&slug),
                    );
                }
            }

            if let Ok(home) = std::env::var("HOME") {
                if !home.trim().is_empty() {
                    candidates.push(
                        PathBuf::from(home)
                            .join(".cache")
                            .join("tessera")
                            .join("workspaces")
                            .join(&slug),
                    );
                }
            }

            candidates.push(
                PathBuf::from("/var/tmp")
                    .join(format!("tessera-workspaces-{uid}"))
                    .join(&slug),
            );
            candidates.push(
                std::env::temp_dir()
                    .join(format!("tessera-workspaces-{uid}"))
                    .join(&slug),
            );
        }

        if let Some(parent) = repo_root.parent() {
            candidates.push(parent.join("tessera-workspaces").join(&slug));
        }

        #[cfg(unix)]
        let repo_dev = fs::metadata(repo_root).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.dev()
        });

        for cand in candidates {
            if fs::create_dir_all(&cand).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    if let Some(parent_dir) = cand.parent() {
                        if let Ok(p_meta) = fs::symlink_metadata(parent_dir) {
                            if p_meta.file_type().is_symlink() || p_meta.uid() != uid {
                                continue;
                            }

                            if parent_dir
                                .file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with("tessera-workspaces"))
                            {
                                let _ = fs::set_permissions(
                                    parent_dir,
                                    fs::Permissions::from_mode(0o700),
                                );
                            }
                        } else {
                            continue;
                        }
                    }

                    let Ok(c_meta) = fs::symlink_metadata(&cand) else {
                        continue;
                    };
                    if c_meta.file_type().is_symlink() || c_meta.uid() != uid {
                        continue;
                    }
                    let _ = fs::set_permissions(&cand, fs::Permissions::from_mode(0o700));

                    if let Some(r_dev) = repo_dev {
                        if r_dev == c_meta.dev() {
                            return cand;
                        }
                        continue;
                    }
                }
                return cand;
            }
        }

        self.git.git_dir().join("tessera")
    }

    /// Returns the directory containing per-branch Copy-on-Write sandboxes for this repository.
    pub fn branches_root(&self) -> PathBuf {
        self.sandboxes_root().join("branches")
    }

    /// Returns the Copy-on-Write sandbox directory for `branch`, automatically migrating any
    /// legacy `.git/tessera/branches/<safe_branch>` directory if present.
    pub fn branch_sandbox_dir(&self, branch: &str) -> PathBuf {
        let safe_branch = Self::sanitize_branch_name(branch);
        let branches_root = self.branches_root();
        let new_dir = branches_root.join(&safe_branch);
        let legacy_root = self.git.git_dir().join("tessera").join("branches");
        let legacy_dir = legacy_root.join(&safe_branch);

        if legacy_dir.is_dir() && legacy_dir != new_dir {
            let _ = fs::create_dir_all(&branches_root);
            if !new_dir.exists() {
                let _ = fs::rename(&legacy_dir, &new_dir);
                let legacy_stamp = legacy_root.join(format!("{safe_branch}.tree"));
                let new_stamp = branches_root.join(format!("{safe_branch}.tree"));
                if legacy_stamp.is_file() {
                    let _ = fs::rename(&legacy_stamp, &new_stamp);
                }
            }
        }

        if new_dir.is_dir() {
            let marker = new_dir.join(".tessera").join("repo_root");
            if !marker.is_file() {
                let _ = atomic_write_file(
                    &marker,
                    self.git.repo_root().display().to_string().as_bytes(),
                );
            }
        }
        new_dir
    }

    /// Materializes or refreshes the Copy-on-Write sandbox for `branch` (or `session_id`'s
    /// active `target_branch`), returning `(target_branch, branch_ws_dir, env_map)`.
    ///
    /// # Errors
    /// Returns [`CoreError`] if resolving refs or materializing the sandbox fails.
    pub fn prepare_branch_sandbox(
        &self,
        session_id: &str,
        branch: Option<&str>,
    ) -> CoreResult<(String, PathBuf, BTreeMap<String, String>)> {
        self.prepare_branch_sandbox_inner(session_id, branch, false)
    }

    fn prepare_branch_sandbox_inner(
        &self,
        session_id: &str,
        branch: Option<&str>,
        harvest_head_sandbox: bool,
    ) -> CoreResult<(String, PathBuf, BTreeMap<String, String>)> {
        let cfg = self.oplog.load_config()?;
        let current = self.oplog.load_current_reconciled()?;
        let meta = match current.view.agents.get(session_id) {
            Some(a) => a.clone(),
            None => self.ensure_session(session_id, None)?,
        };

        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());

        let target_branch = branch
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| meta.target_branch.clone())
            .unwrap_or_else(|| current_head_branch.clone());

        let safe_branch = Self::sanitize_branch_name(&target_branch);
        let branches_root = self.branches_root();
        let branch_ws_dir = self.branch_sandbox_dir(&target_branch);
        let tree_stamp_file = branches_root.join(format!("{safe_branch}.tree"));

        let branch_cid = self
            .git
            .resolve_ref(&format!("refs/heads/{target_branch}"))?
            .or(Some(meta.base_commit))
            .ok_or_else(|| CoreError::NotFound(format!("branch {target_branch}")))?;
        let branch_commit = self.git.read_commit(&branch_cid)?;
        let base_tree_map = self.git.read_tree_to_map(&branch_commit.tree_id)?;

        let raw_stamp = fs::read_to_string(&tree_stamp_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let (stamped_branch_cid, stamped_tree_id) = match raw_stamp.as_deref() {
            Some(s) => match s.split_once(':') {
                Some((b_hex, t_hex)) => (
                    ObjectId::from_hex(b_hex).ok(),
                    ObjectId::from_hex(t_hex).ok(),
                ),
                None => (None, ObjectId::from_hex(s).ok()),
            },
            None => (None, None),
        };
        let stamped_tree_map_opt = stamped_tree_id
            .as_ref()
            .and_then(|tid| self.git.read_tree_to_map(tid).ok());

        // When `branch_cid` has not changed since the last sandbox materialization, start from
        // `stamped_tree_map` so prior file deletions and additions inside the sandbox are preserved.
        let mut tree_map = if branch_ws_dir.is_dir()
            && stamped_branch_cid.as_ref() == Some(&branch_cid)
        {
            stamped_tree_map_opt
                .clone()
                .unwrap_or_else(|| base_tree_map.clone())
        } else {
            base_tree_map.clone()
        };

        // Overlay any uncommitted in-flight `own_blobs` and `deleted_paths` belonging to sessions targeting `target_branch`.
        for agent in current.view.agents.values() {
            let eff_b = agent.effective_branch(current_head_branch.as_str());
            if eff_b == target_branch {
                for (rel_path, blob_id) in &agent.own_blobs {
                    let executable = self
                        .git
                        .read_tree_entry_at_path(&agent.head_tree, rel_path)
                        .ok()
                        .flatten()
                        .map(|e| e.executable)
                        .unwrap_or_else(|| tree_map.get(rel_path).is_some_and(|e| e.executable));
                    tree_map.insert(
                        rel_path.clone(),
                        FileEntry {
                            blob_id: *blob_id,
                            executable,
                        },
                    );
                }

                for deleted_path in &agent.deleted_paths {
                    if !agent.own_blobs.contains_key(deleted_path) {
                        tree_map.remove(deleted_path);
                    }
                }
            }
        }

        // When `target_branch == current_head_branch`, also snapshot `repo_root` so any files
        // created, modified, or mode-changed directly in `repo_root` on `current_head_branch`
        // (and not belonging to a foreign branch session) are preserved in `tree_map` and `branch_ws_dir`.
        if target_branch == current_head_branch {
            let root_cache_key = format!("{session_id}-root-{safe_branch}");
            if let Ok(root_snap) = snapshot_working_dir(
                &self.git,
                self.git.repo_root(),
                &root_cache_key,
                &meta.scopes,
                stamped_tree_id.as_ref().or(Some(&branch_commit.tree_id)),
            ) {
                for (rel_path, entry) in &root_snap.files {
                    if is_harness_config_path(rel_path) {
                        continue;
                    }

                    let already_in_own_blobs = current
                        .view
                        .agents
                        .values()
                        .any(|a| a.own_blobs.contains_key(rel_path));
                    let claimed_by_foreign = current.view.agents.values().any(|a| {
                        let eff_b = a.effective_branch(current_head_branch.as_str());
                        eff_b != target_branch
                            && session_claims_path(a, rel_path, &base_tree_map)
                    });
                    let matches_base = base_tree_map
                        .get(rel_path)
                        .map(|e| (&e.blob_id, e.executable))
                        == Some((&entry.blob_id, entry.executable));
                    let matches_stamp = stamped_tree_map_opt
                        .as_ref()
                        .and_then(|m| m.get(rel_path))
                        .map(|e| (&e.blob_id, e.executable))
                        == Some((&entry.blob_id, entry.executable));

                    if !already_in_own_blobs
                        && !claimed_by_foreign
                        && !matches_base
                        && !matches_stamp
                    {
                        tree_map.insert(rel_path.clone(), entry.clone());
                        if branch_ws_dir.is_dir() {
                            if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                                write_worktree_rel(
                                    &branch_ws_dir,
                                    rel_path,
                                    &bytes,
                                    entry.executable,
                                );
                            }
                        }
                    }
                }
            }
        }

        // If `branch_ws_dir` already exists and is on the same commit `branch_cid`, capture any new,
        // modified, mode-changed, or deleted files inside `branch_ws_dir` since `tree_stamp_file`.
        let _ = harvest_head_sandbox;
        if branch_ws_dir.is_dir() && stamped_branch_cid.as_ref() == Some(&branch_cid) {
            let stamped_tree_map = stamped_tree_map_opt.unwrap_or_else(|| base_tree_map.clone());
            let cache_key = format!("{session_id}-branch-{safe_branch}");
            if let Ok(branch_snap) = snapshot_working_dir(
                &self.git,
                &branch_ws_dir,
                &cache_key,
                &meta.scopes,
                stamped_tree_id.as_ref().or(Some(&branch_commit.tree_id)),
            ) {
                let mut newly_captured = BTreeMap::new();
                let mut newly_deleted = BTreeSet::new();
                let mut tree_changed = false;

                for (rel_path, entry) in &branch_snap.files {
                    if is_harness_config_path(rel_path) {
                        continue;
                    }

                    let changed_since_stamp = stamped_tree_map
                        .get(rel_path)
                        .map(|e| (&e.blob_id, e.executable))
                        != Some((&entry.blob_id, entry.executable));
                    if changed_since_stamp
                        && base_tree_map
                            .get(rel_path)
                            .map(|e| (&e.blob_id, e.executable))
                            != Some((&entry.blob_id, entry.executable))
                        && tree_map.get(rel_path) != Some(entry)
                    {
                        newly_captured.insert(rel_path.clone(), entry.blob_id);
                        tree_map.insert(rel_path.clone(), entry.clone());
                        tree_changed = true;
                        if target_branch == current_head_branch {
                            if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                                write_worktree_rel(
                                    self.git.repo_root(),
                                    rel_path,
                                    &bytes,
                                    entry.executable,
                                );
                            }
                        }
                    }
                }

                if stamped_tree_id.is_some() {
                    for rel_path in stamped_tree_map.keys() {
                        if is_harness_config_path(rel_path) {
                            continue;
                        }

                        let owned_on_target = current.view.agents.values().any(|a| {
                            let eff_b = a.effective_branch(current_head_branch.as_str());
                            eff_b == target_branch && a.own_blobs.contains_key(rel_path)
                        });

                        if !branch_snap.files.contains_key(rel_path) {
                            if owned_on_target && self.git.repo_root().join(rel_path).is_file() {
                                continue;
                            }

                            newly_deleted.insert(rel_path.clone());
                            tree_map.remove(rel_path);
                            tree_changed = true;
                            if target_branch == current_head_branch {
                                remove_worktree_file(
                                    self.git.repo_root(),
                                    rel_path,
                                );
                            }
                        } else if target_branch == current_head_branch
                            && !self.git.repo_root().join(rel_path).exists()
                        {
                            let _ = fs::remove_file(branch_ws_dir.join(rel_path));
                            newly_deleted.insert(rel_path.clone());
                            tree_map.remove(rel_path);
                            tree_changed = true;
                        }
                    }
                }

                if !newly_captured.is_empty() || !newly_deleted.is_empty() || tree_changed {
                    let new_tree = self.git.write_tree_from_map(&tree_map)?;
                    let author = self.git.resolve_git_author();
                    let new_agent_commit = if session_id != "editor" {
                        self.git
                            .write_commit(
                                &new_tree,
                                std::slice::from_ref(&branch_cid),
                                &author,
                                &meta.intent,
                            )
                            .ok()
                    } else {
                        None
                    };
                    let mut updated_change: Option<(ChangeId, ObjectId)> = None;
                    let _ = self.oplog.record_tx(
                        session_id,
                        format!("capture sandbox edits on {target_branch}"),
                        true,
                        |view| {
                            for agent in view.agents.values_mut() {
                                let eff_b = agent.effective_branch(current_head_branch.as_str());
                                if eff_b == target_branch {
                                    for k in &newly_deleted {
                                        agent.own_blobs.remove(k);
                                        if base_tree_map.contains_key(k) {
                                            agent.deleted_paths.insert(k.clone());
                                        } else {
                                            agent.deleted_paths.remove(k);
                                        }
                                    }
                                }
                            }

                            if session_id != "editor" {
                                if let Some(agent) = view.agents.get_mut(session_id) {
                                    for (k, v) in &newly_captured {
                                        agent.deleted_paths.remove(k);
                                        if base_tree_map.get(k).map(|e| &e.blob_id) == Some(v) {
                                            agent.own_blobs.remove(k);
                                        } else {
                                            agent.own_blobs.insert(k.clone(), *v);
                                        }
                                    }
                                    agent.head_tree = new_tree;
                                    if let Some(nc) = new_agent_commit {
                                        agent.head_commit = nc;
                                        updated_change = Some((agent.change_id.clone(), nc));
                                    }
                                    agent.updated_at_ms = now_millis();
                                }
                            }

                            if let Some((cid, hc)) = &updated_change {
                                view.changes.insert(cid.clone(), *hc);
                            }
                            Ok(())
                        },
                    );
                    if let Some((cid, hc)) = updated_change {
                        let _ = self
                            .git
                            .update_ref(&format!("refs/tessera/agents/{session_id}"), &hc);
                        let _ = self
                            .git
                            .update_ref(&format!("refs/tessera/changes/{}", cid.as_str()), &hc);
                    }
                }
            }
        }

        let effective_tree_id = self.git.write_tree_from_map(&tree_map)?;
        let stamp_payload = format!("{branch_cid}:{effective_tree_id}");

        if !branch_ws_dir.exists() {
            create_cow_workspace(
                self.git.repo_root(),
                &branch_ws_dir,
                &[],
                &cfg.cow_cache_dirs,
            )?;
            materialize_tree_to_dir(&self.git, &effective_tree_id, &branch_ws_dir)?;
            let _ = atomic_write_file(&tree_stamp_file, stamp_payload.as_bytes());
        } else {
            let _ = sync_missing_cow_caches(
                self.git.repo_root(),
                &branch_ws_dir,
                &cfg.cow_cache_dirs,
            );
            let cached_tree = fs::read_to_string(&tree_stamp_file).unwrap_or_default();
            if cached_tree.trim() != stamp_payload && cached_tree.trim() != effective_tree_id.as_str() {
                materialize_tree_to_dir(&self.git, &effective_tree_id, &branch_ws_dir)?;
            }

            if cached_tree.trim() != stamp_payload {
                let _ = atomic_write_file(&tree_stamp_file, stamp_payload.as_bytes());
            }
        }

        let marker = branch_ws_dir.join(".tessera").join("repo_root");
        if !marker.is_file() {
            let _ = atomic_write_file(
                &marker,
                self.git.repo_root().display().to_string().as_bytes(),
            );
        }

        let mut env_meta = meta;
        env_meta.target_branch = Some(target_branch.clone());
        let mut env_map = build_agent_env_with_config(&env_meta, &cfg, &branch_ws_dir);
        env_map.insert("TES_BRANCH".to_string(), target_branch.clone());
        env_map.insert(
            "GIT_DIR".to_string(),
            self.git.git_dir().display().to_string(),
        );
        env_map.insert(
            "GIT_WORK_TREE".to_string(),
            branch_ws_dir.display().to_string(),
        );
        env_map.insert(
            "TES_WORKSPACE".to_string(),
            branch_ws_dir.display().to_string(),
        );
        env_map.insert(
            "TES_REPO_ROOT".to_string(),
            self.git.repo_root().display().to_string(),
        );
        let bin_dir = branch_ws_dir.join("node_modules").join(".bin");
        let existing_path = env_map
            .get("PATH")
            .cloned()
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };

        if cfg.is_port_isolation_enabled() {
            let shim_bin_dir = self.sandboxes_root().join("bin");
            ensure_polyglot_port_shims(&shim_bin_dir);

            env_map.insert(
                "TES_SHIM_BIN".to_string(),
                shim_bin_dir.display().to_string(),
            );

            if !existing_path.is_empty() {
                env_map.insert(
                    "PATH".to_string(),
                    format!(
                        "{}{sep}{}{sep}{existing_path}",
                        shim_bin_dir.display(),
                        bin_dir.display()
                    ),
                );
            }
        } else if !existing_path.is_empty() {
            env_map.insert(
                "PATH".to_string(),
                format!("{}{sep}{existing_path}", bin_dir.display()),
            );
        }

        if let Some(tmp_dir) = env_map.get("TMPDIR") {
            let _ = fs::create_dir_all(tmp_dir);
        }

        Ok((target_branch, branch_ws_dir, env_map))
    }

    /// Executes `command_args` (e.g., tests or a dev server) inside an isolated Copy-on-Write
    /// sandbox materialized for `branch` (or `session_id`'s active `target_branch`), with
    /// zero-copy cloned dependencies (`node_modules`, `target`) and isolated `PORT`, `DATABASE_URL`,
    /// `TEST_DATABASE_URL`, `TES_BRANCH`, and `TMPDIR`.
    ///
    /// Multiple branches can run tests, install branch-specific dependencies, or serve HTTP traffic
    /// concurrently without colliding.
    ///
    /// # Errors
    /// Returns [`CoreError`] if materializing the COW branch sandbox or spawning the command fails.
    pub fn exec_on_branch(
        &self,
        session_id: &str,
        branch: Option<&str>,
        command_args: &[String],
    ) -> CoreResult<i32> {
        if command_args.is_empty() {
            return Err(CoreError::InvalidArgument(
                "empty command arguments passed to exec".to_string(),
            ));
        }
        self.reconcile_in_flight_sessions();
        let cfg = self.oplog.load_config()?;
        let (target_branch, branch_ws_dir, env_map) =
            self.prepare_branch_sandbox(session_id, branch)?;

        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        if target_branch != current_head_branch {
            if let (Ok(Some(head_cid)), Ok(current)) = (
                self.git
                    .resolve_ref(&format!("refs/heads/{current_head_branch}")),
                self.oplog.load_current_reconciled(),
            ) {
                if let Ok(head_commit) = self.git.read_commit(&head_cid) {
                    let _ = materialize_tree_to_dir(
                        &self.git,
                        &head_commit.tree_id,
                        self.git.repo_root(),
                    );
                    let mut post_obs = BTreeMap::new();
                    self.reapply_remaining_head_sessions(
                        &current.view.agents,
                        &BTreeSet::new(),
                        &current_head_branch,
                        None,
                        &mut post_obs,
                    );
                }
            }
        }

        let status = Command::new(&command_args[0])
            .args(&command_args[1..])
            .current_dir(&branch_ws_dir)
            .envs(&env_map)
            .status()
            .map_err(|source| CoreError::Io {
                path: command_args[0].clone(),
                source,
            })?;

        let _ = sync_missing_cow_caches(
            &branch_ws_dir,
            self.git.repo_root(),
            &cfg.cow_cache_dirs,
        );
        let _ = self.prepare_branch_sandbox_inner(session_id, branch, true);

        Ok(status.code().unwrap_or(1))
    }

    /// Computes the live **Symbol Collision Radar** across all active agents, surfacing any
    /// overlapping AST symbols, claimed contracts, or touched files along with each agent's intent.
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading the `OpLog` or conflict records fails.
    pub fn radar(&self) -> CoreResult<RadarReport> {
        let cfg = self.oplog.load_config()?;
        let default_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        let current = self.oplog.load_current_reconciled()?;

        let mut symbol_to_agents: BTreeMap<(String, String), BTreeMap<String, String>> =
            BTreeMap::new();
        let mut active_agents = Vec::new();
        let mut file_attributions = Vec::new();
        let mut working_cache: BTreeMap<String, Option<String>> = BTreeMap::new();

        let mut read_working_file = |rel_path: &str| -> Option<String> {
            if let Some(cached) = working_cache.get(rel_path) {
                return cached.clone();
            }

            let full_path = self.git.repo_root().join(rel_path);
            let loaded = fs::read(&full_path)
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            working_cache.insert(rel_path.to_string(), loaded.clone());
            loaded
        };

        for agent in current.view.agents.values() {
            if agent.agent_id == "editor" && !agent.has_own_changes() {
                continue;
            }

            let is_idle_shell = (agent.agent_id.starts_with("term-")
                || agent.agent_id.starts_with("session-"))
                && agent.intent_inferred
                && agent.claimed_symbols.is_empty()
                && !agent.has_own_changes()
                && agent.head_commit == agent.base_commit;

            if is_idle_shell && agent.target_branch.is_none() {
                continue;
            }

            active_agents.push(agent.clone());

            if is_idle_shell {
                continue;
            }

            let eff_branch = agent
                .target_branch
                .as_deref()
                .unwrap_or(default_branch.as_str())
                .to_string();

            for sym in &agent.claimed_symbols {
                symbol_to_agents
                    .entry((eff_branch.clone(), sym.clone()))
                    .or_default()
                    .insert(agent.agent_id.clone(), agent.intent.clone());
            }

            for path in agent.read_blobs.keys().chain(agent.own_blobs.keys()) {
                symbol_to_agents
                    .entry((eff_branch.clone(), path.clone()))
                    .or_default()
                    .insert(agent.agent_id.clone(), agent.intent.clone());
            }

            if eff_branch != default_branch {
                continue;
            }

            let base_tree = self
                .git
                .read_commit(&agent.base_commit)
                .ok()
                .and_then(|c| self.git.read_tree_to_map(&c.tree_id).ok())
                .unwrap_or_default();

            for (rel_path, own_blob_id) in &agent.own_blobs {
                let Some(working_str) = read_working_file(rel_path) else {
                    continue;
                };

                let base_str = base_tree
                    .get(rel_path)
                    .and_then(|entry| self.git.read_blob(&entry.blob_id).ok())
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default();

                let Ok(own_bytes) = self.git.read_blob(own_blob_id) else {
                    continue;
                };
                let own_str = String::from_utf8_lossy(&own_bytes);

                if own_str == base_str {
                    continue;
                }

                let hunks = attribute_agent_hunks(&base_str, &own_str, &working_str);

                for (start_line, end_line, in_working_tree) in hunks {
                    if !in_working_tree {
                        continue;
                    }

                    file_attributions.push(FileLineAttribution {
                        path: rel_path.clone(),
                        agent_id: agent.agent_id.clone(),
                        branch: eff_branch.clone(),
                        intent: agent.intent.clone(),
                        slot: agent.slot,
                        start_line,
                        end_line,
                        in_working_tree: true,
                        symbol: None,
                    });
                }
            }

            for sym in &agent.claimed_symbols {
                let (rel_path, sym_name_opt) = match sym.split_once("::") {
                    Some((p, s)) => (p.trim(), Some(s.trim())),
                    None => (sym.trim(), None),
                };

                if agent.own_blobs.contains_key(rel_path) {
                    continue;
                }

                let Some(working_str) = read_working_file(rel_path) else {
                    continue;
                };

                let line_no = sym_name_opt
                    .and_then(|sym_name| {
                        working_str
                            .lines()
                            .enumerate()
                            .find(|(_, line)| line.contains(sym_name))
                            .map(|(idx, _)| idx + 1)
                    })
                    .unwrap_or(1);

                file_attributions.push(FileLineAttribution {
                    path: rel_path.to_string(),
                    agent_id: agent.agent_id.clone(),
                    branch: eff_branch.clone(),
                    intent: agent.intent.clone(),
                    slot: agent.slot,
                    start_line: line_no,
                    end_line: line_no,
                    in_working_tree: true,
                    symbol: sym_name_opt.map(str::to_string),
                });
            }
        }

        let mut collisions = Vec::new();
        for ((_, target), agents_map) in symbol_to_agents {
            if agents_map.len() > 1 {
                collisions.push(SymbolCollision {
                    target,
                    competing_agents: agents_map,
                });
            }
        }

        let mut active_conflicts = Vec::new();
        for cid in current.view.conflicts.values() {
            if let Ok(rec) = self.oplog.load_conflict(cid) {
                active_conflicts.push(rec);
            }
        }

        Ok(RadarReport {
            active_agents,
            collisions,
            active_conflicts,
            file_attributions,
        })
    }

    /// Closes and cleans up an active agent session (`agent_id`), removing its isolated COW workspace
    /// (`.tessera/workspaces/<agent_id>`) if present, deleting `refs/tessera/agents/<agent_id>`, and
    /// removing the session from the active `View`.
    ///
    /// Note: The session's `ChangeId` ref (`refs/tessera/changes/<change_id>`) and commits in `.git/objects`
    /// remain preserved in history.
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if `agent_id` is not an active session.
    pub fn close_agent(&self, agent_id: &str) -> CoreResult<AgentMetadata> {
        let mut removed_meta: Option<AgentMetadata> = None;
        self.oplog.record_tx(
            "agent-close",
            format!("close agent session {agent_id}"),
            false,
            |view| {
                removed_meta = view.agents.remove(agent_id);
                Ok(())
            },
        )?;

        let meta = removed_meta
            .ok_or_else(|| CoreError::NotFound(format!("agent session '{agent_id}'")))?;
        self.git.delete_ref(&format!("refs/tessera/agents/{agent_id}"));
        if let Some(ws_path) = &meta.workspace_path {
            let p = Path::new(ws_path);
            if p.exists() {
                let _ = fs::remove_dir_all(p);
            }
        }
        Ok(meta)
    }

    /// Clears active agent and terminal sessions from `View::agents`.
    ///
    /// When `include_dirty` is `false`, only sessions with no uncommitted `own_blobs` are removed.
    /// When `include_dirty` is `true`, all sessions in `View::agents` are closed and their refs
    /// (`refs/tessera/agents/<id>`) and dedicated agent COW workspaces are cleaned up.
    ///
    /// # Errors
    /// Returns [`CoreError`] if recording the transaction in the `OpLog` fails.
    pub fn clear_agents(&self, include_dirty: bool) -> CoreResult<Vec<String>> {
        let mut removed = Vec::new();
        let mut removed_workspaces = Vec::new();

        self.oplog.record_tx(
            "agent-clear",
            if include_dirty {
                "clear all agent sessions".to_string()
            } else {
                "clear idle agent sessions".to_string()
            },
            false,
            |view| {
                let to_remove: Vec<String> = view
                    .agents
                    .iter()
                    .filter(|(_, a)| include_dirty || !a.has_own_changes())
                    .map(|(k, _)| k.clone())
                    .collect();

                for id in to_remove {
                    if let Some(meta) = view.agents.remove(&id) {
                        if let Some(ws) = meta.workspace_path {
                            removed_workspaces.push(ws);
                        }
                        removed.push(id);
                    }
                }

                if include_dirty {
                    view.observed_blobs.clear();
                }

                Ok(())
            },
        )?;

        for id in &removed {
            self.git.delete_ref(&format!("refs/tessera/agents/{id}"));
        }

        for ws_path in &removed_workspaces {
            let p = Path::new(ws_path);
            if p.exists() {
                let _ = fs::remove_dir_all(p);
            }
        }

        Ok(removed)
    }

    /// Reconciles any in-flight files written to `repo_root` by active sessions before switching
    /// branches or materializing branch sandboxes.
    pub fn reconcile_in_flight_sessions(&self) {
        let Ok(current) = self.oplog.load_current_reconciled() else {
            return;
        };

        let mut active_ids: Vec<(u8, u64, String)> = current
            .view
            .agents
            .values()
            .filter(|a| {
                a.agent_id != "editor"
                    && (!a.intent_inferred
                        || !a.claimed_symbols.is_empty()
                        || a.has_own_changes()
                        || !a.read_blobs.is_empty())
            })
            .map(|a| {
                let priority = if !a.claimed_symbols.is_empty() || a.has_own_changes() {
                    0
                } else if !a.read_blobs.is_empty() {
                    1
                } else {
                    2
                };
                (priority, a.updated_at_ms, a.agent_id.clone())
            })
            .collect();

        active_ids.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));

        for (_, _, aid) in active_ids {
            let _ = self.watch_tick(&aid);
        }
    }

    /// Scans the repository working tree for files modified since `session_id`'s last recorded
    /// baseline (`read_blobs` or `base_commit`) and reconciles each modified file through
    /// [`handle_post_tool_hook`].
    ///
    /// Used by `tes watch` to provide real-time 3-way AST auto-merging for editors and harnesses
    /// that do not emit tool lifecycle hooks.
    ///
    /// # Errors
    /// Returns [`CoreError`] if snapshotting or hook reconciliation fails.
    pub fn watch_tick(&self, session_id: &str) -> CoreResult<Vec<PostToolOutcome>> {
        if session_id == "editor" {
            return Ok(Vec::new());
        }
        let meta = self.ensure_session(session_id, None)?;
        let current = self.oplog.load_current_reconciled()?;

        // Passive or unclaimed sessions must not steal or reconcile files when an active peer session is working.
        let is_unclaimed_session = meta.intent_inferred
            && meta.claimed_symbols.is_empty()
            && !meta.has_own_changes()
            && meta.read_blobs.is_empty();
        let has_active_peer = current.view.agents.values().any(|a| {
            a.agent_id != session_id
                && (!a.intent_inferred
                    || !a.claimed_symbols.is_empty()
                    || a.has_own_changes()
                    || !a.read_blobs.is_empty())
        });
        if is_unclaimed_session && has_active_peer {
            return Ok(Vec::new());
        }

        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| "main".to_string());
        let eff_branch = meta.effective_branch(&current_head_branch);

        let base_commit = self.git.read_commit(&meta.base_commit)?;
        let base_files = self.git.read_tree_to_map(&base_commit.tree_id)?;

        // Collect committed trees of all known branches so `watch_tick` never mistakes a branch
        // materialized in `repo_root` (e.g. via `switch_editor_branch`) for a new uncommitted edit.
        let mut committed_branch_trees: Vec<BTreeMap<String, FileEntry>> = Vec::new();
        let mut seen_branch_cids: BTreeSet<ObjectId> = BTreeSet::new();
        seen_branch_cids.insert(meta.base_commit);

        for b_cid in current.view.branches.values() {
            if seen_branch_cids.insert(*b_cid) {
                if let Ok(bc) = self.git.read_commit(b_cid) {
                    if let Ok(bm) = self.git.read_tree_to_map(&bc.tree_id) {
                        committed_branch_trees.push(bm);
                    }
                }
            }
        }

        for local_br in self.git.list_local_branches() {
            if let Ok(Some(b_cid)) = self.git.resolve_ref(&format!("refs/heads/{local_br}")) {
                if seen_branch_cids.insert(b_cid) {
                    if let Ok(bc) = self.git.read_commit(&b_cid) {
                        if let Ok(bm) = self.git.read_tree_to_map(&bc.tree_id) {
                            committed_branch_trees.push(bm);
                        }
                    }
                }
            }
        }

        if let Ok(Some(head_cid)) = self.git.resolve_head_commit() {
            if seen_branch_cids.insert(head_cid) {
                if let Ok(hc) = self.git.read_commit(&head_cid) {
                    if let Ok(hm) = self.git.read_tree_to_map(&hc.tree_id) {
                        committed_branch_trees.push(hm);
                    }
                }
            }
        }

        // If invoked from inside a sandbox directory (e.g. when an IDE window opened `branches/main`),
        // sync any newly modified files from `invocation_dir` into `repo_root` before snapshotting.
        if let Some(inv_dir) = self.invocation_dir() {
            if inv_dir != self.git.repo_root() && inv_dir.is_dir() {
                let inv_key = format!("{session_id}-inv");
                if let Ok(inv_snap) = snapshot_working_dir(
                    &self.git,
                    inv_dir,
                    &inv_key,
                    &meta.scopes,
                    Some(&base_commit.tree_id),
                ) {
                    for (rel_path, entry) in &inv_snap.files {
                        let is_claimed_by_me =
                            session_claims_path(&meta, rel_path, &base_files);
                        let is_owned_by_other = current.view.agents.values().any(|a| {
                            a.agent_id != session_id && a.own_blobs.contains_key(rel_path)
                        });
                        if (is_claimed_by_me || !is_owned_by_other)
                            && base_files.get(rel_path).map(|e| &e.blob_id) != Some(&entry.blob_id)
                        {
                            let inv_file = inv_dir.join(rel_path);
                            let repo_file = self.git.repo_root().join(rel_path);
                            if let Ok(bytes) = fs::read(&inv_file) {
                                let _ = atomic_write_file(&repo_file, &bytes);
                            }
                        }
                    }
                }
            }
        }

        let snap = snapshot_working_dir(
            &self.git,
            self.git.repo_root(),
            session_id,
            &meta.scopes,
            Some(&base_commit.tree_id),
        )?;

        let mut outcomes = Vec::new();
        for (rel_path, entry) in &snap.files {
            if base_files.get(rel_path).map(|e| &e.blob_id) == Some(&entry.blob_id) {
                continue;
            }

            if meta.own_blobs.get(rel_path) == Some(&entry.blob_id) {
                continue;
            }

            let baseline_blob = meta
                .read_blobs
                .get(rel_path)
                .or_else(|| base_files.get(rel_path).map(|e| &e.blob_id));
            if baseline_blob == Some(&entry.blob_id) {
                continue;
            }

            if committed_branch_trees
                .iter()
                .any(|bm| bm.get(rel_path).map(|e| &e.blob_id) == Some(&entry.blob_id))
            {
                continue;
            }

            // If this exact disk blob was already reconciled by a previous `post-tool` hook, skip it.
            let already_reconciled = current
                .view
                .observed_blobs
                .get(rel_path)
                .and_then(|v| v.last())
                == Some(&entry.blob_id)
                || current
                    .view
                    .agents
                    .values()
                    .any(|a| a.own_blobs.get(rel_path) == Some(&entry.blob_id));
            if already_reconciled {
                continue;
            }

            // If this session has explicit claims and did not claim `rel_path`, while another active
            // session did claim `rel_path` (or this session targets a non-HEAD branch), do not steal
            // unclaimed working-tree files from `repo_root`.
            let claimed_by_me = session_claims_path(&meta, rel_path, &base_files);
            let claimed_by_other = current.view.agents.values().any(|a| {
                a.agent_id != session_id && session_claims_path(a, rel_path, &base_files)
            });
            let other_claimer_missing_own_blob = current.view.agents.values().any(|a| {
                a.agent_id != session_id
                    && !a.own_blobs.contains_key(rel_path)
                    && session_claims_path(a, rel_path, &base_files)
            });
            if claimed_by_me
                && claimed_by_other
                && meta.own_blobs.contains_key(rel_path)
                && other_claimer_missing_own_blob
            {
                continue;
            }

            if !claimed_by_me
                && (claimed_by_other
                    || eff_branch != current_head_branch
                    || is_harness_config_path(rel_path))
            {
                continue;
            }

            if !session_may_sweep(&meta, rel_path, &base_files) {
                continue;
            }

            let outcome = handle_post_tool_hook(self, session_id, rel_path)?;
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// Semantically merges one or more agent sessions (`agent_ids`) into `target_branch` using:
    /// - `O(1)` directory-subtree and blob-hash skipping;
    /// - Cross-directory / cross-package rename detection;
    /// - Sequential SQL migration auto-resequencing;
    /// - Manifest & lockfile drivers (`Cargo.toml`, `Cargo.lock`, `package.json`);
    /// - Tree-sitter 3-way AST merging;
    /// - Optional **Speculative Verification Gate (`verify_cmd`)** on affected project scopes or packages.
    ///
    /// When `keep_open` is `false` and the merge finishes with zero conflicts and passes verification,
    /// the merged agent sessions are automatically closed (`close_agent`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if Git object manipulation or `OpLog` recording fails.
    pub fn merge_agents(
        &self,
        agent_ids: &[String],
        target_branch: Option<&str>,
        verify_cmd: Option<&str>,
        keep_open: bool,
    ) -> CoreResult<MergeReport> {
        self.merge_agents_with_message(agent_ids, target_branch, verify_cmd, keep_open, None)
    }

    /// Semantically merges one or more agent sessions (`agent_ids`) into `target_branch` (or the
    /// currently checked-out Git branch when `target_branch` is `None`), using `commit_message` if
    /// provided or synthesizing a clean commit message from the participating agents' intents.
    ///
    /// # Errors
    /// Returns [`CoreError`] if Git object manipulation or `OpLog` recording fails.
    pub fn merge_agents_with_message(
        &self,
        agent_ids: &[String],
        target_branch: Option<&str>,
        verify_cmd: Option<&str>,
        keep_open: bool,
        commit_message: Option<&str>,
    ) -> CoreResult<MergeReport> {
        let options = CommitOptions {
            message: commit_message.map(ToString::to_string),
            ..Default::default()
        };
        self.merge_agents_with_options(agent_ids, target_branch, verify_cmd, keep_open, &options)
    }

    /// Re-applies the uncommitted changes of every still-active session targeting `head_branch`
    /// onto `repo_root` (and onto `head_ws` when given) after a commit, merge or restore rewrote
    /// working-tree files.
    ///
    /// Edited blobs (`own_blobs`) are 3-way merged with any concurrent on-disk content and written
    /// with the session's recorded executable bit; deletion tombstones (`deleted_paths`) are removed
    /// from disk so a peer's in-flight deletion is never resurrected in `repo_root`. Every rewritten
    /// blob is recorded in `observed` for the background watcher.
    fn reapply_remaining_head_sessions(
        &self,
        agents: &BTreeMap<String, AgentMetadata>,
        skip_session_ids: &BTreeSet<String>,
        head_branch: &str,
        head_ws: Option<&Path>,
        observed: &mut BTreeMap<String, ObjectId>,
    ) {
        let head_ws = head_ws.filter(|ws| ws.is_dir());

        for (k, rem_agent) in agents {
            if skip_session_ids.contains(k) || !rem_agent.targets_head_branch(head_branch) {
                continue;
            }

            let rem_base_map = self
                .git
                .read_commit(&rem_agent.base_commit)
                .ok()
                .and_then(|c| self.git.read_tree_to_map(&c.tree_id).ok())
                .unwrap_or_default();
            let rem_head_map = self
                .git
                .read_tree_to_map(&rem_agent.head_tree)
                .unwrap_or_default();

            for (rel_path, blob_id) in &rem_agent.own_blobs {
                let Ok(rem_bytes) = self.git.read_blob(blob_id) else {
                    continue;
                };
                let Ok((_, p)) = resolve_confined_repo_path(self.git.repo_root(), rel_path) else {
                    continue;
                };

                let final_bytes = if p.is_file() {
                    let disk_bytes = fs::read(&p).unwrap_or_default();
                    let base_bytes = rem_base_map
                        .get(rel_path)
                        .and_then(|e| self.git.read_blob(&e.blob_id).ok())
                        .unwrap_or_default();
                    match (
                        std::str::from_utf8(&base_bytes),
                        std::str::from_utf8(&disk_bytes),
                        std::str::from_utf8(&rem_bytes),
                    ) {
                        (Ok(base_s), Ok(disk_s), Ok(rem_s)) if disk_s != rem_s && disk_s != base_s => {
                            merge_file_3way(rel_path, base_s, disk_s, rem_s, head_branch, k)
                                .content
                                .into_bytes()
                        }
                        _ => rem_bytes,
                    }
                } else {
                    rem_bytes
                };

                let executable = rem_head_map
                    .get(rel_path)
                    .or_else(|| rem_base_map.get(rel_path))
                    .map_or_else(|| worktree_is_executable(&p), |e| e.executable);
                write_worktree_file(&p, &final_bytes, executable);
                if let Ok(obs_bid) = self.git.write_blob(&final_bytes) {
                    observed.insert(rel_path.clone(), obs_bid);
                }

                if let Some(ws) = head_ws {
                    if let Ok((_, sp)) = resolve_confined_repo_path(ws, rel_path) {
                        write_worktree_file(&sp, &final_bytes, executable);
                    }
                }
            }

            for rel_path in &rem_agent.deleted_paths {
                remove_worktree_file(self.git.repo_root(), rel_path);
                if let Some(ws) = head_ws {
                    remove_worktree_file(ws, rel_path);
                }
            }
        }
    }

    /// Resolves the lowest common ancestor commit (`git merge-base`) between `c1` and `c2`,
    /// falling back to `c2`'s first parent or `c1` when `c1` and `c2` have no common ancestor.
    fn resolve_merge_base(&self, c1: &ObjectId, c2: &ObjectId) -> ObjectId {
        if c1 == c2 {
            return *c1;
        }

        Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("--git-dir")
            .arg(self.git.git_dir())
            .args(["merge-base", c1.as_str(), c2.as_str()])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| ObjectId::from_hex(String::from_utf8_lossy(&o.stdout).trim()).ok())
            .or_else(|| {
                self.git
                    .read_commit(c2)
                    .ok()
                    .and_then(|c| c.parents.first().copied())
            })
            .unwrap_or(*c1)
    }

    /// Semantically merges one or more agent sessions (`agent_ids`) into `target_branch` with
    /// explicit [`CommitOptions`] (`message`, `author`, `co_author`, and `co_author_trailers`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if author validation, Git object manipulation, or `OpLog` recording fails.
    pub fn merge_agents_with_options(
        &self,
        agent_ids: &[String],
        target_branch: Option<&str>,
        verify_cmd: Option<&str>,
        keep_open: bool,
        options: &CommitOptions,
    ) -> CoreResult<MergeReport> {
        let explicit_author = match options.author.as_deref() {
            Some(raw) if !raw.trim().is_empty() => Some(validate_author_override(raw)?),
            _ => None,
        };
        let cfg = self.oplog.load_config()?;
        let current = self.oplog.load_current_reconciled()?;
        let caller_id = detect_session_id(None);

        let explicit_branch = target_branch.map(str::trim).filter(|s| !s.is_empty());
        let default_trunk = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        let caller_target_branch = current
            .view
            .agents
            .get(&caller_id)
            .and_then(|a| a.target_branch.as_deref())
            .filter(|b| *b != default_trunk)
            .or_else(|| {
                if agent_ids.len() == 1 {
                    current
                        .view
                        .agents
                        .get(&agent_ids[0])
                        .and_then(|a| a.target_branch.as_deref())
                } else {
                    None
                }
            });

        let branch = explicit_branch
            .or(caller_target_branch)
            .map(ToString::to_string)
            .unwrap_or_else(|| default_trunk.clone());

        let target_ids: Vec<String> = if agent_ids.is_empty() {
            let active_with_edits: Vec<String> = current
                .view
                .agents
                .values()
                .filter(|a| {
                    let is_passive = (a.agent_id.starts_with("term-")
                        || a.agent_id.starts_with("session-"))
                        && a.intent_inferred
                        && a.claimed_symbols.is_empty()
                        && !a.has_own_changes()
                        && a.agent_id != caller_id;
                    if is_passive {
                        return false;
                    }

                    let has_edits = a.workspace_path.is_some()
                        || a.head_commit != a.base_commit
                        || a.has_own_changes()
                        || a.agent_id == caller_id;
                    let branch_matches = a.effective_branch(&default_trunk) == branch.as_str()
                        || (a.targets_head_branch(&default_trunk) && a.agent_id == caller_id);
                    has_edits && branch_matches
                })
                .map(|a| a.agent_id.clone())
                .collect();
            if active_with_edits.is_empty() {
                current
                    .view
                    .agents
                    .values()
                    .filter(|a| a.effective_branch(&default_trunk) == branch.as_str())
                    .map(|a| a.agent_id.clone())
                    .collect()
            } else {
                active_with_edits
            }
        } else {
            agent_ids.to_vec()
        };

        let first_agent_base = target_ids
            .iter()
            .find_map(|id| current.view.agents.get(id).map(|a| a.base_commit));

        let trunk_commit_id = self
            .git
            .resolve_ref(&format!("refs/heads/{branch}"))?
            .or(first_agent_base)
            .or_else(|| self.git.resolve_head_commit().ok().flatten())
            .ok_or_else(|| CoreError::NotFound(format!("branch {branch}")))?;
        let trunk_commit = self.git.read_commit(&trunk_commit_id)?;

        // Check if active sessions are targeting distinct branches so each branch only receives its own session's edits.
        let distinct_branches_active = current
            .view
            .agents
            .values()
            .any(|a| !a.targets_head_branch(&default_trunk))
            || caller_target_branch.is_some_and(|b| b != default_trunk);

        let mut agents = Vec::new();
        let mut merge_units = Vec::new();
        let mut shared_root_group: Vec<AgentMetadata> = Vec::new();
        let mut merged_session_ids = BTreeSet::new();

        for id in &target_ids {
            if let Some(existing) = current.view.agents.get(id) {
                merged_session_ids.insert(id.clone());
                if existing.workspace_path.is_some() {
                    let snapped = self.snapshot_agent(id, false)?;
                    agents.push(snapped.clone());
                    merge_units.push(snapped);
                } else {
                    shared_root_group.push(existing.clone());
                }
            } else if let Some(branch_cid) = self.git.resolve_ref(&format!("refs/heads/{id}"))? {
                let b_commit = self.git.read_commit(&branch_cid)?;
                let b_base = self.resolve_merge_base(&trunk_commit_id, &branch_cid);
                let b_intent = b_commit
                    .message
                    .lines()
                    .next()
                    .unwrap_or(id.as_str())
                    .to_string();
                let branch_unit = AgentMetadata {
                    agent_id: id.clone(),
                    slot: 0,
                    change_id: ChangeId::from_seed(&format!("branch-{id}-{}", branch_cid.as_str())),
                    base_commit: b_base,
                    head_commit: branch_cid,
                    head_tree: b_commit.tree_id,
                    intent: b_intent,
                    intent_inferred: false,
                    claimed_symbols: BTreeSet::new(),
                    scopes: Vec::new(),
                    workspace_path: None,
                    read_blobs: BTreeMap::new(),
                    own_blobs: BTreeMap::new(),
                    deleted_paths: BTreeSet::new(),
                    foreign_baseline: BTreeSet::new(),
                    integrated_peer_blobs: BTreeMap::new(),
                    target_branch: Some(id.clone()),
                    checkpoints: BTreeMap::new(),
                    waypoint: None,
                    updated_at_ms: now_millis(),
                };
                agents.push(branch_unit.clone());
                merge_units.push(branch_unit);
            } else if options.amend {
                continue;
            } else {
                return Err(CoreError::NotFound(format!("agent session or branch '{id}'")));
            }
        }

        if let Some(last_shared) = shared_root_group.last() {
            let _ = self.watch_tick(&last_shared.agent_id);
            let snapped_shared = self.snapshot_agent(&last_shared.agent_id, false)?;
            let refreshed = self.oplog.load_current_reconciled()?;

            for shared_agent in &shared_root_group {
                let latest_meta = if shared_agent.agent_id == snapped_shared.agent_id {
                    snapped_shared.clone()
                } else {
                    refreshed
                        .view
                        .agents
                        .get(&shared_agent.agent_id)
                        .cloned()
                        .unwrap_or_else(|| shared_agent.clone())
                };
                let mut isolated = latest_meta.clone();
                if latest_meta.has_own_changes()
                    && shared_agent.agent_id != snapped_shared.agent_id
                {
                    let base_c = self.git.read_commit(&latest_meta.base_commit)?;
                    let mut tree_map = self.git.read_tree_to_map(&base_c.tree_id)?;
                    let head_map = self
                        .git
                        .read_tree_to_map(&latest_meta.head_tree)
                        .unwrap_or_default();
                    latest_meta.apply_own_changes(&mut tree_map, &head_map);
                    isolated.head_tree = self.git.write_tree_from_map(&tree_map)?;
                }
                agents.push(isolated.clone());
                merge_units.push(isolated);
            }
        }

        let mut acc_tree = trunk_commit.tree_id;
        let mut acc_label = branch.clone();
        let mut acc_intent = "trunk".to_string();
        let mut total_skipped_subtrees = 0usize;
        let mut all_conflicts = Vec::new();
        let mut all_renames = Vec::new();
        let mut all_migrations = Vec::new();
        let mut all_touched = BTreeSet::new();

        for agent in &merge_units {
            let eff_base_cid = if agent.base_commit != trunk_commit_id {
                self.resolve_merge_base(&trunk_commit_id, &agent.base_commit)
            } else {
                agent.base_commit
            };
            let base_commit_obj = self.git.read_commit(&eff_base_cid)?;
            let outcome = merge_trees_3way(
                &self.git,
                &base_commit_obj.tree_id,
                &acc_tree,
                &agent.head_tree,
                &acc_label,
                &acc_intent,
                &agent.agent_id,
                &agent.intent,
            )?;

            acc_tree = outcome.merged_tree_id;
            acc_label = format!("{acc_label}+{}", agent.agent_id);
            acc_intent = agent.intent.clone();
            total_skipped_subtrees += outcome.skipped_subtrees;
            for c in outcome.conflicts {
                self.oplog.save_conflict(&c)?;
                all_conflicts.push(c);
            }
            all_renames.extend(outcome.renames_applied);
            all_migrations.extend(outcome.migrations_resequenced);
            for p in outcome.touched_paths {
                all_touched.insert(p);
            }
        }

        // If any touched or previously conflicted file in `acc_tree` still contains unresolved
        // Git conflict markers (`<<<<<<< ... ======= ... >>>>>>>`), ensure a `ConflictRecord`
        // is present in `all_conflicts` so the merge/commit is flagged as conflicted rather than
        // silently treating conflict markers as clean text.
        if let Ok(acc_map_check) = self.git.read_tree_to_map(&acc_tree) {
            let mut check_paths: BTreeSet<String> = all_touched.iter().cloned().collect();
            for conf_path in current.view.conflicts.keys() {
                if conf_path != "<speculative-verify>" {
                    check_paths.insert(conf_path.clone());
                }
            }

            for rel_path in check_paths {
                if all_conflicts.iter().any(|c| c.path == rel_path) {
                    continue;
                }

                if let Some(entry) = acc_map_check.get(&rel_path) {
                    if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                        let text = String::from_utf8_lossy(&bytes);
                        if has_git_conflict_markers(&text) {
                            if let Some(existing_cid) = current.view.conflicts.get(&rel_path) {
                                if let Ok(existing_rec) = self.oplog.load_conflict(existing_cid) {
                                    all_conflicts.push(existing_rec);
                                    continue;
                                }
                            }

                            let base_blob_id = self
                                .git
                                .read_tree_entry_at_path(&trunk_commit.tree_id, &rel_path)
                                .ok()
                                .flatten()
                                .map(|e| e.blob_id);
                            let blob_merge = Merge {
                                removes: vec![base_blob_id],
                                adds: vec![base_blob_id, Some(entry.blob_id)],
                            };
                            let cid = ConflictRecord::compute_id(&rel_path, None, &blob_merge);
                            let mut intents = BTreeMap::new();
                            for a in &agents {
                                intents.insert(a.agent_id.clone(), a.intent.clone());
                            }
                            let rec = ConflictRecord {
                                id: cid,
                                path: rel_path.clone(),
                                symbol: None,
                                kind: ConflictKind::TextHunk,
                                merge: blob_merge,
                                agent_intents: intents,
                                base_snippet: String::new(),
                                left_snippet: String::new(),
                                right_snippet: String::new(),
                                diagnostics: None,
                                created_at_ms: now_millis(),
                            };
                            self.oplog.save_conflict(&rec)?;
                            all_conflicts.push(rec);
                        }
                    }
                }
            }
        }

        let touched_vec: Vec<String> = all_touched.into_iter().collect();
        let affected_packages = self.discover_affected_packages(&acc_tree, &touched_vec)?;

        // Speculative Verification Gate (`--verify` or configured `verify_command`)
        let effective_verify = verify_cmd.or(cfg.verify_command.as_deref());
        let mut verification_passed = true;
        let mut verification_output = None;

        if let Some(cmd_str) = effective_verify {
            let verify_dir = self.sandboxes_root().join("verify-sandbox");
            create_cow_workspace(
                self.git.repo_root(),
                &verify_dir,
                &[],
                &cfg.cow_cache_dirs,
            )?;
            materialize_tree_to_dir(&self.git, &acc_tree, &verify_dir)?;
            let pkg_csv = affected_packages.join(",");

            let mut verify_proc = if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.args(["/C", cmd_str]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", cmd_str]);
                c
            };

            let output = verify_proc
                .current_dir(&verify_dir)
                .env("TES_AFFECTED_PACKAGES", &pkg_csv)
                .output()
                .map_err(|source| CoreError::Io {
                    path: verify_dir.display().to_string(),
                    source,
                })?;

            let combined_out = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            verification_output = Some(combined_out.clone());

            if !output.status.success() {
                verification_passed = false;
                let mut intents = BTreeMap::new();
                for a in &agents {
                    intents.insert(a.agent_id.clone(), a.intent.clone());
                }
                let build_merge = Merge {
                    removes: vec![Some(trunk_commit.tree_id)],
                    adds: vec![Some(acc_tree), Some(trunk_commit.tree_id)],
                };
                let cid =
                    ConflictRecord::compute_id("<speculative-verify>", Some(cmd_str), &build_merge);
                let build_conflict = ConflictRecord {
                    id: cid,
                    path: "<speculative-verify>".to_string(),
                    symbol: Some(format!("affected packages: {}", pkg_csv)),
                    kind: ConflictKind::SemanticBuild,
                    merge: build_merge,
                    agent_intents: intents,
                    base_snippet: String::new(),
                    left_snippet: String::new(),
                    right_snippet: String::new(),
                    diagnostics: Some(combined_out),
                    created_at_ms: now_millis(),
                };
                self.oplog.save_conflict(&build_conflict)?;
                all_conflicts.push(build_conflict);
            }
        }

        let fallback_amend_msg = if options.amend && options.message.is_none() {
            Some(trunk_commit.message.trim().to_string())
        } else {
            None
        };
        let eff_message = options
            .message
            .as_deref()
            .or(fallback_amend_msg.as_deref());
        let base_msg = format_merge_commit_message(&agents, &touched_vec, eff_message);
        let commit_msg = apply_co_author_trailer_with_options(&base_msg, &cfg, options);
        let author = explicit_author.unwrap_or_else(|| {
            if options.amend && !trunk_commit.author.trim().is_empty() {
                trunk_commit.author.clone()
            } else {
                self.git.resolve_git_author()
            }
        });
        let safe_target_b = Self::sanitize_branch_name(&branch);
        let pending_merge_file = self
            .branches_root()
            .join(format!("{safe_target_b}.pending-merge"));

        let mut parents = if options.amend {
            trunk_commit.parents.clone()
        } else {
            vec![trunk_commit_id]
        };
        if let Ok(raw) = fs::read_to_string(&pending_merge_file) {
            if let Some((pending_cid, parents_csv)) = raw.trim().split_once(':') {
                if pending_cid == trunk_commit_id.as_str() {
                    let saved_parents: Vec<ObjectId> = parents_csv
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .filter_map(|s| ObjectId::from_hex(s).ok())
                        .collect();
                    if !saved_parents.is_empty() {
                        parents = saved_parents;
                    }
                }
            }
        }

        for a in &agents {
            let is_session = merged_session_ids.contains(&a.agent_id);
            let pinned_branch = a
                .target_branch
                .as_deref()
                .filter(|tb| !is_session || *tb != default_trunk);
            if let Some(tb) = pinned_branch {
                if tb != branch {
                    if let Ok(Some(b_cid)) = self.git.resolve_ref(&format!("refs/heads/{tb}")) {
                        if !parents.contains(&b_cid) {
                            parents.push(b_cid);
                        }
                    }
                }
            }
        }
        // Like `git commit`, never record an empty commit: an unchanged tree on the current tip is
        // a no-op. Amends (rewording or re-authoring) and multi-parent merge commits still record
        // history even when the tree is unchanged.
        let nothing_to_commit = !options.amend
            && all_conflicts.is_empty()
            && parents.as_slice() == [trunk_commit_id]
            && acc_tree == trunk_commit.tree_id;
        let merged_commit_id = if nothing_to_commit {
            trunk_commit_id
        } else {
            self.git
                .write_commit(&acc_tree, &parents, &author, &commit_msg)?
        };

        if !all_conflicts.is_empty() {
            let parents_csv = parents
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let payload = format!("{merged_commit_id}:{parents_csv}");
            let _ = atomic_write_file(&pending_merge_file, payload.as_bytes());
        } else if verification_passed {
            let _ = fs::remove_file(&pending_merge_file);
        }

        let should_auto_close = verification_passed && all_conflicts.is_empty() && !keep_open;
        let mut closed_agents = Vec::new();
        let remaining_sessions_with_edits = current
            .view
            .agents
            .iter()
            .any(|(k, a)| {
                let is_passive = (k.starts_with("term-") || k.starts_with("session-"))
                    && a.intent_inferred
                    && a.claimed_symbols.is_empty()
                    && !a.has_own_changes();
                let is_caller_merging_others =
                    k == &caller_id && !agent_ids.is_empty() && !a.has_own_changes();
                let has_work = a.has_own_changes() || !a.claimed_symbols.is_empty();
                !is_passive && !merged_session_ids.contains(k) && !is_caller_merging_others && has_work
            });

        let switch_head_to_target = explicit_branch.is_some()
            && !remaining_sessions_with_edits
            && (branch == cfg.trunk_branch || !distinct_branches_active);

        let all_shared_root = agents.iter().all(|a| a.workspace_path.is_none());
        let mut post_commit_observed_blobs: BTreeMap<String, ObjectId> = BTreeMap::new();

        // Advance target branch, update branch sandbox if present, and keep repo_root synchronized with its checked-out HEAD branch!
        if verification_passed {
            self.git
                .update_ref(&format!("refs/heads/{branch}"), &merged_commit_id)?;
            let branch_ws = self.branch_sandbox_dir(&branch);
            if branch_ws.is_dir() {
                let _ = materialize_tree_to_dir(&self.git, &acc_tree, &branch_ws);
                let safe_b = Self::sanitize_branch_name(&branch);
                let stamp = self.branches_root().join(format!("{safe_b}.tree"));
                let stamp_payload = format!("{merged_commit_id}:{acc_tree}");
                let _ = atomic_write_file(&stamp, stamp_payload.as_bytes());
            }
            let current_head_branch = self
                .git
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());
            if branch == current_head_branch || switch_head_to_target {
                let switched_head = switch_head_to_target && branch != current_head_branch;
                if switched_head {
                    let _ = self.git.set_symbolic_head(&branch);
                }

                if !distinct_branches_active || !remaining_sessions_with_edits || branch == current_head_branch {
                    if switched_head {
                        materialize_tree_to_dir(&self.git, &acc_tree, self.git.repo_root())?;
                    } else if let Ok(acc_map) = self.git.read_tree_to_map(&acc_tree) {
                        let rename_paths = all_renames
                            .iter()
                            .flat_map(|ren| [ren.old_path.clone(), ren.new_path.clone()]);
                        let sync_paths: BTreeSet<String> =
                            touched_vec.iter().cloned().chain(rename_paths).collect();
                        for rel_path in &sync_paths {
                            if let Some(entry) = acc_map.get(rel_path) {
                                if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                                    write_worktree_file(
                                        &self.git.repo_root().join(rel_path),
                                        &bytes,
                                        entry.executable,
                                    );
                                    post_commit_observed_blobs
                                        .insert(rel_path.clone(), entry.blob_id);
                                }
                            } else if !is_harness_config_path(rel_path) {
                                remove_worktree_file(self.git.repo_root(), rel_path);
                            }
                        }
                    }
                    self.git.sync_git_index(&merged_commit_id);
                    let head_ws = self.branch_sandbox_dir(&branch);
                    if head_ws.is_dir() {
                        let _ = materialize_tree_to_dir(&self.git, &acc_tree, &head_ws);
                    }

                    self.reapply_remaining_head_sessions(
                        &current.view.agents,
                        &merged_session_ids,
                        &branch,
                        Some(&head_ws),
                        &mut post_commit_observed_blobs,
                    );
                }
            } else {
                // We just committed/merged into a non-HEAD branch (`branch`), while `repo_root` is checked out to
                // `current_head_branch`. Revert every path the merged sessions touched or owned back to
                // `current_head_branch` (and re-apply any still-active head-branch session's edits and deletions)
                // so files from `branch` never leak into `repo_root`, without clobbering unrelated uncommitted files.
                if let Some(head_cid) = self
                    .git
                    .resolve_ref(&format!("refs/heads/{current_head_branch}"))?
                    .or_else(|| self.git.resolve_head_commit().ok().flatten())
                {
                    if let Ok(head_commit) = self.git.read_commit(&head_cid) {
                        if let (Ok(head_map), Ok(acc_map)) = (
                            self.git.read_tree_to_map(&head_commit.tree_id),
                            self.git.read_tree_to_map(&acc_tree),
                        ) {
                            let mut revert_paths: BTreeSet<String> =
                                touched_vec.iter().cloned().collect();
                            for a in &agents {
                                if merged_session_ids.contains(&a.agent_id) {
                                    revert_paths.extend(a.owned_paths().cloned());
                                }
                            }

                            for rel_path in acc_map.keys() {
                                if !head_map.contains_key(rel_path) {
                                    revert_paths.insert(rel_path.clone());
                                }
                            }

                            for rel_path in &revert_paths {
                                if is_harness_config_path(rel_path) {
                                    continue;
                                }

                                match head_map.get(rel_path) {
                                    Some(entry) => {
                                        if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                                            write_worktree_file(
                                                &self.git.repo_root().join(rel_path),
                                                &bytes,
                                                entry.executable,
                                            );
                                        }
                                    }
                                    None => remove_worktree_file(self.git.repo_root(), rel_path),
                                }
                            }
                        }
                        self.git.sync_git_index(&head_cid);
                        let head_ws = self.branch_sandbox_dir(&current_head_branch);
                        if head_ws.is_dir() {
                            let _ = materialize_tree_to_dir(&self.git, &head_commit.tree_id, &head_ws);
                            let safe_hb = Self::sanitize_branch_name(&current_head_branch);
                            let stamp = self.branches_root().join(format!("{safe_hb}.tree"));
                            let stamp_payload = format!("{head_cid}:{}", head_commit.tree_id);
                            let _ = atomic_write_file(&stamp, stamp_payload.as_bytes());
                        }

                        self.reapply_remaining_head_sessions(
                            &current.view.agents,
                            &merged_session_ids,
                            &current_head_branch,
                            None,
                            &mut post_commit_observed_blobs,
                        );
                    }
                }
            }
        } else if all_shared_root {
            // Even when `--verify` fails on a shared-root branch merge, materialize the merged tree
            // to `repo_root` and the target branch sandbox so the user/agent can inspect and fix
            // the compiler or test failure directly in the working files on disk.
            let current_head_branch = self
                .git
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());
            if branch == current_head_branch || switch_head_to_target {
                let _ = materialize_tree_to_dir(&self.git, &acc_tree, self.git.repo_root());
            }
            let branch_ws = self.branch_sandbox_dir(&branch);
            if branch_ws.is_dir() {
                let _ = materialize_tree_to_dir(&self.git, &acc_tree, &branch_ws);
            }
        }

        let mut reset_branch_sessions = Vec::new();
        if should_auto_close {
            for a in &agents {
                if merged_session_ids.contains(&a.agent_id) {
                    let is_switched_branch_session = a.workspace_path.is_none()
                        && a.target_branch.as_deref() == Some(branch.as_str())
                        && branch != default_trunk;
                    if is_switched_branch_session {
                        let _ = self.git.update_ref(
                            &format!("refs/tessera/agents/{}", a.agent_id),
                            &merged_commit_id,
                        );
                        reset_branch_sessions.push(a.agent_id.clone());
                    } else {
                        self.git
                            .delete_ref(&format!("refs/tessera/agents/{}", a.agent_id));
                        if let Some(ws_path) = &a.workspace_path {
                            let p = Path::new(ws_path);
                            if p.exists() {
                                let _ = fs::remove_dir_all(p);
                            }
                        }
                        closed_agents.push(a.agent_id.clone());
                    }
                }
            }
        } else if verification_passed && all_conflicts.is_empty() && keep_open {
            for a in &agents {
                if merged_session_ids.contains(&a.agent_id) {
                    let _ = self.git.update_ref(
                        &format!("refs/tessera/agents/{}", a.agent_id),
                        &merged_commit_id,
                    );
                }
            }
        }

        if verification_passed {
            for (aid, a) in &current.view.agents {
                if closed_agents.contains(aid) || reset_branch_sessions.contains(aid) {
                    continue;
                }

                let eff_b = a.effective_branch(&default_trunk);
                let is_caller_or_editor = (switch_head_to_target
                    && (aid == &caller_id || aid == "editor"))
                    || (!agent_ids.is_empty() && aid == &caller_id && eff_b == branch.as_str());
                let is_idle_unclaimed = a.claimed_symbols.is_empty() && a.read_blobs.is_empty();
                if (eff_b == branch.as_str() || is_caller_or_editor)
                    && !a.has_own_changes()
                    && a.head_commit == a.base_commit
                    && (is_idle_unclaimed || is_caller_or_editor)
                {
                    let _ = self
                        .git
                        .update_ref(&format!("refs/tessera/agents/{aid}"), &merged_commit_id);
                }
            }
        }

        // Determine if any previously active conflicts are now resolved in `acc_tree`.
        let acc_map_for_post = if verification_passed {
            self.git.read_tree_to_map(&acc_tree).unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        let mut resolved_conflict_paths = Vec::new();
        if verification_passed && all_conflicts.is_empty() && !current.view.conflicts.is_empty() {
            for (conf_path, conf_id) in &current.view.conflicts {
                let is_resolved = if conf_path == "<speculative-verify>" {
                    true
                } else if let Some(entry) = acc_map_for_post.get(conf_path) {
                    self.git
                        .read_blob(&entry.blob_id)
                        .map(|b| !has_git_conflict_markers(&String::from_utf8_lossy(&b)))
                        .unwrap_or(true)
                } else {
                    true
                };
                if is_resolved {
                    self.oplog.delete_conflict(conf_id);
                    resolved_conflict_paths.push(conf_path.clone());
                }
            }
        }

        // Rebase any remaining unmerged sessions on `branch` onto `merged_commit_id` (`acc_tree`)
        // so their `own_blobs`, `deleted_paths` and `base_commit` stay synchronized with the newly
        // committed branch tip.
        let mut rebased_remaining_agents: BTreeMap<String, RebasedSession> = BTreeMap::new();
        if verification_passed && all_conflicts.is_empty() {
            for (aid, rem_agent) in &current.view.agents {
                if merged_session_ids.contains(aid)
                    || closed_agents.contains(aid)
                    || reset_branch_sessions.contains(aid)
                    || !rem_agent.has_own_changes()
                {
                    continue;
                }

                if rem_agent.effective_branch(&default_trunk) != branch.as_str() {
                    continue;
                }

                let Ok(rem_base_c) = self.git.read_commit(&rem_agent.base_commit) else {
                    continue;
                };
                let Ok(rem_base_map) = self.git.read_tree_to_map(&rem_base_c.tree_id) else {
                    continue;
                };

                let mut rebased_own = BTreeMap::new();
                let mut rebased_deleted = BTreeSet::new();
                let mut can_rebase = true;

                for rel_path in &rem_agent.deleted_paths {
                    let old_bid = rem_base_map.get(rel_path).map(|e| e.blob_id);
                    let new_bid = acc_map_for_post.get(rel_path).map(|e| e.blob_id);

                    if new_bid.is_none() {
                        continue;
                    }

                    if old_bid != new_bid {
                        can_rebase = false;
                        break;
                    }

                    rebased_deleted.insert(rel_path.clone());
                }

                for (rel_path, own_bid) in &rem_agent.own_blobs {
                    if !can_rebase {
                        break;
                    }

                    let old_bid = rem_base_map.get(rel_path).map(|e| e.blob_id);
                    let new_bid = acc_map_for_post.get(rel_path).map(|e| e.blob_id);

                    if Some(*own_bid) == old_bid || Some(*own_bid) == new_bid {
                        continue;
                    }

                    if old_bid == new_bid {
                        rebased_own.insert(rel_path.clone(), *own_bid);
                        continue;
                    }

                    let old_b = old_bid
                        .and_then(|id| self.git.read_blob(&id).ok())
                        .unwrap_or_default();
                    let new_b = new_bid
                        .and_then(|id| self.git.read_blob(&id).ok())
                        .unwrap_or_default();
                    let Ok(own_b) = self.git.read_blob(own_bid) else {
                        can_rebase = false;
                        break;
                    };

                    let reb = merge_file_3way(
                        rel_path,
                        &String::from_utf8_lossy(&old_b),
                        &String::from_utf8_lossy(&new_b),
                        &String::from_utf8_lossy(&own_b),
                        &branch,
                        aid,
                    );

                    if reb.has_conflicts || has_git_conflict_markers(&reb.content) {
                        can_rebase = false;
                        break;
                    }

                    if let Ok(reb_bid) = self.git.write_blob(reb.content.as_bytes()) {
                        if Some(reb_bid) != new_bid {
                            rebased_own.insert(rel_path.clone(), reb_bid);
                        }
                    } else {
                        can_rebase = false;
                        break;
                    }
                }

                if can_rebase {
                    let rem_head_map = self
                        .git
                        .read_tree_to_map(&rem_agent.head_tree)
                        .unwrap_or_default();
                    let mut rem_tree_map = acc_map_for_post.clone();
                    for (p, bid) in &rebased_own {
                        let executable = rem_head_map
                            .get(p)
                            .or_else(|| rem_tree_map.get(p))
                            .is_some_and(|e| e.executable);
                        rem_tree_map.insert(
                            p.clone(),
                            FileEntry {
                                blob_id: *bid,
                                executable,
                            },
                        );
                    }

                    for p in &rebased_deleted {
                        rem_tree_map.remove(p);
                    }

                    if let Ok(rem_new_tree) = self.git.write_tree_from_map(&rem_tree_map) {
                        let rem_new_commit = if rem_new_tree == acc_tree {
                            merged_commit_id
                        } else {
                            self.git
                                .write_commit(
                                    &rem_new_tree,
                                    std::slice::from_ref(&merged_commit_id),
                                    &author,
                                    &rem_agent.intent,
                                )
                                .unwrap_or(merged_commit_id)
                        };
                        let _ = self
                            .git
                            .update_ref(&format!("refs/tessera/agents/{aid}"), &rem_new_commit);
                        let _ = self.git.update_ref(
                            &format!("refs/tessera/changes/{}", rem_agent.change_id.as_str()),
                            &rem_new_commit,
                        );
                        rebased_remaining_agents.insert(
                            aid.clone(),
                            RebasedSession {
                                own_blobs: rebased_own,
                                deleted_paths: rebased_deleted,
                                head_commit: rem_new_commit,
                                head_tree: rem_new_tree,
                            },
                        );
                    }
                }
            }
        }

        // Working-tree changes still left after this merge belong to no committed session (for
        // example a user's unhooked edits). Sessions that just committed record them as their foreign
        // baseline so a later snapshot of the now-claimless session does not sweep them up.
        let leftover_unowned = if self.git.current_branch_name().ok().as_deref() == Some(branch.as_str()) {
            self.unowned_worktree_changes(&current.view, &acc_tree)
        } else {
            BTreeSet::new()
        };

        self.oplog.record_tx(
            "merge",
            format!(
                "merge {:?} into {branch} (verified={verification_passed})",
                target_ids
            ),
            false,
            |view| {
                if verification_passed {
                    view.branches
                        .insert(branch.clone(), merged_commit_id);
                }

                if should_auto_close {
                    for id in &closed_agents {
                        view.agents.remove(id);
                    }

                    for id in &reset_branch_sessions {
                        if let Some(agent_meta) = view.agents.get_mut(id) {
                            agent_meta.base_commit = merged_commit_id;
                            agent_meta.head_commit = merged_commit_id;
                            agent_meta.head_tree = acc_tree;
                            agent_meta.read_blobs.clear();
                            agent_meta.own_blobs.clear();
                            agent_meta.deleted_paths.clear();
                            agent_meta.claimed_symbols.clear();
                            agent_meta.foreign_baseline = if is_ai_session_id(id) {
                                leftover_unowned.clone()
                            } else {
                                BTreeSet::new()
                            };
                            agent_meta.updated_at_ms = now_millis();
                        }
                    }

                    if !remaining_sessions_with_edits {
                        view.observed_blobs.clear();
                    }
                } else if verification_passed && all_conflicts.is_empty() && keep_open {
                    for id in &target_ids {
                        if let Some(agent_meta) = view.agents.get_mut(id) {
                            agent_meta.base_commit = merged_commit_id;
                            agent_meta.head_commit = merged_commit_id;
                            agent_meta.head_tree = acc_tree;
                            agent_meta.read_blobs.clear();
                            agent_meta.own_blobs.clear();
                            agent_meta.deleted_paths.clear();
                            agent_meta.claimed_symbols.clear();
                            agent_meta.foreign_baseline = if is_ai_session_id(id) {
                                leftover_unowned.clone()
                            } else {
                                BTreeSet::new()
                            };
                            agent_meta.updated_at_ms = now_millis();
                        }
                    }

                    if !remaining_sessions_with_edits {
                        view.observed_blobs.clear();
                    }
                }

                for (obs_path, obs_bid) in &post_commit_observed_blobs {
                    let obs_vec = view.observed_blobs.entry(obs_path.clone()).or_default();
                    obs_vec.retain(|b| b != obs_bid);
                    obs_vec.push(*obs_bid);
                }

                if verification_passed {
                    let mut rebased_changes = Vec::new();
                    for (aid, agent_meta) in view.agents.iter_mut() {
                        if let Some(rebased) = rebased_remaining_agents.get(aid) {
                            agent_meta.own_blobs = rebased.own_blobs.clone();
                            agent_meta.deleted_paths = rebased.deleted_paths.clone();
                            agent_meta.base_commit = merged_commit_id;
                            agent_meta.head_commit = rebased.head_commit;
                            agent_meta.head_tree = rebased.head_tree;
                            agent_meta.read_blobs.clear();
                            agent_meta.integrated_peer_blobs.clear();
                            agent_meta.updated_at_ms = now_millis();
                            rebased_changes.push((agent_meta.change_id.clone(), rebased.head_commit));
                            continue;
                        }

                        let eff_b = agent_meta.effective_branch(&default_trunk).to_string();
                        let was_dirty = agent_meta.has_own_changes();
                        if eff_b == branch && was_dirty {
                            agent_meta.own_blobs.retain(|rel_path, blob_id| {
                                acc_map_for_post.get(rel_path).map(|e| &e.blob_id) != Some(blob_id)
                            });
                            agent_meta
                                .deleted_paths
                                .retain(|rel_path| acc_map_for_post.contains_key(rel_path));
                        }
                        let became_clean = was_dirty && !agent_meta.has_own_changes();
                        let is_caller_or_editor = (switch_head_to_target
                            && (aid == &caller_id || aid == "editor"))
                            || (!agent_ids.is_empty()
                                && aid == &caller_id
                                && eff_b == branch);
                        let is_idle_unclaimed = agent_meta.claimed_symbols.is_empty()
                            && agent_meta.read_blobs.is_empty();
                        if (eff_b == branch || is_caller_or_editor)
                            && !agent_meta.has_own_changes()
                            && (became_clean
                                || (agent_meta.head_commit == agent_meta.base_commit
                                    && (is_idle_unclaimed || is_caller_or_editor)))
                        {
                            if is_caller_or_editor && aid == &caller_id {
                                agent_meta.target_branch = Some(branch.clone());
                            }
                            agent_meta.base_commit = merged_commit_id;
                            agent_meta.head_commit = merged_commit_id;
                            agent_meta.head_tree = acc_tree;
                            agent_meta.read_blobs.clear();
                            agent_meta.integrated_peer_blobs.clear();
                            agent_meta.updated_at_ms = now_millis();
                        }
                    }

                    for (cid, head_cid) in rebased_changes {
                        view.changes.insert(cid, head_cid);
                    }
                }

                for p in &resolved_conflict_paths {
                    view.conflicts.remove(p);
                }

                for c in &all_conflicts {
                    view.conflicts.insert(c.path.clone(), c.id.clone());
                }
                Ok(())
            },
        )?;

        Ok(MergeReport {
            target_branch: branch,
            merged_commit_id,
            merged_tree_id: acc_tree,
            skipped_subtrees: total_skipped_subtrees,
            conflicts: all_conflicts,
            renames_applied: all_renames,
            migrations_resequenced: all_migrations,
            affected_packages,
            closed_agents,
            verification_passed,
            verification_output,
            created_commit: !nothing_to_commit,
        })
    }

    /// Non-blocking auto-rebase of all active agent sessions onto the latest commit of the active branch.
    ///
    /// Rebases each agent's tree onto the current branch tree via [`merge_trees_3way`] and updates
    /// any isolated COW workspaces in place.
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading/writing trees or updating the `OpLog` fails.
    pub fn sync_all_agents(&self) -> CoreResult<Vec<AgentMetadata>> {
        let cfg = self.oplog.load_config()?;
        let active_branch = cfg.trunk_branch.clone();
        let trunk_commit_id = self
            .git
            .resolve_ref(&format!("refs/heads/{active_branch}"))?
            .or_else(|| self.git.resolve_head_commit().ok().flatten())
            .ok_or_else(|| CoreError::NotFound("trunk HEAD".to_string()))?;
        let trunk_commit = self.git.read_commit(&trunk_commit_id)?;
        let author = self.git.resolve_git_author();

        let mut git_branches = BTreeMap::new();
        for b in self.git.list_local_branches() {
            if let Ok(Some(cid)) = self.git.resolve_ref(&format!("refs/heads/{b}")) {
                git_branches.insert(b, cid);
            }
        }
        git_branches
            .entry(active_branch.clone())
            .or_insert_with(|| trunk_commit_id);

        let current = self.oplog.load_current_reconciled()?;
        let agents: Vec<AgentMetadata> = current
            .view
            .agents
            .values()
            .filter(|a| {
                let is_passive = (a.agent_id == "editor"
                    || a.agent_id.starts_with("term-")
                    || a.agent_id.starts_with("session-"))
                    && a.intent_inferred
                    && !a.has_own_changes();
                !is_passive
            })
            .cloned()
            .collect();

        let mut rebased_agents = Vec::new();
        let mut new_conflicts = Vec::new();

        for mut agent in agents {
            let eff_branch = agent
                .target_branch
                .as_deref()
                .unwrap_or(&active_branch)
                .to_string();
            let target_cid = git_branches
                .get(&eff_branch)
                .cloned()
                .unwrap_or(trunk_commit_id);

            if agent.base_commit == target_cid {
                rebased_agents.push(agent);
                continue;
            }

            let target_commit = self
                .git
                .read_commit(&target_cid)
                .unwrap_or_else(|_| trunk_commit.clone());

            if !agent.has_own_changes() && agent.head_commit == agent.base_commit {
                agent.base_commit = target_cid;
                agent.head_commit = target_cid;
                agent.head_tree = target_commit.tree_id;
                agent.read_blobs.clear();
                agent.integrated_peer_blobs.clear();
                agent.updated_at_ms = now_millis();
                rebased_agents.push(agent);
                continue;
            }

            let old_base_commit = self.git.read_commit(&agent.base_commit)?;
            let outcome = merge_trees_3way(
                &self.git,
                &old_base_commit.tree_id,
                &target_commit.tree_id,
                &agent.head_tree,
                &eff_branch,
                "branch update",
                &agent.agent_id,
                &agent.intent,
            )?;

            for c in outcome.conflicts {
                self.oplog.save_conflict(&c)?;
                new_conflicts.push(c);
            }

            let rebased_commit = self.git.write_commit(
                &outcome.merged_tree_id,
                std::slice::from_ref(&target_cid),
                &author,
                &agent.intent,
            )?;

            let target_map = self
                .git
                .read_tree_to_map(&target_commit.tree_id)
                .unwrap_or_default();
            if let Ok(rebased_map) = self.git.read_tree_to_map(&outcome.merged_tree_id) {
                for (path, own_bid) in &mut agent.own_blobs {
                    if let Some(entry) = rebased_map.get(path) {
                        *own_bid = entry.blob_id;
                    }
                }
                agent.own_blobs.retain(|path, own_bid| {
                    rebased_map.contains_key(path)
                        && target_map.get(path).map(|e| &e.blob_id) != Some(own_bid)
                });
                agent.deleted_paths.retain(|path| {
                    target_map.contains_key(path) && !rebased_map.contains_key(path)
                });
            }

            agent.base_commit = target_cid;
            agent.head_commit = rebased_commit;
            agent.head_tree = outcome.merged_tree_id;
            agent.integrated_peer_blobs.clear();
            agent.updated_at_ms = now_millis();

            if let Some(ws_path) = &agent.workspace_path {
                materialize_tree_to_dir(&self.git, &outcome.merged_tree_id, Path::new(ws_path))?;
            }

            self.git.update_ref(
                &format!("refs/tessera/agents/{}", agent.agent_id),
                &rebased_commit,
            )?;
            self.git.update_ref(
                &format!("refs/tessera/changes/{}", agent.change_id.as_str()),
                &rebased_commit,
            )?;
            rebased_agents.push(agent);
        }

        let trunk_tree_id = trunk_commit.tree_id;
        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| active_branch.clone());
        let head_branch_cid = git_branches
            .get(&current_head_branch)
            .cloned()
            .or_else(|| self.git.resolve_head_commit().ok().flatten())
            .unwrap_or(trunk_commit_id);
        let head_branch_tree = self
            .git
            .read_commit(&head_branch_cid)
            .map(|c| c.tree_id)
            .unwrap_or_else(|_| trunk_tree_id);

        self.oplog.record_tx(
            "sync",
            format!(
                "auto-rebase {} agent(s) onto {active_branch}",
                rebased_agents.len()
            ),
            false,
            |view| {
                for (b, cid) in &git_branches {
                    view.branches.insert(b.clone(), *cid);
                }

                let stale_branches: Vec<String> = view
                    .branches
                    .keys()
                    .filter(|b| {
                        *b != &active_branch
                            && !git_branches.contains_key(*b)
                            && !view.agents.values().any(|a| {
                                a.target_branch.as_deref() == Some(b.as_str())
                                    && a.has_own_changes()
                            })
                    })
                    .cloned()
                    .collect();

                for stale in &stale_branches {
                    view.branches.remove(stale);
                    for a in view.agents.values_mut() {
                        if a.target_branch.as_deref() == Some(stale.as_str())
                            && !a.has_own_changes()
                        {
                            a.target_branch = None;
                            a.base_commit = trunk_commit_id;
                            a.head_commit = trunk_commit_id;
                            a.head_tree = trunk_tree_id;
                            a.read_blobs.clear();
                            a.integrated_peer_blobs.clear();
                        }
                    }
                }

                for a in view.agents.values_mut() {
                    let is_passive = (a.agent_id == "editor"
                        || a.agent_id.starts_with("term-")
                        || a.agent_id.starts_with("session-"))
                        && a.intent_inferred
                        && !a.has_own_changes()
                        && a.claimed_symbols.is_empty()
                        && a.head_commit == a.base_commit
                        && a.workspace_path.is_none();

                    if is_passive {
                        a.target_branch = Some(current_head_branch.clone());
                        a.base_commit = head_branch_cid;
                        a.head_commit = head_branch_cid;
                        a.head_tree = head_branch_tree;
                        a.read_blobs.clear();
                        a.integrated_peer_blobs.clear();
                    }
                }

                for a in &rebased_agents {
                    view.agents.insert(a.agent_id.clone(), a.clone());
                    view.changes.insert(a.change_id.clone(), a.head_commit);
                }

                for c in &new_conflicts {
                    view.conflicts.insert(c.path.clone(), c.id.clone());
                }
                Ok(())
            },
        )?;

        self.reconcile_in_flight_sessions();
        let caller_id = detect_session_id(None);
        let _ = self.snapshot_agent(&caller_id, true);
        self.git.sync_git_index(&head_branch_cid);

        Ok(rebased_agents)
    }

    /// Resolves a first-class conflict at `path` using `strategy`, updating the file on disk,
    /// recording the resolved blob in `.git/objects`, and clearing the conflict from the `View`.
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if no active conflict exists for `path`.
    pub fn resolve_conflict(
        &self,
        path: &str,
        strategy: ResolveStrategy,
    ) -> CoreResult<ObjectId> {
        let current = self.oplog.load_current_reconciled()?;
        let cid = current
            .view
            .conflicts
            .get(path)
            .ok_or_else(|| CoreError::NotFound(format!("no active conflict at '{path}'")))?;
        let record = self.oplog.load_conflict(cid)?;

        let resolved_bytes = if path == "<speculative-verify>" {
            Vec::new()
        } else {
            match strategy {
                ResolveStrategy::Ours => {
                    let Some(Some(blob_id)) = record.merge.adds.first() else {
                        return Err(CoreError::InvalidArgument(
                            "conflict has no 'ours' blob".to_string(),
                        ));
                    };
                    self.git.read_blob(blob_id)?
                }
                ResolveStrategy::Theirs => {
                    let Some(Some(blob_id)) = record.merge.adds.get(1) else {
                        return Err(CoreError::InvalidArgument(
                            "conflict has no 'theirs' blob".to_string(),
                        ));
                    };
                    self.git.read_blob(blob_id)?
                }
                ResolveStrategy::AstUnion => {
                    let base_bytes = match record.merge.removes.first().and_then(Option::as_ref) {
                        Some(bid) => self.git.read_blob(bid)?,
                        None => Vec::new(),
                    };
                    let Some(Some(ours_id)) = record.merge.adds.first() else {
                        return Err(CoreError::InvalidArgument(
                            "conflict has no 'ours' blob for ast-union".to_string(),
                        ));
                    };
                    let Some(Some(theirs_id)) = record.merge.adds.get(1) else {
                        return Err(CoreError::InvalidArgument(
                            "conflict has no 'theirs' blob for ast-union".to_string(),
                        ));
                    };
                    let ours_bytes = self.git.read_blob(ours_id)?;
                    let theirs_bytes = self.git.read_blob(theirs_id)?;
                    let union_str = merge_ast_union(
                        path,
                        &String::from_utf8_lossy(&base_bytes),
                        &String::from_utf8_lossy(&ours_bytes),
                        &String::from_utf8_lossy(&theirs_bytes),
                    );
                    union_str.into_bytes()
                }
                ResolveStrategy::WorkingFile => {
                    let (_, full_path) = resolve_confined_repo_path(self.git.repo_root(), path)?;
                    fs::read(&full_path).map_err(|source| CoreError::Io {
                        path: full_path.display().to_string(),
                        source,
                    })?
                }
                ResolveStrategy::ExplicitContent(content) => content.into_bytes(),
            }
        };

        let resolved_blob_id = self.git.write_blob(&resolved_bytes)?;
        let caller_id = detect_session_id(None);
        let has_uncommitted_owners = current
            .view
            .agents
            .values()
            .any(|a| a.own_blobs.contains_key(path));
        let mut updated_trunk: Option<(String, ObjectId, ObjectId)> = None;
        if path != "<speculative-verify>" {
            let (_, full_path) = resolve_confined_repo_path(self.git.repo_root(), path)?;
            let was_executable = worktree_is_executable(&full_path);
            atomic_write_file(&full_path, &resolved_bytes)?;
            set_worktree_executable(&full_path, was_executable);

            let cfg = self.oplog.load_config()?;
            let default_branch = self
                .git
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());
            let active_branch = current
                .view
                .agents
                .get(&caller_id)
                .and_then(|a| a.target_branch.clone())
                .or_else(|| {
                    record.agent_intents.keys().find_map(|aid| {
                        current
                            .view
                            .agents
                            .get(aid)
                            .and_then(|a| a.target_branch.clone())
                    })
                })
                .unwrap_or_else(|| default_branch.clone());

            let branch_ws = self.branch_sandbox_dir(&active_branch);
            if branch_ws.is_dir() {
                write_worktree_rel(&branch_ws, path, &resolved_bytes, was_executable);
            }

            if !has_uncommitted_owners {
                if let Ok(Some(trunk_commit_id)) = self
                    .git
                    .resolve_ref(&format!("refs/heads/{active_branch}"))
                {
                    if let Ok(trunk_commit) = self.git.read_commit(&trunk_commit_id) {
                        if let Ok(mut trunk_files) =
                            self.git.read_tree_to_map(&trunk_commit.tree_id)
                        {
                            if trunk_files.get(path).map(|e| &e.blob_id) != Some(&resolved_blob_id)
                            {
                                let executable = trunk_files
                                    .get(path)
                                    .map(|e| e.executable)
                                    .unwrap_or(was_executable);
                                set_worktree_executable(&full_path, executable);
                                trunk_files.insert(
                                    path.to_string(),
                                    FileEntry {
                                        blob_id: resolved_blob_id,
                                        executable,
                                    },
                                );
                                let new_tree_id = self.git.write_tree_from_map(&trunk_files)?;
                                let author = self.git.resolve_git_author();
                                let resolve_msg = apply_co_author_trailer(
                                    &format!("Resolve conflict in {path}"),
                                    &cfg,
                                );
                                let new_commit_id = self.git.write_commit(
                                    &new_tree_id,
                                    &[trunk_commit_id],
                                    &author,
                                    &resolve_msg,
                                )?;
                                self.git.update_ref(
                                    &format!("refs/heads/{active_branch}"),
                                    &new_commit_id,
                                )?;
                                if active_branch == default_branch {
                                    self.git.sync_git_index(&new_commit_id);
                                }
                                updated_trunk =
                                    Some((active_branch, new_commit_id, new_tree_id));
                            }
                        }
                    }
                }
            }
        }

        self.oplog.delete_conflict(cid);

        let cfg_trunk = self
            .oplog
            .load_config()
            .map(|c| c.trunk_branch)
            .unwrap_or_else(|_| "main".to_string());
        let head_branch_now = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg_trunk.clone());
        let mut owners_to_snapshot = Vec::new();
        self.oplog.record_tx(
            "resolve",
            format!("resolve conflict at {path}"),
            false,
            |view| {
                view.conflicts.remove(path);
                if let Some((branch_name, commit_id, tree_id)) = &updated_trunk {
                    view.branches.insert(branch_name.clone(), *commit_id);
                    for agent in view.agents.values_mut() {
                        let eff_b = agent.effective_branch(head_branch_now.as_str());
                        if agent.workspace_path.is_none() && eff_b == branch_name.as_str() {
                            agent.base_commit = *commit_id;
                            agent.head_commit = *commit_id;
                            agent.head_tree = *tree_id;
                        }
                    }
                }

                for (aid, agent) in view.agents.iter_mut() {
                    agent.integrated_peer_blobs.remove(path);
                    if agent.read_blobs.contains_key(path) {
                        agent
                            .read_blobs
                            .insert(path.to_string(), resolved_blob_id);
                    }

                    if agent.own_blobs.contains_key(path) {
                        agent
                            .own_blobs
                            .insert(path.to_string(), resolved_blob_id);
                        owners_to_snapshot.push(aid.clone());
                    }
                }
                Ok(())
            },
        )?;

        for aid in owners_to_snapshot {
            let _ = self.snapshot_agent(&aid, true);
        }

        Ok(resolved_blob_id)
    }

    /// Restores the repository `View`, Git branch references, `.git/index`, and working tree
    /// files on disk to a target operation or named checkpoint (or the parent of the current
    /// operation if `target_op_or_checkpoint` is `None`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if the target operation or checkpoint cannot be found or restored.
    pub fn undo(&self, target_op_or_checkpoint: Option<&str>) -> CoreResult<Operation> {
        let current = self.oplog.load_current_reconciled()?;
        let mut effective_current = current.clone();
        let target_op = if let Some(target) = target_op_or_checkpoint {
            let checkpoint_op_id = current
                .view
                .agents
                .values()
                .find_map(|agent| agent.checkpoints.get(target).cloned())
                .or_else(|| {
                    self.oplog
                        .list_history(256)
                        .ok()
                        .into_iter()
                        .flatten()
                        .find_map(|op| {
                            op.view
                                .agents
                                .values()
                                .find_map(|agent| agent.checkpoints.get(target).cloned())
                        })
                });
            let op_id = match checkpoint_op_id {
                Some(id) => id,
                None => self.oplog.resolve_op_prefix(target)?,
            };
            self.oplog.read_operation(&op_id)?
        } else {
            while effective_current.is_ephemeral
                && effective_current.description.starts_with("ensure session ")
            {
                let Some(parent_id) = effective_current.parents.first() else {
                    break;
                };
                let Ok(parent_op) = self.oplog.read_operation(parent_id) else {
                    break;
                };
                if parent_op.parents.is_empty() {
                    break;
                }
                effective_current = parent_op;
            }

            let parent_id = effective_current.parents.first().ok_or_else(|| {
                CoreError::InvalidArgument("cannot undo root initialization operation".to_string())
            })?;
            self.oplog.read_operation(parent_id)?
        };

        let mut restored_view = target_op.view.clone();

        // Restore Git branch refs, branch sandboxes, and materialize the trunk branch's tree back to repo_root.
        let cfg = self.oplog.load_config()?;
        for (branch, commit_id) in &restored_view.branches {
            self.git
                .update_ref(&format!("refs/heads/{branch}"), commit_id)?;
            let branch_ws = self.branch_sandbox_dir(branch);
            if branch_ws.is_dir() {
                if let Ok(c_obj) = self.git.read_commit(commit_id) {
                    let _ = materialize_tree_to_dir(&self.git, &c_obj.tree_id, &branch_ws);
                    let safe_b = Self::sanitize_branch_name(branch);
                    let stamp = self.branches_root().join(format!("{safe_b}.tree"));
                    let payload = format!("{commit_id}:{}", c_obj.tree_id);
                    let _ = atomic_write_file(&stamp, payload.as_bytes());
                }
            }
        }

        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        let mut restored_trunk_tree = None;
        if let Some(trunk_commit_id) = restored_view
            .branches
            .get(&current_head_branch)
            .or_else(|| restored_view.branches.get(&cfg.trunk_branch))
            .or_else(|| restored_view.branches.values().next())
            .cloned()
        {
            if let Ok(commit_obj) = self.git.read_commit(&trunk_commit_id) {
                materialize_tree_to_dir(&self.git, &commit_obj.tree_id, self.git.repo_root())?;
                self.git.sync_git_index(&trunk_commit_id);
                restored_trunk_tree = Some((trunk_commit_id, commit_obj.tree_id));
            }
        }

        if effective_current.agent_id == "merge" && target_op_or_checkpoint.is_none() {
            if let Some((trunk_cid, trunk_tid)) = &restored_trunk_tree {
                restored_view.observed_blobs.clear();
                for agent in restored_view.agents.values_mut() {
                    let eff_b = agent.effective_branch(current_head_branch.as_str());
                    if agent.workspace_path.is_none() && eff_b == current_head_branch {
                        agent.base_commit = *trunk_cid;
                        agent.head_commit = *trunk_cid;
                        agent.head_tree = *trunk_tid;
                        agent.own_blobs.clear();
                        agent.deleted_paths.clear();
                        agent.read_blobs.clear();
                        agent.integrated_peer_blobs.clear();
                        agent.claimed_symbols.clear();
                    }
                }
            }
        }

        // Restore agent refs and any shared-root or COW workspace state captured at target_op.
        for agent in restored_view.agents.values() {
            let _ = self.git.update_ref(
                &format!("refs/tessera/agents/{}", agent.agent_id),
                &agent.head_commit,
            );
            let _ = self.git.update_ref(
                &format!("refs/tessera/changes/{}", agent.change_id.as_str()),
                &agent.head_commit,
            );
            if effective_current.agent_id != "merge"
                && agent.agent_id == target_op.agent_id
                && agent.workspace_path.is_none()
            {
                if let Ok(commit_obj) = self.git.read_commit(&agent.head_commit) {
                    materialize_tree_to_dir(&self.git, &commit_obj.tree_id, self.git.repo_root())?;
                }
            }
        }

        let undo_op = Operation::new(
            vec![current.id],
            "undo",
            format!("undo to operation {}", target_op.id.short()),
            false,
            restored_view,
        )?;
        self.oplog.write_operation(&undo_op)?;
        Ok(undo_op)
    }

    /// Resolves `rev_or_checkpoint` to a commit [`ObjectId`], checking Git revisions (`0bbe31f`,
    /// `HEAD~1`, `main`, tags) first, and falling back to named Tessera checkpoints or `OpId` prefixes.
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading the `OpLog` or Git refs fails.
    pub fn resolve_checkpoint_or_revision(
        &self,
        rev_or_checkpoint: &str,
        branch_hint: &str,
    ) -> CoreResult<Option<ObjectId>> {
        if let Ok(oid) = self.git.resolve_revision(rev_or_checkpoint) {
            return Ok(Some(oid));
        }

        let current = self.oplog.load_current_reconciled()?;
        let checkpoint_op_id = current
            .view
            .agents
            .values()
            .find_map(|agent| agent.checkpoints.get(rev_or_checkpoint).cloned())
            .or_else(|| {
                self.oplog
                    .list_history(256)
                    .ok()
                    .into_iter()
                    .flatten()
                    .find_map(|op| {
                        op.view
                            .agents
                            .values()
                            .find_map(|agent| agent.checkpoints.get(rev_or_checkpoint).cloned())
                    })
            });
        let op_id_opt = checkpoint_op_id.or_else(|| self.oplog.resolve_op_prefix(rev_or_checkpoint).ok());
        if let Some(op_id) = op_id_opt {
            if let Ok(op) = self.oplog.read_operation(&op_id) {
                if let Some(cid) = op
                    .view
                    .branches
                    .get(branch_hint)
                    .or_else(|| op.view.branches.values().next())
                {
                    return Ok(Some(*cid));
                }
            }
        }
        Ok(None)
    }

    /// Restores either an entire branch (when `paths` is empty) or specific `paths` to a target
    /// Git commit, ref (`HEAD~1`, branch, tag), or named Tessera checkpoint (`source_rev`),
    /// keeping `refs/heads/<branch>`, `.git/index`, `repo_root`, the branch's Copy-on-Write
    /// sandbox (`branches/<branch>`), and `.git/tessera/oplog` in sync.
    ///
    /// When `source_rev` is `None`, restores `paths` from the target branch's `HEAD` commit
    /// (discarding uncommitted changes, just like `git restore <paths...>`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if the revision or paths cannot be resolved or written.
    pub fn restore(
        &self,
        session_id: &str,
        branch: Option<&str>,
        source_rev: Option<&str>,
        paths: &[String],
    ) -> CoreResult<RestoreReport> {
        let cfg = self.oplog.load_config()?;
        let current = self.oplog.load_current_reconciled()?;
        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        let target_branch = branch
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                current
                    .view
                    .agents
                    .get(session_id)
                    .and_then(|a| a.target_branch.clone())
            })
            .unwrap_or_else(|| current_head_branch.clone());
        validate_ref_name(&target_branch)?;

        let branch_head_cid = self
            .git
            .resolve_ref(&format!("refs/heads/{target_branch}"))?
            .or_else(|| self.git.resolve_head_commit().ok().flatten())
            .ok_or_else(|| CoreError::NotFound(format!("branch {target_branch}")))?;

        let source_commit_id = if let Some(rev) = source_rev.map(str::trim).filter(|s| !s.is_empty()) {
            self.resolve_checkpoint_or_revision(rev, &target_branch)?
                .ok_or_else(|| CoreError::NotFound(format!("commit, ref, or checkpoint '{rev}'")))?
        } else {
            branch_head_cid
        };

        let source_commit = self.git.read_commit(&source_commit_id)?;
        let source_map = self.git.read_tree_to_map(&source_commit.tree_id)?;
        let branch_head_commit = self.git.read_commit(&branch_head_cid)?;
        let branch_head_map = self.git.read_tree_to_map(&branch_head_commit.tree_id)?;
        let branch_ws = self.branch_sandbox_dir(&target_branch);

        if paths.is_empty() {
            // Mode A: Restore the entire branch (`target_branch`) to `source_commit_id`.
            self.git
                .update_ref(&format!("refs/heads/{target_branch}"), &source_commit_id)?;

            if branch_ws.is_dir() {
                for rel_path in branch_head_map.keys() {
                    if !source_map.contains_key(rel_path) {
                        if let Ok((_, ws_target)) = resolve_confined_repo_path(&branch_ws, rel_path)
                        {
                            let _ = fs::remove_file(ws_target);
                        }
                    }
                }
                materialize_tree_to_dir(&self.git, &source_commit.tree_id, &branch_ws)?;
                let safe_b = Self::sanitize_branch_name(&target_branch);
                let stamp = self.branches_root().join(format!("{safe_b}.tree"));
                let stamp_payload = format!("{source_commit_id}:{}", source_commit.tree_id);
                let _ = atomic_write_file(&stamp, stamp_payload.as_bytes());
            }

            if target_branch == current_head_branch {
                for rel_path in branch_head_map.keys() {
                    if !source_map.contains_key(rel_path) {
                        if let Ok((_, root_target)) =
                            resolve_confined_repo_path(self.git.repo_root(), rel_path)
                        {
                            let _ = fs::remove_file(root_target);
                        }
                    }
                }
                materialize_tree_to_dir(&self.git, &source_commit.tree_id, self.git.repo_root())?;
                self.git.sync_git_index(&source_commit_id);
            }

            self.oplog.record_tx(
                session_id,
                format!("restore branch {target_branch} to {}", source_commit_id.short()),
                false,
                |view| {
                    view.branches
                        .insert(target_branch.clone(), source_commit_id);
                    for agent in view.agents.values_mut() {
                        let eff_b = agent.effective_branch(current_head_branch.as_str());
                        if eff_b == target_branch {
                            agent.base_commit = source_commit_id;
                            agent.head_commit = source_commit_id;
                            agent.head_tree = source_commit.tree_id;
                            agent.own_blobs.clear();
                            agent.deleted_paths.clear();
                            agent.foreign_baseline.clear();
                            agent.read_blobs.clear();
                            agent.integrated_peer_blobs.clear();
                            agent.claimed_symbols.clear();
                            agent.updated_at_ms = now_millis();
                        }
                    }
                    view.observed_blobs.clear();
                    Ok(())
                },
            )?;

            return Ok(RestoreReport {
                branch: target_branch,
                source_commit_id,
                restored_paths: Vec::new(),
                branch_reset: true,
            });
        }

        // Mode B: Restore specific file(s) or directory prefixes from `source_commit_id`.
        let mut restored_paths = BTreeSet::new();
        let mut restored_blobs = BTreeMap::new();
        let mut removed_paths = BTreeSet::new();

        for raw_p in paths {
            let trimmed_raw = raw_p
                .trim()
                .trim_start_matches("./")
                .trim_matches('/')
                .replace('\\', "/");
            if trimmed_raw.is_empty() || trimmed_raw == "." {
                for (k, entry) in &source_map {
                    restored_paths.insert(k.clone());
                    restored_blobs.insert(k.clone(), entry.clone());
                }

                for k in branch_head_map.keys() {
                    if !source_map.contains_key(k) {
                        restored_paths.insert(k.clone());
                        removed_paths.insert(k.clone());
                    }
                }
                continue;
            }

            let (clean_p, clean_full) =
                resolve_confined_repo_path(self.git.repo_root(), &trimmed_raw)?;
            let prefix = format!("{clean_p}/");
            let mut matched = false;
            for (k, entry) in &source_map {
                if k == &clean_p || k.starts_with(&prefix) {
                    restored_paths.insert(k.clone());
                    restored_blobs.insert(k.clone(), entry.clone());
                    matched = true;
                }
            }

            for k in branch_head_map.keys() {
                if (k == &clean_p || k.starts_with(&prefix)) && !source_map.contains_key(k) {
                    restored_paths.insert(k.clone());
                    removed_paths.insert(k.clone());
                    matched = true;
                }
            }

            if !matched && clean_full.exists() {
                restored_paths.insert(clean_p.clone());
                removed_paths.insert(clean_p);
            } else if !matched {
                return Err(CoreError::NotFound(format!(
                    "path '{clean_p}' in commit {}",
                    source_commit_id.short()
                )));
            }
        }

        for (rel_path, entry) in &restored_blobs {
            let bytes = self.git.read_blob(&entry.blob_id)?;
            if branch_ws.is_dir() {
                write_worktree_rel(&branch_ws, rel_path, &bytes, entry.executable);
            }

            if target_branch == current_head_branch {
                let (_, root_target) =
                    resolve_confined_repo_path(self.git.repo_root(), rel_path)?;
                atomic_write_file(&root_target, &bytes)?;
                set_worktree_executable(&root_target, entry.executable);
            }
        }

        for rel_path in &removed_paths {
            if branch_ws.is_dir() {
                if let Ok((_, ws_target)) = resolve_confined_repo_path(&branch_ws, rel_path) {
                    let _ = fs::remove_file(ws_target);
                }
            }

            if target_branch == current_head_branch {
                if let Ok((_, root_target)) =
                    resolve_confined_repo_path(self.git.repo_root(), rel_path)
                {
                    let _ = fs::remove_file(root_target);
                }
            }
        }

        if branch_ws.is_dir() {
            let safe_b = Self::sanitize_branch_name(&target_branch);
            let _ = fs::remove_file(self.branches_root().join(format!("{safe_b}.tree")));
        }

        let is_head_restore = source_commit_id == branch_head_cid;
        if target_branch == current_head_branch && is_head_restore {
            self.git.sync_git_index(&branch_head_cid);
        }

        let restored_vec: Vec<String> = restored_paths.into_iter().collect();
        let branch_head_tree_id = branch_head_commit.tree_id;
        let current_before = self.oplog.load_current_reconciled()?;
        let mut updated_session_trees: BTreeMap<String, (ObjectId, ObjectId)> = BTreeMap::new();
        let mut repo_root_observed: BTreeMap<String, ObjectId> = BTreeMap::new();
        let mut restored_changes: BTreeMap<String, (BTreeMap<String, ObjectId>, BTreeSet<String>)> =
            BTreeMap::new();

        if target_branch != current_head_branch {
            for p in &restored_vec {
                let root_file = self.git.repo_root().join(p);
                if root_file.is_file() {
                    if let Ok(bytes) = fs::read(&root_file) {
                        if let Ok(bid) = self.git.write_blob(&bytes) {
                            repo_root_observed.insert(p.clone(), bid);
                        }
                    }
                }
            }
        }

        let author = self.git.resolve_git_author();
        for (aid, agent) in &current_before.view.agents {
            let eff_b = agent.effective_branch(current_head_branch.as_str());
            if eff_b != target_branch {
                continue;
            }

            let mut remaining_own = agent.own_blobs.clone();
            let mut remaining_deleted = agent.deleted_paths.clone();
            for p in &restored_vec {
                let matches_branch_head = is_head_restore
                    || restored_blobs.get(p) == branch_head_map.get(p);
                if matches_branch_head {
                    remaining_own.remove(p);
                    remaining_deleted.remove(p);
                } else if aid == session_id || agent.owns_path(p) {
                    match restored_blobs.get(p) {
                        Some(entry) => {
                            remaining_own.insert(p.clone(), entry.blob_id);
                            remaining_deleted.remove(p);
                        }
                        None => {
                            remaining_own.remove(p);
                            if branch_head_map.contains_key(p) {
                                remaining_deleted.insert(p.clone());
                            }
                        }
                    }
                }
            }

            restored_changes.insert(aid.clone(), (remaining_own.clone(), remaining_deleted.clone()));
            if remaining_own.is_empty() && remaining_deleted.is_empty() {
                let _ = self
                    .git
                    .update_ref(&format!("refs/tessera/agents/{aid}"), &branch_head_cid);
                let _ = self.git.update_ref(
                    &format!("refs/tessera/changes/{}", agent.change_id.as_str()),
                    &branch_head_cid,
                );
                updated_session_trees.insert(aid.clone(), (branch_head_cid, branch_head_tree_id));
            } else if remaining_own != agent.own_blobs
                || remaining_deleted != agent.deleted_paths
                || agent.base_commit != branch_head_cid
            {
                let mut tree_map = branch_head_map.clone();
                for (p, bid) in &remaining_own {
                    let executable = restored_blobs
                        .get(p)
                        .or_else(|| tree_map.get(p))
                        .is_some_and(|e| e.executable);
                    tree_map.insert(
                        p.clone(),
                        FileEntry {
                            blob_id: *bid,
                            executable,
                        },
                    );
                }

                for p in &remaining_deleted {
                    tree_map.remove(p);
                }

                if let Ok(new_tree_id) = self.git.write_tree_from_map(&tree_map) {
                    if let Ok(new_cid) = self.git.write_commit(
                        &new_tree_id,
                        std::slice::from_ref(&branch_head_cid),
                        &author,
                        &agent.intent,
                    ) {
                        let _ = self
                            .git
                            .update_ref(&format!("refs/tessera/agents/{aid}"), &new_cid);
                        let _ = self.git.update_ref(
                            &format!("refs/tessera/changes/{}", agent.change_id.as_str()),
                            &new_cid,
                        );
                        updated_session_trees.insert(aid.clone(), (new_cid, new_tree_id));
                    }
                }
            }
        }

        self.oplog.record_tx(
            session_id,
            format!(
                "restore {:?} on {target_branch} from {}",
                restored_vec,
                source_commit_id.short()
            ),
            false,
            |view| {
                for p in &restored_vec {
                    if target_branch == current_head_branch {
                        let matches_branch_head = is_head_restore
                            || restored_blobs.get(p) == branch_head_map.get(p);
                        if matches_branch_head {
                            view.observed_blobs.remove(p);
                        } else if let Some(entry) = restored_blobs.get(p) {
                            view.observed_blobs.insert(p.clone(), vec![entry.blob_id]);
                        }
                    } else if let Some(root_bid) = repo_root_observed.get(p) {
                        let obs = view.observed_blobs.entry(p.clone()).or_default();
                        obs.retain(|b| b != root_bid);
                        obs.push(*root_bid);
                    }
                    view.conflicts.remove(p);
                }

                let mut updated_changes = Vec::new();
                for (aid, agent) in view.agents.iter_mut() {
                    let eff_b = agent.effective_branch(current_head_branch.as_str());
                    if eff_b == target_branch {
                        for p in &restored_vec {
                            agent.integrated_peer_blobs.remove(p);
                            let matches_branch_head = is_head_restore
                                || restored_blobs.get(p) == branch_head_map.get(p);
                            if matches_branch_head {
                                agent.read_blobs.remove(p);
                                let sym_prefix = format!("{p}::");
                                agent
                                    .claimed_symbols
                                    .retain(|s| s != p && !s.starts_with(&sym_prefix));
                            } else if let Some(entry) = restored_blobs.get(p) {
                                if aid == session_id || agent.owns_path(p) {
                                    agent.read_blobs.insert(p.clone(), entry.blob_id);
                                }
                            }
                        }

                        if let Some((own, deleted)) = restored_changes.get(aid) {
                            agent.own_blobs = own.clone();
                            agent.deleted_paths = deleted.clone();
                        }

                        if let Some((new_cid, new_tid)) = updated_session_trees.get(aid) {
                            agent.base_commit = branch_head_cid;
                            agent.head_commit = *new_cid;
                            agent.head_tree = *new_tid;
                            updated_changes.push((agent.change_id.clone(), *new_cid));
                        }

                        agent.updated_at_ms = now_millis();
                    }
                }

                for (cid, head_cid) in updated_changes {
                    view.changes.insert(cid, head_cid);
                }

                Ok(())
            },
        )?;

        Ok(RestoreReport {
            branch: target_branch,
            source_commit_id,
            restored_paths: restored_vec,
            branch_reset: false,
        })
    }

    /// Deletes `branch` from Git (`refs/heads/<branch>`), removes its Copy-on-Write sandbox
    /// directory (`branches/<branch>`), removes it from the `OpLog` `View`, and detaches any
    /// sessions targeting `branch` back to `trunk_branch`.
    ///
    /// When `force` is `false` (`-d`), verifies that `branch` has been merged into `trunk_branch`
    /// and that no active session has uncommitted `own_blobs` on `branch`.
    ///
    /// # Errors
    /// Returns [`CoreError`] if `branch` is the trunk branch, does not exist, or has unmerged changes when `force == false`.
    pub fn delete_branch(
        &self,
        session_id: &str,
        branch: &str,
        force: bool,
    ) -> CoreResult<DeleteBranchReport> {
        let clean_branch = branch.trim();
        validate_ref_name(clean_branch)?;

        let cfg = self.oplog.load_config()?;
        if clean_branch == cfg.trunk_branch {
            return Err(CoreError::InvalidArgument(format!(
                "cannot delete trunk branch '{}'",
                cfg.trunk_branch
            )));
        }

        let current = self.oplog.load_current_reconciled()?;
        let git_cid_opt = self
            .git
            .resolve_ref(&format!("refs/heads/{clean_branch}"))?;
        let view_cid_opt = current.view.branches.get(clean_branch).cloned();
        let targeted_by_session = current
            .view
            .agents
            .values()
            .any(|a| a.target_branch.as_deref() == Some(clean_branch));

        if git_cid_opt.is_none() && view_cid_opt.is_none() && !targeted_by_session {
            return Err(CoreError::NotFound(format!("branch '{clean_branch}'")));
        }

        let branch_cid_opt = git_cid_opt.or(view_cid_opt);
        let trunk_cid = self
            .git
            .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))?
            .or_else(|| self.git.resolve_head_commit().ok().flatten())
            .ok_or_else(|| CoreError::NotFound(format!("trunk branch '{}'", cfg.trunk_branch)))?;
        let trunk_commit = self.git.read_commit(&trunk_cid)?;

        if !force {
            let has_uncommitted = current.view.agents.values().any(|a| {
                a.target_branch.as_deref() == Some(clean_branch) && a.has_own_changes()
            });

            if has_uncommitted {
                return Err(CoreError::InvalidArgument(format!(
                    "branch '{clean_branch}' has uncommitted session changes; use -D (--force) to delete anyway"
                )));
            }

            if let Some(b_cid) = &branch_cid_opt {
                if !self.git.is_ancestor(b_cid, &trunk_cid) {
                    return Err(CoreError::InvalidArgument(format!(
                        "branch '{clean_branch}' is not fully merged into '{}'; use -D (--force) to delete anyway",
                        cfg.trunk_branch
                    )));
                }
            }
        }

        let current_head = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());

        if current_head == clean_branch {
            let _ = self.switch_editor_branch(&cfg.trunk_branch);
        }

        self.git.delete_ref(&format!("refs/heads/{clean_branch}"));

        let safe_b = Self::sanitize_branch_name(clean_branch);
        let sandbox_dir = self.branches_root().join(&safe_b);
        if sandbox_dir.exists() {
            let _ = fs::remove_dir_all(&sandbox_dir);
        }

        let stamp_file = self.branches_root().join(format!("{safe_b}.tree"));
        if stamp_file.exists() {
            let _ = fs::remove_file(&stamp_file);
        }

        let legacy_dir = self.git.git_dir().join("tessera").join("branches").join(&safe_b);
        if legacy_dir.exists() {
            let _ = fs::remove_dir_all(&legacy_dir);
        }

        let mut detached_sessions = Vec::new();
        let trunk_tree_id = trunk_commit.tree_id;

        self.oplog.record_tx(
            session_id,
            format!("delete branch {clean_branch}"),
            false,
            |view| {
                view.branches.remove(clean_branch);

                for (aid, agent) in view.agents.iter_mut() {
                    if agent.target_branch.as_deref() == Some(clean_branch) {
                        agent.target_branch = None;
                        agent.base_commit = trunk_cid;
                        agent.head_commit = trunk_cid;
                        agent.head_tree = trunk_tree_id;
                        agent.own_blobs.clear();
                        agent.read_blobs.clear();
                        agent.claimed_symbols.clear();
                        agent.updated_at_ms = now_millis();
                        detached_sessions.push(aid.clone());
                    }
                }

                Ok(())
            },
        )?;

        Ok(DeleteBranchReport {
            deleted_branch: clean_branch.to_string(),
            previous_commit: branch_cid_opt,
            detached_sessions,
        })
    }

    /// Renames `old_branch` to `new_branch` across Git refs, Copy-on-Write branch sandboxes,
    /// and all active sessions in the `OpLog` `View`.
    ///
    /// # Errors
    /// Returns [`CoreError`] if `old_branch` is the trunk branch, does not exist, or `new_branch` already exists.
    pub fn rename_branch(
        &self,
        session_id: &str,
        old_branch: &str,
        new_branch: &str,
    ) -> CoreResult<RenameBranchReport> {
        let clean_old = old_branch.trim();
        let clean_new = new_branch.trim();
        validate_ref_name(clean_old)?;
        validate_ref_name(clean_new)?;

        if clean_old == clean_new {
            let cid = self
                .git
                .resolve_ref(&format!("refs/heads/{clean_old}"))?
                .or_else(|| self.git.resolve_head_commit().ok().flatten())
                .ok_or_else(|| CoreError::NotFound(format!("branch '{clean_old}'")))?;

            return Ok(RenameBranchReport {
                old_branch: clean_old.to_string(),
                new_branch: clean_new.to_string(),
                commit_id: cid,
            });
        }

        let cfg = self.oplog.load_config()?;
        if clean_old == cfg.trunk_branch {
            return Err(CoreError::InvalidArgument(format!(
                "cannot rename trunk branch '{}'",
                cfg.trunk_branch
            )));
        }

        let current = self.oplog.load_current_reconciled()?;
        if self
            .git
            .resolve_ref(&format!("refs/heads/{clean_new}"))?
            .is_some()
            || current.view.branches.contains_key(clean_new)
        {
            return Err(CoreError::InvalidArgument(format!(
                "branch '{clean_new}' already exists"
            )));
        }

        let old_cid = self
            .git
            .resolve_ref(&format!("refs/heads/{clean_old}"))?
            .or_else(|| current.view.branches.get(clean_old).cloned())
            .ok_or_else(|| CoreError::NotFound(format!("branch '{clean_old}'")))?;

        self.git
            .update_ref(&format!("refs/heads/{clean_new}"), &old_cid)?;
        self.git.delete_ref(&format!("refs/heads/{clean_old}"));

        if self.git.current_branch_name().ok().as_deref() == Some(clean_old) {
            self.git.set_symbolic_head(clean_new)?;
        }

        let safe_old = Self::sanitize_branch_name(clean_old);
        let safe_new = Self::sanitize_branch_name(clean_new);
        let old_ws = self.branches_root().join(&safe_old);
        let new_ws = self.branches_root().join(&safe_new);

        if old_ws.is_dir() && !new_ws.exists() {
            let _ = fs::rename(&old_ws, &new_ws);
        }

        let old_stamp = self.branches_root().join(format!("{safe_old}.tree"));
        let new_stamp = self.branches_root().join(format!("{safe_new}.tree"));

        if old_stamp.is_file() {
            let _ = fs::rename(&old_stamp, &new_stamp);
        }

        self.oplog.record_tx(
            session_id,
            format!("rename branch {clean_old} to {clean_new}"),
            false,
            |view| {
                view.branches.remove(clean_old);
                view.branches.insert(clean_new.to_string(), old_cid);

                for agent in view.agents.values_mut() {
                    if agent.target_branch.as_deref() == Some(clean_old) {
                        agent.target_branch = Some(clean_new.to_string());
                        agent.updated_at_ms = now_millis();
                    }
                }

                Ok(())
            },
        )?;

        Ok(RenameBranchReport {
            old_branch: clean_old.to_string(),
            new_branch: clean_new.to_string(),
            commit_id: old_cid,
        })
    }

    /// Saves the uncommitted session changes on `session_id` (or on `branch_override` if the
    /// caller has no session-specific `own_blobs`) into a [`StashEntry`] at `stash@{0}` in the
    /// `OpLog`, and restores the clean branch files on disk while preserving any concurrent
    /// peer session edits on the same branch.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if there are no local changes to stash.
    pub fn stash_push(
        &self,
        session_id: &str,
        message: Option<&str>,
        branch_override: Option<&str>,
    ) -> CoreResult<StashEntry> {
        self.stash_push_with_paths(session_id, message, branch_override, &[])
    }

    /// Saves the uncommitted session changes on `session_id` (optionally filtered to `paths`)
    /// into a [`StashEntry`] at `stash@{0}` in the `OpLog`, and restores only the stashed paths
    /// on disk while preserving concurrent peer session edits and any un-stashed paths.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if there are no matching local changes to stash.
    pub fn stash_push_with_paths(
        &self,
        session_id: &str,
        message: Option<&str>,
        branch_override: Option<&str>,
        paths: &[String],
    ) -> CoreResult<StashEntry> {
        let _ = self.watch_tick(session_id);
        let meta = self.snapshot_agent(session_id, false)?;
        let cfg = self.oplog.load_config()?;
        let current = self.oplog.load_current_reconciled()?;
        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());

        let eff_branch = branch_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| meta.target_branch.clone())
            .unwrap_or_else(|| current_head_branch.clone());

        let base_cid = self
            .git
            .resolve_ref(&format!("refs/heads/{eff_branch}"))?
            .unwrap_or(meta.base_commit);
        let base_commit = self.git.read_commit(&base_cid)?;
        let base_map = self.git.read_tree_to_map(&base_commit.tree_id)?;

        let mut stashed_blobs = meta.own_blobs.clone();
        let mut stashed_deleted: BTreeSet<String> = meta.deleted_paths.clone();
        let mut stashed_symbols = meta.claimed_symbols.clone();
        let mut target_sessions = vec![session_id.to_string()];

        // Unstaged session changes captured by `snapshot_agent` (edits, additions, deletions and
        // mode-only changes) relative to the session's own base, never against a newer branch tip.
        {
            let session_base_map = self
                .git
                .read_commit(&meta.base_commit)
                .and_then(|c| self.git.read_tree_to_map(&c.tree_id))
                .unwrap_or_else(|_| base_map.clone());
            let head_map = self.git.read_tree_to_map(&meta.head_tree)?;
            for (path, entry) in &head_map {
                if session_base_map.get(path) != Some(entry) && !stashed_blobs.contains_key(path) {
                    stashed_blobs.insert(path.clone(), entry.blob_id);
                }
            }

            for path in session_base_map.keys() {
                if !head_map.contains_key(path) && !stashed_blobs.contains_key(path) {
                    stashed_deleted.insert(path.clone());
                }
            }
        }

        if stashed_blobs.is_empty() && stashed_deleted.is_empty() {
            // If invoked from a terminal or editor session with no direct own_blobs, collect
            // uncommitted own_blobs and tombstones from active sessions targeting `eff_branch`.
            for (aid, agent) in &current.view.agents {
                let a_br = agent.effective_branch(&current_head_branch);

                if a_br == eff_branch && agent.has_own_changes() {
                    for (k, v) in &agent.own_blobs {
                        stashed_blobs.insert(k.clone(), *v);
                    }

                    for k in &agent.deleted_paths {
                        if !stashed_blobs.contains_key(k) {
                            stashed_deleted.insert(k.clone());
                        }
                    }

                    for s in &agent.claimed_symbols {
                        stashed_symbols.insert(s.clone());
                    }

                    if !target_sessions.contains(aid) {
                        target_sessions.push(aid.clone());
                    }
                }
            }
        }

        if stashed_blobs.is_empty() && stashed_deleted.is_empty() && eff_branch == current_head_branch {
            let foreign_owned_paths: BTreeSet<String> = current
                .view
                .agents
                .values()
                .filter(|a| a.effective_branch(&current_head_branch) != eff_branch)
                .flat_map(|a| a.owned_paths().cloned())
                .collect();

            if let Ok(root_snap) = snapshot_working_dir(
                &self.git,
                self.git.repo_root(),
                session_id,
                &meta.scopes,
                Some(&base_commit.tree_id),
            ) {
                for (path, entry) in &root_snap.files {
                    if is_harness_config_path(path) || foreign_owned_paths.contains(path) {
                        continue;
                    }

                    if base_map.get(path) != Some(entry) {
                        stashed_blobs.insert(path.clone(), entry.blob_id);
                    }
                }

                for path in base_map.keys() {
                    if !root_snap.files.contains_key(path)
                        && !is_harness_config_path(path)
                        && !foreign_owned_paths.contains(path)
                    {
                        stashed_deleted.insert(path.clone());
                    }
                }
            }
        }

        let normalized_paths: Vec<String> = paths
            .iter()
            .map(|p| {
                p.trim()
                    .trim_start_matches("./")
                    .trim_end_matches('/')
                    .to_string()
            })
            .filter(|p| !p.is_empty())
            .collect();

        if !normalized_paths.is_empty() {
            let matches_filter = |rel_path: &str| {
                normalized_paths.iter().any(|filter| {
                    rel_path == filter
                        || rel_path
                            .strip_prefix(filter.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                })
            };

            stashed_blobs.retain(|k, _| matches_filter(k));
            stashed_deleted.retain(|k| matches_filter(k));
            stashed_symbols.retain(|sym| {
                let file_part = sym.split("::").next().unwrap_or(sym);
                matches_filter(file_part)
            });
        }

        if stashed_blobs.is_empty() && stashed_deleted.is_empty() {
            return Err(CoreError::InvalidArgument(
                "no local changes to stash".to_string(),
            ));
        }

        let ts = now_millis();
        let stash_msg = message
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| {
                if !meta.intent_inferred && !meta.intent.is_empty() {
                    format!("WIP on {eff_branch}: {}", meta.intent)
                } else {
                    let files: Vec<&str> = stashed_blobs.keys().take(3).map(String::as_str).collect();
                    format!("WIP on {eff_branch}: {}", files.join(", "))
                }
            });

        // Restore stashed files on disk to `base_map` (or to any remaining peer session's isolated blob on `eff_branch`).
        let branch_ws = self.branch_sandbox_dir(&eff_branch);
        let mut isolated_stashed_blobs = stashed_blobs.clone();
        let mut updated_peer_blobs: BTreeMap<(String, String), ObjectId> = BTreeMap::new();

        for (rel_path, stashed_bid) in &stashed_blobs {
            let remaining_peer = current.view.agents.iter().find_map(|(aid, a)| {
                let a_br = a
                    .target_branch
                    .as_deref()
                    .unwrap_or(current_head_branch.as_str());
                if !target_sessions.contains(aid) && a_br == eff_branch {
                    a.own_blobs
                        .get(rel_path)
                        .cloned()
                        .map(|bid| (aid.clone(), bid))
                } else {
                    None
                }
            });

            let target_blob_opt = if let Some((peer_id, peer_bid)) = remaining_peer {
                let base_bytes = base_map
                    .get(rel_path)
                    .and_then(|e| self.git.read_blob(&e.blob_id).ok())
                    .unwrap_or_default();
                let stashed_bytes = self.git.read_blob(stashed_bid).unwrap_or_default();
                let peer_bytes = self.git.read_blob(&peer_bid).unwrap_or_default();

                if let (Ok(base_str), Ok(stashed_str), Ok(peer_str)) = (
                    std::str::from_utf8(&base_bytes),
                    std::str::from_utf8(&stashed_bytes),
                    std::str::from_utf8(&peer_bytes),
                ) {
                    let combined = merge_file_3way(
                        rel_path,
                        base_str,
                        stashed_str,
                        peer_str,
                        session_id,
                        &peer_id,
                    );

                    if combined.content == peer_str && peer_str != stashed_str {
                        let stripped_peer = merge_file_3way(
                            rel_path,
                            stashed_str,
                            base_str,
                            peer_str,
                            "base",
                            &peer_id,
                        );
                        let new_peer_bid = self
                            .git
                            .write_blob(stripped_peer.content.as_bytes())?;
                        updated_peer_blobs
                            .insert((peer_id, rel_path.clone()), new_peer_bid);
                        Some(new_peer_bid)
                    } else if combined.content == stashed_str && stashed_str != peer_str {
                        let stripped_stash = merge_file_3way(
                            rel_path,
                            peer_str,
                            base_str,
                            stashed_str,
                            "base",
                            session_id,
                        );
                        let new_stash_bid = self
                            .git
                            .write_blob(stripped_stash.content.as_bytes())?;
                        isolated_stashed_blobs.insert(rel_path.clone(), new_stash_bid);
                        Some(peer_bid)
                    } else {
                        Some(peer_bid)
                    }
                } else {
                    Some(peer_bid)
                }
            } else {
                base_map.get(rel_path).map(|e| e.blob_id)
            };

            let restore_executable = base_map.get(rel_path).is_some_and(|e| e.executable);
            match target_blob_opt {
                Some(bid) => {
                    if let Ok(bytes) = self.git.read_blob(&bid) {
                        if eff_branch == current_head_branch {
                            if let Ok((_, p)) = resolve_confined_repo_path(self.git.repo_root(), rel_path) {
                                write_worktree_file(&p, &bytes, restore_executable);
                            }
                        }

                        if branch_ws.is_dir() {
                            if let Ok((_, p)) = resolve_confined_repo_path(&branch_ws, rel_path) {
                                write_worktree_file(&p, &bytes, restore_executable);
                            }
                        }
                    }
                }
                None => {
                    if eff_branch == current_head_branch {
                        remove_worktree_file(self.git.repo_root(), rel_path);
                    }

                    if branch_ws.is_dir() {
                        remove_worktree_file(&branch_ws, rel_path);
                    }
                }
            }
        }

        for rel_path in &stashed_deleted {
            let Some(entry) = base_map.get(rel_path) else {
                continue;
            };
            let Ok(bytes) = self.git.read_blob(&entry.blob_id) else {
                continue;
            };

            if eff_branch == current_head_branch {
                if let Ok((_, p)) = resolve_confined_repo_path(self.git.repo_root(), rel_path) {
                    write_worktree_file(&p, &bytes, entry.executable);
                }
            }

            if branch_ws.is_dir() {
                if let Ok((_, p)) = resolve_confined_repo_path(&branch_ws, rel_path) {
                    write_worktree_file(&p, &bytes, entry.executable);
                }
            }
        }

        if eff_branch == current_head_branch && normalized_paths.is_empty() {
            self.git.sync_git_index(&base_cid);
        }

        let stash_head_map = self.git.read_tree_to_map(&meta.head_tree).unwrap_or_default();
        let stashed_executable: BTreeSet<String> = isolated_stashed_blobs
            .keys()
            .filter(|p| {
                stash_head_map
                    .get(*p)
                    .or_else(|| base_map.get(*p))
                    .is_some_and(|e| e.executable)
            })
            .cloned()
            .collect();

        let entry = StashEntry {
            id: format!("stash-{ts}-{}", current.view.stashes.len() + 1),
            session_id: session_id.to_string(),
            branch: eff_branch.clone(),
            base_commit: base_cid,
            message: stash_msg,
            own_blobs: isolated_stashed_blobs,
            deleted_paths: stashed_deleted.clone(),
            executable_paths: Some(stashed_executable),
            claimed_symbols: stashed_symbols,
            created_at_ms: ts,
        };

        let saved_entry = entry.clone();
        let base_tree_id = base_commit.tree_id;
        let partial_stash = !normalized_paths.is_empty();

        self.oplog.record_tx(
            session_id,
            format!("stash push on {eff_branch}: {}", entry.message),
            false,
            |view| {
                view.stashes.insert(0, saved_entry.clone());

                for p in stashed_blobs.keys().chain(stashed_deleted.iter()) {
                    view.observed_blobs.remove(p);
                }

                for ((peer_id, rel_path), new_bid) in &updated_peer_blobs {
                    if let Some(peer) = view.agents.get_mut(peer_id) {
                        peer.own_blobs.insert(rel_path.clone(), *new_bid);
                    }
                }

                for tid in &target_sessions {
                    if let Some(agent) = view.agents.get_mut(tid) {
                        if partial_stash {
                            for p in stashed_blobs.keys().chain(stashed_deleted.iter()) {
                                agent.own_blobs.remove(p);
                                agent.deleted_paths.remove(p);
                                agent.read_blobs.remove(p);
                            }

                            for sym in &saved_entry.claimed_symbols {
                                agent.claimed_symbols.remove(sym);
                            }

                            if !agent.has_own_changes() {
                                agent.base_commit = base_cid;
                                agent.head_commit = base_cid;
                                agent.head_tree = base_tree_id;
                            }
                        } else {
                            agent.own_blobs.clear();
                            agent.deleted_paths.clear();
                            agent.read_blobs.clear();
                            agent.claimed_symbols.clear();
                            agent.base_commit = base_cid;
                            agent.head_commit = base_cid;
                            agent.head_tree = base_tree_id;
                        }

                        agent.updated_at_ms = now_millis();
                    }
                }

                Ok(())
            },
        )?;

        if partial_stash {
            let _ = self.snapshot_agent(session_id, true);
        }

        Ok(entry)
    }

    /// Applies `stash@{index}` onto `session_id`'s active branch using 3-way Tree-sitter AST
    /// merge (`merge_file_3way`), and drops the stash entry if `keep` is `false`.
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if `stash@{index}` does not exist.
    pub fn stash_pop(
        &self,
        session_id: &str,
        index: usize,
        keep: bool,
    ) -> CoreResult<StashPopReport> {
        let current = self.oplog.load_current_reconciled()?;
        let entry = current
            .view
            .stashes
            .get(index)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(format!("stash@{{{index}}}")))?;

        let meta = self.ensure_session(session_id, None)?;
        let cfg = self.oplog.load_config()?;
        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        let applied_branch = meta
            .target_branch
            .clone()
            .unwrap_or_else(|| current_head_branch.clone());

        let stash_base_map = self
            .git
            .read_commit(&entry.base_commit)
            .and_then(|c| self.git.read_tree_to_map(&c.tree_id))
            .unwrap_or_default();

        let branch_cid = self
            .git
            .resolve_ref(&format!("refs/heads/{applied_branch}"))?
            .unwrap_or(meta.base_commit);
        let branch_map = self
            .git
            .read_commit(&branch_cid)
            .and_then(|c| self.git.read_tree_to_map(&c.tree_id))
            .unwrap_or_default();

        let branch_ws = self.branch_sandbox_dir(&applied_branch);
        let mut applied_blobs = BTreeMap::new();
        let mut restored_paths = Vec::new();
        let mut any_conflict = false;

        for (rel_path, stash_bid) in &entry.own_blobs {
            let stash_bytes = self.git.read_blob(stash_bid)?;
            let stash_str = String::from_utf8_lossy(&stash_bytes);

            let base_bytes = stash_base_map
                .get(rel_path)
                .and_then(|e| self.git.read_blob(&e.blob_id).ok())
                .unwrap_or_default();
            let base_str = String::from_utf8_lossy(&base_bytes);

            let curr_bytes = if applied_branch == current_head_branch
                && self.git.repo_root().join(rel_path).is_file()
            {
                fs::read(self.git.repo_root().join(rel_path)).unwrap_or_default()
            } else if branch_ws.join(rel_path).is_file() {
                fs::read(branch_ws.join(rel_path)).unwrap_or_default()
            } else {
                branch_map
                    .get(rel_path)
                    .and_then(|e| self.git.read_blob(&e.blob_id).ok())
                    .unwrap_or_default()
            };
            let curr_str = String::from_utf8_lossy(&curr_bytes);

            let merge_res = merge_file_3way(
                rel_path,
                &base_str,
                &curr_str,
                &stash_str,
                &applied_branch,
                &format!("stash@{{{index}}}"),
            );

            if merge_res.has_conflicts {
                any_conflict = true;
            }

            let merged_bytes = merge_res.content.as_bytes();
            let merged_bid = self.git.write_blob(merged_bytes)?;
            let fallback_executable = branch_map
                .get(rel_path)
                .or_else(|| stash_base_map.get(rel_path))
                .is_some_and(|e| e.executable);
            let executable = entry
                .executable_paths
                .as_ref()
                .map_or(fallback_executable, |paths| paths.contains(rel_path));

            if applied_branch == current_head_branch {
                let (_, root_target) = resolve_confined_repo_path(self.git.repo_root(), rel_path)?;
                write_worktree_file(&root_target, merged_bytes, executable);
            }

            if branch_ws.is_dir() {
                if let Ok((_, ws_target)) = resolve_confined_repo_path(&branch_ws, rel_path) {
                    write_worktree_file(&ws_target, merged_bytes, executable);
                }
            }

            applied_blobs.insert(rel_path.clone(), merged_bid);
            restored_paths.push(rel_path.clone());
        }

        let session_base_map = self
            .git
            .read_commit(&meta.base_commit)
            .and_then(|c| self.git.read_tree_to_map(&c.tree_id))
            .unwrap_or_default();
        let mut applied_deletions = BTreeSet::new();
        for rel_path in &entry.deleted_paths {
            let stash_base_bid = stash_base_map.get(rel_path).map(|e| e.blob_id);
            let curr_bid = if applied_branch == current_head_branch {
                resolve_confined_repo_path(self.git.repo_root(), rel_path)
                    .ok()
                    .filter(|(_, p)| p.is_file())
                    .and_then(|(_, p)| fs::read(p).ok())
                    .map(|bytes| GitBackend::hash_blob_in_memory(&bytes))
            } else {
                branch_map.get(rel_path).map(|e| e.blob_id)
            };

            if curr_bid.is_some() && curr_bid != stash_base_bid {
                any_conflict = true;
                continue;
            }

            if applied_branch == current_head_branch {
                remove_worktree_file(self.git.repo_root(), rel_path);
            }

            if branch_ws.is_dir() {
                remove_worktree_file(&branch_ws, rel_path);
            }

            if session_base_map.contains_key(rel_path) {
                applied_deletions.insert(rel_path.clone());
            }
            restored_paths.push(rel_path.clone());
        }

        let dropped = !keep && !any_conflict;
        let stash_id = entry.id.clone();
        let stash_symbols = entry.claimed_symbols.clone();

        self.oplog.record_tx(
            session_id,
            format!(
                "stash {} stash@{{{index}}} on {applied_branch}",
                if keep { "apply" } else { "pop" }
            ),
            false,
            |view| {
                if dropped {
                    view.stashes.retain(|s| s.id != stash_id);
                }

                if let Some(agent) = view.agents.get_mut(session_id) {
                    for (p, bid) in &applied_blobs {
                        agent.deleted_paths.remove(p);
                        agent.own_blobs.insert(p.clone(), *bid);
                        agent.read_blobs.insert(p.clone(), *bid);
                    }

                    for p in &applied_deletions {
                        agent.own_blobs.remove(p);
                        agent.read_blobs.remove(p);
                        agent.deleted_paths.insert(p.clone());
                    }

                    for sym in &stash_symbols {
                        agent.claimed_symbols.insert(sym.clone());
                    }
                    agent.updated_at_ms = now_millis();
                }

                Ok(())
            },
        )?;

        let _ = self.snapshot_agent(session_id, true);

        Ok(StashPopReport {
            stash: entry,
            applied_branch,
            restored_paths,
            has_conflicts: any_conflict,
            dropped,
        })
    }

    /// Returns all stashed change sets in newest-first order (`stash@{0}` is index `0`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading the `OpLog` fails.
    pub fn stash_list(&self) -> CoreResult<Vec<StashEntry>> {
        let current = self.oplog.load_current_reconciled()?;
        Ok(current.view.stashes)
    }

    /// Drops a single stash entry (`Some(index)`) or clears all stash entries (`None`).
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if `index` is out of bounds.
    pub fn stash_drop(&self, session_id: &str, index: Option<usize>) -> CoreResult<usize> {
        let current = self.oplog.load_current_reconciled()?;
        let removed_count = match index {
            Some(idx) => {
                if idx >= current.view.stashes.len() {
                    return Err(CoreError::NotFound(format!("stash@{{{idx}}}")));
                }
                1
            }
            None => current.view.stashes.len(),
        };

        let target_id = index.and_then(|idx| current.view.stashes.get(idx).map(|s| s.id.clone()));

        self.oplog.record_tx(
            session_id,
            match index {
                Some(idx) => format!("stash drop stash@{{{idx}}}"),
                None => "stash clear".to_string(),
            },
            false,
            |view| {
                match &target_id {
                    Some(tid) => view.stashes.retain(|s| &s.id != tid),
                    None => view.stashes.clear(),
                }
                Ok(())
            },
        )?;

        Ok(removed_count)
    }

    /// Fetches `branch` from `remote`, fast-forwards or 3-way AST-merges `refs/heads/<branch>`,
    /// and automatically rebases all active agent sessions on `<branch>` via [`Self::sync_all_agents`].
    ///
    /// # Errors
    /// Returns [`CoreError`] if `git fetch` fails or the branch cannot be resolved.
    pub fn pull_branch(
        &self,
        session_id: &str,
        remote: &str,
        branch: Option<&str>,
    ) -> CoreResult<PullReport> {
        let clean_remote = remote.trim();
        validate_ref_name(clean_remote)?;

        let cfg = self.oplog.load_config()?;
        let current = self.oplog.load_current_reconciled()?;
        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());

        let target_branch = branch
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                current
                    .view
                    .agents
                    .get(session_id)
                    .and_then(|a| a.target_branch.clone())
            })
            .unwrap_or_else(|| current_head_branch.clone());
        validate_ref_name(&target_branch)?;

        let existing_local_cid = self
            .git
            .resolve_ref(&format!("refs/heads/{target_branch}"))?;
        let fallback_head_cid = self.git.resolve_head_commit().ok().flatten();

        let fetch_out = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir(self.git.repo_root())
            .args(["fetch", clean_remote, &target_branch])
            .output()
            .map_err(|source| CoreError::Io {
                path: "git fetch".to_string(),
                source,
            })?;

        if !fetch_out.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_out.stderr).trim().to_string();
            return Err(CoreError::InvalidArgument(format!(
                "git fetch {clean_remote} {target_branch} failed: {stderr}"
            )));
        }

        let remote_cid = self
            .git
            .resolve_ref(&format!("refs/remotes/{clean_remote}/{target_branch}"))?
            .or_else(|| self.git.resolve_revision("FETCH_HEAD").ok())
            .ok_or_else(|| {
                CoreError::NotFound(format!("remote ref '{clean_remote}/{target_branch}'"))
            })?;

        let old_cid = existing_local_cid
            .or(fallback_head_cid)
            .unwrap_or(remote_cid);

        let (new_cid, fast_forward) = if existing_local_cid.is_none() {
            self.git
                .update_ref(&format!("refs/heads/{target_branch}"), &remote_cid)?;
            (remote_cid, true)
        } else if old_cid == remote_cid || self.git.is_ancestor(&remote_cid, &old_cid) {
            (old_cid, true)
        } else if self.git.is_ancestor(&old_cid, &remote_cid) {
            self.git
                .update_ref(&format!("refs/heads/{target_branch}"), &remote_cid)?;
            (remote_cid, true)
        } else {
            let merge_base_out = Command::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .current_dir(self.git.repo_root())
                .args(["merge-base", old_cid.as_str(), remote_cid.as_str()])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| ObjectId::from_hex(String::from_utf8_lossy(&o.stdout).trim()).ok())
                .unwrap_or(old_cid);

            let base_commit = self.git.read_commit(&merge_base_out)?;
            let local_commit = self.git.read_commit(&old_cid)?;
            let remote_commit = self.git.read_commit(&remote_cid)?;

            let outcome = merge_trees_3way(
                &self.git,
                &base_commit.tree_id,
                &local_commit.tree_id,
                &remote_commit.tree_id,
                &target_branch,
                "local",
                &format!("{clean_remote}/{target_branch}"),
                "remote",
            )?;

            for c in &outcome.conflicts {
                self.oplog.save_conflict(c)?;
            }

            let author = self.git.resolve_git_author();
            let pull_msg = apply_co_author_trailer(
                &format!("Merge {clean_remote}/{target_branch} into {target_branch}"),
                &cfg,
            );
            let merged_cid = self.git.write_commit(
                &outcome.merged_tree_id,
                &[old_cid, remote_cid],
                &author,
                &pull_msg,
            )?;

            self.git
                .update_ref(&format!("refs/heads/{target_branch}"), &merged_cid)?;
            (merged_cid, false)
        };

        if new_cid != old_cid {
            let new_commit = self.git.read_commit(&new_cid)?;
            let branch_ws = self.branch_sandbox_dir(&target_branch);

            if branch_ws.is_dir() {
                let _ = materialize_tree_to_dir(&self.git, &new_commit.tree_id, &branch_ws);
                let safe_b = Self::sanitize_branch_name(&target_branch);
                let stamp = self.branches_root().join(format!("{safe_b}.tree"));
                let stamp_payload = format!("{new_cid}:{}", new_commit.tree_id);
                let _ = atomic_write_file(&stamp, stamp_payload.as_bytes());
            }

            if target_branch == current_head_branch {
                materialize_tree_to_dir(&self.git, &new_commit.tree_id, self.git.repo_root())?;
                self.git.sync_git_index(&new_cid);
            }
        }

        let rebased = self.sync_all_agents()?;

        // Re-overlay any uncommitted own_blobs and deleted_paths of sessions on `current_head_branch` so live work stays intact on disk.
        let refreshed = self.oplog.load_current_reconciled()?;
        let head_ws = self.branch_sandbox_dir(&current_head_branch);
        let head_ws_opt = head_ws.is_dir().then_some(head_ws.as_path());
        let mut post_obs = BTreeMap::new();
        self.reapply_remaining_head_sessions(
            &refreshed.view.agents,
            &BTreeSet::new(),
            &current_head_branch,
            head_ws_opt,
            &mut post_obs,
        );

        Ok(PullReport {
            remote: clean_remote.to_string(),
            branch: target_branch,
            old_commit: old_cid,
            new_commit: new_cid,
            fast_forward,
            rebased_sessions: rebased.len(),
        })
    }

    /// Returns the commit history for `branch` (or all branches if `branch` is `None`),
    /// up to `limit` commits.
    ///
    /// # Errors
    /// Returns [`CoreError`] if executing `git log` fails.
    pub fn commit_log(&self, branch: Option<&str>, limit: usize) -> CoreResult<Vec<LogEntry>> {
        let max_count = format!("-n{}", limit.max(1));
        let mut cmd = Command::new("git");
        cmd.env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("--git-dir")
            .arg(self.git.git_dir())
            .arg("log")
            .arg("--topo-order")
            .arg(&max_count)
            .arg("--format=%H\t%h\t%P\t%an\t%ct\t%D\t%s")
            .arg("--end-of-options");

        if let Some(b) = branch.map(str::trim).filter(|s| !s.is_empty()) {
            if b.starts_with('-') {
                return Err(CoreError::InvalidArgument(format!(
                    "revision or branch cannot start with '-': '{b}'"
                )));
            }
            cmd.arg(b);
        } else {
            cmd.arg("HEAD");
        }

        let out = cmd.output().map_err(|source| CoreError::Io {
            path: "git log".to_string(),
            source,
        })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(CoreError::InvalidArgument(format!("git log failed: {stderr}")));
        }

        let mut entries = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts: Vec<&str> = line.splitn(7, '\t').collect();
            if parts.len() == 7 {
                let sha = parts[0].trim().to_string();
                let short_sha = parts[1].trim().to_string();
                let parents: Vec<String> = parts[2]
                    .split_whitespace()
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
                    .collect();
                let author = parts[3].trim().to_string();
                let timestamp = parts[4].trim().parse::<i64>().unwrap_or(0);
                let refs: Vec<String> = parts[5]
                    .split(',')
                    .map(str::trim)
                    .filter(|r| !r.is_empty() && !r.starts_with("tessera/"))
                    .map(ToString::to_string)
                    .collect();
                let subject = parts[6].trim().to_string();

                entries.push(LogEntry {
                    sha,
                    short_sha,
                    parents,
                    author,
                    timestamp,
                    subject,
                    refs,
                });
            }
        }

        Ok(entries)
    }

    /// Applies the delta introduced by `commit_rev` (relative to its first parent, or an empty
    /// tree if `commit_rev` is a root commit) onto `target_branch` (or the caller's active branch)
    /// using 3-way Tree-sitter AST merge ([`merge_trees_3way`]).
    ///
    /// # Errors
    /// Returns [`CoreError`] if `commit_rev` or `target_branch` cannot be resolved, or if writing
    /// the resulting commit fails.
    pub fn cherry_pick(
        &self,
        session_id: &str,
        commit_rev: &str,
        target_branch: Option<&str>,
    ) -> CoreResult<CherryPickReport> {
        let cfg = self.oplog.load_config()?;
        let current = self.oplog.load_current_reconciled()?;
        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());

        let eff_branch = target_branch
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                current
                    .view
                    .agents
                    .get(session_id)
                    .and_then(|a| a.target_branch.clone())
            })
            .unwrap_or_else(|| current_head_branch.clone());
        validate_ref_name(&eff_branch)?;

        let source_cid = self.git.resolve_revision(commit_rev.trim())?;
        let source_commit = self.git.read_commit(&source_cid)?;

        let parent_tree_id = match source_commit.parents.first() {
            Some(parent_cid) => self.git.read_commit(parent_cid)?.tree_id,
            None => self.git.write_tree_from_map(&BTreeMap::new())?,
        };

        let target_cid = self
            .git
            .resolve_ref(&format!("refs/heads/{eff_branch}"))?
            .or_else(|| self.git.resolve_head_commit().ok().flatten())
            .ok_or_else(|| CoreError::NotFound(format!("branch '{eff_branch}'")))?;
        let target_commit = self.git.read_commit(&target_cid)?;

        let parent_map = self.git.read_tree_to_map(&parent_tree_id)?;
        let source_map = self.git.read_tree_to_map(&source_commit.tree_id)?;
        let mut touched_paths = BTreeSet::new();

        for (p, entry) in &source_map {
            if parent_map.get(p).map(|e| &e.blob_id) != Some(&entry.blob_id) {
                touched_paths.insert(p.clone());
            }
        }

        for p in parent_map.keys() {
            if !source_map.contains_key(p) {
                touched_paths.insert(p.clone());
            }
        }

        let short_src = &source_cid.as_str()[..8.min(source_cid.as_str().len())];
        let outcome = merge_trees_3way(
            &self.git,
            &parent_tree_id,
            &target_commit.tree_id,
            &source_commit.tree_id,
            &eff_branch,
            "target branch",
            short_src,
            source_commit.message.lines().next().unwrap_or("cherry-pick"),
        )?;

        for c in &outcome.conflicts {
            self.oplog.save_conflict(c)?;
        }

        let author = self.git.resolve_git_author();
        let cp_msg = apply_co_author_trailer(&source_commit.message, &cfg);
        let new_commit_id = self.git.write_commit(
            &outcome.merged_tree_id,
            std::slice::from_ref(&target_cid),
            &author,
            &cp_msg,
        )?;

        self.git
            .update_ref(&format!("refs/heads/{eff_branch}"), &new_commit_id)?;

        let branch_ws = self.branch_sandbox_dir(&eff_branch);
        if branch_ws.is_dir() {
            let _ = materialize_tree_to_dir(&self.git, &outcome.merged_tree_id, &branch_ws);
            let safe_b = Self::sanitize_branch_name(&eff_branch);
            let stamp = self.branches_root().join(format!("{safe_b}.tree"));
            let stamp_payload = format!("{new_commit_id}:{}", outcome.merged_tree_id);
            let _ = atomic_write_file(&stamp, stamp_payload.as_bytes());
        }

        if eff_branch == current_head_branch {
            materialize_tree_to_dir(&self.git, &outcome.merged_tree_id, self.git.repo_root())?;
            self.git.sync_git_index(&new_commit_id);
        }

        let saved_conflicts = outcome.conflicts.clone();
        let eff_branch_clone = eff_branch.clone();
        let new_cid_clone = new_commit_id;

        self.oplog.record_tx(
            session_id,
            format!("cherry-pick {short_src} onto {eff_branch}"),
            false,
            |view| {
                view.branches
                    .insert(eff_branch_clone.clone(), new_cid_clone);

                for c in &saved_conflicts {
                    view.conflicts.insert(c.path.clone(), c.id.clone());
                }

                Ok(())
            },
        )?;

        let _ = self.sync_all_agents();

        // Re-overlay any uncommitted own_blobs and deleted_paths of sessions on `current_head_branch` so live work stays intact on disk.
        let refreshed = self.oplog.load_current_reconciled()?;
        let head_ws = self.branch_sandbox_dir(&current_head_branch);
        let head_ws_opt = head_ws.is_dir().then_some(head_ws.as_path());
        let mut post_obs = BTreeMap::new();
        self.reapply_remaining_head_sessions(
            &refreshed.view.agents,
            &BTreeSet::new(),
            &current_head_branch,
            head_ws_opt,
            &mut post_obs,
        );

        Ok(CherryPickReport {
            source_commit: source_cid,
            target_branch: eff_branch,
            new_commit_id,
            conflicts: outcome.conflicts,
            touched_paths: touched_paths.into_iter().collect(),
        })
    }

    /// Walks upward from each path in `touched_paths` inside `tree_id` to identify affected
    /// workspace packages (`Cargo.toml`, `package.json`, `pyproject.toml`, or `go.mod`).
    fn discover_affected_packages(
        &self,
        tree_id: &ObjectId,
        touched_paths: &[String],
    ) -> CoreResult<Vec<String>> {
        let tree_files = self.git.read_tree_to_map(tree_id)?;
        let mut packages = BTreeSet::new();

        for path in touched_paths {
            let mut parts: Vec<&str> = path.split('/').collect();
            let mut matched = false;
            while !parts.is_empty() {
                parts.pop();
                let dir_prefix = parts.join("/");
                let has_manifest = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"]
                    .iter()
                    .any(|manifest| {
                        let candidate = if dir_prefix.is_empty() {
                            (*manifest).to_string()
                        } else {
                            format!("{dir_prefix}/{manifest}")
                        };
                        tree_files.contains_key(&candidate)
                    });
                if has_manifest {
                    let label = if dir_prefix.is_empty() {
                        "<root>".to_string()
                    } else {
                        dir_prefix
                    };
                    packages.insert(label);
                    matched = true;
                    break;
                }
            }

            if !matched {
                packages.insert("<root>".to_string());
            }
        }
        Ok(packages.into_iter().collect())
    }

    /// Resolves `session_id` (matching by `agent_id`, untruncated `waypoint.conversation_id`, or
    /// unique prefix) and brings its terminal tab, multiplexer pane, or IDE window to the foreground.
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if no active session matches `session_id`.
    pub fn focus_session(&self, session_id: &str) -> CoreResult<FocusSessionReport> {
        let query = session_id.trim();

        if query.is_empty() {
            return Err(CoreError::InvalidArgument(
                "session ID cannot be empty".to_string(),
            ));
        }

        let current = self.oplog.load_current_reconciled()?;

        if let Some(exact) = current.view.agents.get(query) {
            return focus_session_waypoint(exact);
        }

        if let Some(by_conv) = current.view.agents.values().find(|a| {
            a.waypoint
                .as_ref()
                .and_then(|w| w.conversation_id.as_deref())
                .is_some_and(|c| c == query)
        }) {
            return focus_session_waypoint(by_conv);
        }

        let mut prefix_matches: Vec<&AgentMetadata> = current
            .view
            .agents
            .values()
            .filter(|a| {
                a.agent_id.starts_with(query)
                    || a.agent_id.contains(query)
                    || a.waypoint
                        .as_ref()
                        .and_then(|w| w.conversation_id.as_deref())
                        .is_some_and(|c| c.starts_with(query))
            })
            .collect();

        if prefix_matches.len() == 1 {
            return focus_session_waypoint(prefix_matches[0]);
        }

        prefix_matches.sort_by_key(|b| std::cmp::Reverse(b.updated_at_ms));
        if let Some(most_recent) = prefix_matches.first() {
            return focus_session_waypoint(most_recent);
        }

        Err(CoreError::NotFound(format!("agent session '{query}'")))
    }
}

/// Validates and normalizes an explicit `--author` override (`"Name <email>"`).
fn validate_author_override(raw: &str) -> CoreResult<String> {
    let trimmed = raw.trim();

    if trimmed.is_empty() || trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(CoreError::InvalidArgument(
            "author must be non-empty and single-line ('Name <email>')".to_string(),
        ));
    }

    let Some((name_part, email_part)) = trimmed.rsplit_once('<') else {
        return Err(CoreError::InvalidArgument(format!(
            "author '{trimmed}' must use 'Name <email>' format"
        )));
    };

    if name_part.trim().is_empty() || !email_part.ends_with('>') || email_part.len() < 2 {
        return Err(CoreError::InvalidArgument(format!(
            "author '{trimmed}' must use 'Name <email>' format"
        )));
    }

    Ok(trimmed.to_string())
}

/// Appends the configured `Co-authored-by: <trailer>` line to `message` when `cfg.co_author` is `true`
/// and the trailer is not already present.
fn apply_co_author_trailer(message: &str, cfg: &core::RepoConfig) -> String {
    apply_co_author_trailer_with_options(message, cfg, &CommitOptions::default())
}

/// Appends `Co-authored-by: <trailer>` lines to `message` according to `options` (`--co-author` /
/// `--no-co-author`) and `cfg` (`co_author` / `co_author_trailer`), deduplicating existing lines.
fn apply_co_author_trailer_with_options(
    message: &str,
    cfg: &core::RepoConfig,
    options: &CommitOptions,
) -> String {
    let trimmed = message.trim_end();

    if options.co_author == Some(false) {
        return trimmed.to_string();
    }

    let mut candidate_idents: Vec<String> = Vec::new();

    if !options.co_author_trailers.is_empty() {
        for raw_trailer in &options.co_author_trailers {
            let t = raw_trailer.trim();

            let ident = if t.is_empty() {
                cfg.co_author_trailer.trim()
            } else {
                t.strip_prefix("Co-authored-by:")
                    .or_else(|| t.strip_prefix("co-authored-by:"))
                    .unwrap_or(t)
                    .trim()
            };

            if !ident.is_empty() && !ident.contains('\n') && !ident.contains('\r') {
                candidate_idents.push(ident.to_string());
            }
        }
    } else if options.co_author == Some(true) || cfg.co_author {
        let ident = cfg.co_author_trailer.trim();

        if !ident.is_empty() && !ident.contains('\n') && !ident.contains('\r') {
            candidate_idents.push(ident.to_string());
        }
    }

    if candidate_idents.is_empty() {
        return trimmed.to_string();
    }

    let mut result = trimmed.to_string();
    let mut appended_any = false;

    for ident in candidate_idents {
        let trailer_line = format!("Co-authored-by: {ident}");

        let already_present = result
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case(&trailer_line));

        if !already_present {
            if !appended_any {
                result.push_str("\n\n");
                appended_any = true;
            } else {
                result.push('\n');
            }
            result.push_str(&trailer_line);
        }
    }

    result
}

/// Formats a commit message from an explicit message override, active agent intents, or touched file paths.
fn format_merge_commit_message(
    agents: &[AgentMetadata],
    touched_paths: &[String],
    explicit_message: Option<&str>,
) -> String {
    if let Some(msg) = explicit_message {
        let trimmed = msg.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let mut meaningful_intents = Vec::new();
    for a in agents {
        let cleaned = a
            .intent
            .trim()
            .strip_prefix("Auto-inferred: ")
            .unwrap_or_else(|| a.intent.trim());
        if !cleaned.is_empty()
            && !cleaned.starts_with("Active session ")
            && !meaningful_intents.iter().any(|existing: &String| existing == cleaned)
        {
            meaningful_intents.push(cleaned.to_string());
        }
    }

    match meaningful_intents.len() {
        1 => meaningful_intents.remove(0),
        n if n > 1 => {
            let joined = meaningful_intents.join("; ");
            if joined.len() <= 72 {
                joined
            } else {
                let mut body = String::new();
                for item in &meaningful_intents {
                    body.push_str(&format!("\n- {item}"));
                }
                format!("{}\n{}", meaningful_intents[0], body)
            }
        }
        _ => {
            if touched_paths.is_empty() {
                "Update workspace".to_string()
            } else if touched_paths.len() == 1 {
                format!("Update {}", touched_paths[0])
            } else {
                let preview: Vec<&str> = touched_paths.iter().take(3).map(String::as_str).collect();
                format!("Update {}", preview.join(", "))
            }
        }
    }
}

/// Writes `bytes` to the working-tree file `path`, creating parent directories, and sets the
/// executable bit to match `executable` so `git status` never reports `100644`/`100755` drift.
///
/// Existing files are rewritten in place (preserving the inode for `kqueue`/`inotify` dev-server
/// watchers), falling back to an atomic rename when the in-place write fails. Errors are ignored:
/// callers re-snapshot the working tree afterwards and surface any divergence via `tes status`.
fn write_worktree_file(path: &Path, bytes: &[u8], executable: bool) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let _ = write_working_tree_file_safe(path, bytes);

    set_worktree_executable(path, executable);
}

/// Writes `bytes` to `rel_path` under `root` via [`write_worktree_file`] after confining the path
/// to `root` (rejecting traversal and symlinks). Invalid paths are ignored.
pub(crate) fn write_worktree_rel(root: &Path, rel_path: &str, bytes: &[u8], executable: bool) {
    if let Ok((_, full_path)) = resolve_confined_repo_path(root, rel_path) {
        write_worktree_file(&full_path, bytes, executable);
    }
}

/// Sets or clears the executable bits of `path` the way Git does on checkout (every readable
/// class gains or loses `x`), leaving the file untouched if it already matches.
fn set_worktree_executable(path: &Path, executable: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Ok(meta) = fs::metadata(path) {
            let mode = meta.permissions().mode();
            let new_mode = if executable {
                mode | ((mode & 0o444) >> 2)
            } else {
                mode & !0o111
            };

            if new_mode != mode {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(new_mode));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
}

/// Returns `true` if the working-tree file at `path` has any executable bit set.
fn worktree_is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Removes the working-tree file `rel_path` under `root` (if present) and prunes any parent
/// directories left empty, stopping at `root`. Paths escaping `root` are ignored.
fn remove_worktree_file(root: &Path, rel_path: &str) {
    let Ok((_, full_path)) = resolve_confined_repo_path(root, rel_path) else {
        return;
    };

    if !full_path.is_file() {
        return;
    }

    let _ = fs::remove_file(&full_path);
    let mut parent_opt = full_path.parent();

    while let Some(parent) = parent_opt {
        if parent == root || !parent.starts_with(root) || fs::remove_dir(parent).is_err() {
            break;
        }

        parent_opt = parent.parent();
    }
}

/// Returns `true` if `session_id` identifies an AI agent session rather than a human terminal
/// (`term-*`, `session-*`) or editor session.
fn is_ai_session_id(session_id: &str) -> bool {
    !session_id.starts_with("term-") && !session_id.starts_with("session-") && session_id != "editor"
}

/// Returns `true` if an unhooked working-tree change at `rel_path` may be attributed to `meta`'s
/// session by a snapshot (`tes status` / `tes commit`) or by the background watcher.
///
/// Attribution rules, in order:
/// 1. Paths the session authored (`own_blobs` / `deleted_paths`) always belong to it.
/// 2. Paths in the session's `foreign_baseline` (changes nobody owned when an AI session started)
///    never do, so a user's in-progress edits are not committed under an agent's name.
/// 3. Human terminal/editor sessions sweep every remaining unowned change, like `git add -A`.
/// 4. AI sessions with explicit claims only sweep paths they claimed, read, or created under a
///    claimed directory; AI sessions without any claim sweep every remaining unowned change.
fn session_may_sweep(
    meta: &AgentMetadata,
    rel_path: &str,
    base_files: &BTreeMap<String, FileEntry>,
) -> bool {
    if meta.owns_path(rel_path) {
        return true;
    }

    if meta.foreign_baseline.contains(rel_path) {
        return false;
    }

    if !is_ai_session_id(&meta.agent_id) || meta.claimed_symbols.is_empty() {
        return true;
    }

    session_claims_path(meta, rel_path, base_files)
}

/// Returns `true` if `rel_path` is an AI harness or IDE configuration file that should be
/// excluded from passive working-tree session ownership.
fn is_harness_config_path(rel_path: &str) -> bool {
    matches!(
        rel_path,
        "AGENTS.md"
            | "AGENTS.override.md"
            | "CLAUDE.md"
            | ".mcp.json"
            | ".windsurfrules"
            | "opencode.json"
    ) || rel_path.starts_with(".agents/")
        || rel_path.starts_with(".claude/")
        || rel_path.starts_with(".cursor/")
        || rel_path.starts_with(".windsurf/")
        || rel_path.starts_with(".kiro/")
        || rel_path.starts_with(".opencode/")
        || rel_path.starts_with(".codex/")
        || rel_path.starts_with(".vscode/")
}

/// Returns `true` if `agent` owns, read, or claimed `rel_path` (including directory prefix claims
/// or newly created sibling files inside a brand-new top-level directory).
fn session_claims_path(
    agent: &AgentMetadata,
    rel_path: &str,
    base_files: &BTreeMap<String, FileEntry>,
) -> bool {
    if agent.owns_path(rel_path) || agent.read_blobs.contains_key(rel_path) {
        return true;
    }

    for s in &agent.claimed_symbols {
        let target = s
            .split("::")
            .next()
            .unwrap_or(s.as_str())
            .trim()
            .trim_end_matches('/');
        if target == rel_path || (!target.is_empty() && rel_path.starts_with(&format!("{target}/")))
        {
            return true;
        }
    }

    if !base_files.contains_key(rel_path) {
        if let Some((top_dir, _)) = rel_path.split_once('/') {
            let dir_prefix = format!("{top_dir}/");
            let existed_in_base = base_files.keys().any(|k| k.starts_with(&dir_prefix));
            if !existed_in_base {
                let claimed_same_new_top_dir = agent.claimed_symbols.iter().any(|s| {
                    let target = s
                        .split("::")
                        .next()
                        .unwrap_or(s.as_str())
                        .trim()
                        .trim_end_matches('/');
                    target == top_dir || target.starts_with(&dir_prefix)
                }) || agent
                    .own_blobs
                    .keys()
                    .any(|k| k == top_dir || k.starts_with(&dir_prefix));
                if claimed_same_new_top_dir {
                    return true;
                }
            }
        }
    }

    false
}

/// Returns `true` if `text` contains standard Git conflict markers (`<<<<<<< `, `=======`, `>>>>>>> `).
pub(crate) fn has_git_conflict_markers(text: &str) -> bool {
    let mut has_open = false;
    let mut has_sep = false;
    for line in text.lines() {
        if line.starts_with("<<<<<<< ") {
            has_open = true;
        } else if has_open && line == "=======" {
            has_sep = true;
        } else if has_open && has_sep && line.starts_with(">>>>>>> ") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_delete_and_rename_clean_up_refs_sandboxes_and_sessions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("app.rs"), "pub fn main() {}\n").expect("write app.rs");

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        coord
            .claim_intent_and_symbols_with_branch(
                "agent-1",
                Some("Add feature A"),
                &["app.rs".to_string()],
                Some("feat/old-name"),
            )
            .expect("claim feat/old-name");

        fs::write(
            root.join("app.rs"),
            "pub fn main() {}\npub fn feat_a() -> i32 { 42 }\n",
        )
        .expect("write feat_a");
        handle_post_tool_hook(&coord, "agent-1", "app.rs").expect("post-tool feat_a");
        coord
            .merge_agents_with_message(
                &["agent-1".to_string()],
                Some("feat/old-name"),
                None,
                true,
                Some("feat: add feat_a"),
            )
            .expect("commit feat/old-name");

        // Rename `feat/old-name` -> `feat/new-name`.
        let rename_rep = coord
            .rename_branch("agent-1", "feat/old-name", "feat/new-name")
            .expect("rename_branch");
        assert_eq!(rename_rep.old_branch, "feat/old-name");
        assert_eq!(rename_rep.new_branch, "feat/new-name");
        assert!(
            coord
                .git()
                .resolve_ref("refs/heads/feat/old-name")
                .expect("resolve old")
                .is_none()
        );
        assert!(
            coord
                .git()
                .resolve_ref("refs/heads/feat/new-name")
                .expect("resolve new")
                .is_some()
        );

        let cur = coord.oplog().load_current_reconciled().expect("load current");
        assert!(!cur.view.branches.contains_key("feat/old-name"));
        assert!(cur.view.branches.contains_key("feat/new-name"));
        assert_eq!(
            cur.view
                .agents
                .get("agent-1")
                .and_then(|a| a.target_branch.as_deref()),
            Some("feat/new-name")
        );

        // Safe delete (`force = false`) should reject `feat/new-name` because it has unmerged commits ahead of `main`.
        let safe_err = coord.delete_branch("agent-1", "feat/new-name", false);
        assert!(
            safe_err.is_err(),
            "unmerged branch must be rejected when force = false"
        );

        // Force delete (`force = true`) must succeed, remove ref & sandbox, and detach `agent-1` back to trunk.
        let del_rep = coord
            .delete_branch("agent-1", "feat/new-name", true)
            .expect("force delete_branch");
        assert_eq!(del_rep.deleted_branch, "feat/new-name");
        assert!(del_rep.detached_sessions.contains(&"agent-1".to_string()));
        assert!(
            coord
                .git()
                .resolve_ref("refs/heads/feat/new-name")
                .expect("resolve deleted")
                .is_none()
        );
        assert!(!coord.branch_sandbox_dir("feat/new-name").exists());

        let cur_after = coord.oplog().load_current_reconciled().expect("load after");
        assert!(!cur_after.view.branches.contains_key("feat/new-name"));
        assert_eq!(
            cur_after
                .view
                .agents
                .get("agent-1")
                .and_then(|a| a.target_branch.as_deref()),
            None
        );
    }

    #[test]
    fn session_isolated_stash_preserves_concurrent_peer_edits_and_ast_merges_on_pop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let initial_code = "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n";
        fs::write(root.join("lib.rs"), initial_code).expect("write lib.rs");

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        coord
            .claim_intent_and_symbols("agent-alpha", Some("Update alpha"), &["lib.rs".to_string()])
            .expect("claim alpha");
        coord
            .claim_intent_and_symbols("agent-beta", Some("Update beta"), &["lib.rs".to_string()])
            .expect("claim beta");

        // Agent alpha modifies `alpha()`.
        let after_alpha = "pub fn alpha() -> i32 {\n    100\n}\n\npub fn beta() -> i32 {\n    2\n}\n";
        fs::write(root.join("lib.rs"), after_alpha).expect("write alpha edit");
        handle_post_tool_hook(&coord, "agent-alpha", "lib.rs").expect("post-tool alpha");

        // Agent beta modifies `beta()` on top of the reconciled file.
        let after_both = "pub fn alpha() -> i32 {\n    100\n}\n\npub fn beta() -> i32 {\n    200\n}\n";
        fs::write(root.join("lib.rs"), after_both).expect("write beta edit");
        handle_post_tool_hook(&coord, "agent-beta", "lib.rs").expect("post-tool beta");

        // Now `agent-alpha` stashes its changes!
        let stash_entry = coord
            .stash_push("agent-alpha", Some("WIP alpha"), None)
            .expect("stash_push alpha");
        assert_eq!(stash_entry.session_id, "agent-alpha");
        assert!(stash_entry.own_blobs.contains_key("lib.rs"));

        // Verify `lib.rs` on disk reverted `alpha()` to `1` while preserving `agent-beta`'s `200`!
        let disk_after_stash = fs::read_to_string(root.join("lib.rs")).expect("read lib.rs");
        assert!(
            disk_after_stash.contains("1\n") && !disk_after_stash.contains("100"),
            "alpha's stashed change should be removed from disk: {disk_after_stash}"
        );
        assert!(
            disk_after_stash.contains("200"),
            "beta's concurrent change must remain intact on disk: {disk_after_stash}"
        );

        let stashes = coord.stash_list().expect("stash_list");
        assert_eq!(stashes.len(), 1);

        // Pop `stash@{0}` back into `agent-alpha` and verify 3-way AST merge restores `100` alongside `200`.
        let pop_rep = coord
            .stash_pop("agent-alpha", 0, false)
            .expect("stash_pop alpha");
        assert!(!pop_rep.has_conflicts);
        assert!(pop_rep.dropped);
        assert!(coord.stash_list().expect("stash_list after pop").is_empty());

        let disk_after_pop = fs::read_to_string(root.join("lib.rs")).expect("read lib.rs after pop");
        assert!(
            disk_after_pop.contains("100") && disk_after_pop.contains("200"),
            "both alpha (100) and beta (200) must be present after stash pop: {disk_after_pop}"
        );

        let log_entries = coord.commit_log(None, 5).expect("commit_log");
        assert!(!log_entries.is_empty());
    }

    #[test]
    fn path_scoped_and_unclaimed_stash_plus_ast_cherry_pick() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("a.rs"), "pub fn a() -> i32 { 1 }\n").expect("write a.rs");
        fs::write(root.join("b.rs"), "pub fn b() -> i32 { 2 }\n").expect("write b.rs");

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        // Unclaimed working-tree edit to both `a.rs` and `b.rs` by a passive terminal session.
        fs::write(root.join("a.rs"), "pub fn a() -> i32 { 10 }\n").expect("edit a.rs");
        fs::write(root.join("b.rs"), "pub fn b() -> i32 { 20 }\n").expect("edit b.rs");

        let stash_a = coord
            .stash_push_with_paths("term-999999", Some("stash only a"), None, &["a.rs".to_string()])
            .expect("stash_push_with_paths a.rs");
        assert_eq!(stash_a.own_blobs.len(), 1);
        assert!(stash_a.own_blobs.contains_key("a.rs"));
        assert_eq!(
            fs::read_to_string(root.join("a.rs")).expect("read a.rs"),
            "pub fn a() -> i32 { 1 }\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("b.rs")).expect("read b.rs"),
            "pub fn b() -> i32 { 20 }\n"
        );

        // Stash remaining unclaimed edit on `b.rs`.
        let stash_b = coord
            .stash_push("term-999999", Some("stash b"), None)
            .expect("stash_push b.rs");
        assert_eq!(stash_b.own_blobs.len(), 1);
        assert!(stash_b.own_blobs.contains_key("b.rs"));
        assert_eq!(
            fs::read_to_string(root.join("b.rs")).expect("read b.rs"),
            "pub fn b() -> i32 { 2 }\n"
        );

        // Create a commit on `feat/cherry` that adds a new function to `a.rs`, then cherry-pick it onto `main`.
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-cp",
                Some("Add helper to a.rs"),
                &["a.rs".to_string()],
                Some("feat/cherry"),
            )
            .expect("claim feat/cherry");
        fs::write(
            root.join("a.rs"),
            "pub fn a() -> i32 { 1 }\npub fn helper() -> i32 { 99 }\n",
        )
        .expect("write helper");
        handle_post_tool_hook(&coord, "agent-cp", "a.rs").expect("post-tool helper");
        let merge_rep = coord
            .merge_agents_with_message(
                &["agent-cp".to_string()],
                Some("feat/cherry"),
                None,
                true,
                Some("feat: add helper to a.rs"),
            )
            .expect("commit feat/cherry");

        let cp_rep = coord
            .cherry_pick("editor", merge_rep.merged_commit_id.as_str(), Some("main"))
            .expect("cherry_pick onto main");
        assert_eq!(cp_rep.target_branch, "main");
        assert!(cp_rep.conflicts.is_empty());
        assert_eq!(cp_rep.touched_paths, vec!["a.rs".to_string()]);
        let main_a = fs::read_to_string(root.join("a.rs")).expect("read a.rs on main");
        assert!(
            main_a.contains("pub fn helper() -> i32 { 99 }"),
            "cherry-picked helper must be materialized on main: {main_a}"
        );
    }

    #[test]
    fn commit_options_author_and_co_author_flags_override_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").expect("write lib.rs");

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        coord
            .claim_intent_and_symbols("agent-1", Some("Update v"), &["lib.rs".to_string()])
            .expect("claim");
        fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 2 }\n").expect("edit 1");
        handle_post_tool_hook(&coord, "agent-1", "lib.rs").expect("post-tool 1");

        // Invalid author format must fail fast.
        let bad_opts = CommitOptions {
            message: Some("feat: invalid author".to_string()),
            author: Some("NotAnEmail".to_string()),
            co_author: None,
            co_author_trailers: Vec::new(),
            amend: false,
        };
        assert!(
            coord
                .merge_agents_with_options(&["agent-1".to_string()], None, None, true, &bad_opts)
                .is_err()
        );

        // Explicit `--author` and `--co-author` trailer when `cfg.co_author` is `false`.
        let opts_with_trailer = CommitOptions {
            message: Some("feat: custom author and co-author".to_string()),
            author: Some("Alice Dev <alice@example.com>".to_string()),
            co_author: Some(true),
            co_author_trailers: vec![
                String::new(),
                "Bob Reviewer <bob@example.com>".to_string(),
            ],
            amend: false,
        };
        let rep1 = coord
            .merge_agents_with_options(
                &["agent-1".to_string()],
                None,
                None,
                true,
                &opts_with_trailer,
            )
            .expect("commit with trailer");
        let c1 = coord
            .git()
            .read_commit(&rep1.merged_commit_id)
            .expect("read c1");
        assert!(
            c1.author.starts_with("Alice Dev <alice@example.com> "),
            "author should be overridden: {}",
            c1.author
        );
        assert!(
            c1.message
                .contains("Co-authored-by: Tessera <noreply@tessera.getlunaris.dev>"),
            "default trailer should be included: {}",
            c1.message
        );
        assert!(
            c1.message
                .contains("Co-authored-by: Bob Reviewer <bob@example.com>"),
            "custom trailer should be included: {}",
            c1.message
        );

        // Enable `co_author = true` in repo config, then verify `--no-co-author` suppresses the trailer.
        let mut cfg = coord.oplog().load_config().expect("load_config");
        cfg.co_author = true;
        coord.oplog().save_config(&cfg).expect("save_config");

        fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 3 }\n").expect("edit 2");
        handle_post_tool_hook(&coord, "agent-1", "lib.rs").expect("post-tool 2");

        let opts_no_co_author = CommitOptions {
            message: Some("feat: suppress co-author".to_string()),
            author: None,
            co_author: Some(false),
            co_author_trailers: Vec::new(),
            amend: false,
        };
        let rep2 = coord
            .merge_agents_with_options(
                &["agent-1".to_string()],
                None,
                None,
                false,
                &opts_no_co_author,
            )
            .expect("commit with --no-co-author");
        let c2 = coord
            .git()
            .read_commit(&rep2.merged_commit_id)
            .expect("read c2");
        assert!(
            !c2.message.contains("Co-authored-by:"),
            "--no-co-author must suppress trailer even when cfg.co_author is true: {}",
            c2.message
        );

        // Amending the tip commit preserves its parent list and replaces the branch tip.
        let opts_amend = CommitOptions {
            message: Some("feat: amended commit message".to_string()),
            author: None,
            co_author: Some(false),
            co_author_trailers: Vec::new(),
            amend: true,
        };
        let rep_amend = coord
            .merge_agents_with_options(&["agent-1".to_string()], None, None, false, &opts_amend)
            .expect("commit --amend");
        let c_amend = coord
            .git()
            .read_commit(&rep_amend.merged_commit_id)
            .expect("read amended commit");
        assert_eq!(c_amend.parents, c2.parents);
        assert_eq!(c_amend.message.trim(), "feat: amended commit message");
    }

    #[test]
    fn session_waypoint_captured_and_focusable_by_id_or_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("app.rs"), "pub fn run() {}\n").expect("write app.rs");

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        let meta = coord
            .ensure_session("antigravity-99887766-aa", Some("Implement navigation"))
            .expect("ensure_session");
        assert!(
            meta.waypoint.is_some(),
            "ensure_session must record a SessionWaypoint for non-editor sessions"
        );
        let wp = meta.waypoint.as_ref().expect("waypoint");
        assert_eq!(wp.harness, "antigravity");

        let focus_exact = coord
            .focus_session("antigravity-99887766-aa")
            .expect("focus exact");
        assert_eq!(focus_exact.session_id, "antigravity-99887766-aa");
        assert_eq!(focus_exact.harness, "antigravity");

        let focus_prefix = coord.focus_session("99887766").expect("focus substring");
        assert_eq!(focus_prefix.session_id, "antigravity-99887766-aa");
    }

    #[test]
    fn codex_harness_integration_and_waypoint_support() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("app.rs"), "pub fn run() {}\n").expect("write app.rs");

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");
        let installed =
            install_harness_integrations_for(root, coord.git().git_dir(), Some("codex"))
                .expect("install codex harness");

        assert!(installed.contains(&".codex/config.toml".to_string()));
        assert!(installed.contains(&".codex/hooks.json".to_string()));
        assert!(installed.contains(&"AGENTS.md".to_string()));

        let config_toml =
            fs::read_to_string(root.join(".codex/config.toml")).expect("read .codex/config.toml");
        assert!(config_toml.contains("[mcp_servers.tessera]"));
        assert!(config_toml.contains("command = \"tes\""));
        assert!(config_toml.contains("args = [\"mcp\"]"));

        let hooks_json =
            fs::read_to_string(root.join(".codex/hooks.json")).expect("read .codex/hooks.json");
        assert!(hooks_json.contains("apply_patch"));
        assert!(hooks_json.contains("tes hook post-tool"));

        assert!(is_harness_config_path(".codex/config.toml"));
        assert!(is_harness_config_path(".codex/hooks.json"));
        assert!(is_harness_config_path("AGENTS.override.md"));

        let meta = coord
            .ensure_session("codex-01972a3b-4c", Some("Refactor module"))
            .expect("ensure codex session");
        assert_eq!(
            meta.waypoint.as_ref().map(|w| w.harness.as_str()),
            Some("codex")
        );
    }

    #[test]
    fn restore_file_clears_claimed_symbols_and_resets_session_trees() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("README.md"), "# Title\n\nLine 1\n\nLine 2\n").expect("write README");

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        // Both sessions must share `main` to exercise same-branch restore; otherwise agent-b's
        // claim on the file agent-a already edited would auto-isolate it onto its own branch.
        let mut cfg = coord.oplog().load_config().expect("load_config");
        cfg.auto_isolate_dirty_claims = false;
        coord.oplog().save_config(&cfg).expect("save_config");

        coord
            .claim_intent_and_symbols(
                "agent-a",
                Some("Edit line 1"),
                &["README.md".to_string()],
            )
            .expect("claim a");
        fs::write(root.join("README.md"), "# Title\n\nLine 1 edited\n\nLine 2\n")
            .expect("edit a");
        handle_post_tool_hook(&coord, "agent-a", "README.md").expect("post-tool a");

        coord
            .claim_intent_and_symbols(
                "agent-b",
                Some("Edit line 2"),
                &["README.md".to_string()],
            )
            .expect("claim b");
        fs::write(
            root.join("README.md"),
            "# Title\n\nLine 1 edited\n\nLine 2 edited\n",
        )
        .expect("edit b");
        handle_post_tool_hook(&coord, "agent-b", "README.md").expect("post-tool b");

        let rep = coord
            .restore("agent-a", None, None, &["README.md".to_string()])
            .expect("restore README.md");
        assert_eq!(rep.restored_paths, vec!["README.md".to_string()]);

        let radar = coord.radar().expect("radar after restore");
        assert!(
            radar.collisions.is_empty(),
            "restoring README.md must clear collisions: {:?}",
            radar.collisions
        );
        assert!(
            radar.file_attributions.is_empty(),
            "restoring README.md must clear file_attributions: {:?}",
            radar.file_attributions
        );

        for agent in &radar.active_agents {
            assert!(
                agent.own_blobs.is_empty() && agent.claimed_symbols.is_empty(),
                "agent {} must have empty own_blobs and claimed_symbols after restore",
                agent.agent_id
            );
            assert_eq!(
                agent.head_commit, agent.base_commit,
                "agent {} head_commit must equal base_commit after restore",
                agent.agent_id
            );
        }
    }

    #[test]
    fn multi_file_session_rebase_preserves_interleaved_peer_commits_and_flags_conflict_markers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        fs::write(
            root.join("config.ts"),
            "export function renderDoc(): string {\n  return 'base';\n}\n\nexport function formatTitle(): string {\n  return 'title-v1';\n}\n",
        )
        .expect("write config.ts");
        fs::write(
            root.join("cli.md"),
            "# CLI Reference\n\n## Flags\n- `--json`\n\n## Notes\nStandard notes.\n",
        )
        .expect("write cli.md");
        fs::write(
            root.join("style.md"),
            "# Style Guide\n\nKeep formatting clean.\n",
        )
        .expect("write style.md");

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        coord
            .claim_intent_and_symbols(
                "agent-style",
                Some("Update style across docs"),
                &[
                    "config.ts".to_string(),
                    "cli.md".to_string(),
                    "style.md".to_string(),
                ],
            )
            .expect("claim agent-style");

        coord
            .claim_intent_and_symbols(
                "agent-feat",
                Some("Add EditorPreview replacement and pager flags"),
                &["config.ts".to_string(), "cli.md".to_string()],
            )
            .expect("claim agent-feat");

        // `agent-style` edits `config.ts` (`formatTitle`) and `style.md` BEFORE `agent-feat` commits.
        fs::write(
            root.join("config.ts"),
            "export function renderDoc(): string {\n  return 'base';\n}\n\nexport function formatTitle(): string {\n  return 'title-v2-clean';\n}\n",
        )
        .expect("style edit config.ts");
        handle_post_tool_hook(&coord, "agent-style", "config.ts").expect("post-tool style config.ts");

        fs::write(
            root.join("style.md"),
            "# Style Guide\n\nKeep formatting clean without dashes.\n",
        )
        .expect("style edit style.md");
        handle_post_tool_hook(&coord, "agent-style", "style.md").expect("post-tool style style.md");

        // Now `agent-feat` edits `config.ts` (`renderDoc`) and `cli.md` (`Flags`) and commits to `main`!
        fs::write(
            root.join("config.ts"),
            "export function renderDoc(): string {\n  return 'replaced-editor-preview';\n}\n\nexport function formatTitle(): string {\n  return 'title-v2-clean';\n}\n",
        )
        .expect("feat edit config.ts");
        handle_post_tool_hook(&coord, "agent-feat", "config.ts").expect("post-tool feat config.ts");

        fs::write(
            root.join("cli.md"),
            "# CLI Reference\n\n## Flags\n- `--json`\n- `-P, --no-pager`\n\n## Notes\nStandard notes.\n",
        )
        .expect("feat edit cli.md");
        handle_post_tool_hook(&coord, "agent-feat", "cli.md").expect("post-tool feat cli.md");

        let feat_commit = coord
            .merge_agents_with_message(
                &["agent-feat".to_string()],
                Some("main"),
                None,
                false,
                Some("feat: replace EditorPreview and add --no-pager"),
            )
            .expect("commit agent-feat");
        assert!(feat_commit.conflicts.is_empty());

        // Next, `agent-style` edits ONLY `cli.md` (`Notes` section) and runs `post-tool`, then commits!
        fs::write(
            root.join("cli.md"),
            "# CLI Reference\n\n## Flags\n- `--json`\n- `-P, --no-pager`\n\n## Notes\nUpdated notes by style session.\n",
        )
        .expect("style edit cli.md");
        handle_post_tool_hook(&coord, "agent-style", "cli.md").expect("post-tool style cli.md");

        let style_commit = coord
            .merge_agents_with_message(
                &["agent-style".to_string()],
                Some("main"),
                None,
                false,
                Some("style: update notes and title"),
            )
            .expect("commit agent-style");
        assert!(style_commit.conflicts.is_empty());

        // Verify `config.ts` on `main` retained `agent-feat`'s `replaced-editor-preview` AND `agent-style`'s `title-v2-clean`!
        let final_config = fs::read_to_string(root.join("config.ts")).expect("read final config.ts");
        assert!(
            final_config.contains("replaced-editor-preview"),
            "agent-style commit must NOT revert agent-feat's committed change in config.ts:\n{final_config}"
        );
        assert!(
            final_config.contains("title-v2-clean"),
            "agent-style commit must include its own change in config.ts:\n{final_config}"
        );

        let final_cli = fs::read_to_string(root.join("cli.md")).expect("read final cli.md");
        assert!(
            final_cli.contains("- `-P, --no-pager`")
                && final_cli.contains("Updated notes by style session."),
            "cli.md must preserve both sessions' changes:\n{final_cli}"
        );
    }
}
