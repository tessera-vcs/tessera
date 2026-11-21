//! Repository history, rollback, restore, branch deletion/rename, session-isolated stash,
//! AST-merging pull, commit log, and cherry-pick operations.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use core::{
    ConflictKind, ConflictRecord, CoreError, CoreResult, EntryMode, FileEntry, GitBackend, Merge,
    ObjectId, Operation, StashEntry, atomic_write_file, now_millis, resolve_confined_repo_path,
    validate_ref_name,
};
use merge::{merge_file_3way, merge_trees_3way};
use workspace::{
    materialize_tree_to_dir, materialize_tree_to_dir_with_baseline, snapshot_working_dir,
};

use crate::merge::apply_co_author_trailer;
use crate::worktree::{
    acquire_file_lock, is_binary_bytes, is_harness_config_path, remove_worktree_file,
    same_canonical_path, write_worktree_file, write_worktree_file_checked, write_worktree_rel,
};
use crate::{
    CherryPickReport, Coordinator, DeleteBranchReport, GcReport, LogEntry, PullReport,
    RenameBranchReport, RestoreReport, StashPopReport,
};

/// Process-local monotonic counter guaranteeing unique stash IDs even within the same millisecond.
static STASH_ID_SEQ: AtomicU64 = AtomicU64::new(1);

/// Returns `true` if `op` is a passive/bookkeeping ephemeral operation that bare `tes undo`
/// should skip over to locate the user's or agent's last meaningful action.
fn is_skippable_undo_op(op: &Operation) -> bool {
    op.is_ephemeral
        && (op.description.starts_with("ensure session ")
            || op.description.starts_with("observe intermediate blob")
            || op.description.starts_with("pre-tool read baseline")
            || op.agent_id == "reconciler")
}

impl Coordinator {
    /// Restores the repository `View`, Git branch references, `.git/index`, and working tree
    /// files on disk to a target operation or named checkpoint (or the parent of the current
    /// meaningful operation if `target_op_or_checkpoint` is `None`).
    ///
    /// Skips passive ephemeral bookkeeping operations on bare `tes undo`, follows GC-compacted
    /// parent forwarding pointers via [`core::OpLogStore::resolve_parent_ids`], writes the undo
    /// transaction under `oplog.lock` via `record_tx`, and preserves innocent concurrent peer
    /// sessions' uncommitted edits.
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
            while is_skippable_undo_op(&effective_current) {
                let resolved_parents = self.oplog.resolve_parent_ids(&effective_current.parents);
                let Some(parent_id) = resolved_parents.first() else {
                    break;
                };
                let Ok(parent_op) = self.oplog.read_operation(parent_id) else {
                    break;
                };

                if self.oplog.resolve_parent_ids(&parent_op.parents).is_empty() {
                    break;
                }

                effective_current = parent_op;
            }

            let resolved_parents = self.oplog.resolve_parent_ids(&effective_current.parents);
            let parent_id = resolved_parents.first().ok_or_else(|| {
                CoreError::InvalidArgument("cannot undo root initialization operation".to_string())
            })?;
            self.oplog.read_operation(parent_id)?
        };

        let mut restored_view = target_op.view.clone();

        let cfg = self.oplog.load_config()?;
        let prev_head_map = self
            .git
            .resolve_head_commit()
            .ok()
            .flatten()
            .and_then(|cid| self.git.read_commit(&cid).ok())
            .and_then(|c| self.git.read_tree_to_map(&c.tree_id).ok());

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
                materialize_tree_to_dir_with_baseline(
                    &self.git,
                    &commit_obj.tree_id,
                    self.git.repo_root(),
                    prev_head_map.as_ref(),
                )?;
                self.git.sync_git_index(&trunk_commit_id);
                restored_trunk_tree = Some((trunk_commit_id, commit_obj.tree_id));
            }
        }

        let mut preserved_peer_ids = BTreeSet::new();

        if effective_current.agent_id == "merge" && target_op_or_checkpoint.is_none() {
            if let Some((trunk_cid, trunk_tid)) = &restored_trunk_tree {
                restored_view.observed_blobs.clear();

                for (aid, agent) in restored_view.agents.iter_mut() {
                    let eff_b = agent.effective_branch(current_head_branch.as_str());

                    if agent.workspace_path.is_none() && eff_b == current_head_branch {
                        // Sessions that STILL have uncommitted changes after the merge were
                        // innocent concurrent peers not merged by `effective_current`. Never wipe
                        // their `own_blobs` or `deleted_paths`!
                        let was_unmerged_peer = effective_current
                            .view
                            .agents
                            .get(aid)
                            .is_some_and(|after| after.has_own_changes());

                        if was_unmerged_peer {
                            preserved_peer_ids.insert(aid.clone());
                            continue;
                        }

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
                    let _ = materialize_tree_to_dir_with_baseline(
                        &self.git,
                        &commit_obj.tree_id,
                        self.git.repo_root(),
                        prev_head_map.as_ref(),
                    );
                }
            }
        }

        if !preserved_peer_ids.is_empty() {
            let skip: BTreeSet<String> = restored_view
                .agents
                .keys()
                .filter(|k| !preserved_peer_ids.contains(*k))
                .cloned()
                .collect();
            let head_ws = self.branch_sandbox_dir(&current_head_branch);
            let head_ws_opt = head_ws.is_dir().then_some(head_ws.as_path());
            let mut post_obs = BTreeMap::new();
            self.reapply_remaining_head_sessions(
                &restored_view.agents,
                &skip,
                &current_head_branch,
                head_ws_opt,
                &mut post_obs,
            );

            for (obs_path, obs_bid) in post_obs {
                let list = restored_view.observed_blobs.entry(obs_path).or_default();
                list.retain(|b| b != &obs_bid);
                list.push(obs_bid);
            }
        }

        let undo_op = self.oplog.record_tx(
            "undo",
            format!("undo to operation {}", target_op.id.short()),
            false,
            |view| {
                *view = restored_view.clone();
                Ok(())
            },
        )?;
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
        let op_id_opt =
            checkpoint_op_id.or_else(|| self.oplog.resolve_op_prefix(rev_or_checkpoint).ok());

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
    /// Git commit, ref (`HEAD~1`, branch, tag), or named Tessera checkpoint (`source_rev`).
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

        let source_commit_id = if let Some(rev) =
            source_rev.map(str::trim).filter(|s| !s.is_empty())
        {
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
            self.git
                .update_ref(&format!("refs/heads/{target_branch}"), &source_commit_id)?;

            if branch_ws.is_dir() {
                materialize_tree_to_dir_with_baseline(
                    &self.git,
                    &source_commit.tree_id,
                    &branch_ws,
                    Some(&branch_head_map),
                )?;
                let safe_b = Self::sanitize_branch_name(&target_branch);
                let stamp = self.branches_root().join(format!("{safe_b}.tree"));
                let stamp_payload = format!("{source_commit_id}:{}", source_commit.tree_id);
                let _ = atomic_write_file(&stamp, stamp_payload.as_bytes());
            }

            if target_branch == current_head_branch {
                materialize_tree_to_dir_with_baseline(
                    &self.git,
                    &source_commit.tree_id,
                    self.git.repo_root(),
                    Some(&branch_head_map),
                )?;
                self.git.sync_git_index(&source_commit_id);
            }

            self.oplog.record_tx(
                session_id,
                format!(
                    "restore branch {target_branch} to {}",
                    source_commit_id.short()
                ),
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

        let mut restored_paths = BTreeSet::new();
        let mut restored_blobs = BTreeMap::new();
        let mut removed_paths = BTreeSet::new();

        for raw_p in paths {
            let trimmed_raw = raw_p
                .trim()
                .replace('\\', "/")
                .trim_start_matches("./")
                .trim_matches('/')
                .to_string();

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
            if entry.is_symlink() || entry.is_gitlink() {
                continue;
            }

            let bytes = self.git.read_blob(&entry.blob_id)?;

            if branch_ws.is_dir() {
                write_worktree_rel(&branch_ws, rel_path, &bytes, entry.executable);
            }

            if target_branch == current_head_branch {
                let (_, root_target) = resolve_confined_repo_path(self.git.repo_root(), rel_path)?;
                write_worktree_file_checked(&root_target, &bytes, entry.executable)?;
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
                let matches_branch_head =
                    is_head_restore || restored_blobs.get(p) == branch_head_map.get(p);

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

            restored_changes.insert(
                aid.clone(),
                (remaining_own.clone(), remaining_deleted.clone()),
            );

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
                    let mode = restored_blobs
                        .get(p)
                        .or_else(|| tree_map.get(p))
                        .map_or(EntryMode::NormalFile, FileEntry::entry_mode);
                    tree_map.insert(p.clone(), FileEntry::from_mode(*bid, mode));
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
                        let matches_branch_head =
                            is_head_restore || restored_blobs.get(p) == branch_head_map.get(p);

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
                            let matches_branch_head =
                                is_head_restore || restored_blobs.get(p) == branch_head_map.get(p);

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
    /// directory (`branches/<branch>`), port lease files, removes it from the `OpLog` `View`,
    /// and detaches any sessions targeting `branch` back to `trunk_branch`.
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
            let has_uncommitted =
                current.view.agents.values().any(|a| {
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
        self.cleanup_branch_leases(clean_branch, &sandbox_dir);

        if sandbox_dir.exists() {
            let _ = fs::remove_dir_all(&sandbox_dir);
        }

        let stamp_file = self.branches_root().join(format!("{safe_b}.tree"));

        if stamp_file.exists() {
            let _ = fs::remove_file(&stamp_file);
        }

        let slot_file = self.branches_root().join(format!("{safe_b}.slot"));
        let slots_lock_path = self.git.common_git_dir().join("tessera").join("slots.lock");

        {
            let _slot_lock = acquire_file_lock(&slots_lock_path).ok();

            if slot_file.exists() {
                let _ = fs::remove_file(&slot_file);
            }
        }

        let pending_file = self.branches_root().join(format!("{safe_b}.pending-merge"));

        if pending_file.exists() {
            let _ = fs::remove_file(&pending_file);
        }

        let legacy_dir = self
            .git
            .common_git_dir()
            .join("tessera")
            .join("branches")
            .join(&safe_b);

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
                        agent.slot = 1;
                        agent.target_branch = None;
                        agent.base_commit = trunk_cid;
                        agent.head_commit = trunk_cid;
                        agent.head_tree = trunk_tree_id;
                        agent.own_blobs.clear();
                        agent.deleted_paths.clear();
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
    /// port lease metadata, and all active sessions in the `OpLog` `View`.
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

        self.rename_branch_leases(clean_old, clean_new, &old_ws, &new_ws);

        if old_ws.is_dir() && !new_ws.exists() {
            let _ = fs::rename(&old_ws, &new_ws);
        }

        let old_stamp = self.branches_root().join(format!("{safe_old}.tree"));
        let new_stamp = self.branches_root().join(format!("{safe_new}.tree"));

        if old_stamp.is_file() {
            let _ = fs::rename(&old_stamp, &new_stamp);
        }

        let old_slot = self.branches_root().join(format!("{safe_old}.slot"));
        let new_slot = self.branches_root().join(format!("{safe_new}.slot"));
        let slots_lock_path = self.git.common_git_dir().join("tessera").join("slots.lock");

        {
            let _slot_lock = acquire_file_lock(&slots_lock_path).ok();

            if old_slot.is_file() {
                let _ = fs::rename(&old_slot, &new_slot);
            }
        }

        let old_pending = self
            .branches_root()
            .join(format!("{safe_old}.pending-merge"));
        let new_pending = self
            .branches_root()
            .join(format!("{safe_new}.pending-merge"));

        if old_pending.is_file() {
            let _ = fs::rename(&old_pending, &new_pending);
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
    /// on disk while preserving concurrent edits from all remaining peer sessions.
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

        if stashed_blobs.is_empty()
            && stashed_deleted.is_empty()
            && eff_branch == current_head_branch
        {
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
                    .replace('\\', "/")
                    .trim_start_matches("./")
                    .trim_end_matches('/')
                    .to_string()
            })
            .filter(|p| !p.is_empty())
            .collect();

        if !normalized_paths.is_empty() {
            let matches_filter = |rel_path: &str| {
                normalized_paths.iter().any(|filter| {
                    filter == "."
                        || rel_path == filter
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
                    let files: Vec<&str> =
                        stashed_blobs.keys().take(3).map(String::as_str).collect();
                    format!("WIP on {eff_branch}: {}", files.join(", "))
                }
            });

        // Restore stashed files on disk to `base_map` merged with ALL remaining concurrent peers on `eff_branch`.
        let branch_ws = self.branch_sandbox_dir(&eff_branch);
        let mut isolated_stashed_blobs = stashed_blobs.clone();
        let mut updated_peer_blobs: BTreeMap<(String, String), ObjectId> = BTreeMap::new();

        for (rel_path, stashed_bid) in &stashed_blobs {
            let remaining_peers: Vec<(String, ObjectId)> = current
                .view
                .agents
                .iter()
                .filter_map(|(aid, a)| {
                    let a_br = a.effective_branch(current_head_branch.as_str());

                    if !target_sessions.contains(aid) && a_br == eff_branch {
                        a.own_blobs
                            .get(rel_path)
                            .copied()
                            .map(|bid| (aid.clone(), bid))
                    } else {
                        None
                    }
                })
                .collect();

            let base_bytes = base_map
                .get(rel_path)
                .and_then(|e| self.git.read_blob(&e.blob_id).ok())
                .unwrap_or_default();
            let mut cur_stashed_bytes = self.git.read_blob(stashed_bid).unwrap_or_default();
            let mut effective_peer_blobs: Vec<(String, ObjectId)> = Vec::new();

            for (peer_id, peer_bid) in &remaining_peers {
                let peer_bytes = self.git.read_blob(peer_bid).unwrap_or_default();

                if !is_binary_bytes(&base_bytes)
                    && !is_binary_bytes(&cur_stashed_bytes)
                    && !is_binary_bytes(&peer_bytes)
                {
                    if let (Ok(base_str), Ok(stashed_str), Ok(peer_str)) = (
                        std::str::from_utf8(&base_bytes),
                        std::str::from_utf8(&cur_stashed_bytes),
                        std::str::from_utf8(&peer_bytes),
                    ) {
                        let combined = merge_file_3way(
                            rel_path,
                            base_str,
                            stashed_str,
                            peer_str,
                            session_id,
                            peer_id,
                        );

                        if combined.content == peer_str && peer_str != stashed_str {
                            let stripped_peer = merge_file_3way(
                                rel_path,
                                stashed_str,
                                base_str,
                                peer_str,
                                "base",
                                peer_id,
                            );
                            let new_peer_bid =
                                self.git.write_blob(stripped_peer.content.as_bytes())?;
                            updated_peer_blobs
                                .insert((peer_id.clone(), rel_path.clone()), new_peer_bid);
                            effective_peer_blobs.push((peer_id.clone(), new_peer_bid));
                            continue;
                        } else if combined.content == stashed_str && stashed_str != peer_str {
                            let stripped_stash = merge_file_3way(
                                rel_path,
                                peer_str,
                                base_str,
                                stashed_str,
                                "base",
                                session_id,
                            );
                            let new_stash_bytes = stripped_stash.content.into_bytes();
                            let new_stash_bid = self.git.write_blob(&new_stash_bytes)?;
                            isolated_stashed_blobs.insert(rel_path.clone(), new_stash_bid);
                            cur_stashed_bytes = new_stash_bytes;
                        }
                    }
                }

                effective_peer_blobs.push((peer_id.clone(), *peer_bid));
            }

            let target_bytes_opt: Option<Vec<u8>> = if effective_peer_blobs.is_empty() {
                base_map
                    .get(rel_path)
                    .and_then(|e| self.git.read_blob(&e.blob_id).ok())
            } else if effective_peer_blobs.len() == 1 || is_binary_bytes(&base_bytes) {
                self.git.read_blob(&effective_peer_blobs[0].1).ok()
            } else {
                let first_bytes = self
                    .git
                    .read_blob(&effective_peer_blobs[0].1)
                    .unwrap_or_default();

                if is_binary_bytes(&first_bytes) {
                    Some(first_bytes)
                } else {
                    let base_str = String::from_utf8_lossy(&base_bytes);
                    let mut acc_str = String::from_utf8_lossy(&first_bytes).into_owned();
                    let mut acc_label = effective_peer_blobs[0].0.clone();

                    for (next_peer_id, next_peer_bid) in effective_peer_blobs.iter().skip(1) {
                        if let Ok(next_bytes) = self.git.read_blob(next_peer_bid) {
                            if !is_binary_bytes(&next_bytes) {
                                let next_str = String::from_utf8_lossy(&next_bytes);
                                let merged = merge_file_3way(
                                    rel_path,
                                    &base_str,
                                    &acc_str,
                                    &next_str,
                                    &acc_label,
                                    next_peer_id,
                                );
                                acc_str = merged.content;
                                acc_label = format!("{acc_label}+{next_peer_id}");
                            }
                        }
                    }

                    Some(acc_str.into_bytes())
                }
            };

            let restore_executable = base_map.get(rel_path).is_some_and(|e| e.executable);

            match target_bytes_opt {
                Some(bytes) => {
                    if eff_branch == current_head_branch {
                        if let Ok((_, p)) =
                            resolve_confined_repo_path(self.git.repo_root(), rel_path)
                        {
                            write_worktree_file(&p, &bytes, restore_executable);
                        }
                    }

                    if branch_ws.is_dir() {
                        if let Ok((_, p)) = resolve_confined_repo_path(&branch_ws, rel_path) {
                            write_worktree_file(&p, &bytes, restore_executable);
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

        if branch_ws.is_dir() {
            let safe_b = Self::sanitize_branch_name(&eff_branch);
            let stamp = self.branches_root().join(format!("{safe_b}.tree"));

            if let Ok(ws_snap) = snapshot_working_dir(
                &self.git,
                &branch_ws,
                &format!("stash-push-{safe_b}"),
                &[],
                Some(&base_commit.tree_id),
            ) {
                let payload = format!("{base_cid}:{}", ws_snap.root_tree_id);
                let _ = atomic_write_file(&stamp, payload.as_bytes());
            } else {
                let _ = fs::remove_file(&stamp);
            }
        }

        if eff_branch == current_head_branch && normalized_paths.is_empty() {
            self.git.sync_git_index(&base_cid);
        }

        let stash_head_map = self
            .git
            .read_tree_to_map(&meta.head_tree)
            .unwrap_or_default();
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

        let seq = STASH_ID_SEQ.fetch_add(1, Ordering::Relaxed);
        let entry = StashEntry {
            id: format!("stash-{ts}-{}-{seq}", std::process::id()),
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

    /// Applies `stash@{index}` onto `session_id`'s active branch using binary-safe 3-way
    /// Tree-sitter AST merge, records any conflicts in `view.conflicts` and
    /// `.git/tessera/conflicts/`, keeps concurrent peer edits isolated from `session_id.own_blobs`,
    /// and drops the stash entry if `keep` is `false` and no conflicts occurred.
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
        let branch_commit = self.git.read_commit(&branch_cid)?;
        let branch_map = self
            .git
            .read_tree_to_map(&branch_commit.tree_id)
            .unwrap_or_default();

        let branch_ws = self.branch_sandbox_dir(&applied_branch);
        let mut applied_own_blobs = BTreeMap::new();
        let mut applied_read_blobs = BTreeMap::new();
        let mut restored_paths = Vec::new();
        let mut stash_conflicts: Vec<ConflictRecord> = Vec::new();
        let mut any_conflict = false;

        for (rel_path, stash_bid) in &entry.own_blobs {
            let stash_bytes = self.git.read_blob(stash_bid)?;
            let base_entry_bid = stash_base_map.get(rel_path).map(|e| e.blob_id);
            let base_bytes = base_entry_bid
                .and_then(|bid| self.git.read_blob(&bid).ok())
                .unwrap_or_default();

            let branch_tip_bytes = branch_map
                .get(rel_path)
                .and_then(|e| self.git.read_blob(&e.blob_id).ok())
                .unwrap_or_default();

            let curr_bytes = if applied_branch == current_head_branch
                && self.git.repo_root().join(rel_path).is_file()
            {
                fs::read(self.git.repo_root().join(rel_path)).unwrap_or_default()
            } else if branch_ws.join(rel_path).is_file() {
                fs::read(branch_ws.join(rel_path)).unwrap_or_default()
            } else {
                branch_tip_bytes.clone()
            };

            let fallback_executable = branch_map
                .get(rel_path)
                .or_else(|| stash_base_map.get(rel_path))
                .is_some_and(|e| e.executable);
            let executable = entry
                .executable_paths
                .as_ref()
                .map_or(fallback_executable, |paths| paths.contains(rel_path));

            let has_peer_on_path = current.view.agents.values().any(|a| {
                a.agent_id != session_id
                    && a.effective_branch(&current_head_branch) == applied_branch.as_str()
                    && a.owns_path(rel_path)
            });

            if is_binary_bytes(&stash_bytes)
                || is_binary_bytes(&base_bytes)
                || is_binary_bytes(&curr_bytes)
            {
                if curr_bytes == base_bytes || curr_bytes == stash_bytes || curr_bytes.is_empty() {
                    let final_bid = *stash_bid;

                    if applied_branch == current_head_branch {
                        let (_, root_target) =
                            resolve_confined_repo_path(self.git.repo_root(), rel_path)?;
                        write_worktree_file(&root_target, &stash_bytes, executable);
                    }

                    if branch_ws.is_dir() {
                        if let Ok((_, ws_target)) = resolve_confined_repo_path(&branch_ws, rel_path)
                        {
                            write_worktree_file(&ws_target, &stash_bytes, executable);
                        }
                    }

                    applied_own_blobs.insert(rel_path.clone(), final_bid);
                    applied_read_blobs.insert(rel_path.clone(), final_bid);
                    restored_paths.push(rel_path.clone());
                } else if stash_bytes == base_bytes {
                    let curr_bid = self.git.write_blob(&curr_bytes)?;
                    applied_read_blobs.insert(rel_path.clone(), curr_bid);
                    restored_paths.push(rel_path.clone());
                } else {
                    any_conflict = true;
                    let curr_bid = self.git.write_blob(&curr_bytes)?;
                    let blob_merge = Merge {
                        removes: vec![base_entry_bid],
                        adds: vec![Some(curr_bid), Some(*stash_bid)],
                    };
                    let cid = ConflictRecord::compute_id(rel_path, None, &blob_merge);
                    let mut intents = BTreeMap::new();
                    intents.insert(applied_branch.clone(), "working tree".to_string());
                    intents.insert(format!("stash@{{{index}}}"), entry.message.clone());
                    let record = ConflictRecord {
                        id: cid,
                        path: rel_path.clone(),
                        symbol: None,
                        kind: ConflictKind::Binary,
                        merge: blob_merge,
                        agent_intents: intents,
                        base_snippet: String::new(),
                        left_snippet: String::new(),
                        right_snippet: String::new(),
                        diagnostics: Some(
                            "Binary conflict while applying stash; resolve with `tes resolve --strategy ours|theirs`."
                                .to_string(),
                        ),
                        created_at_ms: now_millis(),
                    };
                    self.oplog.save_conflict(&record)?;
                    stash_conflicts.push(record);
                }

                continue;
            }

            let base_str = String::from_utf8_lossy(&base_bytes);
            let curr_str = String::from_utf8_lossy(&curr_bytes);
            let stash_str = String::from_utf8_lossy(&stash_bytes);

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
                let curr_bid = self.git.write_blob(&curr_bytes)?;
                let symbol = merge_res.conflicted_symbols.first().cloned();
                let blob_merge = Merge {
                    removes: vec![base_entry_bid],
                    adds: vec![Some(curr_bid), Some(*stash_bid)],
                };
                let cid = ConflictRecord::compute_id(rel_path, symbol.as_deref(), &blob_merge);
                let mut intents = BTreeMap::new();
                intents.insert(applied_branch.clone(), "working tree".to_string());
                intents.insert(format!("stash@{{{index}}}"), entry.message.clone());
                let record = ConflictRecord {
                    id: cid,
                    path: rel_path.clone(),
                    symbol: symbol.clone(),
                    kind: if symbol.is_some() {
                        ConflictKind::AstNode
                    } else {
                        ConflictKind::TextHunk
                    },
                    merge: blob_merge,
                    agent_intents: intents,
                    base_snippet: merge_res.base_conflict_snippet,
                    left_snippet: merge_res.left_conflict_snippet,
                    right_snippet: merge_res.right_conflict_snippet,
                    diagnostics: None,
                    created_at_ms: now_millis(),
                };
                self.oplog.save_conflict(&record)?;
                stash_conflicts.push(record);
            }

            let merged_bytes = merge_res.content.as_bytes();
            let disk_merged_bid = self.git.write_blob(merged_bytes)?;

            // Compute `session_own_bid` strictly against the committed branch tip so concurrent
            // peer edits sitting on disk are never stolen into `session_id.own_blobs`.
            let session_own_bid = if has_peer_on_path && curr_bytes != branch_tip_bytes {
                if is_binary_bytes(&branch_tip_bytes) {
                    *stash_bid
                } else {
                    let tip_str = String::from_utf8_lossy(&branch_tip_bytes);
                    let isolated = merge_file_3way(
                        rel_path,
                        &base_str,
                        &tip_str,
                        &stash_str,
                        &applied_branch,
                        &format!("stash@{{{index}}}"),
                    );

                    if isolated.has_conflicts {
                        *stash_bid
                    } else {
                        self.git.write_blob(isolated.content.as_bytes())?
                    }
                }
            } else {
                disk_merged_bid
            };

            if applied_branch == current_head_branch {
                let (_, root_target) = resolve_confined_repo_path(self.git.repo_root(), rel_path)?;
                write_worktree_file(&root_target, merged_bytes, executable);
            }

            if branch_ws.is_dir() {
                if let Ok((_, ws_target)) = resolve_confined_repo_path(&branch_ws, rel_path) {
                    write_worktree_file(&ws_target, merged_bytes, executable);
                }
            }

            applied_own_blobs.insert(rel_path.clone(), session_own_bid);
            applied_read_blobs.insert(rel_path.clone(), disk_merged_bid);
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
                let blob_merge = Merge {
                    removes: vec![stash_base_bid],
                    adds: vec![curr_bid, None],
                };
                let cid = ConflictRecord::compute_id(rel_path, None, &blob_merge);
                let mut intents = BTreeMap::new();
                intents.insert(applied_branch.clone(), "working tree modified".to_string());
                intents.insert(
                    format!("stash@{{{index}}}"),
                    format!("deleted in stash ({})", entry.message),
                );
                let record = ConflictRecord {
                    id: cid,
                    path: rel_path.clone(),
                    symbol: None,
                    kind: ConflictKind::DeleteModify,
                    merge: blob_merge,
                    agent_intents: intents,
                    base_snippet: String::new(),
                    left_snippet: String::new(),
                    right_snippet: String::new(),
                    diagnostics: None,
                    created_at_ms: now_millis(),
                };
                self.oplog.save_conflict(&record)?;
                stash_conflicts.push(record);
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

        if branch_ws.is_dir() {
            let safe_b = Self::sanitize_branch_name(&applied_branch);
            let stamp = self.branches_root().join(format!("{safe_b}.tree"));

            if let Ok(ws_snap) = snapshot_working_dir(
                &self.git,
                &branch_ws,
                &format!("stash-pop-{safe_b}"),
                &[],
                Some(&branch_commit.tree_id),
            ) {
                let payload = format!("{branch_cid}:{}", ws_snap.root_tree_id);
                let _ = atomic_write_file(&stamp, payload.as_bytes());
            } else {
                let _ = fs::remove_file(&stamp);
            }
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

                for c in &stash_conflicts {
                    view.conflicts.insert(c.path.clone(), c.id.clone());
                }

                if let Some(agent) = view.agents.get_mut(session_id) {
                    for (p, own_bid) in &applied_own_blobs {
                        agent.deleted_paths.remove(p);
                        agent.own_blobs.insert(p.clone(), *own_bid);
                    }

                    for (p, read_bid) in &applied_read_blobs {
                        agent.read_blobs.insert(p.clone(), *read_bid);
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
    /// and automatically rebases active agent sessions on `<branch>` via [`Self::sync_all_agents`].
    ///
    /// If a 3-way pull merge produces conflicts, records every [`ConflictRecord`] into
    /// `view.conflicts` and `.git/tessera/conflicts/`, materializes the conflicted working tree
    /// onto disk without advancing `refs/heads/<branch>` with raw conflict markers baked into
    /// history, and returns them in [`PullReport::conflicts`]. Preserves untracked and
    /// uncommitted session files across materialization and rebase.
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
            let stderr = String::from_utf8_lossy(&fetch_out.stderr)
                .trim()
                .to_string();
            return Err(CoreError::InvalidArgument(format!(
                "git fetch {clean_remote} {target_branch} failed: {stderr}"
            )));
        }

        // Prefer `FETCH_HEAD` (always updated by `git fetch <remote> <branch>` even without a
        // remote tracking refspec), falling back to `refs/remotes/<remote>/<branch>`.
        let remote_cid = self
            .git
            .resolve_revision("FETCH_HEAD")
            .ok()
            .or_else(|| {
                self.git
                    .resolve_ref(&format!("refs/remotes/{clean_remote}/{target_branch}"))
                    .ok()
                    .flatten()
            })
            .ok_or_else(|| {
                CoreError::NotFound(format!("remote ref '{clean_remote}/{target_branch}'"))
            })?;

        let old_cid = existing_local_cid
            .or(fallback_head_cid)
            .unwrap_or(remote_cid);
        let prev_tree_map = self
            .git
            .read_commit(&old_cid)
            .and_then(|c| self.git.read_tree_to_map(&c.tree_id))
            .unwrap_or_default();

        let (new_cid, fast_forward, pull_conflicts) = if existing_local_cid.is_none() {
            self.git
                .update_ref(&format!("refs/heads/{target_branch}"), &remote_cid)?;
            (remote_cid, true, Vec::new())
        } else if old_cid == remote_cid || self.git.is_ancestor(&remote_cid, &old_cid) {
            (old_cid, true, Vec::new())
        } else if self.git.is_ancestor(&old_cid, &remote_cid) {
            self.git
                .update_ref(&format!("refs/heads/{target_branch}"), &remote_cid)?;
            (remote_cid, true, Vec::new())
        } else {
            let merge_base_cid_opt = Command::new("git")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .current_dir(self.git.repo_root())
                .args(["merge-base", old_cid.as_str(), remote_cid.as_str()])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| ObjectId::from_hex(String::from_utf8_lossy(&o.stdout).trim()).ok());

            let merge_base_tree_id = match merge_base_cid_opt {
                Some(mb_cid) => self.git.read_commit(&mb_cid)?.tree_id,
                None => self.git.write_tree_from_map(&BTreeMap::new())?,
            };
            let local_commit = self.git.read_commit(&old_cid)?;
            let remote_commit = self.git.read_commit(&remote_cid)?;

            let outcome = merge_trees_3way(
                &self.git,
                &merge_base_tree_id,
                &local_commit.tree_id,
                &remote_commit.tree_id,
                &target_branch,
                "local",
                &format!("{clean_remote}/{target_branch}"),
                "remote",
            )?;

            if !outcome.conflicts.is_empty() {
                for c in &outcome.conflicts {
                    self.oplog.save_conflict(c)?;
                }

                let branch_ws = self.branch_sandbox_dir(&target_branch);

                if branch_ws.is_dir() {
                    let _ = materialize_tree_to_dir_with_baseline(
                        &self.git,
                        &outcome.merged_tree_id,
                        &branch_ws,
                        Some(&prev_tree_map),
                    );
                }

                if target_branch == current_head_branch {
                    materialize_tree_to_dir_with_baseline(
                        &self.git,
                        &outcome.merged_tree_id,
                        self.git.repo_root(),
                        Some(&prev_tree_map),
                    )?;
                }

                let safe_b = Self::sanitize_branch_name(&target_branch);
                let pending_file = self.branches_root().join(format!("{safe_b}.pending-merge"));
                let payload = format!("{old_cid}:{old_cid},{remote_cid}");
                let _ = atomic_write_file(&pending_file, payload.as_bytes());

                let saved_conflicts = outcome.conflicts.clone();
                self.oplog.record_tx(
                    session_id,
                    format!(
                        "pull {clean_remote}/{target_branch} into {target_branch} ({} conflict(s))",
                        saved_conflicts.len()
                    ),
                    false,
                    |view| {
                        for c in &saved_conflicts {
                            view.conflicts.insert(c.path.clone(), c.id.clone());
                        }

                        Ok(())
                    },
                )?;

                return Ok(PullReport {
                    remote: clean_remote.to_string(),
                    branch: target_branch,
                    old_commit: old_cid,
                    new_commit: old_cid,
                    fast_forward: false,
                    rebased_sessions: 0,
                    conflicts: outcome.conflicts,
                });
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
            (merged_cid, false, Vec::new())
        };

        if new_cid != old_cid {
            let new_commit = self.git.read_commit(&new_cid)?;
            let branch_ws = self.branch_sandbox_dir(&target_branch);

            if branch_ws.is_dir() {
                let _ = materialize_tree_to_dir_with_baseline(
                    &self.git,
                    &new_commit.tree_id,
                    &branch_ws,
                    Some(&prev_tree_map),
                );
                let safe_b = Self::sanitize_branch_name(&target_branch);
                let stamp = self.branches_root().join(format!("{safe_b}.tree"));
                let stamp_payload = format!("{new_cid}:{}", new_commit.tree_id);
                let _ = atomic_write_file(&stamp, stamp_payload.as_bytes());
            }

            if target_branch == current_head_branch {
                materialize_tree_to_dir_with_baseline(
                    &self.git,
                    &new_commit.tree_id,
                    self.git.repo_root(),
                    Some(&prev_tree_map),
                )?;
                self.git.sync_git_index(&new_cid);

                // Re-apply uncommitted head-branch session `own_blobs` and `deleted_paths` BEFORE
                // `sync_all_agents()` calls `snapshot_agent(&caller_id, true)` so caller's
                // uncommitted edits and newly created files are never evicted.
                let head_ws_opt = branch_ws.is_dir().then_some(branch_ws.as_path());
                let mut pre_obs = BTreeMap::new();
                self.reapply_remaining_head_sessions(
                    &current.view.agents,
                    &BTreeSet::new(),
                    &current_head_branch,
                    head_ws_opt,
                    &mut pre_obs,
                );
            }
        }

        let rebased = self.sync_all_agents()?;

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
            conflicts: pull_conflicts,
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
            return Err(CoreError::InvalidArgument(format!(
                "git log failed: {stderr}"
            )));
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
    /// using 3-way Tree-sitter AST merge ([`merge_trees_3way`]), while preserving caller's
    /// uncommitted new files and edits.
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
        let prev_target_map = self.git.read_tree_to_map(&target_commit.tree_id)?;

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
            source_commit
                .message
                .lines()
                .next()
                .unwrap_or("cherry-pick"),
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
            let _ = materialize_tree_to_dir_with_baseline(
                &self.git,
                &outcome.merged_tree_id,
                &branch_ws,
                Some(&prev_target_map),
            );
            let safe_b = Self::sanitize_branch_name(&eff_branch);
            let stamp = self.branches_root().join(format!("{safe_b}.tree"));
            let stamp_payload = format!("{new_commit_id}:{}", outcome.merged_tree_id);
            let _ = atomic_write_file(&stamp, stamp_payload.as_bytes());
        }

        if eff_branch == current_head_branch {
            materialize_tree_to_dir_with_baseline(
                &self.git,
                &outcome.merged_tree_id,
                self.git.repo_root(),
                Some(&prev_target_map),
            )?;
            self.git.sync_git_index(&new_commit_id);

            // Re-apply uncommitted head-branch session `own_blobs` and `deleted_paths` BEFORE
            // `sync_all_agents()` calls `snapshot_agent(&caller_id, true)` so caller's
            // uncommitted edits and newly created files are preserved.
            let head_ws_opt = branch_ws.is_dir().then_some(branch_ws.as_path());
            let mut pre_obs = BTreeMap::new();
            self.reapply_remaining_head_sessions(
                &current.view.agents,
                &BTreeSet::new(),
                &current_head_branch,
                head_ws_opt,
                &mut pre_obs,
            );
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

    /// Compacts ephemeral micro-snapshot operations in `.git/tessera/oplog/` and prunes orphaned
    /// branch sandboxes, stale sandbox sidecar metadata (`.slot`, `.tree`, `.pending-merge`), and
    /// dead port leases whose owning PID is no longer alive.
    ///
    /// When `dry_run` is `true`, computes and returns the complete [`GcReport`] without modifying
    /// any Operation Log files, sandboxes, sidecars, or port leases. Orphaned sandboxes that still
    /// host a live leased process or an active session workspace are skipped and reported in
    /// [`GcReport::skipped_active_sandboxes`].
    ///
    /// # Errors
    /// Returns [`CoreError`] if Operation Log compaction or configuration loading fails.
    pub fn gc(&self, dry_run: bool) -> CoreResult<GcReport> {
        let compacted_operations = self.oplog.gc_compact_with_options(dry_run)?;
        let cfg = self.oplog.load_config()?;
        let current = self.oplog.load_current_reconciled()?;

        let mut known_safe_branches: BTreeSet<String> = BTreeSet::new();
        known_safe_branches.insert(Self::sanitize_branch_name(&cfg.trunk_branch));

        if let Ok(head_br) = self.git.current_branch_name() {
            known_safe_branches.insert(Self::sanitize_branch_name(&head_br));
        }

        for br in self.git.list_local_branches() {
            known_safe_branches.insert(Self::sanitize_branch_name(&br));
        }

        for br in current.view.branches.keys() {
            known_safe_branches.insert(Self::sanitize_branch_name(br));
        }

        let mut active_workspace_paths: Vec<PathBuf> = Vec::new();

        for agent in current.view.agents.values() {
            if let Some(tb) = &agent.target_branch {
                known_safe_branches.insert(Self::sanitize_branch_name(tb));
            }

            if let Some(ws) = &agent.workspace_path {
                active_workspace_paths.push(PathBuf::from(ws));
            }
        }

        let mut live_lease_workspaces: Vec<PathBuf> = Vec::new();
        let mut pruned_dead_leases: Vec<String> = Vec::new();
        let leases_dir = self.sandboxes_root().join("bin").join(".leases");

        if let Ok(entries) = fs::read_dir(&leases_dir) {
            for entry in entries.flatten() {
                let path = entry.path();

                if !path.is_dir() {
                    continue;
                }

                let Some(port_name) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .filter(|s| s.parse::<u16>().is_ok())
                    .map(ToString::to_string)
                else {
                    continue;
                };

                let pid_opt = fs::read_to_string(path.join("pid"))
                    .ok()
                    .and_then(|raw| raw.trim().parse::<i32>().ok());
                let alive = pid_opt.is_some_and(crate::session::is_process_alive);

                if alive {
                    if let Ok(ws_raw) = fs::read_to_string(path.join("workspace")) {
                        let trimmed = ws_raw.trim();

                        if !trimmed.is_empty() {
                            live_lease_workspaces.push(PathBuf::from(trimmed));
                        }
                    }

                    continue;
                }

                if dry_run || fs::remove_dir_all(&path).is_ok() {
                    pruned_dead_leases.push(port_name);
                }
            }
        }

        let mut pruned_sandboxes: Vec<String> = Vec::new();
        let mut pruned_sidecars: Vec<String> = Vec::new();
        let mut skipped_active_sandboxes: Vec<String> = Vec::new();
        let branches_root = self.branches_root();

        if let Ok(entries) = fs::read_dir(&branches_root) {
            let mut dir_entries: Vec<(String, PathBuf)> = Vec::new();
            let mut file_entries: Vec<(String, PathBuf)> = Vec::new();

            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(ToString::to_string)
                else {
                    continue;
                };

                if path.is_dir() {
                    dir_entries.push((name, path));
                } else if path.is_file() {
                    file_entries.push((name, path));
                }
            }

            let mut skipped_set: BTreeSet<String> = BTreeSet::new();
            let locks_dir = self.git.common_git_dir().join("tessera").join("locks");

            for (name, path) in dir_entries {
                if known_safe_branches.contains(&name) {
                    continue;
                }

                let has_live_process = live_lease_workspaces
                    .iter()
                    .chain(active_workspace_paths.iter())
                    .any(|ws| same_canonical_path(&path, ws));

                if has_live_process {
                    skipped_set.insert(name.clone());
                    skipped_active_sandboxes.push(name);
                    continue;
                }

                if dry_run {
                    pruned_sandboxes.push(name);
                } else {
                    let lock_path = locks_dir.join(format!("sandbox-{name}.lock"));
                    let _sandbox_lock = acquire_file_lock(&lock_path).ok();

                    if fs::remove_dir_all(&path).is_ok() {
                        pruned_sandboxes.push(name);
                    }
                }
            }

            let slots_lock_path = self.git.common_git_dir().join("tessera").join("slots.lock");

            for (fname, path) in file_entries {
                let (stem, is_slot) = if let Some(s) = fname.strip_suffix(".slot") {
                    (s, true)
                } else if let Some(s) = fname.strip_suffix(".tree") {
                    (s, false)
                } else if let Some(s) = fname.strip_suffix(".pending-merge") {
                    (s, false)
                } else {
                    continue;
                };

                if known_safe_branches.contains(stem) || skipped_set.contains(stem) {
                    continue;
                }

                if dry_run {
                    pruned_sidecars.push(fname);
                } else if is_slot {
                    let _slot_lock = acquire_file_lock(&slots_lock_path).ok();

                    if fs::remove_file(&path).is_ok() {
                        pruned_sidecars.push(fname);
                    }
                } else if fs::remove_file(&path).is_ok() {
                    pruned_sidecars.push(fname);
                }
            }
        }

        pruned_sandboxes.sort();
        pruned_sidecars.sort();
        pruned_dead_leases.sort();
        skipped_active_sandboxes.sort();

        Ok(GcReport {
            dry_run,
            compacted_operations,
            pruned_sandboxes,
            pruned_sidecars,
            pruned_dead_leases,
            skipped_active_sandboxes,
        })
    }
}
