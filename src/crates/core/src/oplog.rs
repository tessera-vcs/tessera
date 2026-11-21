//! Lock-free content-addressed Operation Log (`OpLog`) and concurrent state reconciliation.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{
    AgentMetadata, ChangeId, ConflictId, ConflictRecord, CoreError, CoreResult, ObjectId, OpId,
    RepoConfig, StashEntry, atomic_write_file, now_millis, validate_hex_id, validate_ref_name,
    validate_repo_rel_path,
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
                (b_opt, Some(l_agent)) => {
                    let combined = merge_agent_metadata(b_opt, l_agent, r_agent);
                    merged.agents.insert(agent_id.clone(), combined);
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

        for base_cid in base.changes.keys() {
            if !right.changes.contains_key(base_cid)
                && left.changes.get(base_cid) == base.changes.get(base_cid)
            {
                merged.changes.remove(base_cid);
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

        let mut all_obs_paths = BTreeSet::new();

        for k in base
            .observed_blobs
            .keys()
            .chain(left.observed_blobs.keys())
            .chain(right.observed_blobs.keys())
        {
            all_obs_paths.insert(k.clone());
        }

        let mut next_observed = BTreeMap::new();

        for path in all_obs_paths {
            let b_list = base.observed_blobs.get(&path);
            let l_list = left.observed_blobs.get(&path);
            let r_list = right.observed_blobs.get(&path);

            match (b_list, l_list, r_list) {
                (b, l, r) if l == b => {
                    if let Some(rv) = r {
                        if !rv.is_empty() {
                            next_observed.insert(path, rv.clone());
                        }
                    }
                }
                (b, l, r) if r == b => {
                    if let Some(lv) = l {
                        if !lv.is_empty() {
                            next_observed.insert(path, lv.clone());
                        }
                    }
                }
                (b_opt, l_opt, r_opt) => {
                    let b_set: BTreeSet<ObjectId> = b_opt
                        .map(|v| v.iter().copied().collect())
                        .unwrap_or_default();
                    let l_cleared = b_opt.is_some() && l_opt.is_none();
                    let r_cleared = b_opt.is_some() && r_opt.is_none();
                    let mut combined = Vec::new();

                    if let Some(lv) = l_opt {
                        for bid in lv {
                            if !(r_cleared && b_set.contains(bid)) && !combined.contains(bid) {
                                combined.push(*bid);
                            }
                        }
                    }

                    if let Some(rv) = r_opt {
                        for bid in rv {
                            if !(l_cleared && b_set.contains(bid)) && !combined.contains(bid) {
                                combined.push(*bid);
                            }
                        }
                    }

                    if !combined.is_empty() {
                        next_observed.insert(path, combined);
                    }
                }
            }
        }

        merged.observed_blobs = next_observed;

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

        let mut seen_slots = BTreeSet::new();

        for agent in merged.agents.values_mut() {
            if agent.slot == 0 || !seen_slots.insert(agent.slot) {
                let mut next_slot = merged.next_agent_slot.max(1);

                while seen_slots.contains(&next_slot) && next_slot < u16::MAX {
                    next_slot = next_slot.saturating_add(1);
                }

                agent.slot = next_slot;
                seen_slots.insert(next_slot);
                merged.next_agent_slot = next_slot.saturating_add(1);
            }
        }

        merged
    }
}

/// Symmetrically 3-way merges two concurrent [`AgentMetadata`] revisions relative to `base_opt`,
/// preserving additions from both sides while honoring deletions/commits from either side.
fn merge_agent_metadata(
    base_opt: Option<&AgentMetadata>,
    left: &AgentMetadata,
    right: &AgentMetadata,
) -> AgentMetadata {
    let (primary, secondary) = if right.updated_at_ms > left.updated_at_ms {
        (right, left)
    } else {
        (left, right)
    };

    let mut combined = primary.clone();

    let empty_syms = BTreeSet::new();
    let base_syms = base_opt.map_or(&empty_syms, |b| &b.claimed_symbols);
    combined.claimed_symbols = merge_set_3way(
        base_syms,
        &primary.claimed_symbols,
        &secondary.claimed_symbols,
    );

    let empty_blobs = BTreeMap::new();
    let base_read = base_opt.map_or(&empty_blobs, |b| &b.read_blobs);
    combined.read_blobs = merge_map_3way(base_read, &primary.read_blobs, &secondary.read_blobs);

    let primary_committed = base_opt.is_some_and(|b| {
        b.base_commit != primary.base_commit
            && primary.own_blobs.is_empty()
            && !b.own_blobs.is_empty()
    });
    let secondary_committed = base_opt.is_some_and(|b| {
        b.base_commit != secondary.base_commit
            && secondary.own_blobs.is_empty()
            && !b.own_blobs.is_empty()
    });

    if !primary_committed && !secondary_committed {
        let base_own = base_opt.map_or(&empty_blobs, |b| &b.own_blobs);
        combined.own_blobs = merge_map_3way(base_own, &primary.own_blobs, &secondary.own_blobs);

        let empty_del = BTreeSet::new();
        let base_del = base_opt.map_or(&empty_del, |b| &b.deleted_paths);
        combined.deleted_paths =
            merge_set_3way(base_del, &primary.deleted_paths, &secondary.deleted_paths);

        for del in &combined.deleted_paths {
            if !primary.own_blobs.contains_key(del) {
                combined.own_blobs.remove(del);
            }
        }
    } else if secondary_committed && !primary_committed {
        combined.base_commit = secondary.base_commit;
        combined.head_commit = secondary.head_commit;
        combined.head_tree = secondary.head_tree;
        let base_own = base_opt.map_or(&empty_blobs, |b| &b.own_blobs);
        combined.own_blobs.retain(|k, v| base_own.get(k) != Some(v));
    }

    let empty_fb = BTreeSet::new();
    let base_fb = base_opt.map_or(&empty_fb, |b| &b.foreign_baseline);
    combined.foreign_baseline = merge_set_3way(
        base_fb,
        &primary.foreign_baseline,
        &secondary.foreign_baseline,
    );

    let empty_ipb: BTreeMap<String, BTreeMap<String, ObjectId>> = BTreeMap::new();
    let base_ipb = base_opt.map_or(&empty_ipb, |b| &b.integrated_peer_blobs);
    combined.integrated_peer_blobs = merge_map_3way(
        base_ipb,
        &primary.integrated_peer_blobs,
        &secondary.integrated_peer_blobs,
    );

    let empty_cp = BTreeMap::new();
    let base_cp = base_opt.map_or(&empty_cp, |b| &b.checkpoints);
    combined.checkpoints = merge_map_3way(base_cp, &primary.checkpoints, &secondary.checkpoints);

    if combined.target_branch.is_none() {
        combined.target_branch = secondary.target_branch.clone();
    }

    if combined.waypoint.is_none() {
        combined.waypoint = secondary.waypoint.clone();
    }

    if combined.scopes.is_empty() && !secondary.scopes.is_empty() {
        combined.scopes = secondary.scopes.clone();
    }

    combined
}

/// 3-way merges two `BTreeSet`s relative to `base`, keeping additions from either side and
/// propagating deletions when the other side left the item unchanged.
fn merge_set_3way<T: Ord + Clone>(
    base: &BTreeSet<T>,
    primary: &BTreeSet<T>,
    secondary: &BTreeSet<T>,
) -> BTreeSet<T> {
    let mut out = primary.clone();

    for item in secondary {
        if !base.contains(item) {
            out.insert(item.clone());
        }
    }

    for item in base {
        if !secondary.contains(item) && primary.contains(item) {
            out.remove(item);
        }
    }

    out
}

/// 3-way merges two `BTreeMap`s relative to `base`, preferring `primary` on concurrent edits to
/// the same key while accepting non-conflicting additions, updates, and deletions from `secondary`.
fn merge_map_3way<K: Ord + Clone, V: Eq + Clone>(
    base: &BTreeMap<K, V>,
    primary: &BTreeMap<K, V>,
    secondary: &BTreeMap<K, V>,
) -> BTreeMap<K, V> {
    let mut out = primary.clone();

    for (k, sec_v) in secondary {
        if primary.get(k) == base.get(k) && Some(sec_v) != base.get(k) {
            out.insert(k.clone(), sec_v.clone());
        }
    }

    for (k, base_v) in base {
        if !secondary.contains_key(k) && primary.get(k) == Some(base_v) {
            out.remove(k);
        }
    }

    out
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

/// Partial configuration overlay deserialized from `<repo_root>/tessera.toml`.
///
/// Using `Option` for every field prevents omitted keys from re-injecting [`RepoConfig::default()`]
/// values over explicit removals in `.git/tessera/config.toml`.
#[derive(Debug, Default, Deserialize)]
struct RepoConfigOverlay {
    trunk_branch: Option<String>,
    base_port: Option<u16>,
    port_stride: Option<u16>,
    cow_cache_dirs: Option<Vec<String>>,
    extra_bin_dirs: Option<Vec<String>>,
    package_globs: Option<Vec<String>>,
    verify_command: Option<String>,
    watch_interval_ms: Option<u64>,
    watch_timeout_secs: Option<u64>,
    co_author: Option<bool>,
    co_author_trailer: Option<String>,
    pager: Option<String>,
    custom_env: Option<BTreeMap<String, String>>,
    auto_isolate_dirty_claims: Option<bool>,
    port_isolation: Option<bool>,
}

/// Manages the colocated `.git/tessera` directory, including the lock-free `OpLog`,
/// `op_heads`, `conflicts` store, and `config.toml`.
#[derive(Debug, Clone)]
pub struct OpLogStore {
    store_dir: PathBuf,
    repo_root: Option<PathBuf>,
}

impl OpLogStore {
    /// Creates an [`OpLogStore`] handle rooted at `<git_dir>/tessera`.
    pub fn new(git_dir: &Path) -> Self {
        Self {
            store_dir: git_dir.join("tessera"),
            repo_root: git_dir.parent().map(Path::to_path_buf),
        }
    }

    /// Creates an [`OpLogStore`] handle with an explicit `repo_root` (required for linked Git
    /// worktrees and submodules where `common_git_dir` is not directly `<repo_root>/.git`).
    pub fn new_with_repo_root(common_git_dir: &Path, repo_root: &Path) -> Self {
        Self {
            store_dir: common_git_dir.join("tessera"),
            repo_root: Some(repo_root.to_path_buf()),
        }
    }

    /// Returns the resolved repository root associated with this store, if known.
    fn resolved_repo_root(&self) -> Option<&Path> {
        self.repo_root.as_deref()
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

    /// Returns the path to the compacted parent forwarding table (`.git/tessera/oplog/compacted_parents.json`).
    fn compacted_parents_path(&self) -> PathBuf {
        self.oplog_dir().join("compacted_parents.json")
    }

    /// Loads the compacted parent forwarding map (`deleted_ephemeral_op -> surviving_ancestor_ops`).
    fn load_compacted_parents(&self) -> BTreeMap<OpId, Vec<OpId>> {
        let path = self.compacted_parents_path();
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Resolves `parents` through the compacted parent forwarding table so callers walking the
    /// OpLog DAG (`list_history`, `find_common_parent_view`, `undo`) seamlessly bridge deleted
    /// ephemeral operations.
    #[must_use]
    pub fn resolve_parent_ids(&self, parents: &[OpId]) -> Vec<OpId> {
        let map = self.load_compacted_parents();

        if map.is_empty() {
            return parents.to_vec();
        }

        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut stack: Vec<OpId> = parents.iter().rev().cloned().collect();

        while let Some(id) = stack.pop() {
            if let Some(forwarded) = map.get(&id) {
                for anc in forwarded.iter().rev() {
                    if !seen.contains(anc) {
                        stack.push(anc.clone());
                    }
                }
            } else if seen.insert(id.clone()) {
                out.push(id);
            }
        }

        out
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
            .resolved_repo_root()
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

    /// Loads only the local `.git/tessera/config.toml` file (or [`RepoConfig::default()`]) without
    /// overlaying `<repo_root>/tessera.toml`.
    ///
    /// Configuration mutation commands (`tes config set`, `add-cache-dir`, etc.) must read via this
    /// method so trusted `tessera.toml` execution settings are never baked permanently into
    /// `.git/tessera/config.toml`.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] or [`CoreError::Serialization`] if `.git/tessera/config.toml` cannot be read or parsed.
    pub fn load_local_config(&self) -> CoreResult<RepoConfig> {
        let path = self.config_path();

        if path.exists() {
            let raw = fs::read_to_string(&path).map_err(|source| CoreError::Io {
                path: path.display().to_string(),
                source,
            })?;
            toml::from_str::<RepoConfig>(&raw).map_err(|e| CoreError::Serialization(e.to_string()))
        } else {
            Ok(RepoConfig::default())
        }
    }

    /// Returns `true` if `<repo_root>/tessera.toml` exists and its Blake3 digest matches `.git/tessera/trusted_config.hash`.
    #[must_use]
    pub fn is_repo_config_trusted(&self) -> bool {
        let Some(repo_root) = self.resolved_repo_root() else {
            return false;
        };
        let root_toml = repo_root.join("tessera.toml");
        let Ok(raw_bytes) = fs::read(&root_toml) else {
            return false;
        };
        let digest = blake3::hash(&raw_bytes).to_hex().to_string();
        fs::read_to_string(self.trusted_config_hash_path())
            .map(|saved| saved.trim() == digest)
            .unwrap_or(false)
    }

    /// Loads `.git/tessera/config.toml` (and overlays declarative settings from `<repo_root>/tessera.toml`
    /// if present, only applying privileged execution fields when `tessera.toml` is explicitly trusted)
    /// or returns [`RepoConfig::default()`].
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] or [`CoreError::Serialization`] if `.git/tessera/config.toml` cannot be read or parsed.
    pub fn load_config(&self) -> CoreResult<RepoConfig> {
        let mut cfg = self.load_local_config()?;

        if let Some(repo_root) = self.resolved_repo_root() {
            let root_toml = repo_root.join("tessera.toml");

            if root_toml.is_file() {
                if let Ok(raw_root) = fs::read_to_string(&root_toml) {
                    if let Ok(overlay) = toml::from_str::<RepoConfigOverlay>(&raw_root) {
                        if let Some(trunk) = overlay.trunk_branch {
                            let clean = trunk.trim();

                            if validate_ref_name(clean).is_ok() {
                                cfg.trunk_branch = clean.to_string();
                            }
                        }

                        if let Some(base_port) = overlay.base_port {
                            if base_port >= 1024 {
                                cfg.base_port = base_port;
                            }
                        }

                        if let Some(port_stride) = overlay.port_stride {
                            if port_stride >= 25 {
                                cfg.port_stride = port_stride;
                            }
                        }

                        if let Some(interval) = overlay.watch_interval_ms {
                            cfg.watch_interval_ms = interval.max(15);
                        }

                        if let Some(timeout) = overlay.watch_timeout_secs {
                            cfg.watch_timeout_secs = timeout.max(5);
                        }

                        if let Some(auto_iso) = overlay.auto_isolate_dirty_claims {
                            cfg.auto_isolate_dirty_claims = auto_iso;
                        }

                        if let Some(cow_dirs) = overlay.cow_cache_dirs {
                            for dir in cow_dirs {
                                if let Ok(clean_dir) = validate_repo_rel_path(&dir) {
                                    if !cfg.cow_cache_dirs.contains(&clean_dir) {
                                        cfg.cow_cache_dirs.push(clean_dir);
                                    }
                                }
                            }
                        }

                        if let Some(globs) = overlay.package_globs {
                            for glob in globs {
                                let trimmed = glob.trim();

                                if !trimmed.is_empty()
                                    && !trimmed.starts_with('/')
                                    && !trimmed.contains("..")
                                    && !cfg.package_globs.contains(&trimmed.to_string())
                                {
                                    cfg.package_globs.push(trimmed.to_string());
                                }
                            }
                        }

                        if let Some(b) = overlay.co_author {
                            cfg.co_author = b;
                        }

                        if let Some(b) = overlay.port_isolation {
                            cfg.port_isolation = b;
                        }

                        if let Some(s) = overlay.co_author_trailer {
                            let clean_trailer = s.trim();

                            if !clean_trailer.is_empty()
                                && !clean_trailer.contains('\n')
                                && !clean_trailer.contains('\r')
                            {
                                cfg.co_author_trailer = clean_trailer.to_string();
                            }
                        }

                        let digest = blake3::hash(raw_root.as_bytes()).to_hex().to_string();
                        let is_trusted = fs::read_to_string(self.trusted_config_hash_path())
                            .map(|saved| saved.trim() == digest)
                            .unwrap_or(false);

                        if is_trusted {
                            if let Some(bin_dirs) = overlay.extra_bin_dirs {
                                for bin_dir in bin_dirs {
                                    if let Ok(clean_bin) = validate_repo_rel_path(&bin_dir) {
                                        if !cfg.extra_bin_dirs.contains(&clean_bin) {
                                            cfg.extra_bin_dirs.push(clean_bin);
                                        }
                                    }
                                }
                            }

                            if let Some(custom_env) = overlay.custom_env {
                                for (k, v) in custom_env {
                                    cfg.custom_env.insert(k, v);
                                }
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

    /// Resolves an [`OpId`] prefix (or full ID) against `.git/tessera/oplog/`, rejecting
    /// ambiguous prefixes that match more than one operation.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `prefix` is not valid hexadecimal or is ambiguous,
    /// or [`CoreError::NotFound`] if no operation matches `prefix`.
    pub fn resolve_op_prefix(&self, prefix: &str) -> CoreResult<OpId> {
        let normalized = prefix.trim().to_ascii_lowercase();
        validate_hex_id("operation prefix", &normalized)?;

        if normalized.len() == 64 {
            let exact = self.oplog_dir().join(format!("{normalized}.json"));

            if exact.is_file() {
                return Ok(OpId(normalized));
            }

            return Err(CoreError::NotFound(format!(
                "no operation matching '{normalized}'"
            )));
        }

        let dir = self.oplog_dir();
        let entries = fs::read_dir(&dir).map_err(|source| CoreError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let mut matches = BTreeSet::new();

        for entry in entries.flatten() {
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }

            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if stem.starts_with(&normalized) && validate_hex_id("operation", stem).is_ok() {
                    matches.insert(OpId(stem.to_string()));
                }
            }
        }

        match matches.len() {
            0 => Err(CoreError::NotFound(format!(
                "no operation matching prefix '{normalized}'"
            ))),
            1 => Ok(matches.into_iter().next().unwrap_or(OpId(normalized))),
            n => Err(CoreError::InvalidArgument(format!(
                "ambiguous operation prefix '{normalized}' matches {n} operations; provide more characters"
            ))),
        }
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

        let _lock = self.acquire_lock()?;
        self.load_current_reconciled_unlocked()
    }

    /// Path to the advisory transaction and reconciliation lock file (`.git/tessera/oplog.lock`).
    #[must_use]
    pub fn oplog_lock_path(&self) -> PathBuf {
        self.store_dir.join("oplog.lock")
    }

    /// Acquires an exclusive cross-process OS file lock on `.git/tessera/oplog.lock` (POSIX `flock`
    /// with `EINTR` retry on Unix, `LockFileEx` on Windows) for the lifetime of the returned handle.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] if `.git/tessera/oplog.lock` cannot be opened or locked.
    pub fn acquire_lock(&self) -> CoreResult<fs::File> {
        let lock_path = self.oplog_lock_path();

        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|source| CoreError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }

        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| CoreError::Io {
                path: lock_path.display().to_string(),
                source,
            })?;

        loop {
            match file.lock() {
                Ok(()) => return Ok(file),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(CoreError::Io {
                        path: lock_path.display().to_string(),
                        source,
                    });
                }
            }
        }
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
            format!("auto-reconcile {} concurrent agent operations", heads.len()),
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

    /// Performs a bounded bidirectional BFS over operation parents (bridging compacted ephemeral
    /// nodes) to locate the most recent common ancestor [`View`] between `left` and `right`.
    fn find_common_parent_view(&self, left: &Operation, right: &Operation) -> Option<View> {
        let compacted = self.load_compacted_parents();
        let expand = |parents: &[OpId]| -> Vec<OpId> {
            let mut out = Vec::new();
            let mut stack: Vec<OpId> = parents.iter().rev().cloned().collect();
            let mut seen = BTreeSet::new();

            while let Some(id) = stack.pop() {
                if let Some(fw) = compacted.get(&id) {
                    for a in fw.iter().rev() {
                        if !seen.contains(a) {
                            stack.push(a.clone());
                        }
                    }
                } else if seen.insert(id.clone()) {
                    out.push(id);
                }
            }

            out
        };

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
                if let Ok(lop) = self.read_operation(&lid) {
                    if right_ancestors.contains(&lid) {
                        return Some(lop.view);
                    }

                    for p in expand(&lop.parents) {
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
                if let Ok(rop) = self.read_operation(&rid) {
                    if left_ancestors.contains(&rid) {
                        return Some(rop.view);
                    }

                    for p in expand(&rop.parents) {
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
        let _lock = self.acquire_lock()?;
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

    /// Walks the operation graph backward from the current head in reverse chronological order,
    /// seamlessly bridging compacted ephemeral operations.
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading operation files fails.
    pub fn list_history(&self, limit: usize) -> CoreResult<Vec<Operation>> {
        let head = self.load_current_reconciled()?;
        let compacted = self.load_compacted_parents();
        let mut out = Vec::new();
        let mut visited = BTreeSet::new();
        let mut heap: BinaryHeap<(u64, OpId)> = BinaryHeap::new();
        let mut loaded: BTreeMap<OpId, Operation> = BTreeMap::new();

        heap.push((head.timestamp_ms, head.id.clone()));
        loaded.insert(head.id.clone(), head);

        while let Some((_, curr_id)) = heap.pop() {
            if !visited.insert(curr_id.clone()) {
                continue;
            }

            let op = match loaded.remove(&curr_id) {
                Some(o) => o,
                None => match self.read_operation(&curr_id) {
                    Ok(o) => o,
                    Err(_) => continue,
                },
            };

            let mut parent_stack: Vec<OpId> = op.parents.iter().rev().cloned().collect();

            while let Some(pid) = parent_stack.pop() {
                if let Some(fw) = compacted.get(&pid) {
                    for anc in fw.iter().rev() {
                        parent_stack.push(anc.clone());
                    }

                    continue;
                }

                if !visited.contains(&pid) && !loaded.contains_key(&pid) {
                    if let Ok(parent_op) = self.read_operation(&pid) {
                        heap.push((parent_op.timestamp_ms, pid.clone()));
                        loaded.insert(pid, parent_op);
                    }
                }
            }

            out.push(op);

            if out.len() >= limit {
                break;
            }
        }

        out.sort_by_key(|b| std::cmp::Reverse(b.timestamp_ms));
        Ok(out)
    }

    /// Compacts ephemeral micro-snapshots in `.git/tessera/oplog/` under `oplog.lock`, keeping
    /// all active `op_heads`, root operation, all non-ephemeral operations, and any operation
    /// referenced by an agent checkpoint while recording parent forwarding links so `list_history()`
    /// and `tes undo` remain unbroken.
    ///
    /// Returns the count of compacted ephemeral operation records removed from disk.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] on filesystem traversal or lock failure.
    pub fn gc_compact(&self) -> CoreResult<usize> {
        let _lock = self.acquire_lock()?;
        let head = self.load_current_reconciled_unlocked()?;
        let active_heads = self.list_op_heads()?;

        let mut protected = BTreeSet::new();
        protected.insert(head.id.clone());

        for h in active_heads {
            protected.insert(h);
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

        let mut all_ops: BTreeMap<OpId, (PathBuf, bool, Vec<OpId>)> = BTreeMap::new();

        for entry in entries.flatten() {
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            if path.file_name().and_then(|n| n.to_str()) == Some("compacted_parents.json") {
                continue;
            }

            if let Ok(bytes) = fs::read(&path) {
                if let Ok(op) = serde_json::from_slice::<Operation>(&bytes) {
                    if !op.is_ephemeral || op.parents.is_empty() {
                        protected.insert(op.id.clone());
                    }

                    all_ops.insert(op.id, (path, op.is_ephemeral, op.parents));
                }
            }
        }

        let to_remove: BTreeSet<OpId> = all_ops
            .iter()
            .filter(|(id, (_, is_eph, parents))| {
                *is_eph && !parents.is_empty() && !protected.contains(*id)
            })
            .map(|(id, _)| id.clone())
            .collect();

        if to_remove.is_empty() {
            return Ok(0);
        }

        let mut compacted_map = self.load_compacted_parents();

        for id in &to_remove {
            let mut surviving = Vec::new();
            let mut seen = BTreeSet::new();
            let mut stack: Vec<OpId> = all_ops
                .get(id)
                .map(|(_, _, p)| p.iter().rev().cloned().collect())
                .unwrap_or_default();

            while let Some(curr) = stack.pop() {
                if !seen.insert(curr.clone()) {
                    continue;
                }

                if let Some(fw) = compacted_map.get(&curr) {
                    for anc in fw.iter().rev() {
                        stack.push(anc.clone());
                    }
                } else if to_remove.contains(&curr) {
                    if let Some((_, _, gp)) = all_ops.get(&curr) {
                        for anc in gp.iter().rev() {
                            stack.push(anc.clone());
                        }
                    }
                } else {
                    surviving.push(curr);
                }
            }

            compacted_map.insert(id.clone(), surviving);
        }

        let forwarding_bytes = serde_json::to_vec(&compacted_map)
            .map_err(|e| CoreError::Serialization(e.to_string()))?;
        atomic_write_file(&self.compacted_parents_path(), &forwarding_bytes)?;

        let mut removed = 0usize;

        for id in &to_remove {
            if let Some((path, _, _)) = all_ops.get(id) {
                if fs::remove_file(path).is_ok() {
                    removed += 1;
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

        let path = self.conflicts_dir().join(format!("{}.json", conflict.id.0));
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
        assert!(
            !cfg_untrusted
                .extra_bin_dirs
                .contains(&"evil_bin".to_string())
        );
        assert!(!cfg_untrusted.custom_env.contains_key("LD_PRELOAD"));
        assert!(
            cfg_untrusted
                .cow_cache_dirs
                .contains(&"custom_cache".to_string())
        );
        assert!(
            !cfg_untrusted
                .cow_cache_dirs
                .contains(&"../outside_cache".to_string())
        );

        store.trust_repo_config().expect("trust config");
        let cfg_trusted = store.load_config().expect("load trusted config");
        assert_eq!(cfg_trusted.verify_command.as_deref(), Some("echo pwned"));
        assert_eq!(cfg_trusted.pager.as_deref(), Some("less -FRX"));
        assert!(cfg_trusted.extra_bin_dirs.contains(&"evil_bin".to_string()));
        assert!(
            !cfg_trusted
                .extra_bin_dirs
                .contains(&"../outside_bin".to_string())
        );

        store.untrust_repo_config().expect("untrust config");
        let cfg_revoked = store.load_config().expect("load revoked config");
        assert!(cfg_revoked.verify_command.is_none());
        assert!(cfg_revoked.pager.is_none());
    }

    #[test]
    fn ephemeral_noop_record_tx_skips_redundant_oplog_writes_and_fallback_base_prunes_dead_sessions()
     {
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

    #[test]
    fn gc_compact_preserves_history_traversal_and_overlay_respects_declarative_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).expect("mkdir .git");
        let store = OpLogStore::new_with_repo_root(&git_dir, tmp.path());
        let root_op = store.init(View::default()).expect("init");

        let eph1 = store
            .record_tx("watcher", "ephemeral 1", true, |v| {
                v.next_agent_slot = 2;
                Ok(())
            })
            .expect("eph1");
        let milestone = store
            .record_tx("agent-a", "commit milestone", false, |v| {
                v.next_agent_slot = 3;
                Ok(())
            })
            .expect("milestone");
        let _head = store
            .record_tx("watcher", "ephemeral head", true, |v| {
                v.next_agent_slot = 4;
                Ok(())
            })
            .expect("head");

        let removed = store.gc_compact().expect("gc_compact");
        assert_eq!(removed, 1);
        assert!(store.read_operation(&eph1.id).is_err());

        let history = store.list_history(20).expect("list_history");
        let ids: Vec<OpId> = history.iter().map(|o| o.id.clone()).collect();
        assert!(ids.contains(&milestone.id));
        assert!(ids.contains(&root_op.id));

        // Verify tessera.toml declarative fields overlay without polluting load_local_config().
        fs::write(
            tmp.path().join("tessera.toml"),
            "trunk_branch = \"master\"\nbase_port = 4000\nport_stride = 50\nverify_command = \"cargo test\"\n",
        )
        .expect("write tessera.toml");
        let merged_cfg = store.load_config().expect("load_config");
        assert_eq!(merged_cfg.trunk_branch, "master");
        assert_eq!(merged_cfg.base_port, 4000);
        assert_eq!(merged_cfg.port_stride, 50);
        assert!(merged_cfg.verify_command.is_none());

        store.trust_repo_config().expect("trust");
        let trusted_cfg = store.load_config().expect("load trusted");
        assert_eq!(trusted_cfg.verify_command.as_deref(), Some("cargo test"));
        let local_cfg = store.load_local_config().expect("load local");
        assert!(local_cfg.verify_command.is_none());
    }
}
