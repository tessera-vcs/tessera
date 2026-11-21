//! Semantic 3-way/octopus merging (`merge_agents*`), dry-run preview mode, speculative
//! verification sandboxes with RAII cleanup, non-blocking agent auto-rebase (`sync_all_agents`),
//! and first-class conflict resolution (`resolve_conflict`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use core::{
    AgentMetadata, ChangeId, ConflictKind, ConflictRecord, CoreError, CoreResult, EntryMode,
    FileEntry, Merge, ObjectId, atomic_write_file, now_millis, resolve_confined_repo_path,
};
use merge::{merge_ast_union, merge_file_3way, merge_trees_3way};
use workspace::{
    create_cow_workspace, materialize_tree_to_dir, materialize_tree_to_dir_with_baseline,
    snapshot_working_dir,
};

use crate::session::detect_session_id;
use crate::worktree::{
    has_git_conflict_markers, is_ai_session_id, is_binary_bytes, is_harness_config_path,
    remove_worktree_file, set_worktree_executable, worktree_is_executable, write_worktree_file,
    write_worktree_file_checked, write_worktree_rel,
};
use crate::{CommitOptions, Coordinator, MergeReport, RebasedSession, ResolveStrategy};

/// Process-local monotonic counter guaranteeing unique speculative verify sandbox directories.
static VERIFY_SANDBOX_SEQ: AtomicU64 = AtomicU64::new(1);

/// RAII guard that removes a temporary speculative verification sandbox directory on scope exit.
struct VerifySandboxGuard {
    dir: PathBuf,
}

impl Drop for VerifySandboxGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Coordinator {
    /// Semantically merges one or more agent sessions (`agent_ids`) into `target_branch`.
    ///
    /// When `keep_open` is `false` and the merge finishes with zero conflicts and passes
    /// verification, the merged agent sessions are automatically closed.
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
    /// onto `repo_root` (and onto `head_ws` when given) after a commit, merge, pull, cherry-pick,
    /// or restore rewrote working-tree files.
    pub(crate) fn reapply_remaining_head_sessions(
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

                    if is_binary_bytes(&base_bytes)
                        || is_binary_bytes(&disk_bytes)
                        || is_binary_bytes(&rem_bytes)
                    {
                        rem_bytes
                    } else {
                        match (
                            std::str::from_utf8(&base_bytes),
                            std::str::from_utf8(&disk_bytes),
                            std::str::from_utf8(&rem_bytes),
                        ) {
                            (Ok(base_s), Ok(disk_s), Ok(rem_s))
                                if disk_s != rem_s && disk_s != base_s =>
                            {
                                merge_file_3way(rel_path, base_s, disk_s, rem_s, head_branch, k)
                                    .content
                                    .into_bytes()
                            }
                            _ => rem_bytes,
                        }
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
    /// explicit [`CommitOptions`] (`message`, `author`, `co_author`, `co_author_trailers`, `amend`,
    /// and `dry_run`).
    ///
    /// When `options.dry_run` is `true`, computes the complete 3-way/octopus AST merge, conflicts,
    /// renames, migrations, affected packages, and optional speculative verification gate inside a
    /// temporary sandbox, returning [`MergeReport`] without mutating any Git refs, `repo_root`,
    /// branch sandboxes, conflict store, or `OpLog`.
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
        let dry_run = options.dry_run;

        if !dry_run {
            self.reconcile_in_flight_sessions();
        }

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
                    if dry_run {
                        let mut preview_meta = existing.clone();

                        if let Some(ws_path) = &existing.workspace_path {
                            let base_c = self.git.read_commit(&existing.base_commit)?;

                            if let Ok(snap) = snapshot_working_dir(
                                &self.git,
                                Path::new(ws_path),
                                &format!("{id}-dry-run"),
                                &existing.scopes,
                                Some(&base_c.tree_id),
                            ) {
                                preview_meta.head_tree = snap.root_tree_id;
                            }
                        }

                        agents.push(preview_meta.clone());
                        merge_units.push(preview_meta);
                    } else {
                        let snapped = self.snapshot_agent(id, false)?;
                        agents.push(snapped.clone());
                        merge_units.push(snapped);
                    }
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
                return Err(CoreError::NotFound(format!(
                    "agent session or branch '{id}'"
                )));
            }
        }

        if let Some(last_shared) = shared_root_group.last() {
            if dry_run {
                for shared_agent in &shared_root_group {
                    let mut isolated = shared_agent.clone();

                    if shared_agent.has_own_changes() {
                        let base_c = self.git.read_commit(&shared_agent.base_commit)?;
                        let mut tree_map = self.git.read_tree_to_map(&base_c.tree_id)?;
                        let head_map = self
                            .git
                            .read_tree_to_map(&shared_agent.head_tree)
                            .unwrap_or_default();
                        shared_agent.apply_own_changes(&mut tree_map, &head_map);
                        isolated.head_tree = self.git.write_tree_from_map(&tree_map)?;
                    } else if shared_agent.targets_head_branch(&default_trunk) {
                        let base_c = self.git.read_commit(&shared_agent.base_commit)?;
                        let base_files = self.git.read_tree_to_map(&base_c.tree_id)?;

                        if let Ok(snap) = snapshot_working_dir(
                            &self.git,
                            self.git.repo_root(),
                            &format!("{}-dry-run", shared_agent.agent_id),
                            &shared_agent.scopes,
                            Some(&base_c.tree_id),
                        ) {
                            let other_owned: BTreeSet<String> = current
                                .view
                                .agents
                                .values()
                                .filter(|a| a.agent_id != shared_agent.agent_id)
                                .flat_map(|a| a.owned_paths().cloned())
                                .collect();
                            let other_touched: BTreeSet<String> = current
                                .view
                                .agents
                                .values()
                                .filter(|a| a.agent_id != shared_agent.agent_id)
                                .flat_map(|a| {
                                    a.owned_paths().cloned().chain(a.read_blobs.keys().cloned())
                                })
                                .collect();
                            let iso_map = Self::isolate_head_branch_tree(
                                shared_agent,
                                &snap.files,
                                &base_files,
                                &other_owned,
                                &other_touched,
                            );
                            isolated.head_tree = self.git.write_tree_from_map(&iso_map)?;
                        }
                    }

                    agents.push(isolated.clone());
                    merge_units.push(isolated);
                }
            } else {
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
                if !dry_run {
                    self.oplog.save_conflict(&c)?;
                }

                all_conflicts.push(c);
            }

            all_renames.extend(outcome.renames_applied);
            all_migrations.extend(outcome.migrations_resequenced);

            for p in outcome.touched_paths {
                all_touched.insert(p);
            }
        }

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
                    if entry.is_symlink() || entry.is_gitlink() {
                        continue;
                    }

                    if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                        if is_binary_bytes(&bytes) {
                            continue;
                        }

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

                            if !dry_run {
                                self.oplog.save_conflict(&rec)?;
                            }

                            all_conflicts.push(rec);
                        }
                    }
                }
            }
        }

        let touched_vec: Vec<String> = all_touched.into_iter().collect();
        let affected_packages = self.discover_affected_packages(&acc_tree, &touched_vec)?;

        let effective_verify = verify_cmd.or(cfg.verify_command.as_deref());
        let mut verification_passed = true;
        let mut verification_output = None;

        if let Some(cmd_str) = effective_verify {
            let seq = VERIFY_SANDBOX_SEQ.fetch_add(1, Ordering::Relaxed);
            let verify_dir = self.sandboxes_root().join(format!(
                "verify-sandbox-{}-{}-{seq}",
                std::process::id(),
                now_millis()
            ));
            let _verify_guard = VerifySandboxGuard {
                dir: verify_dir.clone(),
            };

            create_cow_workspace(self.git.repo_root(), &verify_dir, &[], &cfg.cow_cache_dirs)?;
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

                if !dry_run {
                    self.oplog.save_conflict(&build_conflict)?;
                }

                all_conflicts.push(build_conflict);
            }
        }

        if dry_run {
            return Ok(MergeReport {
                target_branch: branch,
                merged_commit_id: trunk_commit_id,
                merged_tree_id: acc_tree,
                skipped_subtrees: total_skipped_subtrees,
                conflicts: all_conflicts,
                renames_applied: all_renames,
                migrations_resequenced: all_migrations,
                affected_packages,
                closed_agents: Vec::new(),
                verification_passed,
                verification_output,
                created_commit: false,
            });
        }

        let fallback_amend_msg = if options.amend && options.message.is_none() {
            Some(trunk_commit.message.trim().to_string())
        } else {
            None
        };
        let eff_message = options.message.as_deref().or(fallback_amend_msg.as_deref());
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
        let remaining_sessions_with_edits = current.view.agents.iter().any(|(k, a)| {
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

        let prev_head_map = self
            .git
            .resolve_head_commit()
            .ok()
            .flatten()
            .and_then(|cid| self.git.read_commit(&cid).ok())
            .and_then(|c| self.git.read_tree_to_map(&c.tree_id).ok());

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

                if !distinct_branches_active
                    || !remaining_sessions_with_edits
                    || branch == current_head_branch
                {
                    if switched_head {
                        materialize_tree_to_dir_with_baseline(
                            &self.git,
                            &acc_tree,
                            self.git.repo_root(),
                            prev_head_map.as_ref(),
                        )?;
                    } else if let Ok(acc_map) = self.git.read_tree_to_map(&acc_tree) {
                        let rename_paths = all_renames
                            .iter()
                            .flat_map(|ren| [ren.old_path.clone(), ren.new_path.clone()]);
                        let sync_paths: BTreeSet<String> =
                            touched_vec.iter().cloned().chain(rename_paths).collect();

                        for rel_path in &sync_paths {
                            if let Some(entry) = acc_map.get(rel_path) {
                                if !entry.is_symlink() && !entry.is_gitlink() {
                                    if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                                        write_worktree_file(
                                            &self.git.repo_root().join(rel_path),
                                            &bytes,
                                            entry.executable,
                                        );
                                        post_commit_observed_blobs
                                            .insert(rel_path.clone(), entry.blob_id);
                                    }
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
            } else if let Some(head_cid) = self
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
                                Some(entry) if !entry.is_symlink() && !entry.is_gitlink() => {
                                    if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                                        write_worktree_file(
                                            &self.git.repo_root().join(rel_path),
                                            &bytes,
                                            entry.executable,
                                        );
                                    }
                                }
                                None => remove_worktree_file(self.git.repo_root(), rel_path),
                                _ => {}
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
        } else if all_shared_root {
            let current_head_branch = self
                .git
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());

            if branch == current_head_branch || switch_head_to_target {
                let _ = materialize_tree_to_dir_with_baseline(
                    &self.git,
                    &acc_tree,
                    self.git.repo_root(),
                    prev_head_map.as_ref(),
                );
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
                        .map(|b| {
                            is_binary_bytes(&b)
                                || !has_git_conflict_markers(&String::from_utf8_lossy(&b))
                        })
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

        // Rebase remaining unmerged sessions on `branch` onto `merged_commit_id` (`acc_tree`).
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

                    if is_binary_bytes(&old_b) || is_binary_bytes(&new_b) || is_binary_bytes(&own_b)
                    {
                        can_rebase = false;
                        break;
                    }

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
                        let mode = rem_head_map
                            .get(p)
                            .or_else(|| rem_tree_map.get(p))
                            .map_or(EntryMode::NormalFile, FileEntry::entry_mode);
                        rem_tree_map.insert(p.clone(), FileEntry::from_mode(*bid, mode));
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

        let leftover_unowned =
            if self.git.current_branch_name().ok().as_deref() == Some(branch.as_str()) {
                self.unowned_worktree_changes(&current.view, &acc_tree)
            } else {
                BTreeSet::new()
            };

        let mut applied_rebased_refs: Vec<(String, ChangeId, ObjectId)> = Vec::new();

        self.oplog.record_tx(
            "merge",
            format!(
                "merge {:?} into {branch} (verified={verification_passed})",
                target_ids
            ),
            false,
            |view| {
                applied_rebased_refs.clear();

                if verification_passed {
                    view.branches.insert(branch.clone(), merged_commit_id);
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
                    } else {
                        view.observed_blobs
                            .retain(|path, _| view.agents.values().any(|a| a.owns_path(path)));
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
                    } else {
                        view.observed_blobs
                            .retain(|path, _| view.agents.values().any(|a| a.owns_path(path)));
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
                            if agent_meta.effective_branch(&default_trunk) != branch.as_str() {
                                continue;
                            }

                            let snapshot_before = current.view.agents.get(aid);
                            let before_base_map = snapshot_before
                                .and_then(|b| self.git.read_commit(&b.base_commit).ok())
                                .and_then(|c| self.git.read_tree_to_map(&c.tree_id).ok())
                                .unwrap_or_default();

                            let mut merged_own = rebased.own_blobs.clone();
                            let mut merged_deleted = rebased.deleted_paths.clone();
                            let mut had_concurrent_edits = false;
                            let mut skip_rebase = false;

                            if let Some(before) = snapshot_before {
                                for old_path in before.own_blobs.keys() {
                                    if !agent_meta.own_blobs.contains_key(old_path) {
                                        merged_own.remove(old_path);
                                        had_concurrent_edits = true;
                                    }
                                }

                                for old_del in &before.deleted_paths {
                                    if !agent_meta.deleted_paths.contains(old_del) {
                                        merged_deleted.remove(old_del);
                                        had_concurrent_edits = true;
                                    }
                                }
                            }

                            for (rel_path, live_bid) in &agent_meta.own_blobs {
                                let changed_since_load = snapshot_before
                                    .is_none_or(|b| b.own_blobs.get(rel_path) != Some(live_bid));

                                if !changed_since_load {
                                    continue;
                                }

                                had_concurrent_edits = true;
                                merged_deleted.remove(rel_path);

                                let old_bid = before_base_map.get(rel_path).map(|e| e.blob_id);
                                let new_bid = acc_map_for_post.get(rel_path).map(|e| e.blob_id);

                                if Some(*live_bid) == new_bid {
                                    merged_own.remove(rel_path);
                                    continue;
                                }

                                if old_bid == new_bid {
                                    merged_own.insert(rel_path.clone(), *live_bid);
                                    continue;
                                }

                                let old_b = old_bid
                                    .and_then(|id| self.git.read_blob(&id).ok())
                                    .unwrap_or_default();
                                let new_b = new_bid
                                    .and_then(|id| self.git.read_blob(&id).ok())
                                    .unwrap_or_default();
                                let Ok(live_b) = self.git.read_blob(live_bid) else {
                                    skip_rebase = true;
                                    break;
                                };

                                if is_binary_bytes(&old_b)
                                    || is_binary_bytes(&new_b)
                                    || is_binary_bytes(&live_b)
                                {
                                    skip_rebase = true;
                                    break;
                                }

                                let reb = merge_file_3way(
                                    rel_path,
                                    &String::from_utf8_lossy(&old_b),
                                    &String::from_utf8_lossy(&new_b),
                                    &String::from_utf8_lossy(&live_b),
                                    &branch,
                                    aid,
                                );

                                if reb.has_conflicts || has_git_conflict_markers(&reb.content) {
                                    skip_rebase = true;
                                    break;
                                }

                                if let Ok(reb_bid) = self.git.write_blob(reb.content.as_bytes()) {
                                    if Some(reb_bid) == new_bid {
                                        merged_own.remove(rel_path);
                                    } else {
                                        merged_own.insert(rel_path.clone(), reb_bid);
                                    }
                                } else {
                                    skip_rebase = true;
                                    break;
                                }
                            }

                            if !skip_rebase {
                                for rel_path in &agent_meta.deleted_paths {
                                    let added_since_load = snapshot_before
                                        .is_none_or(|b| !b.deleted_paths.contains(rel_path));

                                    if !added_since_load {
                                        continue;
                                    }

                                    had_concurrent_edits = true;
                                    merged_own.remove(rel_path);

                                    let old_bid = before_base_map.get(rel_path).map(|e| e.blob_id);
                                    let new_bid = acc_map_for_post.get(rel_path).map(|e| e.blob_id);

                                    if new_bid.is_none() {
                                        merged_deleted.remove(rel_path);
                                    } else if old_bid != new_bid {
                                        skip_rebase = true;
                                        break;
                                    } else {
                                        merged_deleted.insert(rel_path.clone());
                                    }
                                }
                            }

                            if skip_rebase {
                                continue;
                            }

                            let mut final_head_commit = rebased.head_commit;
                            let mut final_head_tree = rebased.head_tree;

                            if had_concurrent_edits {
                                let live_head_map = self
                                    .git
                                    .read_tree_to_map(&agent_meta.head_tree)
                                    .unwrap_or_default();
                                let mut rebuilt_map = acc_map_for_post.clone();

                                for (p, bid) in &merged_own {
                                    let mode = live_head_map
                                        .get(p)
                                        .or_else(|| rebuilt_map.get(p))
                                        .map_or(EntryMode::NormalFile, FileEntry::entry_mode);
                                    rebuilt_map.insert(p.clone(), FileEntry::from_mode(*bid, mode));
                                }

                                for p in &merged_deleted {
                                    rebuilt_map.remove(p);
                                }

                                if let Ok(rebuilt_tree) = self.git.write_tree_from_map(&rebuilt_map)
                                {
                                    final_head_tree = rebuilt_tree;
                                    final_head_commit = if rebuilt_tree == acc_tree {
                                        merged_commit_id
                                    } else {
                                        self.git
                                            .write_commit(
                                                &rebuilt_tree,
                                                std::slice::from_ref(&merged_commit_id),
                                                &author,
                                                &agent_meta.intent,
                                            )
                                            .unwrap_or(rebased.head_commit)
                                    };
                                }
                            }

                            agent_meta.own_blobs = merged_own;
                            agent_meta.deleted_paths = merged_deleted;
                            agent_meta.base_commit = merged_commit_id;
                            agent_meta.head_commit = final_head_commit;
                            agent_meta.head_tree = final_head_tree;

                            if let Some(before) = snapshot_before {
                                agent_meta
                                    .read_blobs
                                    .retain(|path, bid| before.read_blobs.get(path) != Some(bid));
                            } else {
                                agent_meta.read_blobs.clear();
                            }

                            agent_meta.integrated_peer_blobs.clear();
                            agent_meta.updated_at_ms = now_millis().max(agent_meta.updated_at_ms);
                            rebased_changes.push((agent_meta.change_id.clone(), final_head_commit));
                            applied_rebased_refs.push((
                                aid.clone(),
                                agent_meta.change_id.clone(),
                                final_head_commit,
                            ));
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
                            || (!agent_ids.is_empty() && aid == &caller_id && eff_b == branch);
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

        for (aid, cid, head_cid) in &applied_rebased_refs {
            let _ = self
                .git
                .update_ref(&format!("refs/tessera/agents/{aid}"), head_cid);
            let _ = self
                .git
                .update_ref(&format!("refs/tessera/changes/{}", cid.as_str()), head_cid);
        }

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
        let mut dirty_rebased_ids = BTreeSet::new();
        let mut pending_conflicts: Vec<(String, ConflictRecord)> = Vec::new();

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
                pending_conflicts.push((agent.agent_id.clone(), c));
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

            dirty_rebased_ids.insert(agent.agent_id.clone());
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
            .unwrap_or(trunk_tree_id);

        let mut applied_rebased = Vec::new();
        let mut applied_ids = BTreeSet::new();

        self.oplog.record_tx(
            "sync",
            format!(
                "auto-rebase {} agent(s) onto {active_branch}",
                rebased_agents.len()
            ),
            false,
            |view| {
                applied_rebased.clear();
                applied_ids.clear();

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
                        if a.agent_id == "editor"
                            || a.target_branch
                                .as_deref()
                                .is_none_or(|tb| tb == cfg.trunk_branch)
                        {
                            a.target_branch = Some(current_head_branch.clone());
                            a.base_commit = head_branch_cid;
                            a.head_commit = head_branch_cid;
                            a.head_tree = head_branch_tree;
                            a.read_blobs.clear();
                            a.integrated_peer_blobs.clear();
                        } else if let Some(ref tb) = a.target_branch {
                            if let Some(&tb_cid) = git_branches.get(tb) {
                                if let Ok(tb_commit) = self.git.read_commit(&tb_cid) {
                                    a.base_commit = tb_cid;
                                    a.head_commit = tb_cid;
                                    a.head_tree = tb_commit.tree_id;
                                    a.read_blobs.clear();
                                    a.integrated_peer_blobs.clear();
                                }
                            }
                        }
                    }
                }

                for a in &rebased_agents {
                    let snapshot_before = current.view.agents.get(&a.agent_id);

                    if let Some(updated) = apply_synced_agent_to_view(view, a, snapshot_before) {
                        applied_ids.insert(updated.agent_id.clone());
                        applied_rebased.push(updated);
                    }
                }

                for (aid, c) in &pending_conflicts {
                    if applied_ids.contains(aid) {
                        view.conflicts.insert(c.path.clone(), c.id.clone());
                    }
                }

                Ok(())
            },
        )?;

        for (aid, c) in &pending_conflicts {
            if !applied_ids.contains(aid) {
                self.oplog.delete_conflict(&c.id);
            }
        }

        for agent in &applied_rebased {
            if dirty_rebased_ids.contains(&agent.agent_id) {
                if let Some(ws_path) = &agent.workspace_path {
                    materialize_tree_to_dir(&self.git, &agent.head_tree, Path::new(ws_path))?;
                }
            }

            self.git.update_ref(
                &format!("refs/tessera/agents/{}", agent.agent_id),
                &agent.head_commit,
            )?;
            self.git.update_ref(
                &format!("refs/tessera/changes/{}", agent.change_id.as_str()),
                &agent.head_commit,
            )?;
        }

        self.reconcile_in_flight_sessions();
        let caller_id = detect_session_id(None);
        let _ = self.snapshot_agent(&caller_id, true);
        self.git.sync_git_index(&head_branch_cid);

        Ok(applied_rebased)
    }

    /// Resolves a first-class conflict at `path` using `strategy`, updating the target branch's
    /// working tree (writing to `branch_ws` rather than `repo_root` for off-HEAD branches),
    /// recording the resolved blob in `.git/objects`, and clearing the conflict from the `View`.
    ///
    /// Rejects [`ResolveStrategy::WorkingFile`] if standard Git conflict markers still remain in
    /// the working file, and scopes session updates strictly to agents targeting `active_branch`
    /// without wiping unrelated dirty sessions.
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if no active conflict exists for `path`, or
    /// [`CoreError::InvalidArgument`] if `WorkingFile` still contains conflict markers or
    /// `AstUnion` is applied to a binary conflict.
    pub fn resolve_conflict(&self, path: &str, strategy: ResolveStrategy) -> CoreResult<ObjectId> {
        let current = self.oplog.load_current_reconciled()?;
        let cid = current
            .view
            .conflicts
            .get(path)
            .ok_or_else(|| CoreError::NotFound(format!("no active conflict at '{path}'")))?;
        let record = self.oplog.load_conflict(cid)?;

        let cfg = self.oplog.load_config()?;
        let default_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        let caller_id = detect_session_id(None);
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
                    if record.kind == ConflictKind::Binary {
                        return Err(CoreError::InvalidArgument(
                            "cannot apply ast-union strategy to a binary conflict; choose ours, theirs, or working"
                                .to_string(),
                        ));
                    }

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

                    if is_binary_bytes(&base_bytes)
                        || is_binary_bytes(&ours_bytes)
                        || is_binary_bytes(&theirs_bytes)
                    {
                        return Err(CoreError::InvalidArgument(
                            "cannot apply ast-union strategy to binary file content; choose ours, theirs, or working"
                                .to_string(),
                        ));
                    }

                    let union_str = merge_ast_union(
                        path,
                        &String::from_utf8_lossy(&base_bytes),
                        &String::from_utf8_lossy(&ours_bytes),
                        &String::from_utf8_lossy(&theirs_bytes),
                    );
                    union_str.into_bytes()
                }
                ResolveStrategy::WorkingFile => {
                    let read_root = if active_branch != default_branch
                        && branch_ws.is_dir()
                        && branch_ws.join(path).is_file()
                    {
                        branch_ws.as_path()
                    } else {
                        self.git.repo_root()
                    };
                    let (_, full_path) = resolve_confined_repo_path(read_root, path)?;
                    let bytes = fs::read(&full_path).map_err(|source| CoreError::Io {
                        path: full_path.display().to_string(),
                        source,
                    })?;

                    if !is_binary_bytes(&bytes)
                        && has_git_conflict_markers(&String::from_utf8_lossy(&bytes))
                    {
                        return Err(CoreError::InvalidArgument(format!(
                            "conflict markers still remain in '{path}'; edit the file to resolve them before running `tes resolve --strategy working`"
                        )));
                    }

                    bytes
                }
                ResolveStrategy::ExplicitContent(content) => content.into_bytes(),
            }
        };

        let resolved_blob_id = self.git.write_blob(&resolved_bytes)?;
        let has_uncommitted_owners = current.view.agents.values().any(|a| {
            a.effective_branch(&default_branch) == active_branch.as_str()
                && a.own_blobs.contains_key(path)
        });
        let mut updated_trunk: Option<(String, ObjectId, ObjectId)> = None;

        if path != "<speculative-verify>" {
            let is_head_branch = active_branch == default_branch;
            let (_, root_full_path) = resolve_confined_repo_path(self.git.repo_root(), path)?;
            let was_executable = if is_head_branch {
                worktree_is_executable(&root_full_path)
            } else {
                resolve_confined_repo_path(&branch_ws, path)
                    .ok()
                    .map(|(_, ws_p)| worktree_is_executable(&ws_p))
                    .unwrap_or(false)
            };

            if is_head_branch {
                write_worktree_file_checked(&root_full_path, &resolved_bytes, was_executable)?;

                if branch_ws.is_dir() {
                    write_worktree_rel(&branch_ws, path, &resolved_bytes, was_executable);
                }
            } else if branch_ws.is_dir() {
                let (_, ws_path) = resolve_confined_repo_path(&branch_ws, path)?;
                write_worktree_file_checked(&ws_path, &resolved_bytes, was_executable)?;
            }

            if !has_uncommitted_owners {
                if let Ok(Some(trunk_commit_id)) =
                    self.git.resolve_ref(&format!("refs/heads/{active_branch}"))
                {
                    if let Ok(trunk_commit) = self.git.read_commit(&trunk_commit_id) {
                        if let Ok(mut trunk_files) =
                            self.git.read_tree_to_map(&trunk_commit.tree_id)
                        {
                            if trunk_files.get(path).map(|e| &e.blob_id) != Some(&resolved_blob_id)
                            {
                                let mode = trunk_files.get(path).map_or_else(
                                    || {
                                        if was_executable {
                                            EntryMode::ExecutableFile
                                        } else {
                                            EntryMode::NormalFile
                                        }
                                    },
                                    FileEntry::entry_mode,
                                );
                                let executable = mode == EntryMode::ExecutableFile;

                                if is_head_branch {
                                    set_worktree_executable(&root_full_path, executable);
                                } else if branch_ws.is_dir() {
                                    if let Ok((_, ws_p)) =
                                        resolve_confined_repo_path(&branch_ws, path)
                                    {
                                        set_worktree_executable(&ws_p, executable);
                                    }
                                }

                                trunk_files.insert(
                                    path.to_string(),
                                    FileEntry::from_mode(resolved_blob_id, mode),
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

                                if is_head_branch {
                                    self.git.sync_git_index(&new_commit_id);
                                }

                                if branch_ws.is_dir() {
                                    let safe_b = Self::sanitize_branch_name(&active_branch);
                                    let stamp = self.branches_root().join(format!("{safe_b}.tree"));
                                    let payload = format!("{new_commit_id}:{new_tree_id}");
                                    let _ = atomic_write_file(&stamp, payload.as_bytes());
                                }

                                updated_trunk =
                                    Some((active_branch.clone(), new_commit_id, new_tree_id));
                            }
                        }
                    }
                }
            }
        }

        self.oplog.delete_conflict(cid);

        let head_branch_now = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
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

                        if agent.workspace_path.is_none()
                            && eff_b == branch_name.as_str()
                            && !agent.has_own_changes()
                            && agent.head_commit == agent.base_commit
                        {
                            agent.base_commit = *commit_id;
                            agent.head_commit = *commit_id;
                            agent.head_tree = *tree_id;
                        }
                    }
                }

                for (aid, agent) in view.agents.iter_mut() {
                    if agent.effective_branch(head_branch_now.as_str()) != active_branch.as_str() {
                        continue;
                    }

                    agent.integrated_peer_blobs.remove(path);

                    if agent.read_blobs.contains_key(path) {
                        agent.read_blobs.insert(path.to_string(), resolved_blob_id);
                    }

                    if agent.own_blobs.contains_key(path) {
                        agent.own_blobs.insert(path.to_string(), resolved_blob_id);
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

    /// Walks upward from each path in `touched_paths` inside `tree_id` to identify affected
    /// workspace packages (`Cargo.toml`, `package.json`, `pyproject.toml`, or `go.mod`).
    pub(crate) fn discover_affected_packages(
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
pub(crate) fn apply_co_author_trailer(message: &str, cfg: &core::RepoConfig) -> String {
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
            && !meaningful_intents
                .iter()
                .any(|existing: &String| existing == cleaned)
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

/// Applies a rebased [`AgentMetadata`] entry into `view` inside [`Coordinator::sync_all_agents`],
/// skipping agents that were closed, switched branches, or recorded concurrent file edits since
/// `snapshot_before` was loaded, while preserving concurrent metadata updates (`claimed_symbols`,
/// `intent`, `checkpoints`, `waypoint`, `read_blobs`).
///
/// Returns `Some(updated_agent)` when the rebase was applied, or `None` when skipped.
pub(crate) fn apply_synced_agent_to_view(
    view: &mut core::View,
    rebased: &AgentMetadata,
    snapshot_before: Option<&AgentMetadata>,
) -> Option<AgentMetadata> {
    let live = view.agents.get_mut(&rebased.agent_id)?;

    if let Some(before) = snapshot_before {
        if live.target_branch != before.target_branch
            || live.base_commit != before.base_commit
            || live.own_blobs != before.own_blobs
            || live.deleted_paths != before.deleted_paths
            || live.head_commit != before.head_commit
            || live.head_tree != before.head_tree
        {
            return None;
        }
    }

    live.base_commit = rebased.base_commit;
    live.head_commit = rebased.head_commit;
    live.head_tree = rebased.head_tree;
    live.own_blobs = rebased.own_blobs.clone();
    live.deleted_paths = rebased.deleted_paths.clone();
    live.integrated_peer_blobs.clear();

    if rebased.read_blobs.is_empty() {
        if let Some(before) = snapshot_before {
            live.read_blobs
                .retain(|path, bid| before.read_blobs.get(path) != Some(bid));
        } else {
            live.read_blobs.clear();
        }
    }

    live.updated_at_ms = live.updated_at_ms.max(rebased.updated_at_ms);
    let updated = live.clone();
    view.changes
        .insert(updated.change_id.clone(), updated.head_commit);
    Some(updated)
}
