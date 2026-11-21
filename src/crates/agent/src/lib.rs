//! Multi-agent coordinator, Zero-Command Harness Auto-Pilot, Symbol Collision Radar,
//! Speculative Verification Gate, and non-blocking auto-rebase engine for Tessera.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ::merge::{MigrationResequence, RenameMatch};
use core::{AgentMetadata, ConflictRecord, GitBackend, ObjectId, OpId, OpLogStore, StashEntry};
use serde::{Deserialize, Serialize};

pub mod autopilot;
mod harness;
mod hooks;
mod merge;
mod ops;
mod sandbox;
mod session;
mod watcher;
mod worktree;

#[cfg(test)]
mod tests;

pub use autopilot::{
    FocusSessionReport, PostToolOutcome, PreToolOutcome, detect_session_id,
    detect_session_waypoint, focus_session_waypoint, handle_post_tool_hook, handle_pre_tool_hook,
    install_harness_integrations, install_harness_integrations_for, run_auto_watch_daemon,
};
pub use core::SessionWaypoint;

/// Per-commit authoring, amend, dry-run, and co-author trailer overrides for `tes commit` and `tes merge`.
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
    /// When `true`, computes the full 3-way/octopus merge and optional verification gate in a
    /// temporary sandbox without mutating Git refs, working trees, branch sandboxes, or the `OpLog`.
    #[serde(default)]
    pub dry_run: bool,
}

/// Rebased uncommitted state of a still-active session after a commit advanced its branch tip.
pub(crate) struct RebasedSession {
    /// Edited blobs re-anchored onto the new branch tip.
    pub(crate) own_blobs: BTreeMap<String, ObjectId>,
    /// Deletion tombstones still applicable on the new branch tip.
    pub(crate) deleted_paths: BTreeSet<String>,
    /// New session snapshot commit (parented on the new branch tip).
    pub(crate) head_commit: ObjectId,
    /// New session snapshot tree.
    pub(crate) head_tree: ObjectId,
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
    /// Number of unchanged directory subtrees skipped in O(1).
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
    /// `false` if nothing changed or `dry_run` was requested, so `merged_commit_id` is the
    /// existing branch tip and no commit was written.
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
    /// First-class [`ConflictRecord`]s recorded if the 3-way pull merge encountered conflicts.
    #[serde(default)]
    pub conflicts: Vec<ConflictRecord>,
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
    pub(crate) git: GitBackend,
    pub(crate) oplog: OpLogStore,
    pub(crate) invocation_dir: Option<PathBuf>,
}

impl Coordinator {
    /// Returns a reference to the underlying [`GitBackend`].
    pub fn git(&self) -> &GitBackend {
        &self.git
    }

    /// Returns a reference to the underlying [`OpLogStore`].
    pub fn oplog(&self) -> &OpLogStore {
        &self.oplog
    }
}
