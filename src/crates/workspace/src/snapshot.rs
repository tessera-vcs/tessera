//! Parallel stat-cached working-copy snapshotting with Git-compliant `.gitignore`, global excludes,
//! and `<common_git_dir>/info/exclude` filtering.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use core::{
    CoreError, CoreResult, EntryMode, FileEntry, GitBackend, ObjectId, atomic_write_file_relaxed,
    resolve_dot_git, validate_safe_identifier,
};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

/// Cached file stat metadata mapped to its last computed Git blob [`ObjectId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedStat {
    /// Modification timestamp seconds since Unix epoch.
    pub mtime_secs: u64,
    /// Modification timestamp subsecond nanoseconds.
    pub mtime_nanos: u32,
    /// Metadata change timestamp seconds since Unix epoch (0 on non-Unix platforms).
    #[serde(default)]
    pub ctime_secs: u64,
    /// Metadata change timestamp subsecond nanoseconds (0 on non-Unix platforms).
    #[serde(default)]
    pub ctime_nanos: u32,
    /// File size in bytes.
    pub size: u64,
    /// Filesystem inode number (0 on non-Unix platforms).
    #[serde(default)]
    pub inode: u64,
    /// Filesystem device identifier (0 on non-Unix platforms).
    #[serde(default)]
    pub dev: u64,
    /// Executable permission bit.
    pub executable: bool,
    /// `true` if this entry represents a tracked symbolic link (`120000`).
    #[serde(default)]
    pub is_symlink: bool,
    /// Previously written Git blob [`ObjectId`].
    pub blob_id: ObjectId,
}

/// On-disk stat cache envelope pairing cached file entries with the cache capture timestamp
/// to detect same-tick ("racy Git") modifications.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatCacheFile {
    /// Unix epoch seconds when this stat cache snapshot was captured.
    #[serde(default)]
    cache_mtime_secs: u64,
    /// Subsecond nanoseconds when this stat cache snapshot was captured.
    #[serde(default)]
    cache_mtime_nanos: u32,
    /// Cached entries keyed by normalized repository-relative path.
    #[serde(default)]
    entries: BTreeMap<String, CachedStat>,
}

/// Result of snapshotting a working directory into the Git object database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotOutcome {
    /// Root Git tree [`ObjectId`].
    pub root_tree_id: ObjectId,
    /// Flattened `path -> FileEntry` map.
    pub files: BTreeMap<String, FileEntry>,
    /// Number of changed files that had to be re-hashed and written to `.git/objects`.
    pub rehashed_count: usize,
    /// Number of unchanged files served directly from the stat cache.
    pub cached_count: usize,
}

/// Unambiguous VCS, dependency, virtual-environment, and tool-cache directories always excluded
/// at any depth when untracked.
pub(crate) const ALWAYS_IGNORED_DIRS: &[&str] = &[
    ".git",
    ".tessera",
    "node_modules",
    ".pnpm-store",
    ".turbo",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".astro",
    ".angular",
    ".vite",
    ".parcel-cache",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".gradle",
    ".zig-cache",
    ".dart_tool",
];

/// Build-artifact directories excluded by default only at repository root depth so nested source
/// directories such as `src/target/x86.rs` or `src/dist/mod.rs` rely on `.gitignore`.
const TOP_LEVEL_IGNORED_DIRS: &[&str] = &["target", "dist", ".cache", "venv"];

/// Joins a `/`- or `\`-separated relative path onto `base` by pushing each non-empty segment
/// individually so Windows `\\?\` verbatim paths never receive embedded forward slashes.
pub(crate) fn join_rel_segments(base: &Path, rel: &str) -> PathBuf {
    let mut path = base.to_path_buf();

    for seg in rel.split(['/', '\\']) {
        if !seg.is_empty() {
            path.push(seg);
        }
    }

    path
}

/// Returns `true` if a path segment contains an NTFS Alternate Data Stream colon (`:`) or matches
/// `.git`, `.tessera`, or an NTFS short-name or trailing-dot/space alias targeting repository internals.
pub(crate) fn is_vcs_internal_segment(seg: &str) -> bool {
    let base = seg.split(':').next().unwrap_or(seg);
    let trimmed = base.trim_end_matches(['.', ' ']);

    if seg.contains(':')
        || trimmed.eq_ignore_ascii_case(".git")
        || trimmed.eq_ignore_ascii_case(".tessera")
    {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();

    if let Some(rest) = lower.strip_prefix("git~") {
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }

    if let Some(rest) = lower.strip_prefix("tesser~") {
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }

    false
}

/// Returns `true` if `file_name` matches a built-in secret or credential file pattern (case-insensitive).
///
/// Built-in secret patterns protect untracked `.env`, `.envrc`, `.env.*` (excluding `.env.example`,
/// `.env.sample`, `.env.template`, `.env.dist`, `.env.defaults`, and `.env.test`), `*.pem`, `*.key`,
/// `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, `id_rsa`, `id_ed25519`, `id_ecdsa`, `id_dsa`, `.npmrc`,
/// `.pypirc`, `.netrc`, and `credentials.json` files from accidental ingestion into `.git/objects`.
fn is_builtin_secret_filename(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();

    if matches!(
        lower.as_str(),
        ".env"
            | ".envrc"
            | "id_rsa"
            | "id_ed25519"
            | "id_ecdsa"
            | "id_dsa"
            | ".npmrc"
            | ".pypirc"
            | ".netrc"
            | "credentials.json"
    ) || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
        || lower.ends_with(".jks")
        || lower.ends_with(".keystore")
    {
        return true;
    }

    if lower.starts_with(".env.") {
        return !matches!(
            lower.as_str(),
            ".env.example"
                | ".env.sample"
                | ".env.template"
                | ".env.dist"
                | ".env.defaults"
                | ".env.test"
        );
    }

    false
}

/// Resolves the Git common directory (`common_git_dir`) from `working_dir` or `git_dir` so linked
/// worktrees locate `<common_git_dir>/info/exclude` accurately.
pub(crate) fn resolve_common_git_dir(working_dir: &Path, git_dir: &Path) -> PathBuf {
    if let Some((_, common)) = resolve_dot_git(working_dir) {
        return common;
    }

    let commondir_file = git_dir.join("commondir");

    if commondir_file.is_file() {
        if let Ok(raw) = fs::read_to_string(&commondir_file) {
            let trimmed = raw.trim();

            if !trimmed.is_empty() {
                let p = PathBuf::from(trimmed);
                let resolved = if p.is_absolute() { p } else { git_dir.join(p) };

                if resolved.is_dir() {
                    return resolved;
                }
            }
        }
    }

    git_dir.to_path_buf()
}

/// Compiles a [`Gitignore`] matcher from the UTF-8 contents of `raw` anchored at relative root `.`.
fn build_gitignore_from_str(raw: &str) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(".");
    let content = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut added_any = false;

    for line in content.lines() {
        if builder.add_line(None, line).is_ok() {
            added_any = true;
        }
    }

    if !added_any {
        return None;
    }

    builder.build().ok().filter(|gi| !gi.is_empty())
}

/// Compiles a [`Gitignore`] matcher from a `.gitignore` or excludes file on disk.
fn build_gitignore_from_file(path: &Path) -> Option<Gitignore> {
    let raw = fs::read_to_string(path).ok()?;
    build_gitignore_from_str(&raw)
}

/// Scoped `.gitignore` matcher bound to a repository-relative directory prefix.
struct ScopedGitignore {
    /// Repository-relative directory prefix (`""` for repository root).
    base_dir: String,
    /// Compiled `ignore::gitignore::Gitignore` matcher for patterns in this directory.
    matcher: Gitignore,
}

/// Layered Git ignore matcher evaluating global excludes, repository root `.gitignore`, nested
/// subdirectory `.gitignore` files, and `<common_git_dir>/info/exclude` in Git precedence order,
/// followed by case-insensitive built-in secret and OS metadata exclusions.
pub(crate) struct IgnoreMatcher {
    /// User-global excludes (`core.excludesFile` / `$XDG_CONFIG_HOME/git/ignore` / `~/.config/git/ignore`).
    global: Option<Gitignore>,
    /// Root and nested `.gitignore` layers in parent-before-child discovery order.
    dir_layers: Vec<ScopedGitignore>,
    /// Repository-local `<common_git_dir>/info/exclude` matcher.
    info_exclude: Option<Gitignore>,
}

impl IgnoreMatcher {
    /// Loads global excludes, `<working_dir>/.gitignore`, and `<common_git_dir>/info/exclude`.
    ///
    /// When `explicit_global` is `Some(path)`, global excludes are loaded from that path; otherwise
    /// standard global Git excludes discovery (`ignore::gitignore::GitignoreBuilder::build_global`)
    /// is used.
    pub(crate) fn load(working_dir: &Path, git_dir: &Path, explicit_global: Option<&Path>) -> Self {
        let common_git_dir = resolve_common_git_dir(working_dir, git_dir);

        let global = match explicit_global {
            Some(path) => build_gitignore_from_file(path),
            None => {
                let (gi, _) = GitignoreBuilder::new(".").build_global();

                if gi.is_empty() { None } else { Some(gi) }
            }
        };

        let mut dir_layers = Vec::new();
        let root_gi = working_dir.join(".gitignore");

        if root_gi.is_file() {
            if let Some(matcher) = build_gitignore_from_file(&root_gi) {
                dir_layers.push(ScopedGitignore {
                    base_dir: String::new(),
                    matcher,
                });
            }
        }

        let info_exclude = build_gitignore_from_file(&common_git_dir.join("info").join("exclude"));

        Self {
            global,
            dir_layers,
            info_exclude,
        }
    }

    /// Appends rules from a `.gitignore` file scoped to `base_dir` (empty string for repo root).
    pub(crate) fn add_dir_file(&mut self, path: &Path, base_dir: &str) {
        if let Some(matcher) = build_gitignore_from_file(path) {
            self.dir_layers.push(ScopedGitignore {
                base_dir: base_dir.to_string(),
                matcher,
            });
        }
    }

    /// Appends rules parsed from `raw` `.gitignore` text scoped to `base_dir`.
    pub(crate) fn add_dir_str(&mut self, raw: &str, base_dir: &str) {
        if let Some(matcher) = build_gitignore_from_str(raw) {
            self.dir_layers.push(ScopedGitignore {
                base_dir: base_dir.to_string(),
                matcher,
            });
        }
    }

    /// Evaluates `.gitignore` and exclude layers against a single normalized `rel_path` without
    /// walking parent directories or checking built-in secrets.
    fn eval_layers_single(&self, rel_path: &str, is_dir: bool) -> bool {
        let mut ignored = false;

        if let Some(ref global) = self.global {
            match global.matched(rel_path, is_dir) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {}
            }
        }

        for layer in &self.dir_layers {
            let scoped_rel = if layer.base_dir.is_empty() {
                rel_path
            } else if let Some(rest) = rel_path
                .strip_prefix(&layer.base_dir)
                .and_then(|s| s.strip_prefix('/'))
            {
                rest
            } else {
                continue;
            };

            match layer.matcher.matched(scoped_rel, is_dir) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {}
            }
        }

        if let Some(ref exclude) = self.info_exclude {
            match exclude.matched(rel_path, is_dir) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {}
            }
        }

        ignored
    }

    /// Evaluates whether `rel_path` is ignored by global excludes, `.gitignore`, nested
    /// `.gitignore` files, `<common_git_dir>/info/exclude`, OS metadata rules, or built-in
    /// case-insensitive secret filename protections.
    ///
    /// Built-in secret checks run after `.gitignore` evaluation so negation rules like `!*.json`
    /// cannot un-ignore credential files such as `credentials.json`.
    pub(crate) fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        let trimmed = rel_path.trim_matches('/');

        if trimmed.is_empty() {
            return true;
        }

        let file_name = trimmed.rsplit('/').next().unwrap_or(trimmed);

        if file_name.eq_ignore_ascii_case(".ds_store") {
            return true;
        }

        // Per Git semantics, once an ancestor directory is ignored, descendants cannot be re-included.
        if let Some((parent, _)) = trimmed.rsplit_once('/') {
            let mut anc = String::new();

            for seg in parent.split('/') {
                if !anc.is_empty() {
                    anc.push('/');
                }

                anc.push_str(seg);

                if self.eval_layers_single(&anc, true) {
                    return true;
                }
            }
        }

        let ignored_by_rules = self.eval_layers_single(trimmed, is_dir);

        if ignored_by_rules {
            return true;
        }

        if !is_dir && is_builtin_secret_filename(file_name) {
            return true;
        }

        false
    }
}

/// Evaluates whether a repository-relative path should be excluded from automatic ingestion
/// into the Git object store.
///
/// When `is_tracked` is `true` (the path already exists in the base commit tree), this function
/// only rejects `.git` and `.tessera` internals so intentionally tracked files continue to update.
/// When `is_tracked` is `false`, this function rejects paths inside ignored directories, nested
/// Git submodules, built-in secret filenames, and paths matching global excludes, `.gitignore`,
/// nested `.gitignore`, or `<common_git_dir>/info/exclude`.
pub fn is_ignored_or_secret_path(
    working_dir: &Path,
    git_dir: &Path,
    rel_path: &str,
    is_tracked: bool,
) -> bool {
    let norm = rel_path.replace('\\', "/");
    let trimmed = norm.trim_matches('/');

    if trimmed.is_empty() {
        return true;
    }

    for (idx, segment) in trimmed.split('/').enumerate() {
        if is_vcs_internal_segment(segment) {
            return true;
        }

        if !is_tracked {
            if ALWAYS_IGNORED_DIRS.contains(&segment) {
                return true;
            }

            if idx == 0 && TOP_LEVEL_IGNORED_DIRS.contains(&segment) {
                return true;
            }
        }
    }

    if is_tracked {
        return false;
    }

    let mut matcher = IgnoreMatcher::load(working_dir, git_dir, None);
    let mut prefix = String::new();
    let mut parent_dir = working_dir.to_path_buf();

    if let Some((parent, _)) = trimmed.rsplit_once('/') {
        for segment in parent.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }

            prefix.push_str(segment);
            parent_dir.push(segment);

            // Never ingest files residing inside a nested Git submodule or sub-repository.
            if fs::symlink_metadata(parent_dir.join(".git")).is_ok() {
                return true;
            }

            let nested_gitignore = parent_dir.join(".gitignore");

            if nested_gitignore.is_file() {
                matcher.add_dir_file(&nested_gitignore, &prefix);
            }
        }
    }

    let is_dir = join_rel_segments(working_dir, trimmed).is_dir();
    matcher.is_ignored(trimmed, is_dir)
}

/// Returns the current Unix epoch timestamp as `(seconds, subsec_nanos)`.
fn current_unix_timestamp() -> (u64, u32) {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0))
}

/// Extracts `(mtime_secs, mtime_nanos)` from filesystem metadata.
fn metadata_mtime(meta: &fs::Metadata) -> (u64, u32) {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0))
}

/// Resolves `(is_symlink_mode, executable)` for a discovered entry on non-Unix filesystems where
/// POSIX executable bits and symlinks (`120000`) may be materialized as regular files on disk.
///
/// When `disk_is_symlink` is `false` and `disk_is_file` is `true`, preserves a tracked
/// [`EntryMode::Symlink`] or executable bit recorded in `base_map_opt` so snapshots on Windows
/// never silently downgrade tracked `120000` symlinks or `100755` scripts to `100644`.
#[cfg_attr(unix, allow(dead_code))]
fn resolve_non_unix_entry_kind(
    disk_is_symlink: bool,
    disk_is_file: bool,
    rel_str: &str,
    base_map_opt: Option<&BTreeMap<String, FileEntry>>,
) -> (bool, bool) {
    if disk_is_symlink {
        return (true, false);
    }

    if !disk_is_file {
        return (false, false);
    }

    match base_map_opt.and_then(|bm| bm.get(rel_str)) {
        Some(entry) if entry.is_symlink() => (true, false),
        Some(entry) if !entry.is_gitlink() => (false, entry.executable),
        _ => (false, false),
    }
}

/// Extracts platform stat attributes `(is_symlink, executable, inode, dev, ctime_secs, ctime_nanos)`
/// for a discovered file or symlink, preserving tracked symlink (`120000`) and executable (`100755`)
/// modes on non-Unix platforms.
fn extract_stat_attrs(
    meta: &fs::Metadata,
    disk_is_symlink: bool,
    disk_is_file: bool,
    rel_str: &str,
    base_map_opt: Option<&BTreeMap<String, FileEntry>>,
) -> (bool, bool, u64, u64, u64, u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let _ = (disk_is_file, rel_str, base_map_opt);
        let executable = !disk_is_symlink && (meta.permissions().mode() & 0o111 != 0);
        let inode = meta.ino();
        let dev = meta.dev();
        let ctime_secs = u64::try_from(meta.ctime()).unwrap_or(0);
        let ctime_nanos = u32::try_from(meta.ctime_nsec()).unwrap_or(0);

        (
            disk_is_symlink,
            executable,
            inode,
            dev,
            ctime_secs,
            ctime_nanos,
        )
    }

    #[cfg(not(unix))]
    {
        let _ = meta;
        let (is_symlink, executable) =
            resolve_non_unix_entry_kind(disk_is_symlink, disk_is_file, rel_str, base_map_opt);

        (is_symlink, executable, 0u64, 0u64, 0u64, 0u32)
    }
}

/// Discovered working-tree file or symlink metadata collected prior to parallel hashing.
struct DiscoveredEntry {
    rel_path: String,
    full_path: PathBuf,
    mtime_secs: u64,
    mtime_nanos: u32,
    ctime_secs: u64,
    ctime_nanos: u32,
    size: u64,
    inode: u64,
    dev: u64,
    executable: bool,
    is_symlink: bool,
    disk_is_symlink: bool,
}

/// Loads the persisted stat cache and computes the conservative racy-Git cutoff timestamp.
fn load_stat_cache(cache_path: &Path) -> (BTreeMap<String, CachedStat>, (u64, u32)) {
    if !cache_path.is_file() {
        return (BTreeMap::new(), (0, 0));
    }

    let file_mtime = fs::metadata(cache_path)
        .ok()
        .map(|m| metadata_mtime(&m))
        .unwrap_or((0, 0));

    let Ok(bytes) = fs::read(cache_path) else {
        return (BTreeMap::new(), (0, 0));
    };

    if let Ok(envelope) = serde_json::from_slice::<StatCacheFile>(&bytes) {
        let stored = (envelope.cache_mtime_secs, envelope.cache_mtime_nanos);
        let effective = match (stored.0 > 0, file_mtime.0 > 0) {
            (true, true) => stored.min(file_mtime),
            (true, false) => stored,
            (false, true) => file_mtime,
            (false, false) => (0, 0),
        };

        return (envelope.entries, effective);
    }

    let legacy: BTreeMap<String, CachedStat> = serde_json::from_slice(&bytes).unwrap_or_default();
    (legacy, file_mtime)
}

/// Reads the link target of a symbolic link as raw Git symlink blob bytes.
fn read_symlink_target_bytes(path: &Path) -> CoreResult<Vec<u8>> {
    let target = fs::read_link(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(target.as_os_str().as_bytes().to_vec())
    }

    #[cfg(not(unix))]
    {
        Ok(target.to_string_lossy().replace('\\', "/").into_bytes())
    }
}

/// Incrementally snapshots `working_dir` into `git` using the stat cache identified by `cache_key`.
///
/// If `base_tree_for_scoped` is provided alongside non-empty `scopes`, any paths outside `scopes`
/// are preserved from `base_tree_for_scoped` so scoped agent workspaces do not delete unmaterialized
/// directories or packages. Tracked submodule gitlinks (`160000`) and symbolic links (`120000`)
/// are preserved faithfully.
///
/// # Errors
/// Returns [`CoreError`] if directory walking or Git object writing fails.
pub fn snapshot_working_dir(
    git: &GitBackend,
    working_dir: &Path,
    cache_key: &str,
    scopes: &[String],
    base_tree_for_scoped: Option<&ObjectId>,
) -> CoreResult<SnapshotOutcome> {
    let safe_cache_key = if validate_safe_identifier("cache_key", cache_key).is_ok() {
        cache_key.to_string()
    } else {
        GitBackend::hash_blob_in_memory(cache_key.as_bytes())
            .as_str()
            .to_string()
    };

    let cache_path = git
        .git_dir()
        .join("tessera")
        .join("stat_cache")
        .join(format!("{safe_cache_key}.json"));
    let (prev_cache, (cache_mtime_secs, cache_mtime_nanos)) = load_stat_cache(&cache_path);
    let (snap_start_secs, snap_start_nanos) = current_unix_timestamp();

    let global_excludes = git.global_excludes_file();
    let ignore_matcher = std::cell::RefCell::new(IgnoreMatcher::load(
        working_dir,
        git.common_git_dir(),
        global_excludes.as_deref(),
    ));
    let base_map_opt = match base_tree_for_scoped {
        Some(base_tid) => Some(git.read_tree_to_map(base_tid)?),
        None => None,
    };

    let mut discovered: Vec<DiscoveredEntry> = Vec::new();
    let walker = WalkDir::new(working_dir).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }

        if let Some(name) = e.file_name().to_str() {
            // Never ingest `.git` (directory or worktree/submodule pointer file) or `.tessera`.
            if is_vcs_internal_segment(name) {
                return false;
            }

            if e.file_type().is_dir() {
                if ALWAYS_IGNORED_DIRS.contains(&name) {
                    return false;
                }

                if e.depth() == 1 && TOP_LEVEL_IGNORED_DIRS.contains(&name) {
                    return false;
                }
            }
        }

        if e.file_type().is_dir() {
            // Do not descend into nested Git submodules or sub-repositories.
            if fs::symlink_metadata(e.path().join(".git")).is_ok() {
                return false;
            }

            if let Ok(rel) = e.path().strip_prefix(working_dir) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");

                if ignore_matcher.borrow().is_ignored(&rel_str, true) {
                    return false;
                }

                let nested_gitignore = e.path().join(".gitignore");

                if nested_gitignore.is_file() {
                    ignore_matcher
                        .borrow_mut()
                        .add_dir_file(&nested_gitignore, &rel_str);
                }
            }
        }

        true
    });

    for entry in walker.flatten() {
        let ft = entry.file_type();
        let disk_is_symlink = ft.is_symlink();
        let disk_is_file = ft.is_file();

        if !disk_is_file && !disk_is_symlink {
            continue;
        }

        let Ok(rel) = entry.path().strip_prefix(working_dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if rel_str.is_empty() || ignore_matcher.borrow().is_ignored(&rel_str, false) {
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let (mtime_secs, mtime_nanos) = metadata_mtime(&meta);
        let (is_symlink, executable, inode, dev, ctime_secs, ctime_nanos) = extract_stat_attrs(
            &meta,
            disk_is_symlink,
            disk_is_file,
            &rel_str,
            base_map_opt.as_ref(),
        );

        discovered.push(DiscoveredEntry {
            rel_path: rel_str,
            full_path: entry.path().to_path_buf(),
            mtime_secs,
            mtime_nanos,
            ctime_secs,
            ctime_nanos,
            size: meta.len(),
            inode,
            dev,
            executable,
            is_symlink,
            disk_is_symlink,
        });
    }

    let mut preserved_gitlinks: BTreeMap<String, FileEntry> = BTreeMap::new();

    if let Some(ref base_map) = base_map_opt {
        let discovered_paths: BTreeSet<String> =
            discovered.iter().map(|d| d.rel_path.clone()).collect();

        for (tracked_path, base_entry) in base_map {
            if discovered_paths.contains(tracked_path) {
                continue;
            }

            if !scopes.is_empty()
                && !scopes.iter().any(|s| {
                    let norm = s.trim_matches('/');
                    tracked_path == norm || tracked_path.starts_with(&format!("{norm}/"))
                })
            {
                continue;
            }

            let full_path = join_rel_segments(working_dir, tracked_path);
            let Ok(meta) = fs::symlink_metadata(&full_path) else {
                continue;
            };

            if base_entry.is_gitlink() {
                if meta.is_dir() {
                    preserved_gitlinks.insert(
                        tracked_path.clone(),
                        FileEntry::from_mode(base_entry.blob_id, EntryMode::Gitlink),
                    );
                }

                continue;
            }

            let disk_is_symlink = meta.file_type().is_symlink();
            let disk_is_file = meta.is_file();

            if !disk_is_file && !disk_is_symlink {
                continue;
            }

            let (mtime_secs, mtime_nanos) = metadata_mtime(&meta);
            let (is_symlink, executable, inode, dev, ctime_secs, ctime_nanos) = extract_stat_attrs(
                &meta,
                disk_is_symlink,
                disk_is_file,
                tracked_path,
                Some(base_map),
            );

            discovered.push(DiscoveredEntry {
                rel_path: tracked_path.clone(),
                full_path,
                mtime_secs,
                mtime_nanos,
                ctime_secs,
                ctime_nanos,
                size: meta.len(),
                inode,
                dev,
                executable,
                is_symlink,
                disk_is_symlink,
            });
        }
    }

    let results: Vec<CoreResult<(String, CachedStat, bool)>> = discovered
        .into_par_iter()
        .map(|entry| {
            let is_racy = if cache_mtime_secs == 0 {
                false
            } else if entry.mtime_nanos == 0 {
                entry.mtime_secs >= cache_mtime_secs
            } else {
                (entry.mtime_secs, entry.mtime_nanos) >= (cache_mtime_secs, cache_mtime_nanos)
            };

            if !is_racy {
                if let Some(cached) = prev_cache.get(&entry.rel_path) {
                    if cached.mtime_secs == entry.mtime_secs
                        && cached.mtime_nanos == entry.mtime_nanos
                        && cached.ctime_secs == entry.ctime_secs
                        && cached.ctime_nanos == entry.ctime_nanos
                        && cached.size == entry.size
                        && cached.inode == entry.inode
                        && cached.dev == entry.dev
                        && cached.executable == entry.executable
                        && cached.is_symlink == entry.is_symlink
                    {
                        return Ok((entry.rel_path, cached.clone(), false));
                    }
                }
            }

            let bytes = if entry.disk_is_symlink {
                read_symlink_target_bytes(&entry.full_path)?
            } else {
                fs::read(&entry.full_path).map_err(|source| CoreError::Io {
                    path: entry.full_path.display().to_string(),
                    source,
                })?
            };

            let blob_id = git.write_blob(&bytes)?;

            Ok((
                entry.rel_path,
                CachedStat {
                    mtime_secs: entry.mtime_secs,
                    mtime_nanos: entry.mtime_nanos,
                    ctime_secs: entry.ctime_secs,
                    ctime_nanos: entry.ctime_nanos,
                    size: entry.size,
                    inode: entry.inode,
                    dev: entry.dev,
                    executable: entry.executable,
                    is_symlink: entry.is_symlink,
                    blob_id,
                },
                true,
            ))
        })
        .collect();

    let mut new_cache = BTreeMap::new();
    let mut files = if !scopes.is_empty() {
        if let Some(mut base_map) = base_map_opt {
            // Clear scoped prefixes from base_map so deleted files inside the scope are honored.
            base_map.retain(|path, _| {
                !scopes.iter().any(|s| {
                    let norm = s.trim_matches('/');
                    path == norm || path.starts_with(&format!("{norm}/"))
                })
            });
            base_map
        } else {
            BTreeMap::new()
        }
    } else {
        BTreeMap::new()
    };

    for (path, gitlink_entry) in preserved_gitlinks {
        files.insert(path, gitlink_entry);
    }

    let mut rehashed_count = 0usize;
    let mut cached_count = 0usize;

    for res in results {
        let (rel_str, stat, was_rehashed) = res?;

        if was_rehashed {
            rehashed_count += 1;
        } else {
            cached_count += 1;
        }

        let file_entry = if stat.is_symlink {
            FileEntry::from_mode(stat.blob_id, EntryMode::Symlink)
        } else {
            FileEntry::regular(stat.blob_id, stat.executable)
        };
        files.insert(rel_str.clone(), file_entry);
        new_cache.insert(rel_str, stat);
    }

    if rehashed_count > 0 || new_cache.len() != prev_cache.len() {
        let envelope = StatCacheFile {
            cache_mtime_secs: snap_start_secs,
            cache_mtime_nanos: snap_start_nanos,
            entries: new_cache,
        };

        if let Ok(serialized) = serde_json::to_vec(&envelope) {
            let _ = atomic_write_file_relaxed(&cache_path, &serialized);
        }
    }

    let root_tree_id = git.write_tree_from_map(&files)?;

    Ok(SnapshotOutcome {
        root_tree_id,
        files,
        rehashed_count,
        cached_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_snapshot_uses_stat_cache_and_respects_gitignore() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");
        let oplog_dir = git.git_dir().join("tessera/stat_cache");
        fs::create_dir_all(&oplog_dir).expect("mkdir stat_cache");

        fs::write(
            tmp.path().join(".gitignore"),
            "*.log\n.env*\ncoverage/\n**/*/ignored-lock.json\n",
        )
        .expect("gitignore");
        fs::write(tmp.path().join("README.md"), "# Monorepo\n").expect("write readme");
        fs::write(tmp.path().join("debug.log"), "ignored log\n").expect("write log");
        fs::write(tmp.path().join(".env.local"), "SECRET=1\n").expect("write env");
        fs::create_dir_all(tmp.path().join("coverage")).expect("mkdir coverage");
        fs::write(tmp.path().join("coverage/lcov.info"), "ignored\n").expect("write cov");
        fs::create_dir_all(tmp.path().join("packages/ui")).expect("mkdir packages/ui");
        fs::write(tmp.path().join("packages/ui/ignored-lock.json"), "{}\n")
            .expect("write ignored lock");

        let snap1 =
            snapshot_working_dir(&git, tmp.path(), "test-agent", &[], None).expect("snap 1");
        assert_eq!(snap1.rehashed_count, 2); // .gitignore + README.md
        assert_eq!(snap1.cached_count, 0);
        assert!(snap1.files.contains_key("README.md"));
        assert!(!snap1.files.contains_key("debug.log"));
        assert!(!snap1.files.contains_key(".env.local"));
        assert!(!snap1.files.contains_key("coverage/lcov.info"));
        assert!(!snap1.files.contains_key("packages/ui/ignored-lock.json"));

        let snap2 =
            snapshot_working_dir(&git, tmp.path(), "test-agent", &[], None).expect("snap 2");
        assert_eq!(snap2.rehashed_count, 0);
        assert_eq!(snap2.cached_count, 2);
        assert_eq!(snap1.root_tree_id, snap2.root_tree_id);
    }

    #[test]
    fn builtin_secret_files_excluded_without_gitignore() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        fs::write(tmp.path().join(".env"), "API_KEY=secret\n").expect("write .env");
        fs::write(tmp.path().join(".env.production"), "PROD_KEY=secret\n")
            .expect("write .env.prod");
        fs::write(tmp.path().join(".env.example"), "API_KEY=\n").expect("write .env.example");
        fs::write(tmp.path().join(".env.dist"), "API_KEY=\n").expect("write .env.dist");
        fs::write(tmp.path().join(".env.defaults"), "API_KEY=\n").expect("write .env.defaults");
        fs::write(tmp.path().join(".env.test"), "API_KEY=test\n").expect("write .env.test");
        fs::write(tmp.path().join("cert.pem"), "PRIVATE KEY\n").expect("write pem");
        fs::write(tmp.path().join("CERT.PEM"), "UPPERCASE KEY\n").expect("write CERT.PEM");
        fs::write(tmp.path().join("PRIVATE.KEY"), "SECRET\n").expect("write PRIVATE.KEY");
        fs::write(tmp.path().join("Credentials.json"), "{}\n").expect("write Credentials.json");

        let snap = snapshot_working_dir(&git, tmp.path(), "test-secrets", &[], None).expect("snap");
        assert!(!snap.files.contains_key(".env"));
        assert!(!snap.files.contains_key(".env.production"));
        assert!(!snap.files.contains_key("cert.pem"));
        assert!(!snap.files.contains_key("CERT.PEM"));
        assert!(!snap.files.contains_key("PRIVATE.KEY"));
        assert!(!snap.files.contains_key("Credentials.json"));
        assert!(snap.files.contains_key(".env.example"));
        assert!(snap.files.contains_key(".env.dist"));
        assert!(snap.files.contains_key(".env.defaults"));
        assert!(snap.files.contains_key(".env.test"));

        assert!(is_ignored_or_secret_path(
            tmp.path(),
            git.git_dir(),
            ".env",
            false
        ));
        assert!(is_ignored_or_secret_path(
            tmp.path(),
            git.git_dir(),
            ".ENV",
            false
        ));
        assert!(!is_ignored_or_secret_path(
            tmp.path(),
            git.git_dir(),
            ".env",
            true
        ));
        assert!(!is_ignored_or_secret_path(
            tmp.path(),
            git.git_dir(),
            ".env.example",
            false
        ));
    }

    #[test]
    fn ignore_supports_anchored_globs_char_classes_negation_and_secret_override_protection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        fs::write(
            tmp.path().join(".gitignore"),
            "/root-only.txt\n/build/\nfoo/*.txt\n*.py[cod]\n*.[oa]\nfoo?bar.txt\n*.json\n!*.json\n",
        )
        .expect("write .gitignore");
        fs::create_dir_all(tmp.path().join("sub")).expect("mkdir sub");
        fs::create_dir_all(tmp.path().join("build")).expect("mkdir build");
        fs::create_dir_all(tmp.path().join("src/build")).expect("mkdir src/build");
        fs::create_dir_all(tmp.path().join("src/target")).expect("mkdir src/target");
        fs::create_dir_all(tmp.path().join("foo/deep")).expect("mkdir foo/deep");

        fs::write(tmp.path().join("root-only.txt"), "root\n").expect("root-only");
        fs::write(tmp.path().join("sub/root-only.txt"), "sub\n").expect("sub root-only");
        fs::write(tmp.path().join("build/out.js"), "ignored\n").expect("build/out");
        fs::write(tmp.path().join("src/build/mod.rs"), "keep\n").expect("src/build/mod.rs");
        fs::write(tmp.path().join("src/target/x86.rs"), "keep\n").expect("src/target/x86.rs");
        fs::write(tmp.path().join("foo/a.txt"), "ignored\n").expect("foo/a.txt");
        fs::write(tmp.path().join("foo/deep/a.txt"), "keep\n").expect("foo/deep/a.txt");
        fs::write(tmp.path().join("mod.pyc"), "ignored\n").expect("mod.pyc");
        fs::write(tmp.path().join("mod.py"), "keep\n").expect("mod.py");
        fs::write(tmp.path().join("lib.o"), "ignored\n").expect("lib.o");
        fs::write(tmp.path().join("foo1bar.txt"), "ignored\n").expect("foo1bar");
        fs::write(tmp.path().join("foo12bar.txt"), "keep\n").expect("foo12bar");
        fs::write(tmp.path().join("config.json"), "{}\n").expect("config.json");
        fs::write(tmp.path().join("credentials.json"), "{\"secret\":1}\n")
            .expect("credentials.json");

        let snap = snapshot_working_dir(&git, tmp.path(), "ignore-rules", &[], None).expect("snap");

        assert!(!snap.files.contains_key("root-only.txt"));
        assert!(snap.files.contains_key("sub/root-only.txt"));
        assert!(!snap.files.contains_key("build/out.js"));
        assert!(snap.files.contains_key("src/build/mod.rs"));
        assert!(snap.files.contains_key("src/target/x86.rs"));
        assert!(!snap.files.contains_key("foo/a.txt"));
        assert!(snap.files.contains_key("foo/deep/a.txt"));
        assert!(!snap.files.contains_key("mod.pyc"));
        assert!(snap.files.contains_key("mod.py"));
        assert!(!snap.files.contains_key("lib.o"));
        assert!(!snap.files.contains_key("foo1bar.txt"));
        assert!(snap.files.contains_key("foo12bar.txt"));
        assert!(snap.files.contains_key("config.json"));
        // Negation `!*.json` must never un-ignore built-in secret files.
        assert!(!snap.files.contains_key("credentials.json"));
    }

    #[test]
    fn snapshot_preserves_symlinks_skips_submodules_and_worktree_git_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main_dir = tmp.path().join("main-repo");
        fs::create_dir_all(&main_dir).expect("mkdir main");
        let main_git = GitBackend::init_or_open(&main_dir).expect("init main");

        let wt_git_dir = main_git.git_dir().join("worktrees").join("wt1");
        fs::create_dir_all(&wt_git_dir).expect("mkdir wt gitdir");
        atomic_write_file_relaxed(&wt_git_dir.join("commondir"), b"../..\n").expect("commondir");
        atomic_write_file_relaxed(&wt_git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .expect("HEAD");
        fs::create_dir_all(main_git.git_dir().join("info")).expect("mkdir info");
        fs::write(
            main_git.git_dir().join("info/exclude"),
            "excluded-local.txt\n",
        )
        .expect("write info/exclude");

        let wt_dir = tmp.path().join("wt1");
        fs::create_dir_all(&wt_dir).expect("mkdir wt");
        fs::write(
            wt_dir.join(".git"),
            format!("gitdir: {}\n", wt_git_dir.display()),
        )
        .expect("write .git file");
        fs::write(wt_dir.join("app.rs"), "fn main() {}\n").expect("write app.rs");
        fs::write(wt_dir.join("excluded-local.txt"), "skip me\n").expect("write excluded");

        // Nested submodule directory with its own `.git` pointer file.
        let sub_dir = wt_dir.join("vendor/submod");
        fs::create_dir_all(&sub_dir).expect("mkdir submod");
        fs::write(sub_dir.join(".git"), "gitdir: ../../.git/modules/submod\n")
            .expect("write submod .git");
        fs::write(sub_dir.join("inside.rs"), "pub fn sub() {}\n").expect("write inside.rs");

        #[cfg(unix)]
        std::os::unix::fs::symlink(Path::new("app.rs"), wt_dir.join("link.rs"))
            .expect("create symlink");

        let wt_git = GitBackend::discover(&wt_dir).expect("discover worktree");
        let sub_commit =
            ObjectId::from_hex("1234567890abcdef1234567890abcdef12345678").expect("oid");
        let mut base_files = BTreeMap::new();
        base_files.insert(
            "vendor/submod".to_string(),
            FileEntry::from_mode(sub_commit, EntryMode::Gitlink),
        );
        let base_tree = wt_git
            .write_tree_from_map(&base_files)
            .expect("write base tree");

        let snap = snapshot_working_dir(&wt_git, &wt_dir, "wt-snap", &[], Some(&base_tree))
            .expect("snapshot worktree");

        assert!(!snap.files.contains_key(".git"));
        assert!(!snap.files.contains_key("excluded-local.txt"));
        assert!(!snap.files.contains_key("vendor/submod/inside.rs"));
        assert!(!snap.files.contains_key("vendor/submod/.git"));
        assert!(snap.files.contains_key("app.rs"));
        assert_eq!(
            snap.files.get("vendor/submod"),
            Some(&FileEntry::from_mode(sub_commit, EntryMode::Gitlink))
        );

        #[cfg(unix)]
        {
            let link_entry = snap.files.get("link.rs").expect("symlink captured");
            assert!(link_entry.is_symlink());
            assert_eq!(
                wt_git
                    .read_blob(&link_entry.blob_id)
                    .expect("read link blob"),
                b"app.rs"
            );
        }
    }

    #[test]
    fn racy_git_stat_cache_rehashes_when_file_mtime_is_in_smudge_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");
        let file_path = tmp.path().join("data.txt");

        fs::write(&file_path, "aaaa\n").expect("write initial");
        let snap1 = snapshot_working_dir(&git, tmp.path(), "racy-key", &[], None).expect("snap 1");
        let initial_blob = snap1.files["data.txt"].blob_id;

        let cache_path = git
            .git_dir()
            .join("tessera")
            .join("stat_cache")
            .join("racy-key.json");
        let raw_cache = fs::read(&cache_path).expect("read cache");
        let mut envelope: StatCacheFile = serde_json::from_slice(&raw_cache).expect("parse cache");
        let cached_entry = envelope.entries.get("data.txt").expect("cached data.txt");

        // Simulate a same-tick ("racy") cache where the cache timestamp matches the file's mtime
        // and the file was overwritten with identical size and identical recorded stat tuple.
        envelope.cache_mtime_secs = cached_entry.mtime_secs;
        envelope.cache_mtime_nanos = cached_entry.mtime_nanos;
        let rewritten = serde_json::to_vec(&envelope).expect("serialize");
        fs::write(&cache_path, rewritten).expect("overwrite cache");

        // Write same-length mutated content.
        fs::write(&file_path, "bbbb\n").expect("write mutated");

        // Update the cache entry's stat fields to match the new file's stat so only the racy-git
        // timestamp check can force a rehash.
        let new_meta = fs::metadata(&file_path).expect("meta");
        let (mtime_secs, mtime_nanos) = metadata_mtime(&new_meta);
        let (_is_symlink, executable, inode, dev, ctime_secs, ctime_nanos) =
            extract_stat_attrs(&new_meta, false, true, "data.txt", None);
        envelope.cache_mtime_secs = mtime_secs;
        envelope.cache_mtime_nanos = mtime_nanos;
        envelope.entries.insert(
            "data.txt".to_string(),
            CachedStat {
                mtime_secs,
                mtime_nanos,
                ctime_secs,
                ctime_nanos,
                size: new_meta.len(),
                inode,
                dev,
                executable,
                is_symlink: false,
                blob_id: initial_blob,
            },
        );
        fs::write(&cache_path, serde_json::to_vec(&envelope).expect("ser")).expect("write cache");

        let snap2 = snapshot_working_dir(&git, tmp.path(), "racy-key", &[], None).expect("snap 2");
        assert_eq!(snap2.rehashed_count, 1);
        assert_ne!(snap2.files["data.txt"].blob_id, initial_blob);
        assert_eq!(
            git.read_blob(&snap2.files["data.txt"].blob_id)
                .expect("read blob"),
            b"bbbb\n"
        );
    }

    #[test]
    fn ignore_precedence_respects_global_root_nested_and_info_exclude() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");
        let global_excludes = tmp.path().join("global_ignore");

        fs::write(&global_excludes, "*.tmp\n*.bak\n").expect("write global");
        fs::write(tmp.path().join(".gitignore"), "!keep.tmp\n*.log\n").expect("write root gi");
        fs::create_dir_all(tmp.path().join("sub")).expect("mkdir sub");
        fs::write(tmp.path().join("sub/.gitignore"), "!important.log\n").expect("write sub gi");
        fs::create_dir_all(git.common_git_dir().join("info")).expect("mkdir info");
        fs::write(
            git.common_git_dir().join("info/exclude"),
            "sub/important.log\n!override.bak\n",
        )
        .expect("write info/exclude");

        let mut matcher =
            IgnoreMatcher::load(tmp.path(), git.common_git_dir(), Some(&global_excludes));
        matcher.add_dir_file(&tmp.path().join("sub/.gitignore"), "sub");

        assert!(matcher.is_ignored("scratch.tmp", false));
        assert!(!matcher.is_ignored("keep.tmp", false));
        assert!(matcher.is_ignored("sub/debug.log", false));
        // `<common_git_dir>/info/exclude` overrides nested `.gitignore` and global excludes.
        assert!(matcher.is_ignored("sub/important.log", false));
        assert!(!matcher.is_ignored("override.bak", false));
        assert!(matcher.is_ignored("other.bak", false));
    }

    #[test]
    fn vcs_internal_segment_rejects_ntfs_ads_and_aliases() {
        assert!(is_vcs_internal_segment(".git"));
        assert!(is_vcs_internal_segment(".tessera"));
        assert!(is_vcs_internal_segment(".git::$INDEX_ALLOCATION"));
        assert!(is_vcs_internal_segment(".git:$I30:$INDEX_ALLOCATION"));
        assert!(is_vcs_internal_segment(".tessera:stream"));
        assert!(is_vcs_internal_segment("git~1:stream"));
        assert!(is_vcs_internal_segment("foo:bar"));
        assert!(!is_vcs_internal_segment("src"));
        assert!(!is_vcs_internal_segment(".github"));
    }

    #[test]
    fn non_unix_entry_kind_preserves_tracked_symlink_and_executable_modes() {
        let oid = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
        let mut base_map = BTreeMap::new();
        base_map.insert(
            "links/latest".to_string(),
            FileEntry::from_mode(oid, EntryMode::Symlink),
        );
        base_map.insert("scripts/run.sh".to_string(), FileEntry::regular(oid, true));
        base_map.insert("src/lib.rs".to_string(), FileEntry::regular(oid, false));
        base_map.insert(
            "vendor/submod".to_string(),
            FileEntry::from_mode(oid, EntryMode::Gitlink),
        );

        assert_eq!(
            resolve_non_unix_entry_kind(false, true, "links/latest", Some(&base_map)),
            (true, false)
        );
        assert_eq!(
            resolve_non_unix_entry_kind(true, false, "src/lib.rs", Some(&base_map)),
            (true, false)
        );
        assert_eq!(
            resolve_non_unix_entry_kind(false, true, "scripts/run.sh", Some(&base_map)),
            (false, true)
        );
        assert_eq!(
            resolve_non_unix_entry_kind(false, true, "src/lib.rs", Some(&base_map)),
            (false, false)
        );
        assert_eq!(
            resolve_non_unix_entry_kind(false, true, "new.txt", Some(&base_map)),
            (false, false)
        );
        assert_eq!(
            resolve_non_unix_entry_kind(false, true, "vendor/submod", Some(&base_map)),
            (false, false)
        );
    }
}
