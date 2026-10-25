//! Lock-free content-addressed Operation Log (`OpLog`) and concurrent state reconciliation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{
    atomic_write_file, now_millis, validate_hex_id, validate_repo_rel_path, AgentMetadata,
    ChangeId, ConflictId, ConflictRecord, CoreError, CoreResult, ObjectId, OpId, RepoConfig,
    StashEntry,
};

/// Complete repository state snapshot at a specific [`Operation`] in the lock-free `OpLog`.
///
/// All maps use [`BTreeMap`] so serialized bytes and Blake3 [`OpId`] digests are 100%
/// deterministic across processes and platforms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    /// Standard Git branch names (`main`, `feat/x`) mapped to their head [`ObjectId`].
    pub branches: BTreeMap<String, ObjectId>,
    /// Active AI agent sessions mapped by `agent_id`.
    pub agents: BTreeMap<String, AgentMetadata>,
    /// Logical [`ChangeId`]s mapped to their latest Git commit [`ObjectId`].
    pub changes: BTreeMap<ChangeId, ObjectId>,
    /// Active first-class conflicts mapped by `path -> ConflictId`.
    pub conflicts: BTreeMap<String, ConflictId>,
    /// Chronological list of intermediate disk [`ObjectId`] blobs captured by the background watcher
    /// for claimed files, preventing unmerged writes from being lost before `post-tool` runs.
    #[serde(default)]
    pub observed_blobs: BTreeMap<String, Vec<ObjectId>>,
    /// Stashed session change sets in newest-first order (`stash@{0}` is index `0`).
    #[serde(default)]
    pub stashes: Vec<StashEntry>,
    /// Monotonically increasing slot allocator so each agent gets a unique runtime slot (`1..=N`).
    pub next_agent_slot: u16,
}

impl View {
    /// Deterministically merges two concurrent [`View`] states (`left` and `right`) relative to
    /// their common `base` [`View`].
    ///
    /// This enables lock-free multi-agent concurrency: when two agents write operations to
    /// `.git/tessera/op_heads/` at the exact same microsecond, the next command automatically
    /// reconciles both views without failing or requiring `.git/index.lock`.
    pub fn merge_views(base: &Self, left: &Self, right: &Self) -> Self {
        let mut merged = left.clone();

        for (branch, r_id) in &right.branches {
            let b_id = base.branches.get(branch);
            let l_id = left.branches.get(branch);

            if l_id == b_id && Some(r_id) != b_id {
                merged.branches.insert(branch.clone(), *r_id);
            }
        }

        for base_branch in base.branches.keys() {
            if !right.branches.contains_key(base_branch)
                && left.branches.get(base_branch) == base.branches.get(base_branch)
            {
                merged.branches.remove(base_branch);
            }
        }

        for (agent_id, r_agent) in &right.agents {
            match (base.agents.get(agent_id), left.agents.get(agent_id)) {
                (b, l) if l == b => {
                    merged.agents.insert(agent_id.clone(), r_agent.clone());
                }
                (_, Some(l_agent)) => {
                    if r_agent.updated_at_ms > l_agent.updated_at_ms {
                        let mut combined = r_agent.clone();

                        for sym in &l_agent.claimed_symbols {
                            combined.claimed_symbols.insert(sym.clone());
                        }

                        for (k, v) in &l_agent.read_blobs {
                            combined.read_blobs.entry(k.clone()).or_insert(*v);
                        }

                        for (k, v) in &l_agent.own_blobs {
                            if !combined.deleted_paths.contains(k) {
                                combined.own_blobs.entry(k.clone()).or_insert(*v);
                            }
                        }

                        for k in &l_agent.deleted_paths {
                            if !combined.own_blobs.contains_key(k) {
                                combined.deleted_paths.insert(k.clone());
                            }
                        }

                        if combined.target_branch.is_none() {
                            combined.target_branch = l_agent.target_branch.clone();
                        }

                        for (k, v) in &l_agent.checkpoints {
                            combined
                                .checkpoints
                                .entry(k.clone())
                                .or_insert_with(|| v.clone());
                        }

                        merged.agents.insert(agent_id.clone(), combined);
                    }
                }
                (_, None) => {}
            }
        }

        for base_agent_id in base.agents.keys() {
            if !right.agents.contains_key(base_agent_id)
                && left.agents.get(base_agent_id) == base.agents.get(base_agent_id)
            {
                merged.agents.remove(base_agent_id);
            }
        }

        for (cid, r_commit) in &right.changes {
            if left.changes.get(cid) == base.changes.get(cid) {
                merged.changes.insert(cid.clone(), *r_commit);
            }
        }

        for (path, r_cid) in &right.conflicts {
            if left.conflicts.get(path) == base.conflicts.get(path) {
                merged.conflicts.insert(path.clone(), r_cid.clone());
            }
        }

        for base_conflict_path in base.conflicts.keys() {
            if !right.conflicts.contains_key(base_conflict_path)
                && left.conflicts.get(base_conflict_path) == base.conflicts.get(base_conflict_path)
            {
                merged.conflicts.remove(base_conflict_path);
            }
        }

        for (path, r_list) in &right.observed_blobs {
            let entry = merged.observed_blobs.entry(path.clone()).or_default();

            for bid in r_list {
                if !entry.contains(bid) {
                    entry.push(*bid);
                }
            }
        }

        if left.stashes == base.stashes && right.stashes != base.stashes {
            merged.stashes = right.stashes.clone();
        } else if right.stashes != base.stashes {
            let removed_ids: BTreeSet<&str> = base
                .stashes
                .iter()
                .map(|s| s.id.as_str())
                .filter(|id| !right.stashes.iter().any(|rs| rs.id.as_str() == *id))
                .collect();

            merged
                .stashes
                .retain(|s| !removed_ids.contains(s.id.as_str()));

            for rs in right.stashes.iter().rev() {
                let in_base = base.stashes.iter().any(|bs| bs.id == rs.id);
                let in_merged = merged.stashes.iter().any(|ms| ms.id == rs.id);

                if !in_base && !in_merged {
                    merged.stashes.insert(0, rs.clone());
                }
            }
        }

        merged.next_agent_slot = left.next_agent_slot.max(right.next_agent_slot);
        merged
    }
}

/// Immutable, content-addressed entry in `.git/tessera/oplog/<op_id>.json`.
///
/// ```text
/// [Op 0: init] <--- [Op 1: agent A edit] <--- [Op 3: auto-reconcile]
///              \--- [Op 2: agent B edit] <--/
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// Content-addressed Blake3 hash of `(parents, agent_id, description, is_ephemeral, view)`.
    pub id: OpId,
    /// Parent operation IDs (`0` for root init, `1` for normal transition, `>=2` for concurrent reconciliation).
    pub parents: Vec<OpId>,
    /// Unix epoch timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Session or agent identifier that recorded this operation.
    pub agent_id: String,
    /// Human-readable description of the state transition.
    pub description: String,
    /// Marks high-frequency working-copy micro-snapshots eligible for compaction during `tes gc`.
    pub is_ephemeral: bool,
    /// Complete repository [`View`] after this operation.
    pub view: View,
}

impl Operation {
    /// Constructs a new [`Operation`] and computes its deterministic Blake3 [`OpId`].
    ///
    /// # Errors
    /// Returns [`CoreError::Serialization`] if the [`View`] cannot be serialized.
    pub fn new(
        parents: Vec<OpId>,
        agent_id: impl Into<String>,
        description: impl Into<String>,
        is_ephemeral: bool,
        view: View,
    ) -> CoreResult<Self> {
        let agent_id = agent_id.into();
        let description = description.into();
        let timestamp_ms = now_millis();

        let mut hasher = blake3::Hasher::new();

        for p in &parents {
            hasher.update(p.as_str().as_bytes());
            hasher.update(b"|");
        }

        hasher.update(agent_id.as_bytes());
        hasher.update(b"|");
        hasher.update(description.as_bytes());
        hasher.update(b"|");
        hasher.update(&[u8::from(is_ephemeral)]);
        hasher.update(b"|");

        serde_json::to_writer(&mut hasher, &view)
            .map_err(|e| CoreError::Serialization(e.to_string()))?;

        // Include timestamp_ms so repeated identical commands produce distinct operations if needed.
        hasher.update(&timestamp_ms.to_le_bytes());

        let id = OpId(hasher.finalize().to_hex().to_string());

        Ok(Self {
            id,
            parents,
            timestamp_ms,
            agent_id,
            description,
            is_ephemeral,
            view,
        })
    }
}

/// Manages the colocated `.git/tessera` directory, including the lock-free `OpLog`,
/// `op_heads`, `conflicts` store, and `config.toml`.
#[derive(Debug, Clone)]
pub struct OpLogStore {
    store_dir: PathBuf,
}

impl OpLogStore {
    /// Creates an [`OpLogStore`] handle rooted at `.git/tessera`.
    pub fn new(git_dir: &Path) -> Self {
        Self {
            store_dir: git_dir.join("tessera"),
        }
    }

    /// Returns the path to `.git/tessera`.
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }

    /// Returns `true` if `.git/tessera/op_heads` exists.
    pub fn is_initialized(&self) -> bool {
        self.op_heads_dir().is_dir()
    }

    /// Returns the directory storing serialized [`Operation`] JSON records (`.git/tessera/oplog`).
    fn oplog_dir(&self) -> PathBuf {
        self.store_dir.join("oplog")
    }

    /// Returns the directory storing active operation head markers (`.git/tessera/op_heads`).
    fn op_heads_dir(&self) -> PathBuf {
        self.store_dir.join("op_heads")
    }

    /// Returns the directory storing structured [`ConflictRecord`] files (`.git/tessera/conflicts`).
    fn conflicts_dir(&self) -> PathBuf {
        self.store_dir.join("conflicts")
    }

    /// Returns the path to `.git/tessera/config.toml`.
    fn config_path(&self) -> PathBuf {
        self.store_dir.join("config.toml")
    }

    /// Returns the path to `.git/tessera/trusted_config.hash` storing the approved Blake3 digest
    /// of `<repo_root>/tessera.toml`.
    fn trusted_config_hash_path(&self) -> PathBuf {
        self.store_dir.join("trusted_config.hash")
    }

    /// Initializes the `.git/tessera` directory layout, default `config.toml`, and initial root [`Operation`].
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] or [`CoreError::Serialization`] if directory creation or state persistence fails.
    pub fn init(&self, initial_view: View) -> CoreResult<Operation> {
        for dir in [
            self.store_dir.clone(),
            self.oplog_dir(),
            self.op_heads_dir(),
            self.conflicts_dir(),
            self.store_dir.join("stat_cache"),
        ] {
            fs::create_dir_all(&dir).map_err(|source| CoreError::Io {
                path: dir.display().to_string(),
                source,
            })?;
        }

        if !self.config_path().exists() {
            let cfg = RepoConfig::default();
            self.save_config(&cfg)?;
        }

        let current_heads = self.list_op_heads()?;

        if let Some(first_head) = current_heads.first() {
            return self.read_operation(first_head);
        }

        let root_op = Operation::new(
            Vec::new(),
            "system",
            "initialize tes repository",
            false,
            initial_view,
        )?;
        self.write_operation(&root_op)?;
        Ok(root_op)
    }

    /// Marks the current `<repo_root>/tessera.toml` file as locally trusted by recording its
    /// Blake3 digest in `.git/tessera/trusted_config.hash`.
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if `<repo_root>/tessera.toml` does not exist, or [`CoreError::Io`]
    /// if reading or persisting the digest fails.
    pub fn trust_repo_config(&self) -> CoreResult<String> {
        let repo_root = self
            .store_dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| CoreError::NotFound("repository root".to_string()))?;
        let root_toml = repo_root.join("tessera.toml");

        if !root_toml.is_file() {
            return Err(CoreError::NotFound(root_toml.display().to_string()));
        }

        let raw_bytes = fs::read(&root_toml).map_err(|source| CoreError::Io {
            path: root_toml.display().to_string(),
            source,
        })?;
        let digest = blake3::hash(&raw_bytes).to_hex().to_string();
        atomic_write_file(&self.trusted_config_hash_path(), digest.as_bytes())?;
        Ok(digest)
    }

    /// Revokes local trust for `<repo_root>/tessera.toml` by removing `.git/tessera/trusted_config.hash`.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] if removing an existing `.git/tessera/trusted_config.hash` file fails.
    pub fn untrust_repo_config(&self) -> CoreResult<bool> {
        let hash_path = self.trusted_config_hash_path();

        if !hash_path.exists() {
            return Ok(false);
        }

        fs::remove_file(&hash_path).map_err(|source| CoreError::Io {
            path: hash_path.display().to_string(),
            source,
        })?;
        Ok(true)
    }

    /// Loads `.git/tessera/config.toml` (and overlays declarative settings from `<repo_root>/tessera.toml`
    /// if present, only applying privileged execution fields when `tessera.toml` is explicitly trusted)
    /// or returns [`RepoConfig::default()`].
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] or [`CoreError::Serialization`] if `.git/tessera/config.toml` cannot be read or parsed.
    pub fn load_config(&self) -> CoreResult<RepoConfig> {
        let path = self.config_path();
        let mut cfg = if path.exists() {
            let raw = fs::read_to_string(&path).map_err(|source| CoreError::Io {
                path: path.display().to_string(),
                source,
            })?;
            toml::from_str::<RepoConfig>(&raw)
                .map_err(|e| CoreError::Serialization(e.to_string()))?
        } else {
            RepoConfig::default()
        };

        if let Some(repo_root) = self.store_dir.parent().and_then(Path::parent) {
            let root_toml = repo_root.join("tessera.toml");

            if root_toml.is_file() {
                if let Ok(raw_root) = fs::read_to_string(&root_toml) {
                    if let Ok(overlay) = toml::from_str::<RepoConfig>(&raw_root) {
                        for dir in overlay.cow_cache_dirs {
                            if let Ok(clean_dir) = validate_repo_rel_path(&dir) {
                                if !cfg.cow_cache_dirs.contains(&clean_dir) {
                                    cfg.cow_cache_dirs.push(clean_dir);
                                }
                            }
                        }

                        for glob in overlay.package_globs {
                            let trimmed = glob.trim();

                            if !trimmed.is_empty()
                                && !trimmed.starts_with('/')
                                && !trimmed.contains("..")
                                && !cfg.package_globs.contains(&trimmed.to_string())
                            {
                                cfg.package_globs.push(trimmed.to_string());
                            }
                        }

                        if let Ok(table) = toml::from_str::<toml::Table>(&raw_root) {
                            if let Some(toml::Value::Boolean(b)) = table.get("co_author") {
                                cfg.co_author = *b;
                            }

                            if let Some(toml::Value::Boolean(b)) = table.get("port_isolation") {
                                cfg.port_isolation = *b;
                            }

                            if let Some(toml::Value::String(s)) = table.get("co_author_trailer") {
                                let clean_trailer = s.trim();

                                if !clean_trailer.is_empty() && !clean_trailer.contains('\n') {
                                    cfg.co_author_trailer = clean_trailer.to_string();
                                }
                            }
                        }

                        let digest = blake3::hash(raw_root.as_bytes()).to_hex().to_string();
                        let is_trusted = fs::read_to_string(self.trusted_config_hash_path())
                            .map(|saved| saved.trim() == digest)
                            .unwrap_or(false);

                        if is_trusted {
                            for bin_dir in overlay.extra_bin_dirs {
                                if let Ok(clean_bin) = validate_repo_rel_path(&bin_dir) {
                                    if !cfg.extra_bin_dirs.contains(&clean_bin) {
                                        cfg.extra_bin_dirs.push(clean_bin);
                                    }
                                }
                            }

                            for (k, v) in overlay.custom_env {
                                cfg.custom_env.insert(k, v);
                            }

                            if overlay.verify_command.is_some() {
                                cfg.verify_command = overlay.verify_command;
                            }

                            if overlay.pager.is_some() {
                                cfg.pager = overlay.pager;
                            }
                        }
                    }
                }
            }
        }

        Ok(cfg)
    }

    /// Atomically saves [`RepoConfig`] to `.git/tessera/config.toml`.
    ///
    /// # Errors
    /// Returns [`CoreError::Serialization`] or [`CoreError::Io`] on failure.
    pub fn save_config(&self, config: &RepoConfig) -> CoreResult<()> {
        let serialized =
            toml::to_string_pretty(config).map_err(|e| CoreError::Serialization(e.to_string()))?;
        atomic_write_file(&self.config_path(), serialized.as_bytes())
    }

    /// Lists all active [`OpId`] heads currently present in `.git/tessera/op_heads/`.
    ///
    /// Sorted deterministically by [`OpId`].
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] if `.git/tessera/op_heads` cannot be read.
    pub fn list_op_heads(&self) -> CoreResult<Vec<OpId>> {
        let dir = self.op_heads_dir();

        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut heads = BTreeSet::new();
        let entries = fs::read_dir(&dir).map_err(|source| CoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;

        for entry in entries {
            let entry = entry.map_err(|source| CoreError::Io {
                path: dir.display().to_string(),
                source,
            })?;
            let name = entry.file_name();

            if let Some(name_str) = name.to_str() {
                if !name_str.starts_with('.') && validate_hex_id("operation", name_str).is_ok() {
                    heads.insert(OpId(name_str.to_string()));
                }
            }
        }

        Ok(heads.into_iter().collect())
    }

    /// Reads an [`Operation`] by its [`OpId`] from `.git/tessera/oplog/<id>.json`.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `id` is not a valid hex identifier,
    /// [`CoreError::NotFound`] if the operation does not exist, or [`CoreError::Serialization`]
    /// if the JSON payload is corrupt.
    pub fn read_operation(&self, id: &OpId) -> CoreResult<Operation> {
        validate_hex_id("operation", id.as_str())?;
        let path = self.oplog_dir().join(format!("{}.json", id.as_str()));

        if !path.exists() {
            return Err(CoreError::NotFound(format!("operation {}", id.as_str())));
        }

        let bytes = fs::read(&path).map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;

        serde_json::from_slice(&bytes).map_err(|e| CoreError::Serialization(e.to_string()))
    }

    /// Resolves an [`OpId`] prefix (or full ID) against `.git/tessera/oplog/`.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `prefix` is not valid hexadecimal, or
    /// [`CoreError::NotFound`] if no operation matches `prefix`.
    pub fn resolve_op_prefix(&self, prefix: &str) -> CoreResult<OpId> {
        validate_hex_id("operation prefix", prefix.trim())?;
        let dir = self.oplog_dir();
        let entries = fs::read_dir(&dir).map_err(|source| CoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;

        for entry in entries.flatten() {
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                if stem.starts_with(prefix) && validate_hex_id("operation", stem).is_ok() {
                    return Ok(OpId(stem.to_string()));
                }
            }
        }

        Err(CoreError::NotFound(format!(
            "no operation matching prefix '{prefix}'"
        )))
    }

    /// Persists `op` into `.git/tessera/oplog/<id>.json`, atomically adds `<id>` to `.git/tessera/op_heads/`,
    /// and removes its parent head markers.
    ///
    /// # Crash & Concurrency Safety
    /// - The operation payload is written atomically *before* its head pointer is published.
    /// - The new head pointer is created *before* parent head pointers are unlinked, guaranteeing
    ///   `op_heads/` is never transiently empty even on `SIGKILL`.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`], [`CoreError::Io`], or [`CoreError::Serialization`] on write failure.
    pub fn write_operation(&self, op: &Operation) -> CoreResult<()> {
        validate_hex_id("operation", op.id.as_str())?;
        let op_path = self.oplog_dir().join(format!("{}.json", op.id.as_str()));
        let payload =
            serde_json::to_vec_pretty(op).map_err(|e| CoreError::Serialization(e.to_string()))?;
        atomic_write_file(&op_path, &payload)?;

        let head_path = self.op_heads_dir().join(op.id.as_str());
        atomic_write_file(&head_path, op.id.as_str().as_bytes())?;

        for parent in &op.parents {
            if *parent != op.id && validate_hex_id("operation", parent.as_str()).is_ok() {
                let parent_head = self.op_heads_dir().join(parent.as_str());
                let _ = fs::remove_file(parent_head);
            }
        }

        Ok(())
    }

    /// Loads the current head [`Operation`]. If multiple concurrent `op_heads` exist because
    /// multiple agents wrote simultaneously, automatically reconciles their [`View`]s and
    /// writes a single merged head [`Operation`].
    ///
    /// # Errors
    /// Returns [`CoreError::RepoNotFound`] if `.git/tessera` is not initialized.
    pub fn load_current_reconciled(&self) -> CoreResult<Operation> {
        let heads = self.list_op_heads()?;

        if heads.is_empty() {
            return Err(CoreError::RepoNotFound(
                self.store_dir.display().to_string(),
            ));
        }

        if heads.len() == 1 {
            if let Ok(op) = self.read_operation(&heads[0]) {
                return Ok(op);
            }
        }

        let _lock = self.acquire_oplog_lock();
        self.load_current_reconciled_unlocked()
    }

    /// Path to the advisory transaction and reconciliation lock file (`.git/tessera/oplog.lock`).
    #[must_use]
    pub fn oplog_lock_path(&self) -> PathBuf {
        self.store_dir.join("oplog.lock")
    }

    /// Acquires an exclusive cross-process advisory lock on `.git/tessera/oplog.lock` for the
    /// lifetime of the returned [`fs::File`] handle.
    fn acquire_oplog_lock(&self) -> Option<fs::File> {
        let lock_path = self.oplog_lock_path();
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .ok()?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: `file.as_raw_fd()` is a valid open file descriptor owned by `file` for the
            // duration of this call, and `libc::LOCK_EX` is a standard POSIX advisory flock call.
            let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        }

        Some(file)
    }

    /// Reconciles multiple `op_heads` assuming the caller already holds [`Self::acquire_oplog_lock`].
    fn load_current_reconciled_unlocked(&self) -> CoreResult<Operation> {
        let heads = self.list_op_heads()?;

        if heads.is_empty() {
            return Err(CoreError::RepoNotFound(
                self.store_dir.display().to_string(),
            ));
        }

        if heads.len() == 1 {
            return self.read_operation(&heads[0]);
        }

        let first_op = self.read_operation(&heads[0])?;
        let mut acc_view = first_op.view.clone();

        for next_head_id in &heads[1..] {
            let next_op = self.read_operation(next_head_id)?;

            let base_view = self
                .find_common_parent_view(&first_op, &next_op)
                .unwrap_or_else(|| Self::synthesize_fallback_base_view(&first_op, &next_op));

            acc_view = View::merge_views(&base_view, &acc_view, &next_op.view);
        }

        let reconciled_op = Operation::new(
            heads.clone(),
            "reconciler",
            format!(
                "auto-reconcile {} concurrent agent operations",
                heads.len()
            ),
            true,
            acc_view,
        )?;

        self.write_operation(&reconciled_op)?;
        Ok(reconciled_op)
    }

    /// Synthesizes a conservative common-ancestor [`View`] when two concurrent operations are
    /// separated by more than the bounded BFS window, preventing deleted sessions or branches
    /// from being resurrected during 3-way [`View::merge_views`].
    fn synthesize_fallback_base_view(left: &Operation, right: &Operation) -> View {
        let mut base = View::default();

        for (branch, l_oid) in &left.view.branches {
            if right.view.branches.get(branch) == Some(l_oid) {
                base.branches.insert(branch.clone(), *l_oid);
            }
        }

        for (agent_id, r_agent) in &right.view.agents {
            if !left.view.agents.contains_key(agent_id)
                && r_agent.updated_at_ms <= left.timestamp_ms
            {
                base.agents.insert(agent_id.clone(), r_agent.clone());
            }
        }

        for (agent_id, l_agent) in &left.view.agents {
            if (!right.view.agents.contains_key(agent_id)
                && l_agent.updated_at_ms <= right.timestamp_ms)
                || right.view.agents.get(agent_id) == Some(l_agent)
            {
                base.agents.insert(agent_id.clone(), l_agent.clone());
            }
        }

        for (path, l_cid) in &left.view.conflicts {
            if right.view.conflicts.get(path) == Some(l_cid) {
                base.conflicts.insert(path.clone(), l_cid.clone());
            }
        }

        base
    }

    /// Compares two [`View`] states ignoring `updated_at_ms` timestamp jitter on unchanged agents.
    fn views_semantically_equal(left: &View, right: &View) -> bool {
        if left == right {
            return true;
        }

        if left.branches != right.branches
            || left.changes != right.changes
            || left.conflicts != right.conflicts
            || left.observed_blobs != right.observed_blobs
            || left.stashes != right.stashes
            || left.next_agent_slot != right.next_agent_slot
            || left.agents.len() != right.agents.len()
        {
            return false;
        }

        for (agent_id, l_agent) in &left.agents {
            let Some(r_agent) = right.agents.get(agent_id) else {
                return false;
            };

            let mut l_norm = l_agent.clone();
            let mut r_norm = r_agent.clone();
            l_norm.updated_at_ms = 0;
            r_norm.updated_at_ms = 0;

            if l_norm != r_norm {
                return false;
            }
        }

        true
    }

    /// Performs a bounded bidirectional BFS over operation parents to locate the most recent
    /// common ancestor [`View`] between `left` and `right`.
    fn find_common_parent_view(&self, left: &Operation, right: &Operation) -> Option<View> {
        let mut left_ancestors = BTreeSet::new();
        let mut left_queue = std::collections::VecDeque::new();
        left_ancestors.insert(left.id.clone());
        left_queue.push_back(left.id.clone());

        let mut right_ancestors = BTreeSet::new();
        let mut right_queue = std::collections::VecDeque::new();
        right_ancestors.insert(right.id.clone());
        right_queue.push_back(right.id.clone());

        for _ in 0..512 {
            if left_queue.is_empty() && right_queue.is_empty() {
                break;
            }

            if let Some(lid) = left_queue.pop_front() {
                if right_ancestors.contains(&lid) {
                    if let Ok(op) = self.read_operation(&lid) {
                        return Some(op.view);
                    }
                }

                if let Ok(lop) = self.read_operation(&lid) {
                    for p in lop.parents {
                        if right_ancestors.contains(&p) {
                            if let Ok(op) = self.read_operation(&p) {
                                return Some(op.view);
                            }
                        }

                        if left_ancestors.insert(p.clone()) {
                            left_queue.push_back(p);
                        }
                    }
                }
            }

            if let Some(rid) = right_queue.pop_front() {
                if left_ancestors.contains(&rid) {
                    if let Ok(op) = self.read_operation(&rid) {
                        return Some(op.view);
                    }
                }

                if let Ok(rop) = self.read_operation(&rid) {
                    for p in rop.parents {
                        if left_ancestors.contains(&p) {
                            if let Ok(op) = self.read_operation(&p) {
                                return Some(op.view);
                            }
                        }

                        if right_ancestors.insert(p.clone()) {
                            right_queue.push_back(p);
                        }
                    }
                }
            }
        }

        None
    }

    /// Atomically applies a state transition closure `f` to the current [`View`] and records
    /// a new [`Operation`] in the lock-free `OpLog`.
    ///
    /// # Errors
    /// Propagates any error from loading the current state, executing `f`, or writing the new operation.
    pub fn record_tx<F>(
        &self,
        agent_id: &str,
        description: impl Into<String>,
        is_ephemeral: bool,
        f: F,
    ) -> CoreResult<Operation>
    where
        F: FnOnce(&mut View) -> CoreResult<()>,
    {
        let _lock = self.acquire_oplog_lock();
        let current = self.load_current_reconciled_unlocked()?;
        let mut next_view = current.view.clone();
        f(&mut next_view)?;

        if is_ephemeral && Self::views_semantically_equal(&current.view, &next_view) {
            return Ok(current);
        }

        let next_op = Operation::new(
            vec![current.id],
            agent_id,
            description,
            is_ephemeral,
            next_view,
        )?;

        self.write_operation(&next_op)?;
        Ok(next_op)
    }

    /// Walks the operation graph backward from the current head and returns operations in
    /// reverse chronological order.
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading operation files fails.
    pub fn list_history(&self, limit: usize) -> CoreResult<Vec<Operation>> {
        let head = self.load_current_reconciled()?;
        let mut out = Vec::new();
        let mut visited = BTreeSet::new();
        let mut queue = vec![head.id];

        while let Some(curr_id) = queue.pop() {
            if !visited.insert(curr_id.clone()) {
                continue;
            }

            if let Ok(op) = self.read_operation(&curr_id) {
                for parent in op.parents.iter().rev() {
                    queue.push(parent.clone());
                }

                out.push(op);

                if out.len() >= limit {
                    break;
                }
            }
        }

        out.sort_by_key(|b| std::cmp::Reverse(b.timestamp_ms));
        Ok(out)
    }

    /// Compacts ephemeral micro-snapshots in `.git/tessera/oplog/`, keeping the current head,
    /// root operation, all non-ephemeral operations, and any operation referenced by an agent checkpoint.
    ///
    /// Returns the count of compacted ephemeral operation records removed from disk.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] on filesystem traversal failure.
    pub fn gc_compact(&self) -> CoreResult<usize> {
        let head = self.load_current_reconciled()?;
        let mut protected = BTreeSet::new();
        protected.insert(head.id.clone());

        for parent in &head.parents {
            protected.insert(parent.clone());
        }

        for agent in head.view.agents.values() {
            for cp_op in agent.checkpoints.values() {
                protected.insert(cp_op.clone());
            }
        }

        let dir = self.oplog_dir();
        let entries = fs::read_dir(&dir).map_err(|source| CoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;

        let mut removed = 0usize;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            if let Ok(bytes) = fs::read(&path) {
                if let Ok(op) = serde_json::from_slice::<Operation>(&bytes) {
                    if op.is_ephemeral
                        && !op.parents.is_empty()
                        && !protected.contains(&op.id)
                        && fs::remove_file(&path).is_ok()
                    {
                        removed += 1;
                    }
                }
            }
        }

        Ok(removed)
    }

    /// Saves a structured [`ConflictRecord`] to `.git/tessera/conflicts/<id>.json`.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`], [`CoreError::Serialization`], or [`CoreError::Io`] on failure.
    pub fn save_conflict(&self, conflict: &ConflictRecord) -> CoreResult<()> {
        validate_hex_id("conflict", &conflict.id.0)?;

        if conflict.path != "<speculative-verify>" {
            validate_repo_rel_path(&conflict.path)?;
        }

        let path = self
            .conflicts_dir()
            .join(format!("{}.json", conflict.id.0));
        let bytes = serde_json::to_vec_pretty(conflict)
            .map_err(|e| CoreError::Serialization(e.to_string()))?;
        atomic_write_file(&path, &bytes)
    }

    /// Loads a [`ConflictRecord`] by its [`ConflictId`].
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `id` is not valid hex, or [`CoreError::NotFound`]
    /// if the conflict record does not exist.
    pub fn load_conflict(&self, id: &ConflictId) -> CoreResult<ConflictRecord> {
        validate_hex_id("conflict", &id.0)?;
        let path = self.conflicts_dir().join(format!("{}.json", id.0));

        if !path.exists() {
            return Err(CoreError::NotFound(format!("conflict {}", id.0)));
        }

        let bytes = fs::read(&path).map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;

        let record: ConflictRecord =
            serde_json::from_slice(&bytes).map_err(|e| CoreError::Serialization(e.to_string()))?;

        if record.path != "<speculative-verify>" {
            validate_repo_rel_path(&record.path)?;
        }

        Ok(record)
    }

    /// Removes a resolved [`ConflictRecord`] file from `.git/tessera/conflicts/<id>.json`.
    pub fn delete_conflict(&self, id: &ConflictId) {
        if validate_hex_id("conflict", &id.0).is_ok() {
            let path = self.conflicts_dir().join(format!("{}.json", id.0));
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn concurrent_multi_agent_oplog_writes_auto_reconcile_without_locks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(OpLogStore::new(tmp.path()));
        let root_op = store.init(View::default()).expect("init oplog");

        // Spawn 8 threads writing simultaneous operations referencing the same parent head.
        let mut handles = Vec::new();
        for idx in 0..8u16 {
            let store_clone = Arc::clone(&store);
            let parent_id = root_op.id.clone();
            let parent_view = root_op.view.clone();
            handles.push(thread::spawn(move || {
                let mut view = parent_view;
                let agent_id = format!("agent-{idx}");
                let cid = ChangeId::from_seed(&agent_id);
                let dummy_oid =
                    ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
                view.agents.insert(
                    agent_id.clone(),
                    AgentMetadata {
                        agent_id: agent_id.clone(),
                        slot: idx + 1,
                        change_id: cid,
                        base_commit: dummy_oid,
                        head_commit: dummy_oid,
                        head_tree: dummy_oid,
                        intent: format!("Task for {agent_id}"),
                        intent_inferred: false,
                        claimed_symbols: BTreeSet::new(),
                        scopes: Vec::new(),
                        workspace_path: None,
                        read_blobs: BTreeMap::new(),
                        own_blobs: BTreeMap::new(),
                        deleted_paths: BTreeSet::new(),
                        foreign_baseline: BTreeSet::new(),
                        integrated_peer_blobs: BTreeMap::new(),
                        target_branch: None,
                        checkpoints: BTreeMap::new(),
                        waypoint: None,
                        updated_at_ms: 1000 + u64::from(idx),
                    },
                );
                view.next_agent_slot = idx + 2;
                let op = Operation::new(
                    vec![parent_id],
                    &agent_id,
                    "concurrent agent registration",
                    true,
                    view,
                )
                .expect("create op");
                store_clone.write_operation(&op).expect("write op");
            }));
        }

        for h in handles {
            h.join().expect("thread join");
        }

        // Reconcile all 8 concurrent heads and verify zero lost updates.
        let reconciled = store.load_current_reconciled().expect("reconcile");
        assert_eq!(reconciled.view.agents.len(), 8);
        for idx in 0..8 {
            assert!(reconciled.view.agents.contains_key(&format!("agent-{idx}")));
        }
        assert_eq!(store.list_op_heads().expect("heads").len(), 1);
    }

    #[test]
    fn untrusted_repo_config_does_not_overlay_privileged_execution_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).expect("create .git");
        let store = OpLogStore::new(&git_dir);
        store.init(View::default()).expect("init store");

        let root_toml = tmp.path().join("tessera.toml");
        fs::write(
            &root_toml,
            r#"
verify_command = "echo pwned"
pager = "less -FRX"
extra_bin_dirs = ["evil_bin", "../outside_bin"]
cow_cache_dirs = ["custom_cache", "../outside_cache"]

[custom_env]
LD_PRELOAD = "/tmp/evil.so"
"#,
        )
        .expect("write tessera.toml");

        let cfg_untrusted = store.load_config().expect("load untrusted config");
        assert!(cfg_untrusted.verify_command.is_none());
        assert!(cfg_untrusted.pager.is_none());
        assert!(!cfg_untrusted.extra_bin_dirs.contains(&"evil_bin".to_string()));
        assert!(!cfg_untrusted.custom_env.contains_key("LD_PRELOAD"));
        assert!(cfg_untrusted.cow_cache_dirs.contains(&"custom_cache".to_string()));
        assert!(!cfg_untrusted.cow_cache_dirs.contains(&"../outside_cache".to_string()));

        store.trust_repo_config().expect("trust config");
        let cfg_trusted = store.load_config().expect("load trusted config");
        assert_eq!(cfg_trusted.verify_command.as_deref(), Some("echo pwned"));
        assert_eq!(cfg_trusted.pager.as_deref(), Some("less -FRX"));
        assert!(cfg_trusted.extra_bin_dirs.contains(&"evil_bin".to_string()));
        assert!(!cfg_trusted.extra_bin_dirs.contains(&"../outside_bin".to_string()));

        store.untrust_repo_config().expect("untrust config");
        let cfg_revoked = store.load_config().expect("load revoked config");
        assert!(cfg_revoked.verify_command.is_none());
        assert!(cfg_revoked.pager.is_none());
    }

    #[test]
    fn ephemeral_noop_record_tx_skips_redundant_oplog_writes_and_fallback_base_prunes_dead_sessions(
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = OpLogStore::new(tmp.path());
        store.init(View::default()).expect("init store");

        let dummy_oid =
            ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");

        let op1 = store
            .record_tx("term-100", "register session", true, |view| {
                view.agents.insert(
                    "term-100".to_string(),
                    AgentMetadata {
                        agent_id: "term-100".to_string(),
                        slot: 1,
                        change_id: ChangeId::from_seed("term-100"),
                        base_commit: dummy_oid,
                        head_commit: dummy_oid,
                        head_tree: dummy_oid,
                        intent: "Active session term-100".to_string(),
                        intent_inferred: true,
                        claimed_symbols: BTreeSet::new(),
                        scopes: Vec::new(),
                        workspace_path: None,
                        read_blobs: BTreeMap::new(),
                        own_blobs: BTreeMap::new(),
                        deleted_paths: BTreeSet::new(),
                        foreign_baseline: BTreeSet::new(),
                        integrated_peer_blobs: BTreeMap::new(),
                        target_branch: Some("main".to_string()),
                        checkpoints: BTreeMap::new(),
                        waypoint: None,
                        updated_at_ms: 500,
                    },
                );

                view.next_agent_slot = 2;
                Ok(())
            })
            .expect("record op1");

        let op2 = store
            .record_tx("term-100", "ensure session noop", true, |view| {
                if let Some(agent) = view.agents.get_mut("term-100") {
                    agent.updated_at_ms = 999;
                }

                Ok(())
            })
            .expect("record noop op2");

        assert_eq!(op1.id, op2.id);

        let mut pruned_view = op1.view.clone();
        pruned_view.agents.remove("term-100");

        let left_op = Operation {
            id: OpId("2222222222222222222222222222222222222222".to_string()),
            parents: Vec::new(),
            timestamp_ms: 1000,
            agent_id: "gc".to_string(),
            description: "prune dead shell sessions".to_string(),
            is_ephemeral: true,
            view: pruned_view,
        };

        let right_op = Operation {
            id: OpId("3333333333333333333333333333333333333333".to_string()),
            parents: Vec::new(),
            timestamp_ms: 900,
            agent_id: "watcher".to_string(),
            description: "concurrent op with stale session".to_string(),
            is_ephemeral: true,
            view: op1.view.clone(),
        };

        let fallback_base = OpLogStore::synthesize_fallback_base_view(&left_op, &right_op);
        let merged = View::merge_views(&fallback_base, &left_op.view, &right_op.view);
        assert!(!merged.agents.contains_key("term-100"));
    }
}

