//! Copy-on-Write branch sandboxes, deterministic runtime slot allocation, port lease tracking,
//! branch switching (`switch_branch*`, `switch_editor_branch`), and isolated branch execution
//! (`exec_on_branch`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use core::{
    AgentMetadata, ChangeId, CoreError, CoreResult, EntryMode, FileEntry, ObjectId, View,
    atomic_write_file, now_millis, validate_ref_name,
};
use workspace::{
    build_agent_env_with_config, create_cow_workspace, ensure_polyglot_port_shims,
    materialize_tree_to_dir, materialize_tree_to_dir_with_baseline, snapshot_working_dir,
    sync_missing_cow_caches,
};

use crate::Coordinator;
use crate::worktree::{
    acquire_file_lock, is_harness_config_path, remove_worktree_file, same_canonical_path,
    session_claims_path, write_worktree_rel,
};

impl Coordinator {
    /// Switches or binds the specified terminal/agent session (`session_id`) to `branch`,
    /// creating `refs/heads/<branch>` at the session's current branch commit (or `HEAD`) if the
    /// branch does not exist yet.
    ///
    /// Unlike `git checkout`, this does not overwrite `.git/HEAD` for other concurrent terminals
    /// or sessions in the same repository.
    ///
    /// # Errors
    /// Returns [`CoreError`] if resolving `HEAD` or updating the `OpLog` fails.
    pub fn switch_branch(&self, session_id: &str, branch: &str) -> CoreResult<AgentMetadata> {
        self.switch_branch_with_base(session_id, branch, None)
    }

    /// Switches or binds the specified terminal/agent session (`session_id`) to `branch`,
    /// optionally forking a new `branch` from an explicit `from_branch` revision (`--from <base>`
    /// or `<start-point>`).
    ///
    /// Refuses to silently reset an existing branch when `from_branch` points to a different commit.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `branch` already exists at a different commit than
    /// `from_branch`, or [`CoreError`] if `branch`/`from_branch` is invalid/missing or updating the `OpLog` fails.
    pub fn switch_branch_with_base(
        &self,
        session_id: &str,
        branch: &str,
        from_branch: Option<&str>,
    ) -> CoreResult<AgentMetadata> {
        let clean_branch = branch.trim();
        validate_ref_name(clean_branch)?;

        let explicit_from_cid = match from_branch.map(str::trim).filter(|s| !s.is_empty()) {
            Some(base_spec) => Some(self.git.resolve_revision(base_spec)?),
            None => None,
        };

        self.reconcile_in_flight_sessions();
        let prev_meta = self.ensure_session(session_id, None)?;
        let ref_name = format!("refs/heads/{clean_branch}");
        let existing_cid = self.git.resolve_ref(&ref_name)?;

        let branch_cid = match (existing_cid, explicit_from_cid) {
            (Some(cid), Some(from_cid)) if cid != from_cid => {
                return Err(CoreError::InvalidArgument(format!(
                    "branch '{clean_branch}' already exists at {} (cannot reset it to {} via --from; delete or use `tes restore` first)",
                    cid.short(),
                    from_cid.short()
                )));
            }
            (Some(cid), _) => cid,
            (None, Some(from_cid)) => {
                self.git.update_ref(&ref_name, &from_cid)?;
                from_cid
            }
            (None, None) => {
                let remote_cid = self
                    .git
                    .resolve_ref(&format!("refs/remotes/origin/{clean_branch}"))?;
                let session_prev_cid = prev_meta
                    .target_branch
                    .as_deref()
                    .filter(|prev_b| *prev_b != clean_branch)
                    .and_then(|prev_b| {
                        self.git
                            .resolve_ref(&format!("refs/heads/{prev_b}"))
                            .ok()
                            .flatten()
                            .or(Some(prev_meta.base_commit))
                    });
                let target_cid = match remote_cid.or(session_prev_cid) {
                    Some(cid) => cid,
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
            let is_passive =
                (id.starts_with("term-") || id.starts_with("session-") || id == "editor")
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
        let prev_head_tree_id = self
            .git
            .resolve_head_commit()
            .ok()
            .flatten()
            .and_then(|cid| self.git.read_commit(&cid).ok())
            .map(|c| c.tree_id);
        let prev_head_map = prev_head_tree_id
            .as_ref()
            .and_then(|tid| self.git.read_tree_to_map(tid).ok());
        let has_unowned_changes = prev_head_tree_id
            .as_ref()
            .is_some_and(|tid| !self.unowned_worktree_changes(&current.view, tid).is_empty());
        let restore_trunk_to_repo_root = clean_branch == cfg.trunk_branch
            && !other_has_uncommitted_edits
            && !caller_has_uncommitted_edits
            && !has_unowned_changes;

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

                let target_slot = self.branch_slot(view, clean_branch, &cfg.trunk_branch);

                if clean_branch != cfg.trunk_branch {
                    for a in view.agents.values_mut() {
                        if a.target_branch.as_deref() == Some(clean_branch) {
                            a.slot = target_slot;
                        }
                    }
                }

                if let Some(agent) = view.agents.get_mut(session_id) {
                    agent.slot = target_slot;
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
            let _ = materialize_tree_to_dir_with_baseline(
                &self.git,
                &branch_commit.tree_id,
                self.git.repo_root(),
                prev_head_map.as_ref(),
            );
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

        let mut prev_tree_map = self
            .git
            .resolve_head_commit()
            .ok()
            .flatten()
            .and_then(|cid| self.git.read_commit(&cid).ok())
            .and_then(|c| self.git.read_tree_to_map(&c.tree_id).ok())
            .unwrap_or_default();

        if !old_branch.is_empty() {
            let safe_old = Self::sanitize_branch_name(&old_branch);
            let old_stamp = self.branches_root().join(format!("{safe_old}.tree"));

            if let Some(old_stamp_tid) = fs::read_to_string(&old_stamp).ok().and_then(|s| {
                let trimmed = s.trim();
                let tree_hex = trimmed.rsplit(':').next().unwrap_or(trimmed);
                ObjectId::from_hex(tree_hex).ok()
            }) {
                if let Ok(old_stamp_map) = self.git.read_tree_to_map(&old_stamp_tid) {
                    for (k, v) in old_stamp_map {
                        prev_tree_map.insert(k, v);
                    }
                }
            }
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
        materialize_tree_to_dir_with_baseline(
            &self.git,
            &effective_tree_id,
            self.git.repo_root(),
            Some(&prev_tree_map),
        )?;
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

    /// Sanitizes a branch name into a filesystem-safe, collision-free directory slug.
    ///
    /// Pure lowercase ASCII slugs (`[a-z0-9_-]+`) remain readable verbatim (`main`, `feat-auth`).
    /// Branch names containing uppercase letters or characters that get replaced (such as `/` or
    /// `.`) receive a deterministic 8-hex-character Blake3 suffix derived from the raw branch name
    /// so `feat/auth` vs `feat-auth` and `Feat/A` vs `feat/a` never collide on POSIX or
    /// case-insensitive filesystems.
    #[must_use]
    pub fn sanitize_branch_name(branch: &str) -> String {
        let is_pure_lower = !branch.is_empty()
            && branch
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');

        if is_pure_lower {
            return branch.to_string();
        }

        let base: String = branch
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let hash = blake3::hash(branch.as_bytes()).to_hex();
        format!("{base}-{}", &hash[..8])
    }

    /// Returns the external Copy-on-Write sandbox root directory for this repository.
    ///
    /// Resolves candidate workspace bases via [`core::candidate_workspaces_bases`] (honoring
    /// `TES_WORKSPACES_ROOT`, containing ephemeral temp-dir test repositories inside the temp
    /// hierarchy, and using standard OS cache directories on the same filesystem device `st_dev`
    /// so tools like Vite, Next.js, and ESLint never block file serving due to `**/.git/**` rules
    /// while APFS `clonefile(2)` and Linux `FICLONE` remain zero-copy).
    ///
    /// Also writes cache hygiene markers (`CACHEDIR.TAG`, macOS `.metadata_never_index`, and Time
    /// Machine exclusion xattr) on the base directory and records `<slug>/.repo_root`.
    pub fn sandboxes_root(&self) -> PathBuf {
        let repo_root = self.git.repo_root();
        let slug = core::compute_repo_slug(repo_root);
        let bases = core::candidate_workspaces_bases(repo_root);

        // SAFETY: `libc::geteuid()` is a pure POSIX syscall with no arguments or memory dereferences.
        #[cfg(unix)]
        let uid = unsafe { libc::geteuid() };

        #[cfg(unix)]
        let repo_dev = fs::metadata(repo_root).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.dev()
        });

        for base in bases {
            let cand = base.join(&slug);

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
                        if r_dev != c_meta.dev() {
                            continue;
                        }
                    }
                }

                core::ensure_cache_dir_hygiene(&base);

                let repo_marker = cand.join(".repo_root");

                if !repo_marker.is_file() {
                    let _ = core::atomic_write_file_relaxed(
                        &repo_marker,
                        repo_root.display().to_string().as_bytes(),
                    );
                }

                return cand;
            }
        }

        self.git.common_git_dir().join("tessera")
    }

    /// Returns the directory containing per-branch Copy-on-Write sandboxes for this repository.
    pub fn branches_root(&self) -> PathBuf {
        self.sandboxes_root().join("branches")
    }

    /// Returns the deterministic 1-based slot assigned to `branch`.
    ///
    /// The trunk branch (`trunk_branch`) is always assigned slot `1` (`base_port + port_stride`).
    /// Each non-trunk branch receives a persistent slot `>= 2` stored at
    /// `<branches_root>/<safe_branch>.slot` under a cross-process lock (`slots.lock`) while
    /// avoiding collisions with both other branches' slots and active trunk sessions in `view.agents`.
    pub fn branch_slot(&self, view: &View, branch: &str, trunk_branch: &str) -> u16 {
        let clean = branch.trim();

        if clean.is_empty() || clean == trunk_branch {
            return 1;
        }

        let safe_b = Self::sanitize_branch_name(clean);
        let branches_root = self.branches_root();
        let slot_file = branches_root.join(format!("{safe_b}.slot"));

        if let Ok(raw) = fs::read_to_string(&slot_file) {
            if let Ok(parsed) = raw.trim().parse::<u16>() {
                if parsed >= 2 {
                    return parsed;
                }
            }
        }

        let lock_path = self.git.common_git_dir().join("tessera").join("slots.lock");
        let _slot_lock = acquire_file_lock(&lock_path).ok();

        if let Ok(raw) = fs::read_to_string(&slot_file) {
            if let Ok(parsed) = raw.trim().parse::<u16>() {
                if parsed >= 2 {
                    return parsed;
                }
            }
        }

        let mut all_branches: BTreeSet<String> = view.branches.keys().cloned().collect();

        for git_br in self.git.list_local_branches() {
            all_branches.insert(git_br);
        }

        for a in view.agents.values() {
            if let Some(tb) = &a.target_branch {
                all_branches.insert(tb.clone());
            }
        }

        let mut used_slots = BTreeSet::new();

        for other_br in &all_branches {
            if other_br == trunk_branch || other_br == clean {
                continue;
            }

            let other_safe = Self::sanitize_branch_name(other_br);
            let other_file = branches_root.join(format!("{other_safe}.slot"));

            if let Ok(raw) = fs::read_to_string(&other_file) {
                if let Ok(parsed) = raw.trim().parse::<u16>() {
                    if parsed >= 2 {
                        used_slots.insert(parsed);
                    }
                }
            }
        }

        // Also scan all existing `*.slot` files in `branches_root` so concurrent branch creations
        // that have not yet landed in `view.branches` are observed immediately.
        if let Ok(entries) = fs::read_dir(&branches_root) {
            let my_slot_name = format!("{safe_b}.slot");

            for entry in entries.flatten() {
                let path = entry.path();
                let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };

                if fname == my_slot_name || !fname.ends_with(".slot") {
                    continue;
                }

                if let Ok(raw) = fs::read_to_string(&path) {
                    if let Ok(parsed) = raw.trim().parse::<u16>() {
                        if parsed >= 2 {
                            used_slots.insert(parsed);
                        }
                    }
                }
            }
        }

        // Include slots already used by active trunk sessions (and other non-matching sessions)
        // so a new feature branch never collides with a running trunk session on `slot = 2`.
        for a in view.agents.values() {
            if a.slot >= 2 && a.target_branch.as_deref() != Some(clean) {
                used_slots.insert(a.slot);
            }
        }

        let assigned = (2u16..=u16::MAX)
            .find(|s| !used_slots.contains(s))
            .unwrap_or(2);
        let _ = atomic_write_file(&slot_file, assigned.to_string().as_bytes());
        assigned
    }

    /// Returns the lowest active TCP port currently leased in `<sandboxes_root>/bin/.leases/`
    /// by a live process running on `branch`, if any.
    pub fn active_leased_port_for_branch(&self, branch: &str, trunk_branch: &str) -> Option<u16> {
        let leases_dir = self.sandboxes_root().join("bin").join(".leases");
        let entries = fs::read_dir(&leases_dir).ok()?;
        let branch_ws = if branch == trunk_branch {
            self.git.repo_root().to_path_buf()
        } else {
            self.branch_sandbox_dir(branch)
        };
        let alt_ws = self.branch_sandbox_dir(branch);

        let mut matched_ports = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let Some(port_num) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| s.parse::<u16>().ok())
            else {
                continue;
            };

            let pid_str = fs::read_to_string(path.join("pid")).unwrap_or_default();
            let Ok(pid) = pid_str.trim().parse::<i32>() else {
                continue;
            };

            if !crate::session::is_process_alive(pid) {
                continue;
            }

            let leased_branch = fs::read_to_string(path.join("branch")).unwrap_or_default();
            let leased_ws = fs::read_to_string(path.join("workspace")).unwrap_or_default();
            let lb = leased_branch.trim();
            let lw = leased_ws.trim();
            let lw_path = Path::new(lw);

            if lb == branch
                || (!lw.is_empty()
                    && (same_canonical_path(lw_path, &branch_ws)
                        || same_canonical_path(lw_path, &alt_ws)))
            {
                matched_ports.push(port_num);
            }
        }

        matched_ports.sort_unstable();
        matched_ports.into_iter().next()
    }

    /// Removes any `<sandboxes_root>/bin/.leases/<port>` lease directories matching `branch` or
    /// `sandbox_dir` when a branch is deleted.
    pub(crate) fn cleanup_branch_leases(&self, branch: &str, sandbox_dir: &Path) {
        let leases_dir = self.sandboxes_root().join("bin").join(".leases");
        let Ok(entries) = fs::read_dir(&leases_dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let leased_branch = fs::read_to_string(path.join("branch")).unwrap_or_default();
            let leased_ws = fs::read_to_string(path.join("workspace")).unwrap_or_default();
            let lb = leased_branch.trim();
            let lw = leased_ws.trim();

            if lb == branch || (!lw.is_empty() && same_canonical_path(Path::new(lw), sandbox_dir)) {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }

    /// Updates matching `<sandboxes_root>/bin/.leases/<port>` metadata files when a branch is
    /// renamed from `old_branch` (`old_ws`) to `new_branch` (`new_ws`).
    pub(crate) fn rename_branch_leases(
        &self,
        old_branch: &str,
        new_branch: &str,
        old_ws: &Path,
        new_ws: &Path,
    ) {
        let leases_dir = self.sandboxes_root().join("bin").join(".leases");
        let Ok(entries) = fs::read_dir(&leases_dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let leased_branch = fs::read_to_string(path.join("branch")).unwrap_or_default();
            let leased_ws = fs::read_to_string(path.join("workspace")).unwrap_or_default();
            let lb = leased_branch.trim();
            let lw = leased_ws.trim();

            if lb == old_branch || (!lw.is_empty() && same_canonical_path(Path::new(lw), old_ws)) {
                let _ = atomic_write_file(&path.join("branch"), new_branch.as_bytes());
                let _ = atomic_write_file(
                    &path.join("workspace"),
                    new_ws.display().to_string().as_bytes(),
                );
            }
        }
    }

    /// Returns the Copy-on-Write sandbox directory for `branch`, automatically migrating any
    /// legacy `<common_git_dir>/tessera/branches/<safe_branch>` directory if present.
    pub fn branch_sandbox_dir(&self, branch: &str) -> PathBuf {
        let safe_branch = Self::sanitize_branch_name(branch);
        let branches_root = self.branches_root();
        let new_dir = branches_root.join(&safe_branch);
        let legacy_root = self.git.common_git_dir().join("tessera").join("branches");
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

    /// Core implementation backing [`Self::prepare_branch_sandbox`] and [`Self::exec_on_branch`].
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

        let mut tree_map =
            if branch_ws_dir.is_dir() && stamped_branch_cid.as_ref() == Some(&branch_cid) {
                stamped_tree_map_opt
                    .clone()
                    .unwrap_or_else(|| base_tree_map.clone())
            } else {
                base_tree_map.clone()
            };

        for agent in current.view.agents.values() {
            let eff_b = agent.effective_branch(current_head_branch.as_str());

            if eff_b == target_branch {
                for (rel_path, blob_id) in &agent.own_blobs {
                    let mode = self
                        .git
                        .read_tree_entry_at_path(&agent.head_tree, rel_path)
                        .ok()
                        .flatten()
                        .map(|e| e.entry_mode())
                        .unwrap_or_else(|| {
                            tree_map
                                .get(rel_path)
                                .map_or(EntryMode::NormalFile, FileEntry::entry_mode)
                        });
                    tree_map.insert(rel_path.clone(), FileEntry::from_mode(*blob_id, mode));
                }

                for deleted_path in &agent.deleted_paths {
                    if !agent.own_blobs.contains_key(deleted_path) {
                        tree_map.remove(deleted_path);
                    }
                }
            }
        }

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
                        eff_b != target_branch && session_claims_path(a, rel_path, &base_tree_map)
                    });
                    let matches_base = base_tree_map
                        .get(rel_path)
                        .map(|e| (&e.blob_id, e.entry_mode()))
                        == Some((&entry.blob_id, entry.entry_mode()));
                    let matches_stamp = stamped_tree_map_opt
                        .as_ref()
                        .and_then(|m| m.get(rel_path))
                        .map(|e| (&e.blob_id, e.entry_mode()))
                        == Some((&entry.blob_id, entry.entry_mode()));

                    if !already_in_own_blobs
                        && !claimed_by_foreign
                        && !matches_base
                        && !matches_stamp
                    {
                        tree_map.insert(rel_path.clone(), entry.clone());

                        if branch_ws_dir.is_dir() && !entry.is_symlink() && !entry.is_gitlink() {
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

                    let owned_on_target = current.view.agents.values().any(|a| {
                        let eff_b = a.effective_branch(current_head_branch.as_str());
                        eff_b == target_branch && a.own_blobs.contains_key(rel_path)
                    });

                    if target_branch == current_head_branch
                        && self.invocation_dir.is_none()
                        && owned_on_target
                    {
                        continue;
                    }

                    let changed_since_stamp = stamped_tree_map
                        .get(rel_path)
                        .map(|e| (&e.blob_id, e.entry_mode()))
                        != Some((&entry.blob_id, entry.entry_mode()));

                    if changed_since_stamp
                        && base_tree_map
                            .get(rel_path)
                            .map(|e| (&e.blob_id, e.entry_mode()))
                            != Some((&entry.blob_id, entry.entry_mode()))
                        && tree_map.get(rel_path) != Some(entry)
                    {
                        newly_captured.insert(rel_path.clone(), entry.blob_id);
                        tree_map.insert(rel_path.clone(), entry.clone());
                        tree_changed = true;

                        if target_branch == current_head_branch
                            && !entry.is_symlink()
                            && !entry.is_gitlink()
                        {
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
                                remove_worktree_file(self.git.repo_root(), rel_path);
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

        let sandbox_lock_path = self
            .git
            .common_git_dir()
            .join("tessera")
            .join("locks")
            .join(format!("sandbox-{safe_branch}.lock"));
        let _sandbox_lock = acquire_file_lock(&sandbox_lock_path)?;

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
            let _ =
                sync_missing_cow_caches(self.git.repo_root(), &branch_ws_dir, &cfg.cow_cache_dirs);
            let cached_tree = fs::read_to_string(&tree_stamp_file).unwrap_or_default();

            if cached_tree.trim() != stamp_payload
                && cached_tree.trim() != effective_tree_id.as_str()
            {
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

        drop(_sandbox_lock);

        let mut env_meta = meta;
        env_meta.target_branch = Some(target_branch.clone());

        if target_branch != cfg.trunk_branch {
            env_meta.slot = self.branch_slot(&current.view, &target_branch, &cfg.trunk_branch);
        }

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
    /// zero-copy cloned dependencies (`node_modules`, `target`) and isolated `TES_PORT`,
    /// `DATABASE_URL`, `TEST_DATABASE_URL`, `TES_BRANCH`, and `TMPDIR`.
    ///
    /// Preserves unowned human edits and untracked files in `repo_root`.
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
                    let unowned =
                        self.unowned_worktree_changes(&current.view, &head_commit.tree_id);

                    if unowned.is_empty() {
                        let mut prev_tracked = self
                            .git
                            .read_tree_to_map(&head_commit.tree_id)
                            .unwrap_or_default();

                        // Also include paths owned by non-head sessions so their in-flight files
                        // do not remain leaked in `repo_root`, while untracked human files stay untouched.
                        for agent in current.view.agents.values() {
                            if !agent.targets_head_branch(&current_head_branch) {
                                for (p, bid) in &agent.own_blobs {
                                    prev_tracked
                                        .entry(p.clone())
                                        .or_insert_with(|| FileEntry::regular(*bid, false));
                                }
                            }
                        }

                        let _ = materialize_tree_to_dir_with_baseline(
                            &self.git,
                            &head_commit.tree_id,
                            self.git.repo_root(),
                            Some(&prev_tracked),
                        );
                    } else if let Ok(head_map) = self.git.read_tree_to_map(&head_commit.tree_id) {
                        // Preserve unowned human edits in `repo_root`: only revert paths authored
                        // by non-head sessions back to `head_map` (or remove them if absent on HEAD).
                        let mut foreign_paths = BTreeSet::new();

                        for agent in current.view.agents.values() {
                            if !agent.targets_head_branch(&current_head_branch) {
                                foreign_paths.extend(agent.owned_paths().cloned());
                            }
                        }

                        for rel_path in &foreign_paths {
                            if unowned.contains(rel_path) || is_harness_config_path(rel_path) {
                                continue;
                            }

                            match head_map.get(rel_path) {
                                Some(entry) if !entry.is_symlink() && !entry.is_gitlink() => {
                                    if let Ok(bytes) = self.git.read_blob(&entry.blob_id) {
                                        write_worktree_rel(
                                            self.git.repo_root(),
                                            rel_path,
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

        let _ = sync_missing_cow_caches(&branch_ws_dir, self.git.repo_root(), &cfg.cow_cache_dirs);
        let _ = self.prepare_branch_sandbox_inner(session_id, branch, true);

        Ok(status.code().unwrap_or(1))
    }
}
