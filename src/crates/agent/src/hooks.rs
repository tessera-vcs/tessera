//! Pre-tool and post-tool lifecycle hooks (`tes hook pre-tool` / `tes hook post-tool`),
//! directory post-tool expansion, binary-safe 3-way reconciliation, and cross-branch
//! contamination untangling.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use core::{
    ConflictKind, ConflictRecord, CoreError, CoreResult, FileEntry, Merge, ObjectId,
    is_valid_sandbox_repo_root, now_millis, resolve_confined_repo_path,
};
use imara_diff::{Algorithm, Diff, InternedInput};
use merge::{diff_modified_symbols, merge_file_3way};
use serde::{Deserialize, Serialize};
use workspace::is_ignored_or_secret_path;

use crate::Coordinator;
use crate::watcher::{MAX_OBSERVED_BLOBS_PER_PATH, ensure_background_watcher};
use crate::worktree::{
    acquire_file_lock, has_git_conflict_markers, is_binary_bytes, set_worktree_executable,
    worktree_is_executable, write_worktree_file_checked, write_worktree_rel,
};

/// Outcome of `tes hook pre-tool` before an agent reads or edits a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreToolOutcome {
    /// Detected or explicit session ID.
    pub session_id: String,
    /// Repository-relative file path inspected.
    pub path: String,
    /// Optional warning if another active agent session is concurrently editing `path`.
    pub radar_warning: Option<String>,
}

/// Outcome of `tes hook post-tool` after an agent edits a file in the shared working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostToolOutcome {
    /// Detected or explicit session ID.
    pub session_id: String,
    /// Repository-relative file path reconciled.
    pub path: String,
    /// `true` if another session concurrently modified `path` and Tessera performed an automatic 3-way AST merge on disk.
    pub auto_merged_concurrent_edit: bool,
    /// `true` if the concurrent edit had an overlapping conflict recorded in `.git/tessera/conflicts/`.
    pub has_conflict: bool,
    /// Notice message injected back into the AI harness context window.
    pub context_notice: Option<String>,
    /// Current intent string for this session (captured from prompt or inferred from AST).
    pub session_intent: String,
}

/// Returns `true` if `blob_id` matches the committed version of `rel_path` on any known local branch.
fn is_committed_on_any_branch(
    coord: &Coordinator,
    view: &core::View,
    rel_path: &str,
    blob_id: &ObjectId,
) -> bool {
    let mut seen_cids = BTreeSet::new();

    for cid in view.branches.values() {
        seen_cids.insert(*cid);
    }

    for local_br in coord.git().list_local_branches() {
        if let Ok(Some(cid)) = coord.git().resolve_ref(&format!("refs/heads/{local_br}")) {
            seen_cids.insert(cid);
        }
    }

    for cid in seen_cids {
        if let Ok(commit) = coord.git().read_commit(&cid) {
            if let Ok(Some(entry)) = coord
                .git()
                .read_tree_entry_at_path(&commit.tree_id, rel_path)
            {
                if entry.blob_id == *blob_id {
                    return true;
                }
            }
        }
    }

    false
}

/// Handles `tes hook pre-tool`: records the baseline blob of `rel_path` seen by `session_id`
/// before an edit occurs, and warns if another active session is concurrently modifying `rel_path`.
///
/// Serializes `repo_root` baseline reads/writes under `<common_git_dir>/tessera/post_tool.lock`
/// so concurrent `pre-tool` and `post-tool` invocations cannot race on shared working-tree files.
/// Never silently overwrites unowned uncommitted human edits in `repo_root`, preserves executable
/// permissions on any staged baseline restoration, and refreshes stale `read_blobs` when the
/// target branch base has advanced.
///
/// # Errors
/// Returns [`CoreError`] if reading or updating the `OpLog` fails.
pub fn handle_pre_tool_hook(
    coord: &Coordinator,
    session_id: &str,
    rel_path: &str,
) -> CoreResult<PreToolOutcome> {
    let normalized_path = normalize_repo_rel_path(coord.git().repo_root(), rel_path)?;
    let mut meta = coord.ensure_session(session_id, None)?;
    let initial_base_tree_blob = coord
        .git()
        .read_commit(&meta.base_commit)
        .ok()
        .and_then(|c| {
            coord
                .git()
                .read_tree_entry_at_path(&c.tree_id, &normalized_path)
                .ok()
                .flatten()
                .map(|e| e.blob_id)
        });
    let common_git_dir = coord.git().common_git_dir();

    if is_ignored_or_secret_path(
        coord.git().repo_root(),
        common_git_dir,
        &normalized_path,
        initial_base_tree_blob.is_some(),
    ) {
        return Ok(PreToolOutcome {
            session_id: session_id.to_string(),
            path: normalized_path,
            radar_warning: None,
        });
    }

    let _post_tool_lock =
        acquire_file_lock(&common_git_dir.join("tessera").join("post_tool.lock"))?;

    let cur_op_opt = coord.oplog().load_current_reconciled().ok();

    if let Some(latest) = cur_op_opt
        .as_ref()
        .and_then(|op| op.view.agents.get(session_id))
    {
        meta = latest.clone();
    }

    let base_tree_entry = coord
        .git()
        .read_commit(&meta.base_commit)
        .ok()
        .and_then(|c| {
            coord
                .git()
                .read_tree_entry_at_path(&c.tree_id, &normalized_path)
                .ok()
                .flatten()
        });
    let base_tree_blob = base_tree_entry.as_ref().map(|e| e.blob_id);

    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| "main".to_string());
    let my_eff_branch = meta
        .target_branch
        .as_deref()
        .unwrap_or(default_branch.as_str())
        .to_string();

    let branch_tip_cid = coord
        .git()
        .resolve_ref(&format!("refs/heads/{my_eff_branch}"))
        .ok()
        .flatten();
    let branch_tip_entry = branch_tip_cid.and_then(|cid| {
        coord.git().read_commit(&cid).ok().and_then(|c| {
            coord
                .git()
                .read_tree_entry_at_path(&c.tree_id, &normalized_path)
                .ok()
                .flatten()
        })
    });
    let branch_tip_blob = branch_tip_entry.as_ref().map(|e| e.blob_id);

    let full_path = coord.git().repo_root().join(&normalized_path);
    let source_path = coord
        .invocation_dir()
        .map(|d| d.join(&normalized_path))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| full_path.clone());

    let head_cid_opt = coord.git().resolve_head_commit().ok().flatten();
    let head_blob_at_path = head_cid_opt.and_then(|hcid| {
        coord.git().read_commit(&hcid).ok().and_then(|c| {
            coord
                .git()
                .read_tree_entry_at_path(&c.tree_id, &normalized_path)
                .ok()
                .flatten()
                .map(|e| e.blob_id)
        })
    });

    let expected_executable = coord
        .git()
        .read_tree_entry_at_path(&meta.head_tree, &normalized_path)
        .ok()
        .flatten()
        .or(base_tree_entry.clone())
        .or(branch_tip_entry.clone())
        .map_or_else(|| worktree_is_executable(&full_path), |e| e.executable);

    // If `source_path` is in `repo_root` and currently holds an uncommitted blob from a foreign
    // branch session (or `session_id` is on a diverged branch while `repo_root` holds a clean
    // committed tree rather than unowned uncommitted human edits), stage `session_id`'s own
    // branch version before reading.
    if source_path == full_path && full_path.is_file() {
        if let (Ok(disk_bytes), Some(cur_op)) = (fs::read(&full_path), cur_op_opt.as_ref()) {
            if let Ok(disk_blob) = coord.git().write_blob(&disk_bytes) {
                let my_expected = meta
                    .own_blobs
                    .get(&normalized_path)
                    .or(base_tree_blob.as_ref());
                let owned_by_foreign_branch = cur_op.view.agents.values().any(|other| {
                    other.agent_id != session_id
                        && other.effective_branch(&default_branch) != my_eff_branch
                        && other.own_blobs.get(&normalized_path) == Some(&disk_blob)
                });
                let owned_by_any_agent = cur_op
                    .view
                    .agents
                    .values()
                    .any(|a| a.own_blobs.get(&normalized_path) == Some(&disk_blob));
                let disk_is_unowned_human_edit = !owned_by_any_agent
                    && head_blob_at_path != Some(disk_blob)
                    && !is_committed_on_any_branch(
                        coord,
                        &cur_op.view,
                        &normalized_path,
                        &disk_blob,
                    );
                let foreign_peer_awaiting_disk = cur_op.view.agents.values().any(|other| {
                    let is_passive = (other.agent_id.starts_with("term-")
                        || other.agent_id.starts_with("session-")
                        || other.agent_id == "editor")
                        && other.intent_inferred
                        && other.claimed_symbols.is_empty()
                        && !other.has_own_changes();
                    !is_passive
                        && other.agent_id != session_id
                        && other.effective_branch(&default_branch) != my_eff_branch
                        && (other.own_blobs.get(&normalized_path) == Some(&disk_blob)
                            || (!other.owns_path(&normalized_path)
                                && other.read_blobs.get(&normalized_path) == Some(&disk_blob)))
                });
                let diverged_branch_base = my_eff_branch != default_branch
                    && head_cid_opt.is_some_and(|hcid| hcid != meta.base_commit)
                    && !foreign_peer_awaiting_disk
                    && !disk_is_unowned_human_edit;

                if (owned_by_foreign_branch || diverged_branch_base)
                    && my_expected != Some(&disk_blob)
                {
                    if let Some(exp_blob) = my_expected {
                        if let Ok(exp_bytes) = coord.git().read_blob(exp_blob) {
                            let _ = write_worktree_file_checked(
                                &full_path,
                                &exp_bytes,
                                expected_executable,
                            );
                        }
                    }
                }
            }
        }
    } else if source_path == full_path
        && !full_path.is_file()
        && my_eff_branch != default_branch
        && !meta.deleted_paths.contains(&normalized_path)
        && head_blob_at_path.is_none()
        && !cur_op_opt.as_ref().is_some_and(|cur_op| {
            cur_op.view.agents.values().any(|other| {
                other.effective_branch(&default_branch) == my_eff_branch
                    && other.deleted_paths.contains(&normalized_path)
            })
        })
    {
        if let Some(exp_blob) = meta
            .own_blobs
            .get(&normalized_path)
            .or(base_tree_blob.as_ref())
        {
            if let Ok(exp_bytes) = coord.git().read_blob(exp_blob) {
                let _ = write_worktree_file_checked(&full_path, &exp_bytes, expected_executable);
            }
        }
    }

    let current_blob_opt = if source_path.is_file() {
        let bytes = fs::read(&source_path).map_err(|source| CoreError::Io {
            path: source_path.display().to_string(),
            source,
        })?;
        Some(coord.git().write_blob(&bytes)?)
    } else {
        None
    };

    let branch_base_advanced = branch_tip_cid.is_some_and(|cid| cid != meta.base_commit)
        && branch_tip_blob != base_tree_blob;

    let mut warning: Option<String> = None;

    coord.oplog().record_tx(
        session_id,
        format!("pre-tool read baseline for {normalized_path}"),
        true,
        |view| {
            for (other_id, other_agent) in &view.agents {
                let is_passive = (other_id.starts_with("term-")
                    || other_id.starts_with("session-"))
                    && other_agent.intent_inferred
                    && other_agent.claimed_symbols.is_empty()
                    && !other_agent.has_own_changes();
                let other_b = other_agent.effective_branch(&default_branch);

                if other_id != session_id
                    && !is_passive
                    && other_b == my_eff_branch
                    && (other_agent.read_blobs.contains_key(&normalized_path)
                        || other_agent.owns_path(&normalized_path))
                {
                    warning = Some(format!(
                        "Session '{}' (Intent: \"{}\") is also active on '{}'",
                        other_id, other_agent.intent, normalized_path
                    ));
                }
            }

            let refreshed_base_blob = if branch_base_advanced {
                branch_tip_blob.or(base_tree_blob)
            } else {
                base_tree_blob
            };
            let initial_baseline = if my_eff_branch != default_branch || warning.is_some() {
                refreshed_base_blob.or(current_blob_opt)
            } else {
                current_blob_opt.or(refreshed_base_blob)
            };

            if let Some(blob_id) = initial_baseline {
                let obs = view
                    .observed_blobs
                    .entry(normalized_path.clone())
                    .or_default();

                if !obs.contains(&blob_id) {
                    if obs.len() >= MAX_OBSERVED_BLOBS_PER_PATH {
                        obs.remove(0);
                    }

                    obs.push(blob_id);
                }

                if let Some(agent) = view.agents.get_mut(session_id) {
                    let prev_read = agent.read_blobs.get(&normalized_path).copied();
                    let should_refresh = prev_read.is_none()
                        || (!agent.owns_path(&normalized_path)
                            && (branch_base_advanced
                                || (base_tree_blob.is_some() && prev_read != base_tree_blob)));

                    if should_refresh {
                        let next_read = if branch_base_advanced {
                            branch_tip_blob.unwrap_or(blob_id)
                        } else if prev_read.is_some() {
                            base_tree_blob.unwrap_or(blob_id)
                        } else {
                            blob_id
                        };
                        agent.read_blobs.insert(normalized_path.clone(), next_read);
                    }

                    agent.updated_at_ms = now_millis();
                }
            }

            Ok(())
        },
    )?;

    drop(_post_tool_lock);

    ensure_background_watcher(coord.git().repo_root(), common_git_dir);

    Ok(PreToolOutcome {
        session_id: session_id.to_string(),
        path: normalized_path,
        radar_warning: warning,
    })
}

/// Handles `tes hook post-tool`: reconciles an in-place edit to `rel_path` in the shared working
/// directory when multiple sessions edit the same file without separate worktrees.
///
/// Serializes concurrent post-tool invocations via `<common_git_dir>/tessera/post_tool.lock`,
/// handles directory operations by expanding to individual files, checks off-HEAD branch sandboxes
/// before treating missing `repo_root` files as deletions, preserves executable permissions, and
/// records binary conflicts cleanly without corrupting binary bytes.
///
/// # Errors
/// Returns [`CoreError`] if reading/writing blobs or updating the `OpLog` fails.
pub fn handle_post_tool_hook(
    coord: &Coordinator,
    session_id: &str,
    rel_path: &str,
) -> CoreResult<PostToolOutcome> {
    let normalized_path = normalize_repo_rel_path(coord.git().repo_root(), rel_path)?;
    coord.ensure_session(session_id, None)?;

    if let Some(dir_outcome) = reconcile_directory_post_tool(coord, session_id, &normalized_path)? {
        return Ok(dir_outcome);
    }

    handle_post_tool_hook_single(coord, session_id, rel_path, normalized_path)
}

/// Reconciles a single non-directory file under `<common_git_dir>/tessera/post_tool.lock`.
fn handle_post_tool_hook_single(
    coord: &Coordinator,
    session_id: &str,
    raw_rel_path: &str,
    normalized_path: String,
) -> CoreResult<PostToolOutcome> {
    let common_git_dir = coord.git().common_git_dir();
    let _post_tool_lock =
        acquire_file_lock(&common_git_dir.join("tessera").join("post_tool.lock"))?;

    let current_op = coord.oplog().load_current_reconciled()?;
    let my_agent = current_op
        .view
        .agents
        .get(session_id)
        .cloned()
        .ok_or_else(|| CoreError::NotFound(format!("session {session_id}")))?;

    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| "main".to_string());
    let my_eff_branch = my_agent.effective_branch(&default_branch);

    let full_path = coord.git().repo_root().join(&normalized_path);
    let branch_ws = coord.branch_sandbox_dir(my_eff_branch);
    let branch_sb_file = branch_ws.join(&normalized_path);

    let base_commit_obj_opt = coord.git().read_commit(&my_agent.base_commit).ok();
    let base_tree_entry = base_commit_obj_opt.as_ref().and_then(|c| {
        coord
            .git()
            .read_tree_entry_at_path(&c.tree_id, &normalized_path)
            .ok()
            .flatten()
    });
    let base_tree_blob = base_tree_entry.as_ref().map(|e| e.blob_id);

    let session_could_delete =
        my_agent.own_blobs.contains_key(&normalized_path) || base_tree_blob.is_some();

    let raw_p = Path::new(raw_rel_path);
    let mut deleted_in_sandbox = false;
    let mut sandbox_read_source: Option<std::path::PathBuf> = None;

    if raw_p.is_absolute() && raw_p != full_path {
        if raw_p.is_file() {
            if my_eff_branch == default_branch {
                if let Ok(raw_bytes) = fs::read(raw_p) {
                    let raw_exec = worktree_is_executable(raw_p);
                    let _ = write_worktree_file_checked(&full_path, &raw_bytes, raw_exec);
                }
            } else {
                sandbox_read_source = Some(raw_p.to_path_buf());
            }
        } else {
            deleted_in_sandbox = session_could_delete;
        }
    } else if let Some(inv_dir) = coord.invocation_dir() {
        let inv_file = inv_dir.join(&normalized_path);

        if inv_file != full_path {
            if inv_file.is_file() {
                if my_eff_branch == default_branch {
                    if let Ok(inv_bytes) = fs::read(&inv_file) {
                        let inv_exec = worktree_is_executable(&inv_file);
                        let _ = write_worktree_file_checked(&full_path, &inv_bytes, inv_exec);
                    }
                } else {
                    sandbox_read_source = Some(inv_file);
                }
            } else {
                deleted_in_sandbox = session_could_delete;
            }
        }
    }

    // Check `branch_sandbox_dir(my_eff_branch)` BEFORE treating `!full_path.is_file()` as a deletion
    // so off-HEAD branch sandboxes can create or edit files that do not exist on the checked-out branch.
    let off_head_sandbox_has_file =
        my_eff_branch != default_branch && branch_sb_file.is_file() && branch_sb_file != full_path;
    let effective_file_exists =
        full_path.is_file() || sandbox_read_source.is_some() || off_head_sandbox_has_file;

    if !effective_file_exists || deleted_in_sandbox {
        let meta = coord.record_single_path_edit(session_id, &normalized_path, None)?;

        if my_eff_branch == default_branch && full_path.is_file() {
            let _ = fs::remove_file(&full_path);
        }

        if branch_ws.is_dir() {
            let _ = fs::remove_file(&branch_sb_file);
        }

        return Ok(PostToolOutcome {
            session_id: session_id.to_string(),
            path: normalized_path,
            auto_merged_concurrent_edit: false,
            has_conflict: false,
            context_notice: None,
            session_intent: meta.intent,
        });
    }

    if is_ignored_or_secret_path(
        coord.git().repo_root(),
        common_git_dir,
        &normalized_path,
        base_tree_blob.is_some(),
    ) {
        return Ok(PostToolOutcome {
            session_id: session_id.to_string(),
            path: normalized_path,
            auto_merged_concurrent_edit: false,
            has_conflict: false,
            context_notice: None,
            session_intent: my_agent.intent,
        });
    }

    let base_blob = base_tree_blob.or_else(|| my_agent.read_blobs.get(&normalized_path).copied());

    let mut edited_in_branch_sandbox = false;
    let (mut disk_after_bytes, mut executable) = if let Some(ref src_path) = sandbox_read_source {
        edited_in_branch_sandbox = true;
        let bytes = fs::read(src_path).map_err(|source| CoreError::Io {
            path: src_path.display().to_string(),
            source,
        })?;
        let exec = worktree_is_executable(src_path);
        (bytes, exec)
    } else if full_path.is_file() {
        let bytes = fs::read(&full_path).map_err(|source| CoreError::Io {
            path: full_path.display().to_string(),
            source,
        })?;
        let exec = if cfg!(unix) {
            worktree_is_executable(&full_path)
        } else {
            base_tree_entry.as_ref().is_some_and(|e| e.executable)
        };
        (bytes, exec)
    } else {
        edited_in_branch_sandbox = true;
        let bytes = fs::read(&branch_sb_file).map_err(|source| CoreError::Io {
            path: branch_sb_file.display().to_string(),
            source,
        })?;
        let exec = if cfg!(unix) {
            worktree_is_executable(&branch_sb_file)
        } else {
            base_tree_entry.as_ref().is_some_and(|e| e.executable)
        };
        (bytes, exec)
    };

    let mut disk_after_blob = coord.git().write_blob(&disk_after_bytes)?;

    if !edited_in_branch_sandbox && off_head_sandbox_has_file {
        if let Ok(sb_bytes) = fs::read(&branch_sb_file) {
            if let Ok(sb_blob) = coord.git().write_blob(&sb_bytes) {
                let prev_expected = my_agent
                    .own_blobs
                    .get(&normalized_path)
                    .or(base_blob.as_ref());

                if Some(&sb_blob) != prev_expected && Some(&sb_blob) != base_blob.as_ref() {
                    disk_after_bytes = sb_bytes;
                    disk_after_blob = sb_blob;
                    edited_in_branch_sandbox = true;

                    if cfg!(unix) {
                        executable = worktree_is_executable(&branch_sb_file);
                    }
                }
            }
        }
    }

    let base_bytes = match &base_blob {
        Some(bid) => coord.git().read_blob(bid)?,
        None => Vec::new(),
    };
    let disk_is_binary = is_binary_bytes(&disk_after_bytes);
    let base_is_binary = is_binary_bytes(&base_bytes);
    let file_is_binary = disk_is_binary || base_is_binary;

    let disk_after_str = if disk_is_binary {
        String::new()
    } else {
        String::from_utf8_lossy(&disk_after_bytes).into_owned()
    };
    let base_str = if base_is_binary {
        String::new()
    } else {
        String::from_utf8_lossy(&base_bytes).into_owned()
    };

    let is_active_peer = |other_id: &str, other_agent: &core::AgentMetadata| -> bool {
        if other_id == session_id {
            return false;
        }

        let is_passive = (other_id.starts_with("term-") || other_id.starts_with("session-"))
            && other_agent.intent_inferred
            && other_agent.claimed_symbols.is_empty()
            && !other_agent.has_own_changes();

        if is_passive {
            return false;
        }

        other_agent.owns_path(&normalized_path)
            || other_agent.read_blobs.contains_key(&normalized_path)
            || other_agent
                .claimed_symbols
                .iter()
                .any(|s| s.split("::").next().unwrap_or(s.as_str()).trim() == normalized_path)
    };

    let is_same_branch_peer = |other_id: &str, other_agent: &core::AgentMetadata| -> bool {
        if !is_active_peer(other_id, other_agent) {
            return false;
        }

        let other_eff_branch = other_agent.effective_branch(&default_branch);
        other_eff_branch == my_eff_branch
    };

    let mut foreign_blobs: Vec<ObjectId> = Vec::new();

    for (other_id, other_agent) in &current_op.view.agents {
        if !is_active_peer(other_id, other_agent) {
            continue;
        }

        if let Some(peer_own_blob) = other_agent.own_blobs.get(&normalized_path) {
            if Some(peer_own_blob) != base_blob.as_ref()
                && *peer_own_blob != disk_after_blob
                && !foreign_blobs.contains(peer_own_blob)
            {
                foreign_blobs.push(*peer_own_blob);
            }
        }
    }

    let trunk_branch = coord
        .oplog()
        .load_config()
        .map(|c| c.trunk_branch)
        .unwrap_or_else(|_| default_branch.clone());

    if my_eff_branch != default_branch {
        for (b_name, b_cid) in &current_op.view.branches {
            if b_name == my_eff_branch
                || b_name == &default_branch
                || b_name == &trunk_branch
                || *b_cid == my_agent.base_commit
            {
                continue;
            }

            if let Ok(b_commit) = coord.git().read_commit(b_cid) {
                if let Ok(Some(b_entry)) = coord
                    .git()
                    .read_tree_entry_at_path(&b_commit.tree_id, &normalized_path)
                {
                    if Some(&b_entry.blob_id) != base_blob.as_ref()
                        && b_entry.blob_id != disk_after_blob
                        && !foreign_blobs.contains(&b_entry.blob_id)
                    {
                        foreign_blobs.push(b_entry.blob_id);
                    }
                }
            }
        }
    }

    let mut effective_my_blob = disk_after_blob;
    let mut current_my_str = disk_after_str.clone();
    let mut untangled_cross_branch = false;

    let disk_has_markers = !disk_is_binary && has_git_conflict_markers(&disk_after_str);

    let disk_is_foreign_peer_own_blob =
        current_op
            .view
            .agents
            .iter()
            .any(|(other_id, other_agent)| {
                is_active_peer(other_id, other_agent)
                    && !is_same_branch_peer(other_id, other_agent)
                    && other_agent.own_blobs.get(&normalized_path) == Some(&disk_after_blob)
            })
            || foreign_blobs.contains(&disk_after_blob)
            || (my_eff_branch != default_branch && Some(&disk_after_blob) == base_blob.as_ref());

    if disk_is_foreign_peer_own_blob || disk_has_markers {
        if let Some(existing_own) = my_agent.own_blobs.get(&normalized_path) {
            if *existing_own != disk_after_blob {
                if let Ok(existing_bytes) = coord.git().read_blob(existing_own) {
                    if is_binary_bytes(&existing_bytes) {
                        effective_my_blob = *existing_own;

                        if disk_is_foreign_peer_own_blob {
                            untangled_cross_branch = true;
                        }
                    } else {
                        let existing_str = String::from_utf8_lossy(&existing_bytes).into_owned();

                        if !has_git_conflict_markers(&existing_str) {
                            effective_my_blob = *existing_own;
                            current_my_str = existing_str;

                            if disk_is_foreign_peer_own_blob {
                                untangled_cross_branch = true;
                            }
                        }
                    }
                }
            }
        } else if disk_is_foreign_peer_own_blob && !file_is_binary {
            if let Some(obs_list) = current_op.view.observed_blobs.get(&normalized_path) {
                let mut best_rescued: Option<(isize, usize, ObjectId, String)> = None;

                for obs_blob in obs_list.iter().rev() {
                    let owned_by_other = current_op
                        .view
                        .agents
                        .values()
                        .any(|a| a.own_blobs.get(&normalized_path) == Some(obs_blob));

                    if *obs_blob != disk_after_blob
                        && Some(obs_blob) != base_blob.as_ref()
                        && Some(obs_blob) != base_tree_blob.as_ref()
                        && !foreign_blobs.contains(obs_blob)
                        && !owned_by_other
                    {
                        if let Ok(obs_bytes) = coord.git().read_blob(obs_blob) {
                            if is_binary_bytes(&obs_bytes) {
                                continue;
                            }

                            let obs_str = String::from_utf8_lossy(&obs_bytes).into_owned();

                            if !has_git_conflict_markers(&obs_str) {
                                let my_score = score_blob_for_agent(
                                    &base_str,
                                    &obs_str,
                                    &my_agent.intent,
                                    Some(my_eff_branch),
                                );
                                let max_other_score = current_op
                                    .view
                                    .agents
                                    .iter()
                                    .filter(|(other_id, other_agent)| {
                                        is_active_peer(other_id, other_agent)
                                            && !is_same_branch_peer(other_id, other_agent)
                                            && !other_agent.owns_path(&normalized_path)
                                    })
                                    .map(|(_, other_agent)| {
                                        score_blob_for_agent(
                                            &base_str,
                                            &obs_str,
                                            &other_agent.intent,
                                            Some(other_agent.effective_branch(&default_branch)),
                                        )
                                    })
                                    .max()
                                    .unwrap_or(0);
                                let net_score = (my_score as isize) - (max_other_score as isize);

                                if best_rescued
                                    .as_ref()
                                    .is_none_or(|(best_net, best_raw, _, _)| {
                                        (net_score, my_score) > (*best_net, *best_raw)
                                    })
                                {
                                    best_rescued = Some((net_score, my_score, *obs_blob, obs_str));
                                }
                            }
                        }
                    }
                }

                if let Some((_, _, rescued_blob, rescued_str)) = best_rescued {
                    effective_my_blob = rescued_blob;
                    current_my_str = rescued_str;
                    untangled_cross_branch = true;
                }
            }
        }
    }

    let my_prev_blob = my_agent
        .own_blobs
        .get(&normalized_path)
        .cloned()
        .or(base_blob);
    let my_prev_bytes = match &my_prev_blob {
        Some(bid) => coord.git().read_blob(bid).unwrap_or_default(),
        None => Vec::new(),
    };
    let my_prev_is_binary = is_binary_bytes(&my_prev_bytes);
    let my_prev_str = if my_prev_is_binary {
        String::new()
    } else {
        String::from_utf8_lossy(&my_prev_bytes).into_owned()
    };

    let mut stripped_peer_blobs: BTreeSet<ObjectId> = BTreeSet::new();

    if !file_is_binary && !my_prev_is_binary {
        for peer_own_blob in &foreign_blobs {
            if let Ok(peer_bytes) = coord.git().read_blob(peer_own_blob) {
                if is_binary_bytes(&peer_bytes) {
                    continue;
                }

                let peer_str = String::from_utf8_lossy(&peer_bytes);

                if is_contaminated_by_foreign_peer(
                    &base_str,
                    &my_prev_str,
                    &peer_str,
                    &current_my_str,
                ) {
                    let stripped = merge_file_3way(
                        &normalized_path,
                        &peer_str,
                        &my_prev_str,
                        &current_my_str,
                        session_id,
                        session_id,
                    );
                    let isolated_candidate =
                        if !stripped.has_conflicts && stripped.content != base_str {
                            Some(stripped.content)
                        } else {
                            strip_peer_inserted_lines(
                                &base_str,
                                &my_prev_str,
                                &peer_str,
                                &current_my_str,
                            )
                            .filter(|s| s != &base_str)
                        };

                    if let Some(clean_str) = isolated_candidate {
                        if let Ok(isolated_id) = coord.git().write_blob(clean_str.as_bytes()) {
                            effective_my_blob = isolated_id;
                            current_my_str = clean_str;
                            untangled_cross_branch = true;
                            stripped_peer_blobs.insert(*peer_own_blob);
                        }
                    }
                }
            }
        }
    }

    let any_active_peer_missing_own_blob =
        current_op
            .view
            .agents
            .iter()
            .any(|(other_id, other_agent)| {
                is_active_peer(other_id, other_agent) && !other_agent.owns_path(&normalized_path)
            });
    let mut assigned_own_blobs: BTreeMap<String, ObjectId> = BTreeMap::new();

    if effective_my_blob != disk_after_blob {
        assigned_own_blobs.insert(session_id.to_string(), effective_my_blob);
    }

    if any_active_peer_missing_own_blob && !file_is_binary {
        let mut attribution_blobs: Vec<ObjectId> = Vec::new();

        if let Some(obs_list) = current_op.view.observed_blobs.get(&normalized_path) {
            for obs_blob in obs_list {
                if Some(obs_blob) != base_blob.as_ref()
                    && Some(obs_blob) != base_tree_blob.as_ref()
                    && !foreign_blobs.contains(obs_blob)
                    && !attribution_blobs.contains(obs_blob)
                {
                    attribution_blobs.push(*obs_blob);
                }
            }
        }

        for extra_blob in [effective_my_blob, disk_after_blob] {
            if Some(&extra_blob) != base_blob.as_ref()
                && Some(&extra_blob) != base_tree_blob.as_ref()
                && !foreign_blobs.contains(&extra_blob)
                && !attribution_blobs.contains(&extra_blob)
            {
                attribution_blobs.push(extra_blob);
            }
        }

        if attribution_blobs.len() >= 2 {
            for (aid, agent_meta) in &current_op.view.agents {
                if aid != session_id && !is_active_peer(aid, agent_meta) {
                    continue;
                }

                if aid == session_id && assigned_own_blobs.contains_key(session_id) {
                    continue;
                }

                if aid != session_id && agent_meta.owns_path(&normalized_path) {
                    continue;
                }

                let mut best_blob: Option<(usize, ObjectId)> = None;

                for cand_blob in &attribution_blobs {
                    if effective_my_blob != disk_after_blob
                        && *cand_blob == effective_my_blob
                        && aid != session_id
                    {
                        continue;
                    }

                    if let Ok(cand_bytes) = coord.git().read_blob(cand_blob) {
                        if is_binary_bytes(&cand_bytes) {
                            continue;
                        }

                        let cand_str = String::from_utf8_lossy(&cand_bytes);
                        let score = score_blob_for_agent(
                            &base_str,
                            &cand_str,
                            &agent_meta.intent,
                            Some(agent_meta.effective_branch(&default_branch)),
                        );

                        if score > 0 && best_blob.as_ref().is_none_or(|(best_s, _)| score > *best_s)
                        {
                            best_blob = Some((score, *cand_blob));
                        }
                    }
                }

                if let Some((_, matched_blob)) = best_blob {
                    assigned_own_blobs.insert(aid.clone(), matched_blob);
                }
            }

            if assigned_own_blobs.len() >= 2 {
                untangled_cross_branch = true;
            }
        }
    }

    if !assigned_own_blobs.contains_key(session_id) {
        assigned_own_blobs.insert(session_id.to_string(), effective_my_blob);
    }

    let my_own_blob = *assigned_own_blobs
        .get(session_id)
        .or_else(|| my_agent.own_blobs.get(&normalized_path))
        .unwrap_or(&effective_my_blob);

    let mut candidate_blobs: Vec<(String, String, ObjectId)> = Vec::new();
    let push_candidate =
        |list: &mut Vec<(String, String, ObjectId)>, id: String, intent: String, blob: ObjectId| {
            if Some(&blob) != base_blob.as_ref()
                && Some(&blob) != base_tree_blob.as_ref()
                && !list.iter().any(|(_, _, b)| *b == blob)
            {
                list.push((id, intent, blob));
            }
        };

    let mut reconciled_head_commit: Option<ObjectId> = None;
    let mut head_changed_norm_path = false;
    let target_branch_ref = format!("refs/heads/{my_eff_branch}");
    let head_commit_opt = coord
        .git()
        .resolve_ref(&target_branch_ref)
        .ok()
        .flatten()
        .or_else(|| coord.git().resolve_head_commit().ok().flatten());

    let resolving_existing_conflict =
        current_op.view.conflicts.contains_key(&normalized_path) && !disk_has_markers;

    let has_concurrent_same_branch_peer = head_commit_opt.as_ref() != Some(&my_agent.base_commit)
        || current_op
            .view
            .agents
            .iter()
            .any(|(other_id, other_agent)| is_same_branch_peer(other_id, other_agent));

    let any_same_branch_peer_missing_own_blob =
        current_op
            .view
            .agents
            .iter()
            .any(|(other_id, other_agent)| {
                is_same_branch_peer(other_id, other_agent)
                    && !other_agent.owns_path(&normalized_path)
            });

    if !resolving_existing_conflict
        && has_concurrent_same_branch_peer
        && any_same_branch_peer_missing_own_blob
    {
        if let Some(obs_list) = current_op.view.observed_blobs.get(&normalized_path) {
            for obs_blob in obs_list {
                let assigned_to_foreign = assigned_own_blobs.iter().any(|(aid, b)| {
                    b == obs_blob
                        && aid != session_id
                        && current_op
                            .view
                            .agents
                            .get(aid)
                            .is_some_and(|a| !is_same_branch_peer(aid, a))
                });

                if *obs_blob != my_own_blob
                    && my_agent.own_blobs.get(&normalized_path) != Some(obs_blob)
                    && !foreign_blobs.contains(obs_blob)
                    && !assigned_to_foreign
                {
                    push_candidate(
                        &mut candidate_blobs,
                        "peer".to_string(),
                        "concurrent edit".to_string(),
                        *obs_blob,
                    );
                }
            }
        }
    }

    let mut peers_integrated_now: BTreeMap<String, ObjectId> = BTreeMap::new();
    let integrated_peers = my_agent.integrated_peer_blobs.get(&normalized_path);

    if !resolving_existing_conflict {
        for (other_id, other_agent) in &current_op.view.agents {
            if !is_same_branch_peer(other_id, other_agent) {
                continue;
            }

            if let Some(other_own) = other_agent
                .own_blobs
                .get(&normalized_path)
                .or_else(|| assigned_own_blobs.get(other_id))
            {
                let candidate_opt = if base_tree_blob.is_some()
                    || stripped_peer_blobs.contains(other_own)
                {
                    Some(*other_own)
                } else {
                    match integrated_peers.and_then(|m| m.get(other_id)) {
                        Some(seen) if seen == other_own => None,
                        Some(seen) => {
                            let seen_bytes = coord.git().read_blob(seen).unwrap_or_default();
                            let peer_bytes = coord.git().read_blob(other_own).unwrap_or_default();

                            if file_is_binary
                                || is_binary_bytes(&seen_bytes)
                                || is_binary_bytes(&peer_bytes)
                            {
                                Some(*other_own)
                            } else {
                                let seen_str = String::from_utf8_lossy(&seen_bytes);
                                let peer_str = String::from_utf8_lossy(&peer_bytes);
                                let rebased = merge_file_3way(
                                    &normalized_path,
                                    &seen_str,
                                    &base_str,
                                    &peer_str,
                                    "base",
                                    other_id,
                                );
                                coord.git().write_blob(rebased.content.as_bytes()).ok()
                            }
                        }
                        None => Some(*other_own),
                    }
                };
                peers_integrated_now.insert(other_id.clone(), *other_own);

                if let Some(cand_blob) = candidate_opt {
                    push_candidate(
                        &mut candidate_blobs,
                        other_id.clone(),
                        other_agent.intent.clone(),
                        cand_blob,
                    );
                }
            } else if let Some(other_blob) = other_agent.read_blobs.get(&normalized_path) {
                if !other_agent.deleted_paths.contains(&normalized_path) {
                    peers_integrated_now.insert(other_id.clone(), *other_blob);

                    if *other_blob != my_own_blob
                        && my_agent.own_blobs.get(&normalized_path) != Some(other_blob)
                        && integrated_peers.and_then(|m| m.get(other_id)) != Some(other_blob)
                    {
                        push_candidate(
                            &mut candidate_blobs,
                            other_id.clone(),
                            other_agent.intent.clone(),
                            *other_blob,
                        );
                    }
                }
            }
        }
    }

    if let Some(head_commit_id) = head_commit_opt {
        if head_commit_id != my_agent.base_commit {
            reconciled_head_commit = Some(head_commit_id);
        }

        if !resolving_existing_conflict {
            if let Ok(head_commit) = coord.git().read_commit(&head_commit_id) {
                if let Ok(Some(head_entry)) = coord
                    .git()
                    .read_tree_entry_at_path(&head_commit.tree_id, &normalized_path)
                {
                    if Some(&head_entry.blob_id) != base_blob.as_ref()
                        && head_entry.blob_id != my_own_blob
                    {
                        let head_bytes = coord
                            .git()
                            .read_blob(&head_entry.blob_id)
                            .unwrap_or_default();
                        let already_based_on_head =
                            if file_is_binary || is_binary_bytes(&head_bytes) {
                                false
                            } else {
                                let head_str = String::from_utf8_lossy(&head_bytes);
                                has_git_conflict_markers(&head_str)
                                    || is_contaminated_by_foreign_peer(
                                        &base_str,
                                        &base_str,
                                        &head_str,
                                        &current_my_str,
                                    )
                            };

                        if !already_based_on_head {
                            let branch_name = my_eff_branch.to_string();
                            let summary = head_commit
                                .message
                                .lines()
                                .next()
                                .unwrap_or("merged commit")
                                .to_string();
                            push_candidate(
                                &mut candidate_blobs,
                                branch_name,
                                summary,
                                head_entry.blob_id,
                            );
                            head_changed_norm_path = true;
                        }

                        reconciled_head_commit = Some(head_commit_id);
                    }
                }
            }
        }
    }

    let current_my_has_markers = !file_is_binary && has_git_conflict_markers(&current_my_str);

    if !current_my_has_markers {
        push_candidate(
            &mut candidate_blobs,
            session_id.to_string(),
            my_agent.intent.clone(),
            my_own_blob,
        );
    }

    let my_own_bytes = coord
        .git()
        .read_blob(&my_own_blob)
        .unwrap_or_else(|_| disk_after_bytes.clone());
    let my_own_is_binary = is_binary_bytes(&my_own_bytes);

    let touched_syms = if file_is_binary || my_own_is_binary {
        Vec::new()
    } else {
        let my_own_str = String::from_utf8_lossy(&my_own_bytes);
        diff_modified_symbols(&normalized_path, &base_str, &my_own_str)
    };

    let mut final_blob = if disk_has_markers {
        disk_after_blob
    } else {
        my_own_blob
    };
    let mut final_disk_bytes = if disk_has_markers {
        disk_after_bytes.clone()
    } else {
        my_own_bytes.clone()
    };
    let mut auto_merged = untangled_cross_branch;
    let mut has_conflict = disk_has_markers;
    let mut context_notice = None;
    let mut conflict_to_save: Option<ConflictRecord> = None;
    let mut merged_peer_blobs: Vec<ObjectId> = Vec::new();

    let waiting_peer_baseline_blob: Option<ObjectId> = if my_eff_branch != default_branch {
        current_op
            .view
            .agents
            .iter()
            .filter(|(other_id, other)| {
                is_active_peer(other_id, other)
                    && !is_same_branch_peer(other_id, other)
                    && (other.targets_head_branch(&default_branch)
                        || (!other.owns_path(&normalized_path)
                            && !assigned_own_blobs.contains_key(*other_id)))
            })
            .find_map(|(_, other)| {
                other
                    .own_blobs
                    .get(&normalized_path)
                    .or_else(|| other.read_blobs.get(&normalized_path))
                    .copied()
                    .or(base_blob)
            })
    } else {
        None
    };
    let preserve_head_baseline_for_peer = waiting_peer_baseline_blob.is_some();

    if candidate_blobs.len() >= 2 {
        let (first_id, first_intent, first_blob_id) = &candidate_blobs[0];
        let first_bytes = coord.git().read_blob(first_blob_id)?;
        let any_candidate_binary = file_is_binary
            || is_binary_bytes(&first_bytes)
            || candidate_blobs.iter().skip(1).any(|(_, _, bid)| {
                coord
                    .git()
                    .read_blob(bid)
                    .is_ok_and(|bytes| is_binary_bytes(&bytes))
            });

        if any_candidate_binary {
            // Never run UTF-8 lossy conversion or text 3-way merge on binary blobs.
            let (second_id, second_intent, second_blob_id) = &candidate_blobs[1];
            merged_peer_blobs.push(*first_blob_id);
            merged_peer_blobs.push(*second_blob_id);
            has_conflict = true;
            let blob_merge = Merge {
                removes: vec![base_blob],
                adds: vec![Some(*first_blob_id), Some(*second_blob_id)],
            };
            let cid = ConflictRecord::compute_id(&normalized_path, None, &blob_merge);
            let mut intents = BTreeMap::new();
            intents.insert(first_id.clone(), first_intent.clone());
            intents.insert(second_id.clone(), second_intent.clone());
            intents.insert(session_id.to_string(), my_agent.intent.clone());

            let record = ConflictRecord {
                id: cid,
                path: normalized_path.clone(),
                symbol: None,
                kind: ConflictKind::Binary,
                merge: blob_merge,
                agent_intents: intents,
                base_snippet: String::new(),
                left_snippet: String::new(),
                right_snippet: String::new(),
                diagnostics: Some(
                    "Concurrent modifications to binary file; resolve with `tes resolve --strategy ours|theirs`."
                        .to_string(),
                ),
                created_at_ms: now_millis(),
            };
            coord.oplog().save_conflict(&record)?;
            conflict_to_save = Some(record);
            context_notice = Some(format!(
                "[Tessera Binary Conflict]: Concurrent edit on binary file '{}' with session '{}'. Resolve with `tes resolve {} --strategy ours|theirs`.",
                normalized_path, second_id, normalized_path
            ));
        } else {
            let mut acc_str = String::from_utf8_lossy(&first_bytes).into_owned();
            let mut acc_blob_id = *first_blob_id;
            let mut acc_label = format!("{first_id} ({first_intent})");
            merged_peer_blobs.push(*first_blob_id);

            for (next_id, next_intent, next_blob_id) in candidate_blobs.iter().skip(1) {
                merged_peer_blobs.push(*next_blob_id);
                let next_bytes = coord.git().read_blob(next_blob_id)?;
                let next_str = String::from_utf8_lossy(&next_bytes);
                let next_label = format!("{next_id} ({next_intent})");

                let merge_res = merge_file_3way(
                    &normalized_path,
                    &base_str,
                    &acc_str,
                    &next_str,
                    &acc_label,
                    &next_label,
                );

                acc_str = merge_res.content;
                acc_blob_id = coord.git().write_blob(acc_str.as_bytes())?;
                acc_label = format!("{acc_label} + {next_label}");
                auto_merged = true;

                if merge_res.has_conflicts {
                    has_conflict = true;
                    let symbol = merge_res.conflicted_symbols.first().cloned();
                    let blob_merge = Merge {
                        removes: vec![base_blob],
                        adds: vec![Some(*first_blob_id), Some(*next_blob_id)],
                    };
                    let cid = ConflictRecord::compute_id(
                        &normalized_path,
                        symbol.as_deref(),
                        &blob_merge,
                    );
                    let mut intents = BTreeMap::new();
                    intents.insert(first_id.clone(), first_intent.clone());
                    intents.insert(session_id.to_string(), my_agent.intent.clone());

                    let record = ConflictRecord {
                        id: cid,
                        path: normalized_path.clone(),
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
                    coord.oplog().save_conflict(&record)?;
                    conflict_to_save = Some(record);

                    context_notice = Some(format!(
                        "[Tessera Conflict]: Concurrent edit on '{}' with session '{}' (Intent: \"{}\") overlapped on {:?}. Inspect with `tes conflicts --json`.",
                        normalized_path, next_id, next_intent, merge_res.conflicted_symbols
                    ));
                }
            }

            let default_branch_peer_owns_path = edited_in_branch_sandbox
                && current_op.view.agents.values().any(|other| {
                    other.agent_id != session_id
                        && other.targets_head_branch(&default_branch)
                        && other.owns_path(&normalized_path)
                });
            let skip_repo_root_write = edited_in_branch_sandbox && my_eff_branch != default_branch;

            if !default_branch_peer_owns_path
                && !preserve_head_baseline_for_peer
                && !skip_repo_root_write
            {
                write_worktree_file_checked(&full_path, acc_str.as_bytes(), executable)?;
            } else if let Some(b_id) = waiting_peer_baseline_blob {
                if !skip_repo_root_write && disk_after_blob != b_id && !disk_has_markers {
                    if let Ok(peer_base_bytes) = coord.git().read_blob(&b_id) {
                        let peer_exec = base_tree_entry
                            .as_ref()
                            .map_or(executable, |e| e.executable);
                        let _ =
                            write_worktree_file_checked(&full_path, &peer_base_bytes, peer_exec);
                    }
                }
            }

            final_disk_bytes = acc_str.into_bytes();
            final_blob = acc_blob_id;

            if !has_conflict {
                context_notice = Some(format!(
                    "[Tessera Auto-Merge]: Cleanly AST-merged concurrent edits to '{}'.",
                    normalized_path
                ));
            }
        }
    } else if let Some(b_id) = waiting_peer_baseline_blob {
        let skip_repo_root_write = edited_in_branch_sandbox && my_eff_branch != default_branch;

        if !skip_repo_root_write && disk_after_blob != b_id && !disk_has_markers {
            if let Ok(peer_base_bytes) = coord.git().read_blob(&b_id) {
                let peer_exec = base_tree_entry
                    .as_ref()
                    .map_or(executable, |e| e.executable);
                let _ = write_worktree_file_checked(&full_path, &peer_base_bytes, peer_exec);
            }
        }
    } else if my_own_blob != disk_after_blob && !disk_has_markers {
        let default_branch_peer_owns_path = edited_in_branch_sandbox
            && current_op.view.agents.values().any(|other| {
                other.agent_id != session_id
                    && other.targets_head_branch(&default_branch)
                    && other.owns_path(&normalized_path)
            });
        let skip_repo_root_write = edited_in_branch_sandbox && my_eff_branch != default_branch;

        if !default_branch_peer_owns_path && !skip_repo_root_write {
            let _ = write_worktree_file_checked(&full_path, &my_own_bytes, executable);
        }
    }

    for sym in &my_agent.claimed_symbols {
        let claimed_file = sym.split("::").next().unwrap_or(sym.as_str()).trim();

        if claimed_file.is_empty()
            || claimed_file == normalized_path
            || my_agent.deleted_paths.contains(claimed_file)
        {
            continue;
        }

        let Ok((_, claimed_full)) =
            resolve_confined_repo_path(coord.git().repo_root(), claimed_file)
        else {
            continue;
        };

        if !claimed_full.is_file() {
            continue;
        }

        if let Ok(c_bytes) = fs::read(&claimed_full) {
            if let Ok(c_blob) = coord.git().write_blob(&c_bytes) {
                let claimed_base_entry = base_commit_obj_opt.as_ref().and_then(|c| {
                    coord
                        .git()
                        .read_tree_entry_at_path(&c.tree_id, claimed_file)
                        .ok()
                        .flatten()
                });
                let claimed_base_blob = claimed_base_entry.as_ref().map(|e| e.blob_id);
                let my_expected = my_agent
                    .own_blobs
                    .get(claimed_file)
                    .copied()
                    .or(claimed_base_blob);
                let is_foreign = current_op.view.agents.values().any(|other| {
                    let other_b = other.effective_branch(&default_branch);
                    other.agent_id != session_id
                        && other_b != my_eff_branch
                        && other.own_blobs.get(claimed_file) == Some(&c_blob)
                });

                if is_foreign && my_expected != Some(c_blob) {
                    if let Some(exp_blob) = &my_expected {
                        if let Ok(exp_bytes) = coord.git().read_blob(exp_blob) {
                            let exp_exec = claimed_base_entry.as_ref().map_or_else(
                                || worktree_is_executable(&claimed_full),
                                |e| e.executable,
                            );
                            let _ =
                                write_worktree_file_checked(&claimed_full, &exp_bytes, exp_exec);
                        }
                    }
                }
            }
        }
    }

    let mut rebased_session_own_blobs: Option<BTreeMap<String, ObjectId>> = None;
    let mut rebased_session_deleted_paths: Option<BTreeSet<String>> = None;

    if !has_conflict {
        if let Some(new_base) = &reconciled_head_commit {
            if let (Ok(old_base_c), Ok(new_base_c)) = (
                coord.git().read_commit(&my_agent.base_commit),
                coord.git().read_commit(new_base),
            ) {
                if let (Ok(old_map), Ok(new_map)) = (
                    coord.git().read_tree_to_map(&old_base_c.tree_id),
                    coord.git().read_tree_to_map(&new_base_c.tree_id),
                ) {
                    let mut candidate_own = my_agent.own_blobs.clone();
                    candidate_own.insert(normalized_path.clone(), my_own_blob);

                    let mut rebased_map = BTreeMap::new();
                    let mut all_rebased_cleanly = true;

                    for (p, own_bid) in &candidate_own {
                        let old_bid = old_map.get(p).map(|e| e.blob_id);
                        let new_bid = new_map.get(p).map(|e| e.blob_id);

                        if old_bid == new_bid {
                            if p == &normalized_path || new_bid.as_ref() != Some(own_bid) {
                                rebased_map.insert(p.clone(), *own_bid);
                            }

                            continue;
                        }

                        if p != &normalized_path
                            && (Some(*own_bid) == old_bid || Some(*own_bid) == new_bid)
                        {
                            continue;
                        }

                        if p == &normalized_path
                            && head_changed_norm_path
                            && foreign_blobs.is_empty()
                            && candidate_blobs.len() <= 2
                        {
                            rebased_map.insert(p.clone(), final_blob);
                            continue;
                        }

                        let old_b = old_bid
                            .and_then(|id| coord.git().read_blob(&id).ok())
                            .unwrap_or_default();
                        let new_b = new_bid
                            .and_then(|id| coord.git().read_blob(&id).ok())
                            .unwrap_or_default();
                        let own_b = coord.git().read_blob(own_bid).unwrap_or_default();

                        if is_binary_bytes(&old_b)
                            || is_binary_bytes(&new_b)
                            || is_binary_bytes(&own_b)
                        {
                            all_rebased_cleanly = false;
                            break;
                        }

                        let reb = merge_file_3way(
                            p,
                            &String::from_utf8_lossy(&old_b),
                            &String::from_utf8_lossy(&new_b),
                            &String::from_utf8_lossy(&own_b),
                            my_eff_branch,
                            session_id,
                        );

                        if reb.has_conflicts || has_git_conflict_markers(&reb.content) {
                            all_rebased_cleanly = false;
                            break;
                        }

                        if let Ok(reb_bid) = coord.git().write_blob(reb.content.as_bytes()) {
                            if p == &normalized_path || new_bid.as_ref() != Some(&reb_bid) {
                                rebased_map.insert(p.clone(), reb_bid);
                            }
                        } else {
                            all_rebased_cleanly = false;
                            break;
                        }
                    }

                    if all_rebased_cleanly {
                        let mut rebased_del = my_agent.deleted_paths.clone();
                        rebased_del.retain(|p| p != &normalized_path && new_map.contains_key(p));
                        rebased_session_own_blobs = Some(rebased_map);
                        rebased_session_deleted_paths = Some(rebased_del);
                    }
                }
            }
        }
    }

    let final_disk_has_markers = !is_binary_bytes(&final_disk_bytes)
        && has_git_conflict_markers(&String::from_utf8_lossy(&final_disk_bytes));

    let mut updated_intent = my_agent.intent.clone();
    coord.oplog().record_tx(
        session_id,
        format!("post-tool reconcile {normalized_path}"),
        true,
        |view| {
            let mut next_obs = Vec::new();
            let candidate_obs = view
                .observed_blobs
                .get(&normalized_path)
                .into_iter()
                .flatten()
                .copied()
                .chain(std::iter::once(disk_after_blob));

            for b in candidate_obs {
                let is_owned = assigned_own_blobs.values().any(|ob| *ob == b)
                    || view
                        .agents
                        .values()
                        .any(|a| a.own_blobs.get(&normalized_path) == Some(&b));

                if !is_owned
                    && Some(&b) != base_blob.as_ref()
                    && Some(&b) != base_tree_blob.as_ref()
                    && b != final_blob
                    && b != my_own_blob
                    && !next_obs.contains(&b)
                {
                    next_obs.push(b);
                }
            }

            if !next_obs.contains(&final_blob) {
                next_obs.push(final_blob);
            }

            view.observed_blobs
                .insert(normalized_path.clone(), next_obs);

            for (aid, own_b) in &assigned_own_blobs {
                if let Some(agent) = view.agents.get_mut(aid) {
                    if aid == session_id {
                        if !current_my_has_markers {
                            agent.own_blobs.insert(normalized_path.clone(), *own_b);
                            agent.deleted_paths.remove(&normalized_path);
                        }
                    } else if !agent.deleted_paths.contains(&normalized_path) {
                        agent
                            .own_blobs
                            .entry(normalized_path.clone())
                            .or_insert(*own_b);
                    }
                }
            }

            for agent in view.agents.values_mut() {
                let contributed = agent
                    .read_blobs
                    .get(&normalized_path)
                    .is_some_and(|b| merged_peer_blobs.contains(b));
                let same_branch = agent.effective_branch(&default_branch) == my_eff_branch;

                if agent.agent_id == session_id || (auto_merged && contributed) {
                    agent.read_blobs.insert(normalized_path.clone(), final_blob);
                } else if resolving_existing_conflict && same_branch {
                    if agent.own_blobs.contains_key(&normalized_path) {
                        agent.own_blobs.insert(normalized_path.clone(), final_blob);
                    }

                    if agent.read_blobs.contains_key(&normalized_path) {
                        agent.read_blobs.insert(normalized_path.clone(), final_blob);
                    }
                }
            }

            if let Some(agent) = view.agents.get_mut(session_id) {
                if let (Some(new_base), Some(rebased_map)) = (
                    reconciled_head_commit.as_ref(),
                    rebased_session_own_blobs.as_ref(),
                ) {
                    agent.base_commit = *new_base;
                    agent.own_blobs = rebased_map.clone();

                    if let Some(rebased_del) = &rebased_session_deleted_paths {
                        agent.deleted_paths = rebased_del.clone();
                    }
                }

                if !peers_integrated_now.is_empty() {
                    let entry = agent
                        .integrated_peer_blobs
                        .entry(normalized_path.clone())
                        .or_default();

                    for (pid, pblob) in &peers_integrated_now {
                        entry.insert(pid.clone(), *pblob);
                    }
                }

                for sym in &touched_syms {
                    agent
                        .claimed_symbols
                        .insert(format!("{}::{}", normalized_path, sym.label));
                }

                if agent.intent_inferred && !touched_syms.is_empty() {
                    let sym_list = touched_syms
                        .iter()
                        .take(3)
                        .map(|s| s.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    agent.intent = format!("Update {normalized_path} ({sym_list})");
                }

                updated_intent = agent.intent.clone();
                agent.updated_at_ms = now_millis();
            }

            if let Some(conflict) = &conflict_to_save {
                view.conflicts
                    .insert(normalized_path.clone(), conflict.id.clone());
            } else if !final_disk_has_markers && !has_conflict {
                if let Some(old_cid) = view.conflicts.remove(&normalized_path) {
                    coord.oplog().delete_conflict(&old_cid);
                }

                if !view.conflicts.keys().any(|k| k != "<speculative-verify>") {
                    if let Some(spec_cid) = view.conflicts.remove("<speculative-verify>") {
                        coord.oplog().delete_conflict(&spec_cid);
                    }
                }
            }

            Ok(())
        },
    )?;

    let final_own_blob = rebased_session_own_blobs
        .as_ref()
        .and_then(|m| m.get(&normalized_path).copied())
        .unwrap_or(my_own_blob);

    if !current_my_has_markers {
        let _ = coord.record_single_path_edit(
            session_id,
            &normalized_path,
            Some(FileEntry::regular(final_own_blob, executable)),
        )?;
    }

    for (aid, own_b) in &assigned_own_blobs {
        if aid != session_id {
            let _ = coord.record_single_path_edit(
                aid,
                &normalized_path,
                Some(FileEntry::regular(*own_b, executable)),
            );

            if let Some(peer_meta) = current_op.view.agents.get(aid) {
                let peer_branch = peer_meta.effective_branch(&default_branch);
                let peer_ws = coord.branch_sandbox_dir(peer_branch);

                if peer_ws.is_dir() {
                    if let Ok(peer_bytes) = coord.git().read_blob(own_b) {
                        write_worktree_rel(&peer_ws, &normalized_path, &peer_bytes, executable);
                    }
                }
            }
        }
    }

    let eff_branch = my_eff_branch.to_string();

    if branch_ws.is_dir() {
        let ws_bytes = if eff_branch == default_branch {
            &final_disk_bytes
        } else {
            &my_own_bytes
        };
        write_worktree_rel(&branch_ws, &normalized_path, ws_bytes, executable);
    }

    if let Some(inv_dir) = coord.invocation_dir() {
        if inv_dir != coord.git().repo_root() && inv_dir != branch_ws {
            write_worktree_rel(inv_dir, &normalized_path, &final_disk_bytes, executable);
        }
    }

    if cfg!(unix) && full_path.is_file() && my_eff_branch == default_branch {
        set_worktree_executable(&full_path, executable);
    }

    Ok(PostToolOutcome {
        session_id: session_id.to_string(),
        path: normalized_path,
        auto_merged_concurrent_edit: auto_merged,
        has_conflict,
        context_notice,
        session_intent: updated_intent,
    })
}

/// Expands `tes hook post-tool` on a directory path into per-file reconciliation.
///
/// Harnesses report directory-level operations (`mv dirA dirB`, `rm -r dir`, `cp -r src dst`) with
/// the directory path. Returns `Ok(None)` when `normalized_path` is neither an existing directory
/// nor a vanished directory that held tracked or session-owned files.
///
/// # Errors
/// Propagates any error from reconciling an individual file under the directory.
fn reconcile_directory_post_tool(
    coord: &Coordinator,
    session_id: &str,
    normalized_path: &str,
) -> CoreResult<Option<PostToolOutcome>> {
    let repo_root = coord.git().repo_root();
    let common_git_dir = coord.git().common_git_dir();
    let full_path = repo_root.join(normalized_path);
    let prefix = format!("{normalized_path}/");

    let current = coord.oplog().load_current_reconciled()?;
    let Some(meta) = current.view.agents.get(session_id) else {
        return Ok(None);
    };
    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| "main".to_string());
    let eff_branch = meta.effective_branch(&default_branch);
    let branch_ws = coord.branch_sandbox_dir(eff_branch);
    let branch_dir = branch_ws.join(normalized_path);

    let base_map = coord
        .git()
        .read_commit(&meta.base_commit)
        .and_then(|c| coord.git().read_tree_to_map(&c.tree_id))
        .unwrap_or_default();

    let mut paths: BTreeSet<String> = base_map
        .keys()
        .chain(meta.own_blobs.keys())
        .filter(|p| p.starts_with(&prefix))
        .cloned()
        .collect();

    let mut is_dir_target = false;

    if full_path.is_dir() {
        is_dir_target = true;
        collect_worktree_files(repo_root, common_git_dir, &full_path, &base_map, &mut paths);
    }

    if eff_branch != default_branch && branch_dir.is_dir() {
        is_dir_target = true;
        collect_worktree_files(
            &branch_ws,
            common_git_dir,
            &branch_dir,
            &base_map,
            &mut paths,
        );
    }

    if !is_dir_target && (full_path.exists() || paths.is_empty()) {
        return Ok(None);
    }

    let mut outcome = PostToolOutcome {
        session_id: session_id.to_string(),
        path: normalized_path.to_string(),
        auto_merged_concurrent_edit: false,
        has_conflict: false,
        context_notice: None,
        session_intent: meta.intent.clone(),
    };
    let mut notices = Vec::new();

    for rel_path in &paths {
        let file_outcome =
            handle_post_tool_hook_single(coord, session_id, rel_path, rel_path.clone())?;
        outcome.auto_merged_concurrent_edit |= file_outcome.auto_merged_concurrent_edit;
        outcome.has_conflict |= file_outcome.has_conflict;
        outcome.session_intent = file_outcome.session_intent;

        if let Some(notice) = file_outcome.context_notice {
            notices.push(notice);
        }
    }

    if !notices.is_empty() {
        outcome.context_notice = Some(notices.join("\n"));
    }

    Ok(Some(outcome))
}

/// Recursively collects repository-relative paths of regular files under `dir`, skipping symbolic
/// links and untracked paths excluded by `.gitignore`, `.git/info/exclude` or built-in secret rules.
fn collect_worktree_files(
    worktree_root: &Path,
    common_git_dir: &Path,
    dir: &Path,
    base_map: &BTreeMap<String, FileEntry>,
    out: &mut BTreeSet<String>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(worktree_root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if file_type.is_dir() {
            let dir_prefix = format!("{rel_str}/");
            let tracked = base_map.keys().any(|k| k.starts_with(&dir_prefix));

            if !is_ignored_or_secret_path(worktree_root, common_git_dir, &rel_str, tracked) {
                collect_worktree_files(worktree_root, common_git_dir, &path, base_map, out);
            }
        } else if file_type.is_file()
            && !is_ignored_or_secret_path(
                worktree_root,
                common_git_dir,
                &rel_str,
                base_map.contains_key(&rel_str),
            )
        {
            out.insert(rel_str);
        }
    }
}

/// Returns `true` if `trimmed` is a short delimiter or standard single-file component wrapper tag
/// that should not be treated as a substantive peer-only addition during cross-branch untangling.
fn is_trivial_boilerplate_line(trimmed: &str) -> bool {
    trimmed.len() <= 3
        || matches!(
            trimmed,
            "<template>"
                | "</template>"
                | "<script>"
                | "<script setup>"
                | "<script setup lang=\"ts\">"
                | "<script lang=\"ts\">"
                | "</script>"
                | "<style>"
                | "<style scoped>"
                | "</style>"
        )
}

/// Returns `true` if `trimmed` is a common import/include line or a generic single-line control
/// flow guard that two independent agents frequently author identically on separate branches.
fn is_common_import_or_guard_line(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();

    lower.starts_with("import ")
        || lower.starts_with("import{")
        || lower.starts_with("use ")
        || lower.starts_with("from ")
        || lower.starts_with("#include ")
        || lower.starts_with("using ")
        || lower.starts_with("extern crate ")
        || lower.starts_with("require(")
        || matches!(
            trimmed,
            "return;"
                | "return null;"
                | "return undefined;"
                | "return false;"
                | "return true;"
                | "return None;"
                | "return Ok(());"
                | "continue;"
                | "break;"
                | "else {"
                | "} else {"
                | "try {"
                | "} catch {"
        )
        || (lower.starts_with("if ") && lower.contains("return"))
        || (lower.starts_with("if(") && lower.contains("return"))
}

/// Returns `true` if `current_my_str` was authored on top of a foreign branch peer's uncommitted
/// disk buffer (`peer_str` relative to `base_str` and `my_prev_str`).
///
/// Requires either at least two matched substantive non-import lines (with at least two-thirds of
/// all peer substantive lines present) or a 100% match including a substantive non-import line so
/// two agents independently adding the same 1-line import or guard never strip each other's code.
pub(crate) fn is_contaminated_by_foreign_peer(
    base_str: &str,
    my_prev_str: &str,
    peer_str: &str,
    current_my_str: &str,
) -> bool {
    let baseline_lines: BTreeSet<&str> = base_str
        .lines()
        .chain(my_prev_str.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let my_lines: BTreeSet<&str> = current_my_str
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let peer_substantive_added: Vec<&str> = peer_str
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !baseline_lines.contains(l) && !is_trivial_boilerplate_line(l))
        .collect();

    if peer_substantive_added.is_empty() {
        return false;
    }

    let peer_non_import_added: Vec<&str> = peer_substantive_added
        .iter()
        .copied()
        .filter(|l| !is_common_import_or_guard_line(l))
        .collect();

    if peer_non_import_added.is_empty() {
        return false;
    }

    let matched = peer_substantive_added
        .iter()
        .filter(|l| my_lines.contains(**l))
        .count();
    let matched_non_import = peer_non_import_added
        .iter()
        .filter(|l| my_lines.contains(**l))
        .count();

    if matched_non_import >= 2 {
        matched * 3 >= peer_substantive_added.len() * 2
    } else if peer_non_import_added.len() == 1 && matched_non_import == 1 {
        matched == peer_substantive_added.len()
    } else {
        false
    }
}

/// Strips contiguous hunks inserted by a foreign peer (`peer_str` relative to `base_str` and
/// `my_prev_str`) from `disk_after_str` using LCS line alignment, handling adjacent insertions
/// while refusing to strip standalone common 1-line imports or guards.
pub(crate) fn strip_peer_inserted_lines(
    base_str: &str,
    my_prev_str: &str,
    peer_str: &str,
    disk_after_str: &str,
) -> Option<String> {
    let b_lines: Vec<&str> = base_str.lines().collect();
    let m_lines: Vec<&str> = my_prev_str.lines().collect();
    let p_lines: Vec<&str> = peer_str.lines().collect();
    let d_lines: Vec<&str> = disk_after_str.lines().collect();

    if p_lines.is_empty() || d_lines.is_empty() {
        return None;
    }

    let bp_pairs = lcs_line_pairs(&b_lines, &p_lines);
    let mp_pairs = lcs_line_pairs(&m_lines, &p_lines);
    let bd_pairs = lcs_line_pairs(&b_lines, &d_lines);
    let mut b_in_p = vec![false; b_lines.len()];
    let mut in_base_or_mine = vec![false; p_lines.len()];

    for &(b_idx, p_idx) in &bp_pairs {
        b_in_p[b_idx] = true;
        in_base_or_mine[p_idx] = true;
    }

    for &(_, p_idx) in &mp_pairs {
        in_base_or_mine[p_idx] = true;
    }

    let mut b_in_d = vec![false; b_lines.len()];

    for &(b_idx, _) in &bd_pairs {
        b_in_d[b_idx] = true;
    }

    let pd_pairs = lcs_line_pairs(&p_lines, &d_lines);
    let mut p_to_d: Vec<Option<usize>> = vec![None; p_lines.len()];

    for (p_idx, d_idx) in pd_pairs {
        p_to_d[p_idx] = Some(d_idx);
    }

    let mut matched_hunks: Vec<(usize, usize, usize, bool)> = Vec::new();
    let mut p_idx = 0;

    while p_idx < p_lines.len() {
        if in_base_or_mine[p_idx] {
            p_idx += 1;
            continue;
        }

        let p_start = p_idx;

        while p_idx < p_lines.len() && !in_base_or_mine[p_idx] {
            p_idx += 1;
        }

        let p_end = p_idx;
        let hunk_len = p_end - p_start;

        let has_substantive =
            (p_start..p_end).any(|i| !is_trivial_boilerplate_line(p_lines[i].trim()));

        if !has_substantive {
            continue;
        }

        let has_non_import_substantive = (p_start..p_end).any(|i| {
            let t = p_lines[i].trim();
            !t.is_empty() && !is_trivial_boilerplate_line(t) && !is_common_import_or_guard_line(t)
        });

        if let (Some(d_first), Some(d_last)) = (p_to_d[p_start], p_to_d[p_end - 1]) {
            let all_matched = (p_start..p_end).all(|i| p_to_d[i].is_some());

            if all_matched && d_last + 1 == d_first + hunk_len {
                matched_hunks.push((p_start, p_end, d_first, has_non_import_substantive));
            }
        }
    }

    let total_non_import_hunks = matched_hunks.iter().filter(|h| h.3).count();

    if total_non_import_hunks == 0 {
        return None;
    }

    let mut drop_d = vec![false; d_lines.len()];
    let mut restore_at_d: Vec<Vec<&str>> = vec![Vec::new(); d_lines.len()];
    let mut b_restored = vec![false; b_lines.len()];
    let mut dropped_any = false;

    for (p_start, p_end, d_first, _) in matched_hunks {
        let hunk_len = p_end - p_start;

        for flag in &mut drop_d[d_first..(d_first + hunk_len)] {
            *flag = true;
        }

        let bp_range_start = bp_pairs
            .iter()
            .rev()
            .find(|&&(_, pi)| pi < p_start)
            .map_or(0, |&(bi, _)| bi + 1);
        let bp_range_end = bp_pairs
            .iter()
            .find(|&&(_, pi)| pi >= p_end)
            .map_or(b_lines.len(), |&(bi, _)| bi);
        let bd_range_start = bd_pairs
            .iter()
            .rev()
            .find(|&&(_, di)| di < d_first)
            .map_or(0, |&(bi, _)| bi + 1);
        let bd_range_end = bd_pairs
            .iter()
            .find(|&&(_, di)| di >= d_first + hunk_len)
            .map_or(b_lines.len(), |&(bi, _)| bi);
        let rep_start = bp_range_start.max(bd_range_start);
        let rep_end = bp_range_end.min(bd_range_end);

        for b_idx in rep_start..rep_end {
            if !b_in_p[b_idx] && !b_in_d[b_idx] && !b_restored[b_idx] {
                restore_at_d[d_first].push(b_lines[b_idx]);
                b_restored[b_idx] = true;
            }
        }

        dropped_any = true;
    }

    if !dropped_any {
        return None;
    }

    let mut out_lines: Vec<&str> = Vec::with_capacity(d_lines.len());

    for (idx, line) in d_lines.iter().enumerate() {
        for &restored_line in &restore_at_d[idx] {
            out_lines.push(restored_line);
        }

        if drop_d[idx] {
            continue;
        }

        out_lines.push(*line);
    }

    let mut out = out_lines.join("\n");

    if disk_after_str.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }

    Some(out)
}

/// Computes exact longest common subsequence (LCS) index pairs on a bounded slice pair.
fn lcs_small_slice(
    a: &[&str],
    b: &[&str],
    a_offset: usize,
    b_offset: usize,
    out: &mut Vec<(usize, usize)>,
) {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];

    for i in (0..n).rev() {
        for j in (0..m).rev() {
            if a[i] == b[j] {
                dp[i][j] = 1 + dp[i + 1][j + 1];
            } else {
                dp[i][j] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    let (mut i, mut j) = (0, 0);

    while i < n && j < m {
        if a[i] == b[j] {
            out.push((a_offset + i, b_offset + j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
}

/// Computes the longest common subsequence (LCS) index pairs `(a_idx, b_idx)` between two line slices
/// in O(N + M) memory using `imara_diff`'s Histogram algorithm with bounded intra-hunk refinement.
fn lcs_line_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();

    if n == 0 || m == 0 {
        return Vec::new();
    }

    let mut pairs = Vec::with_capacity(n.min(m));

    if n.saturating_mul(m) <= 16_384 {
        lcs_small_slice(a, b, 0, 0, &mut pairs);
        return pairs;
    }

    let mut a_text = String::new();

    for line in a {
        a_text.push_str(line);
        a_text.push('\n');
    }

    let mut b_text = String::new();

    for line in b {
        b_text.push_str(line);
        b_text.push('\n');
    }

    let input = InternedInput::new(a_text.as_str(), b_text.as_str());
    let computed_diff = Diff::compute(Algorithm::Histogram, &input);

    let mut a_idx = 0usize;
    let mut b_idx = 0usize;

    for hunk in computed_diff.hunks() {
        let a_hunk_start = hunk.before.start as usize;
        let a_hunk_end = hunk.before.end as usize;
        let b_hunk_start = hunk.after.start as usize;
        let b_hunk_end = hunk.after.end as usize;

        while a_idx < a_hunk_start && b_idx < b_hunk_start {
            pairs.push((a_idx, b_idx));
            a_idx += 1;
            b_idx += 1;
        }

        let hunk_n = a_hunk_end.saturating_sub(a_hunk_start);
        let hunk_m = b_hunk_end.saturating_sub(b_hunk_start);

        if hunk_n > 0 && hunk_m > 0 && hunk_n.saturating_mul(hunk_m) <= 16_384 {
            lcs_small_slice(
                &a[a_hunk_start..a_hunk_end],
                &b[b_hunk_start..b_hunk_end],
                a_hunk_start,
                b_hunk_start,
                &mut pairs,
            );
        }

        a_idx = a_hunk_end;
        b_idx = b_hunk_end;
    }

    while a_idx < n && b_idx < m {
        pairs.push((a_idx, b_idx));
        a_idx += 1;
        b_idx += 1;
    }

    pairs
}

/// Scores how strongly the added/modified lines in `candidate_str` (relative to `base_str`)
/// correlate with an agent's `intent` and `target_branch` keywords.
fn score_blob_for_agent(
    base_str: &str,
    candidate_str: &str,
    intent: &str,
    target_branch: Option<&str>,
) -> usize {
    let base_lines: BTreeSet<&str> = base_str.lines().map(str::trim).collect();
    let mut added_lower = String::new();

    for line in candidate_str.lines() {
        let trimmed = line.trim();

        if !trimmed.is_empty() && !base_lines.contains(trimmed) {
            added_lower.push_str(&trimmed.to_ascii_lowercase());
            added_lower.push(' ');
        }
    }

    if added_lower.is_empty() {
        return 0;
    }

    let stop_words = [
        "add",
        "optional",
        "parameter",
        "function",
        "update",
        "the",
        "and",
        "for",
        "into",
        "with",
        "from",
        "feat",
        "fix",
        "chore",
        "src",
        "app",
    ];
    let combined_meta =
        format!("{} {}", intent, target_branch.unwrap_or_default()).to_ascii_lowercase();
    let mut keywords = BTreeSet::new();

    for token in combined_meta.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.len() >= 3 && !stop_words.contains(&token) {
            keywords.insert(token);
        }
    }

    keywords
        .into_iter()
        .filter(|kw| added_lower.contains(kw))
        .count()
}

/// Normalizes and validates an absolute or relative path (including paths inside a Copy-on-Write
/// branch sandbox) into a confined forward-slash repository-relative path.
///
/// # Errors
/// Returns [`CoreError::InvalidArgument`] if `raw_path` escapes `repo_root`, targets `.git`,
/// or traverses a symlink outside `repo_root`.
fn normalize_repo_rel_path(repo_root: &Path, raw_path: &str) -> CoreResult<String> {
    let p = Path::new(raw_path);

    if p.is_absolute() {
        if let Ok(stripped) = p.strip_prefix(repo_root) {
            let rel = stripped.to_string_lossy().replace('\\', "/");
            let (norm, _) = resolve_confined_repo_path(repo_root, &rel)?;
            return Ok(norm);
        }

        let mut anc = p.parent();

        while let Some(dir) = anc {
            if is_valid_sandbox_repo_root(dir, Some(repo_root)).is_some() {
                if let Ok(stripped) = p.strip_prefix(dir) {
                    let rel = stripped.to_string_lossy().replace('\\', "/");
                    let _ = resolve_confined_repo_path(dir, &rel)?;
                    let (norm, _) = resolve_confined_repo_path(repo_root, &rel)?;
                    return Ok(norm);
                }
            }

            anc = dir.parent();
        }

        return Err(CoreError::InvalidArgument(format!(
            "absolute path '{}' is outside repository root '{}'",
            raw_path,
            repo_root.display()
        )));
    }

    let (norm, _) = resolve_confined_repo_path(repo_root, raw_path)?;
    Ok(norm)
}
