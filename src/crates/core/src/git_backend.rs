//! Lock-free native Git object database (`blob`, `tree`, `commit`) and reference backend.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use sha1::{Digest, Sha1};

use crate::model::{
    CoreError, CoreResult, EntryMode, FileEntry, ObjectId, atomic_write_file,
    atomic_write_file_relaxed, now_millis, validate_ref_name, validate_repo_rel_path,
};

/// Maximum decompressed Git object payload size (64 MiB) to prevent memory exhaustion on corrupt objects.
const MAX_GIT_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum number of parsed trees and commits retained in the per-handle in-memory cache.
const MAX_CACHED_OBJECTS: usize = 1024;

/// Parsed representation of a single entry in a native Git tree object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTreeNode {
    /// File or directory entry mode.
    pub mode: EntryMode,
    /// Single path component name (no slashes).
    pub name: String,
    /// Target blob or subtree SHA-1 [`ObjectId`].
    pub id: ObjectId,
}

/// Parsed representation of a native Git commit object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommit {
    /// Root [`ObjectId`] of the tree attached to this commit.
    pub tree_id: ObjectId,
    /// Parent commit [`ObjectId`]s.
    pub parents: Vec<ObjectId>,
    /// Author header line.
    pub author: String,
    /// Committer header line.
    pub committer: String,
    /// Commit message body (including any `Tessera-*` provenance trailers).
    pub message: String,
}

/// Persistent `git cat-file --batch` subprocess used to read packed objects without per-object process spawning.
struct CatFileBatchProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFileBatchProcess {
    /// Spawns a new `git --git-dir <git_dir> cat-file --batch` child process with piped stdio.
    fn spawn(git_dir: &Path) -> Option<Self> {
        let mut child = Command::new("git")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);

        Some(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Queries `<id>` over the batch pipe and returns `Some((kind, bytes))` or `None` if missing/broken.
    fn query(&mut self, id: &ObjectId) -> io::Result<Option<(String, Vec<u8>)>> {
        writeln!(self.stdin, "{}", id.as_str())?;
        self.stdin.flush()?;

        let mut header_line = String::new();
        let read_bytes = self.stdout.read_line(&mut header_line)?;

        if read_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "git cat-file --batch closed stdout",
            ));
        }

        let trimmed = header_line.trim_end_matches(&['\r', '\n'][..]);

        if trimmed.ends_with(" missing") {
            return Ok(None);
        }

        let mut parts = trimmed.split_whitespace();
        let _oid_str = parts.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing oid in batch header")
        })?;
        let kind = parts
            .next()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing kind in batch header")
            })?
            .to_string();
        let size_str = parts.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing size in batch header")
        })?;
        let size: usize = size_str
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if (size as u64) > MAX_GIT_OBJECT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "packed git object exceeds maximum size limit",
            ));
        }

        let mut payload = vec![0u8; size];
        self.stdout.read_exact(&mut payload)?;

        let mut lf = [0u8; 1];
        self.stdout.read_exact(&mut lf)?;

        Ok(Some((kind, payload)))
    }
}

impl Drop for CatFileBatchProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

use std::io;

/// Thread-safe in-memory cache of parsed Git trees, commits, and the persistent batch reader.
#[derive(Default)]
struct GitObjectCache {
    trees: BTreeMap<ObjectId, Vec<GitTreeNode>>,
    commits: BTreeMap<ObjectId, GitCommit>,
    batch: Option<CatFileBatchProcess>,
}

/// Lock-free bridge to a colocated `.git` directory (including linked worktrees and submodules).
///
/// Reads and writes native Git objects (`blob`, `tree`, `commit`) directly in `.git/objects`
/// without ever acquiring `.git/index.lock`.
#[derive(Clone)]
pub struct GitBackend {
    repo_root: PathBuf,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    cache: Arc<Mutex<GitObjectCache>>,
}

impl fmt::Debug for GitBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitBackend")
            .field("repo_root", &self.repo_root)
            .field("git_dir", &self.git_dir)
            .field("common_git_dir", &self.common_git_dir)
            .finish()
    }
}

/// Resolves `.git` at `repo_dir` into `(git_dir, common_git_dir)`.
///
/// Supports standard repositories (`.git/` directory), linked Git worktrees (`.git` file with
/// `gitdir:` plus `<git_dir>/commondir`), and Git submodules (`.git` file with `gitdir:`).
#[must_use]
pub fn resolve_dot_git(repo_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let dot_git = repo_dir.join(".git");

    if dot_git.is_dir() {
        return Some((dot_git.clone(), dot_git));
    }

    if !dot_git.is_file() {
        return None;
    }

    let raw = fs::read_to_string(&dot_git).ok()?;
    let target_str = raw.trim().strip_prefix("gitdir:")?.trim();

    if target_str.is_empty() {
        return None;
    }

    let raw_target = PathBuf::from(target_str);
    let git_dir = if raw_target.is_absolute() {
        raw_target
    } else {
        repo_dir.join(raw_target)
    };

    if !git_dir.is_dir() {
        return None;
    }

    let commondir_file = git_dir.join("commondir");
    let common_git_dir = if commondir_file.is_file() {
        if let Ok(common_raw) = fs::read_to_string(&commondir_file) {
            let trimmed = common_raw.trim();

            if !trimmed.is_empty() {
                let p = PathBuf::from(trimmed);
                let resolved = if p.is_absolute() { p } else { git_dir.join(p) };

                if resolved.is_dir() {
                    resolved
                } else {
                    git_dir.clone()
                }
            } else {
                git_dir.clone()
            }
        } else {
            git_dir.clone()
        }
    } else {
        git_dir.clone()
    };

    Some((git_dir, common_git_dir))
}

/// Compares two Git tree entry names according to Git's canonical sort order, where
/// directory entries sort as if they had a trailing `'/'` byte, without heap allocations.
fn cmp_git_tree_entry_names(
    a_name: &str,
    a_is_dir: bool,
    b_name: &str,
    b_is_dir: bool,
) -> Ordering {
    let a_bytes = a_name.as_bytes();
    let b_bytes = b_name.as_bytes();
    let min_len = a_bytes.len().min(b_bytes.len());

    let prefix_cmp = a_bytes[..min_len].cmp(&b_bytes[..min_len]);

    if prefix_cmp != Ordering::Equal {
        return prefix_cmp;
    }

    let a_next = if a_bytes.len() > min_len {
        Some(a_bytes[min_len])
    } else if a_is_dir {
        Some(b'/')
    } else {
        None
    };

    let b_next = if b_bytes.len() > min_len {
        Some(b_bytes[min_len])
    } else if b_is_dir {
        Some(b'/')
    } else {
        None
    };

    a_next.cmp(&b_next)
}

/// Validates that a single Git tree entry name cannot escape its directory, inject an NTFS
/// Alternate Data Stream, or target `.git` / `.tessera`.
fn validate_tree_entry_name(tree_id: &ObjectId, name: &str) -> CoreResult<()> {
    let base = name.split(':').next().unwrap_or(name);
    let trimmed = base.trim_end_matches(['.', ' ']);
    let lower = trimmed.to_ascii_lowercase();
    let is_short_alias = lower
        .strip_prefix("git~")
        .or_else(|| lower.strip_prefix("tesser~"))
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));

    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains(':')
        || trimmed.eq_ignore_ascii_case(".git")
        || trimmed.eq_ignore_ascii_case(".tessera")
        || is_short_alias
    {
        return Err(CoreError::InvalidGitObject {
            id: tree_id.to_string(),
            reason: format!("unsafe or invalid tree entry name '{name}'"),
        });
    }

    Ok(())
}

/// Joins a `/`- or `\`-separated relative path onto `base` by pushing each non-empty segment
/// individually so Windows `\\?\` verbatim paths never receive embedded forward slashes.
fn join_rel_segments(base: &Path, rel: &str) -> PathBuf {
    let mut path = base.to_path_buf();

    for seg in rel.split(['/', '\\']) {
        if !seg.is_empty() {
            path.push(seg);
        }
    }

    path
}

/// Returns `true` if `path` is equal to or nested inside `prefix`, comparing both lexical paths
/// and canonicalized forms to handle OS symlinked roots (e.g. `/var` vs `/private/var`) and
/// Windows `\\?\` verbatim prefixes.
fn path_starts_with_canonical(path: &Path, prefix: &Path) -> bool {
    if path.starts_with(prefix) {
        return true;
    }

    if let (Ok(canon_path), Ok(canon_prefix)) = (path.canonicalize(), prefix.canonicalize()) {
        return canon_path.starts_with(&canon_prefix);
    }

    false
}

/// Returns `true` if `slug_path` resides directly inside one of the authorized Tessera external
/// sandbox base directories for the active user and `target_root`.
fn is_authorized_sandbox_parent(slug_path: &Path, target_root: &Path) -> bool {
    let Some(base_dir) = slug_path.parent() else {
        return false;
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: `libc::geteuid()` is a pure POSIX syscall with no arguments or memory dereferences.
        let uid = unsafe { libc::geteuid() };

        if let Ok(meta) = fs::symlink_metadata(slug_path) {
            if meta.file_type().is_symlink() || meta.uid() != uid {
                return false;
            }
        }
    }

    let mut allowed_bases: Vec<PathBuf> = Vec::new();

    if let Some(parent) = target_root.parent() {
        allowed_bases.push(parent.join("tessera-workspaces"));
    }

    if let Ok(canon_root) = target_root.canonicalize() {
        if let Some(parent) = canon_root.parent() {
            allowed_bases.push(parent.join("tessera-workspaces"));
        }
    }

    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME") {
            allowed_bases.push(
                PathBuf::from(home)
                    .join("Library")
                    .join("Caches")
                    .join("tessera")
                    .join("workspaces"),
            );
        }
    } else if cfg!(windows) {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            if !local_app_data.trim().is_empty() {
                allowed_bases.push(
                    PathBuf::from(local_app_data)
                        .join("tessera")
                        .join("workspaces"),
                );
            }
        }

        allowed_bases.push(std::env::temp_dir().join("tessera-workspaces"));
    } else {
        // SAFETY: `libc::geteuid()` is a pure POSIX syscall with no arguments or memory dereferences.
        #[cfg(unix)]
        let uid = unsafe { libc::geteuid() };
        #[cfg(not(unix))]
        let uid = 0u32;

        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            if !xdg.trim().is_empty() {
                allowed_bases.push(PathBuf::from(xdg).join("tessera").join("workspaces"));
            }
        }

        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                allowed_bases.push(
                    PathBuf::from(home)
                        .join(".cache")
                        .join("tessera")
                        .join("workspaces"),
                );
            }
        }

        allowed_bases.push(PathBuf::from("/var/tmp").join(format!("tessera-workspaces-{uid}")));
        allowed_bases.push(std::env::temp_dir().join(format!("tessera-workspaces-{uid}")));
    }

    let canon_base = base_dir.canonicalize().ok();

    allowed_bases.iter().any(|allowed| {
        base_dir == allowed
            || canon_base
                .as_ref()
                .is_some_and(|cb| allowed.canonicalize().ok().as_ref() == Some(cb))
    })
}

/// Verifies that `sandbox_dir/.tessera/repo_root` is a non-symlink file inside a legitimate
/// Tessera branch sandbox or agent workspace pointing to an initialized Tessera repository root.
#[must_use]
pub fn is_valid_sandbox_repo_root(
    sandbox_dir: &Path,
    expected_repo_root: Option<&Path>,
) -> Option<PathBuf> {
    let dot_tessera = sandbox_dir.join(".tessera");
    let dt_meta = fs::symlink_metadata(&dot_tessera).ok()?;

    if dt_meta.file_type().is_symlink() || !dt_meta.is_dir() {
        return None;
    }

    let repo_link = dot_tessera.join("repo_root");
    let link_meta = fs::symlink_metadata(&repo_link).ok()?;

    if link_meta.file_type().is_symlink() || !link_meta.is_file() {
        return None;
    }

    let target = fs::read_to_string(&repo_link).ok()?;
    let target_root = PathBuf::from(target.trim());

    if !target_root.is_absolute() {
        return None;
    }

    let (_, common_git_dir) = resolve_dot_git(&target_root)?;

    if !common_git_dir.join("tessera").is_dir() {
        return None;
    }

    if let Some(exp) = expected_repo_root {
        let same = target_root == exp
            || (target_root.canonicalize().ok() == exp.canonicalize().ok()
                && target_root.canonicalize().is_ok());

        if !same {
            return None;
        }
    }

    let workspaces_dir = target_root.join(".tessera").join("workspaces");
    let tessera_store_dir = common_git_dir.join("tessera");
    let is_inside_repo = path_starts_with_canonical(sandbox_dir, &workspaces_dir)
        || path_starts_with_canonical(sandbox_dir, &tessera_store_dir);

    if is_inside_repo {
        return Some(target_root);
    }

    let parent = sandbox_dir.parent()?;

    if parent.file_name().and_then(|n| n.to_str()) != Some("branches") {
        return None;
    }

    let slug_path = parent.parent()?;

    if !is_authorized_sandbox_parent(slug_path, &target_root) {
        return None;
    }

    let slug_dir = slug_path.file_name()?.to_str()?;
    let raw_hash = blake3::hash(target_root.display().to_string().as_bytes()).to_hex();
    let expected_suffix = format!("-{}", &raw_hash[..32]);

    if slug_dir.ends_with(&expected_suffix) {
        return Some(target_root);
    }

    if let Ok(canon_root) = target_root.canonicalize() {
        let canon_hash = blake3::hash(canon_root.display().to_string().as_bytes()).to_hex();
        let canon_suffix = format!("-{}", &canon_hash[..32]);

        if slug_dir.ends_with(&canon_suffix) {
            return Some(target_root);
        }
    }

    None
}

impl GitBackend {
    /// Constructs a [`GitBackend`] handle for `(repo_root, git_dir, common_git_dir)` with an initialized cache.
    fn with_paths(repo_root: PathBuf, git_dir: PathBuf, common_git_dir: PathBuf) -> Self {
        Self {
            repo_root,
            git_dir,
            common_git_dir,
            cache: Arc::new(Mutex::new(GitObjectCache::default())),
        }
    }

    /// Discovers a Git repository starting at `start_dir` (walking upward if needed) and returns
    /// a [`GitBackend`] handle. Supports standard `.git` directories, linked worktrees, and submodules.
    ///
    /// # Errors
    /// Returns [`CoreError::RepoNotFound`] if no `.git` directory or pointer exists in `start_dir` or its ancestors.
    pub fn discover(start_dir: &Path) -> CoreResult<Self> {
        let cwd = std::env::current_dir().ok();
        let env_root = std::env::var("TES_REPO_ROOT").ok();
        let env_ws = std::env::var("TES_WORKSPACE").ok();

        Self::discover_with_context(
            start_dir,
            cwd.as_deref(),
            env_root.as_deref(),
            env_ws.as_deref(),
        )
    }

    /// Internal discovery implementation accepting explicit `cwd` and sandbox environment variables
    /// so relative start paths and `TES_WORKSPACE` fallback can be tested deterministically.
    fn discover_with_context(
        start_dir: &Path,
        cwd: Option<&Path>,
        env_root: Option<&str>,
        env_ws: Option<&str>,
    ) -> CoreResult<Self> {
        let abs_start = if start_dir.is_absolute() {
            start_dir.to_path_buf()
        } else {
            cwd.map(|dir| {
                if start_dir == Path::new(".") || start_dir.as_os_str().is_empty() {
                    dir.to_path_buf()
                } else {
                    dir.join(start_dir)
                }
            })
            .unwrap_or_else(|| start_dir.to_path_buf())
        };
        let mut curr = abs_start.clone();

        loop {
            if let Some((git_dir, common_git_dir)) = resolve_dot_git(&curr) {
                return Ok(Self::with_paths(curr, git_dir, common_git_dir));
            }

            if curr.file_name().and_then(|s| s.to_str()) == Some("tmp")
                && curr
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    == Some(".tessera")
            {
                if let (Some(raw_root), Some(raw_ws)) = (env_root, env_ws) {
                    let ws_path = PathBuf::from(raw_ws.trim());
                    let target_root = PathBuf::from(raw_root.trim());

                    // Only honor `TES_WORKSPACE` inside `.tessera/tmp` when `TES_WORKSPACE` itself
                    // resides inside this `.tessera/tmp` boundary so tempdirs created under `tes exec`
                    // never escape back into the enclosing host sandbox.
                    if !ws_path.as_os_str().is_empty()
                        && path_starts_with_canonical(&ws_path, &curr)
                        && path_starts_with_canonical(&abs_start, &ws_path)
                    {
                        if let Some((target_git, target_common)) = resolve_dot_git(&target_root) {
                            return Ok(Self::with_paths(target_root, target_git, target_common));
                        }
                    }
                }

                return Err(CoreError::RepoNotFound(start_dir.display().to_string()));
            }

            if let Some(target_root) = is_valid_sandbox_repo_root(&curr, None) {
                if let Some((target_git, target_common)) = resolve_dot_git(&target_root) {
                    return Ok(Self::with_paths(target_root, target_git, target_common));
                }
            }

            if !curr.pop() {
                if let (Some(raw_root), Some(raw_ws)) = (env_root, env_ws) {
                    let ws_path = PathBuf::from(raw_ws.trim());
                    let target_root = PathBuf::from(raw_root.trim());

                    if !ws_path.as_os_str().is_empty()
                        && path_starts_with_canonical(&abs_start, &ws_path)
                    {
                        if let Some((target_git, target_common)) = resolve_dot_git(&target_root) {
                            return Ok(Self::with_paths(target_root, target_git, target_common));
                        }
                    }
                }

                return Err(CoreError::RepoNotFound(start_dir.display().to_string()));
            }
        }
    }

    /// Initializes a new `.git` repository at `repo_root` if one does not already exist,
    /// ensuring an initial root commit exists on `main`.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] or [`CoreError::InvalidGitObject`] if initialization fails.
    pub fn init_or_open(repo_root: &Path) -> CoreResult<Self> {
        let (git_dir, common_git_dir) = if let Some(resolved) = resolve_dot_git(repo_root) {
            resolved
        } else {
            let dot_git = repo_root.join(".git");
            fs::create_dir_all(join_rel_segments(&dot_git, "objects/info")).map_err(|source| {
                CoreError::Io {
                    path: dot_git.display().to_string(),
                    source,
                }
            })?;
            fs::create_dir_all(join_rel_segments(&dot_git, "objects/pack")).map_err(|source| {
                CoreError::Io {
                    path: dot_git.display().to_string(),
                    source,
                }
            })?;
            fs::create_dir_all(join_rel_segments(&dot_git, "refs/heads")).map_err(|source| {
                CoreError::Io {
                    path: dot_git.display().to_string(),
                    source,
                }
            })?;
            fs::create_dir_all(join_rel_segments(&dot_git, "refs/tags")).map_err(|source| {
                CoreError::Io {
                    path: dot_git.display().to_string(),
                    source,
                }
            })?;

            atomic_write_file(&dot_git.join("HEAD"), b"ref: refs/heads/main\n")?;
            (dot_git.clone(), dot_git)
        };

        let backend = Self::with_paths(repo_root.to_path_buf(), git_dir, common_git_dir);

        // Ensure at least one commit exists so agents always have a valid base commit.
        if backend.resolve_head_commit()?.is_none() {
            let empty_tree = backend.write_tree_from_map(&BTreeMap::new())?;
            let author = backend.resolve_git_author();
            let root_commit = backend.write_commit(&empty_tree, &[], &author, "Initial commit")?;
            let branch = backend
                .current_branch_name()
                .unwrap_or_else(|_| "main".to_string());

            backend.update_ref(&format!("refs/heads/{branch}"), &root_commit)?;
        }

        Ok(backend)
    }

    /// Returns the working tree root path of the repository.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Returns the per-worktree `.git` directory path (`HEAD`, `index`, `FETCH_HEAD`).
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Returns the shared Git common directory (`objects`, `refs`, `packed-refs`, `config`, `tessera`).
    ///
    /// Equal to [`Self::git_dir`] for standard repositories and submodules; points to the main
    /// repository's `.git` directory for linked Git worktrees.
    pub fn common_git_dir(&self) -> &Path {
        &self.common_git_dir
    }

    /// Computes the loose object filesystem path `.git/objects/<2-hex>/<38-hex>` for `id`.
    fn loose_object_path(&self, id: &ObjectId) -> PathBuf {
        let hex_str = id.as_str();
        self.common_git_dir
            .join("objects")
            .join(&hex_str[..2])
            .join(&hex_str[2..])
    }

    /// Computes a Git SHA-1 [`ObjectId`] and writes the zlib-compressed loose object into
    /// `.git/objects/<xx>/<38-hex>` atomically if it does not already exist.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] if writing the loose object fails.
    pub fn write_raw_object(&self, kind: &str, body: &[u8]) -> CoreResult<ObjectId> {
        let header = format!("{kind} {}\0", body.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(body);
        let digest: [u8; 20] = hasher.finalize().into();
        let oid = ObjectId::from_bytes(&digest);

        let path = self.loose_object_path(&oid);

        if path.exists() {
            return Ok(oid);
        }

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder
            .write_all(header.as_bytes())
            .map_err(|source| CoreError::Io {
                path: path.display().to_string(),
                source,
            })?;
        encoder.write_all(body).map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let compressed = encoder.finish().map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?;

        if let Err(err) = atomic_write_file_relaxed(&path, &compressed) {
            // Git loose objects are immutable and content-addressed: if a concurrent writer or
            // Git process created the same read-only object between our check and rename, succeed.
            if path.is_file() {
                return Ok(oid);
            }

            return Err(err);
        }

        Ok(oid)
    }

    /// Reads a raw Git object `(kind, payload_bytes)` by its [`ObjectId`].
    ///
    /// Fast-paths loose objects in `.git/objects/<xx>/<38-hex>` directly in-process via bounded zlib,
    /// and transparently falls back to a persistent `git cat-file --batch` pipe if the object resides in a packfile.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidGitObject`] or [`CoreError::NotFound`] if the object cannot be read
    /// or exceeds [`MAX_GIT_OBJECT_BYTES`].
    pub fn read_raw_object(&self, id: &ObjectId) -> CoreResult<(String, Vec<u8>)> {
        let path = self.loose_object_path(id);

        match fs::read(&path) {
            Ok(compressed) => {
                let limit = MAX_GIT_OBJECT_BYTES + 64;
                let mut decoder = ZlibDecoder::new(&compressed[..]).take(limit + 1);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .map_err(|source| CoreError::Io {
                        path: path.display().to_string(),
                        source,
                    })?;

                if (decompressed.len() as u64) > limit {
                    return Err(CoreError::InvalidGitObject {
                        id: id.to_string(),
                        reason: "decompressed git object exceeds maximum size limit".to_string(),
                    });
                }

                let nul_pos = decompressed.iter().position(|&b| b == 0).ok_or_else(|| {
                    CoreError::InvalidGitObject {
                        id: id.to_string(),
                        reason: "missing NUL byte in object header".to_string(),
                    }
                })?;
                let header = std::str::from_utf8(&decompressed[..nul_pos]).map_err(|e| {
                    CoreError::InvalidGitObject {
                        id: id.to_string(),
                        reason: e.to_string(),
                    }
                })?;
                let mut header_parts = header.split_whitespace();
                let kind = header_parts
                    .next()
                    .ok_or_else(|| CoreError::InvalidGitObject {
                        id: id.to_string(),
                        reason: "missing object kind in header".to_string(),
                    })?
                    .to_string();

                let size_str = header_parts
                    .next()
                    .ok_or_else(|| CoreError::InvalidGitObject {
                        id: id.to_string(),
                        reason: "missing object size in header".to_string(),
                    })?;
                let expected_size =
                    size_str
                        .parse::<usize>()
                        .map_err(|e| CoreError::InvalidGitObject {
                            id: id.to_string(),
                            reason: format!("invalid object size '{size_str}': {e}"),
                        })?;
                let actual_size = decompressed.len() - (nul_pos + 1);

                if expected_size != actual_size {
                    return Err(CoreError::InvalidGitObject {
                        id: id.to_string(),
                        reason: format!(
                            "header size {expected_size} does not match payload size {actual_size}"
                        ),
                    });
                }

                decompressed.drain(..=nul_pos);
                return Ok((kind, decompressed));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CoreError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        }

        if let Ok(mut guard) = self.cache.lock() {
            if guard.batch.is_none() {
                guard.batch = CatFileBatchProcess::spawn(&self.git_dir);
            }

            if let Some(batch) = guard.batch.as_mut() {
                match batch.query(id) {
                    Ok(Some(res)) => return Ok(res),
                    Ok(None) => {
                        return Err(CoreError::NotFound(format!("git object {}", id.as_str())));
                    }
                    Err(_) => {
                        guard.batch = None;
                    }
                }
            }
        }

        // Fallback if persistent batch pipe was reset.
        let type_out = Command::new("git")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["cat-file", "-t", id.as_str()])
            .output()
            .map_err(|source| CoreError::Io {
                path: self.git_dir.display().to_string(),
                source,
            })?;

        if !type_out.status.success() {
            return Err(CoreError::NotFound(format!("git object {}", id.as_str())));
        }

        let kind = String::from_utf8_lossy(&type_out.stdout).trim().to_string();

        let mut child = Command::new("git")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["cat-file", &kind, id.as_str()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| CoreError::Io {
                path: self.git_dir.display().to_string(),
                source,
            })?;

        let mut body = Vec::new();

        if let Some(stdout) = child.stdout.take() {
            stdout
                .take(MAX_GIT_OBJECT_BYTES + 1)
                .read_to_end(&mut body)
                .map_err(|source| CoreError::Io {
                    path: self.git_dir.display().to_string(),
                    source,
                })?;
        }

        let status = child.wait().map_err(|source| CoreError::Io {
            path: self.git_dir.display().to_string(),
            source,
        })?;

        if (body.len() as u64) > MAX_GIT_OBJECT_BYTES {
            return Err(CoreError::InvalidGitObject {
                id: id.to_string(),
                reason: format!("git object exceeds size limit ({MAX_GIT_OBJECT_BYTES} bytes)"),
            });
        }

        if !status.success() {
            return Err(CoreError::NotFound(format!("git object {}", id.as_str())));
        }

        Ok((kind, body))
    }

    /// Writes `bytes` as a Git `blob` object and returns its [`ObjectId`].
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] if writing to `.git/objects` fails.
    pub fn write_blob(&self, bytes: &[u8]) -> CoreResult<ObjectId> {
        self.write_raw_object("blob", bytes)
    }

    /// Computes the Git `blob` [`ObjectId`] for `bytes` in memory without writing to disk.
    pub fn hash_blob_in_memory(bytes: &[u8]) -> ObjectId {
        let header = format!("blob {}\0", bytes.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(bytes);
        let digest: [u8; 20] = hasher.finalize().into();
        ObjectId::from_bytes(&digest)
    }

    /// Reads a Git `blob` object by its [`ObjectId`].
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidGitObject`] if `id` is not a blob.
    pub fn read_blob(&self, id: &ObjectId) -> CoreResult<Vec<u8>> {
        let (kind, body) = self.read_raw_object(id)?;

        if kind != "blob" {
            return Err(CoreError::InvalidGitObject {
                id: id.to_string(),
                reason: format!("expected blob, found {kind}"),
            });
        }

        Ok(body)
    }

    /// Encodes and writes a single-level Git `tree` object from `entries`.
    ///
    /// Automatically sorts `entries` using Git's canonical tree ordering rule (where directory
    /// entries sort as if suffixed with `'/'`) without intermediate string allocations.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] or [`CoreError::InvalidGitObject`] on failure.
    pub fn write_tree_nodes(&self, mut entries: Vec<GitTreeNode>) -> CoreResult<ObjectId> {
        entries.sort_by(|a, b| {
            cmp_git_tree_entry_names(
                &a.name,
                a.mode == EntryMode::Directory,
                &b.name,
                b.mode == EntryMode::Directory,
            )
        });

        let mut buf = Vec::with_capacity(entries.len() * 48);

        for entry in &entries {
            buf.extend_from_slice(entry.mode.as_git_octal().as_bytes());
            buf.push(b' ');
            buf.extend_from_slice(entry.name.as_bytes());
            buf.push(0);
            let raw_sha = entry.id.to_bytes()?;
            buf.extend_from_slice(&raw_sha);
        }

        let tree_id = self.write_raw_object("tree", &buf)?;

        if let Ok(mut guard) = self.cache.lock() {
            if guard.trees.len() >= MAX_CACHED_OBJECTS {
                guard.trees.clear();
            }
            guard.trees.insert(tree_id, entries);
        }

        Ok(tree_id)
    }

    /// Parses a single-level Git `tree` object into its [`GitTreeNode`] children.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidGitObject`] if the tree payload is malformed or contains unsafe entry names.
    pub fn read_tree_nodes(&self, tree_id: &ObjectId) -> CoreResult<Vec<GitTreeNode>> {
        if let Ok(guard) = self.cache.lock() {
            if let Some(cached) = guard.trees.get(tree_id) {
                return Ok(cached.clone());
            }
        }

        let (kind, body) = self.read_raw_object(tree_id)?;

        if kind != "tree" {
            return Err(CoreError::InvalidGitObject {
                id: tree_id.to_string(),
                reason: format!("expected tree, found {kind}"),
            });
        }

        let mut nodes = Vec::new();
        let mut pos = 0usize;

        while pos < body.len() {
            let space_offset = body[pos..].iter().position(|&b| b == b' ').ok_or_else(|| {
                CoreError::InvalidGitObject {
                    id: tree_id.to_string(),
                    reason: "missing space in tree entry".to_string(),
                }
            })?;
            let mode_str = std::str::from_utf8(&body[pos..pos + space_offset]).map_err(|e| {
                CoreError::InvalidGitObject {
                    id: tree_id.to_string(),
                    reason: e.to_string(),
                }
            })?;
            let mode = EntryMode::from_git_octal(mode_str)?;
            pos += space_offset + 1;

            let nul_offset = body[pos..].iter().position(|&b| b == 0).ok_or_else(|| {
                CoreError::InvalidGitObject {
                    id: tree_id.to_string(),
                    reason: "missing NUL after tree entry name".to_string(),
                }
            })?;
            let name = std::str::from_utf8(&body[pos..pos + nul_offset])
                .map_err(|e| CoreError::InvalidGitObject {
                    id: tree_id.to_string(),
                    reason: e.to_string(),
                })?
                .to_string();
            validate_tree_entry_name(tree_id, &name)?;
            pos += nul_offset + 1;

            if pos + 20 > body.len() {
                return Err(CoreError::InvalidGitObject {
                    id: tree_id.to_string(),
                    reason: "truncated 20-byte SHA in tree entry".to_string(),
                });
            }

            let sha_slice: [u8; 20] =
                body[pos..pos + 20]
                    .try_into()
                    .map_err(|_| CoreError::InvalidGitObject {
                        id: tree_id.to_string(),
                        reason: "invalid 20-byte slice".to_string(),
                    })?;
            let id = ObjectId::from_bytes(&sha_slice);
            pos += 20;

            nodes.push(GitTreeNode { mode, name, id });
        }

        if let Ok(mut guard) = self.cache.lock() {
            if guard.trees.len() >= MAX_CACHED_OBJECTS {
                guard.trees.clear();
            }
            guard.trees.insert(*tree_id, nodes.clone());
        }

        Ok(nodes)
    }

    /// Recursively builds hierarchical Git `tree` objects from a flat `path -> FileEntry` map,
    /// validating every repository-relative path and rejecting duplicate or file/directory name
    /// collisions, and returns the root tree [`ObjectId`].
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if any path is invalid, contains empty segments, or
    /// collides with another leaf or directory entry, or [`CoreError::Io`] / [`CoreError::InvalidGitObject`]
    /// if writing any tree object fails.
    pub fn write_tree_from_map(&self, files: &BTreeMap<String, FileEntry>) -> CoreResult<ObjectId> {
        let mut normalized: BTreeMap<String, &FileEntry> = BTreeMap::new();

        for (raw_path, entry) in files {
            if raw_path.split(['/', '\\']).any(str::is_empty) {
                return Err(CoreError::InvalidArgument(format!(
                    "tree entry path '{raw_path}' contains empty path segment"
                )));
            }

            let clean = validate_repo_rel_path(raw_path)?;

            if normalized.insert(clean.clone(), entry).is_some() {
                return Err(CoreError::InvalidArgument(format!(
                    "duplicate normalized tree entry path '{clean}'"
                )));
            }
        }

        let items: Vec<(&str, &FileEntry)> =
            normalized.iter().map(|(k, v)| (k.as_str(), *v)).collect();

        self.write_tree_from_slice(&items)
    }

    /// Recursively partitions a sorted `(rel_path, &FileEntry)` slice by directory prefix
    /// without allocating intermediate `BTreeMap`s or suffix `String`s, rejecting leaf-vs-directory
    /// name collisions at every level.
    fn write_tree_from_slice(&self, items: &[(&str, &FileEntry)]) -> CoreResult<ObjectId> {
        let mut nodes = Vec::new();
        let mut seen_names: BTreeSet<&str> = BTreeSet::new();
        let mut idx = 0usize;

        while idx < items.len() {
            let (rel_path, entry) = items[idx];

            if let Some((dir_name, _rest)) = rel_path.split_once('/') {
                if dir_name.is_empty() {
                    return Err(CoreError::InvalidArgument(format!(
                        "tree entry path '{rel_path}' contains empty directory segment"
                    )));
                }

                if !seen_names.insert(dir_name) {
                    return Err(CoreError::InvalidArgument(format!(
                        "tree path collision on '{dir_name}': cannot store both a file and a directory"
                    )));
                }

                let mut end = idx;
                let mut sub_items = Vec::new();

                while end < items.len() {
                    if let Some((d, r)) = items[end].0.split_once('/') {
                        if d == dir_name {
                            sub_items.push((r, items[end].1));
                            end += 1;
                            continue;
                        }
                    }

                    break;
                }

                let sub_tree_id = self.write_tree_from_slice(&sub_items)?;
                nodes.push(GitTreeNode {
                    mode: EntryMode::Directory,
                    name: dir_name.to_string(),
                    id: sub_tree_id,
                });
                idx = end;
            } else {
                if !seen_names.insert(rel_path) {
                    return Err(CoreError::InvalidArgument(format!(
                        "tree path collision on '{rel_path}': cannot store both a file and a directory"
                    )));
                }

                nodes.push(GitTreeNode {
                    mode: entry.entry_mode(),
                    name: rel_path.to_string(),
                    id: entry.blob_id,
                });
                idx += 1;
            }
        }

        self.write_tree_nodes(nodes)
    }

    /// Looks up a single repository-relative file path inside `root_tree_id` in $O(\text{depth})$ time
    /// without reading or flattening unrelated subtrees.
    ///
    /// # Errors
    /// Returns [`CoreError`] if `rel_path` is invalid or any tree along the path is corrupt.
    pub fn read_tree_entry_at_path(
        &self,
        root_tree_id: &ObjectId,
        rel_path: &str,
    ) -> CoreResult<Option<FileEntry>> {
        let clean = validate_repo_rel_path(rel_path)?;
        let segments: Vec<&str> = clean.split('/').collect();
        let mut curr_tree = *root_tree_id;

        for (i, seg) in segments.iter().enumerate() {
            let is_last = i + 1 == segments.len();
            let nodes = self.read_tree_nodes(&curr_tree)?;
            let Some(found) = nodes.into_iter().find(|n| n.name == *seg) else {
                return Ok(None);
            };

            if is_last {
                return match found.mode {
                    EntryMode::Directory => Ok(None),
                    other => Ok(Some(FileEntry::from_mode(found.id, other))),
                };
            }

            if found.mode != EntryMode::Directory {
                return Ok(None);
            }

            curr_tree = found.id;
        }

        Ok(None)
    }

    /// Incrementally updates a set of file paths (`Some(FileEntry)` to insert/replace, `None` to remove)
    /// starting from `root_tree_id`, rewriting only the $O(\text{depth})$ modified directory spine.
    ///
    /// # Errors
    /// Returns [`CoreError`] if any path is invalid or reading/writing tree objects fails.
    pub fn update_tree_entries(
        &self,
        root_tree_id: &ObjectId,
        updates: &BTreeMap<String, Option<FileEntry>>,
    ) -> CoreResult<ObjectId> {
        if updates.is_empty() {
            return Ok(*root_tree_id);
        }

        let mut normalized_updates: Vec<(String, &Option<FileEntry>)> =
            Vec::with_capacity(updates.len());

        for (raw_path, entry_opt) in updates {
            let trimmed = raw_path.trim_matches('/');

            if !trimmed.is_empty() {
                let clean = validate_repo_rel_path(trimmed)?;
                normalized_updates.push((clean, entry_opt));
            }
        }

        normalized_updates.sort_by(|a, b| a.0.cmp(&b.0));
        let clean_updates: Vec<(&str, &Option<FileEntry>)> = normalized_updates
            .iter()
            .map(|(p, e)| (p.as_str(), *e))
            .collect();

        self.update_tree_slice(Some(root_tree_id), &clean_updates)?
            .map(Ok)
            .unwrap_or_else(|| self.write_tree_nodes(Vec::new()))
    }

    /// Recursive helper that applies sorted `updates` to `base_tree_id` and returns `Ok(None)` if the resulting directory is empty.
    fn update_tree_slice(
        &self,
        base_tree_id: Option<&ObjectId>,
        updates: &[(&str, &Option<FileEntry>)],
    ) -> CoreResult<Option<ObjectId>> {
        let mut existing: BTreeMap<String, GitTreeNode> = BTreeMap::new();

        if let Some(tid) = base_tree_id {
            for node in self.read_tree_nodes(tid)? {
                existing.insert(node.name.clone(), node);
            }
        }

        let mut idx = 0usize;

        while idx < updates.len() {
            let (rel_path, entry_opt) = updates[idx];

            if let Some((dir_name, _rest)) = rel_path.split_once('/') {
                let mut end = idx;
                let mut sub_updates = Vec::new();

                while end < updates.len() {
                    if let Some((d, r)) = updates[end].0.split_once('/') {
                        if d == dir_name {
                            sub_updates.push((r, updates[end].1));
                            end += 1;
                            continue;
                        }
                    }

                    break;
                }

                let prev_sub = existing
                    .get(dir_name)
                    .filter(|n| n.mode == EntryMode::Directory)
                    .map(|n| n.id);

                if let Some(new_sub_id) = self.update_tree_slice(prev_sub.as_ref(), &sub_updates)? {
                    existing.insert(
                        dir_name.to_string(),
                        GitTreeNode {
                            mode: EntryMode::Directory,
                            name: dir_name.to_string(),
                            id: new_sub_id,
                        },
                    );
                } else if existing
                    .get(dir_name)
                    .is_some_and(|n| n.mode == EntryMode::Directory)
                {
                    existing.remove(dir_name);
                }

                idx = end;
            } else {
                match entry_opt {
                    Some(entry) => {
                        existing.insert(
                            rel_path.to_string(),
                            GitTreeNode {
                                mode: entry.entry_mode(),
                                name: rel_path.to_string(),
                                id: entry.blob_id,
                            },
                        );
                    }
                    None => {
                        existing.remove(rel_path);
                    }
                }

                idx += 1;
            }
        }

        if existing.is_empty() {
            return Ok(None);
        }

        let nodes: Vec<GitTreeNode> = existing.into_values().collect();
        self.write_tree_nodes(nodes).map(Some)
    }

    /// Recursively flattens a Git root `tree` [`ObjectId`] into a sorted `BTreeMap<String, FileEntry>`.
    ///
    /// # Errors
    /// Returns [`CoreError`] if any referenced tree object is unreadable.
    pub fn read_tree_to_map(&self, tree_id: &ObjectId) -> CoreResult<BTreeMap<String, FileEntry>> {
        let mut out = BTreeMap::new();
        self.collect_tree_recursive(tree_id, "", &mut out)?;
        Ok(out)
    }

    /// Recursively traverses `tree_id` under `prefix` and populates `out` with leaf entries.
    fn collect_tree_recursive(
        &self,
        tree_id: &ObjectId,
        prefix: &str,
        out: &mut BTreeMap<String, FileEntry>,
    ) -> CoreResult<()> {
        for node in self.read_tree_nodes(tree_id)? {
            let rel_path = if prefix.is_empty() {
                node.name.clone()
            } else {
                format!("{prefix}/{}", node.name)
            };

            match node.mode {
                EntryMode::Directory => {
                    self.collect_tree_recursive(&node.id, &rel_path, out)?;
                }
                other => {
                    out.insert(rel_path, FileEntry::from_mode(node.id, other));
                }
            }
        }

        Ok(())
    }

    /// Writes a native Git `commit` object and returns its [`ObjectId`].
    ///
    /// Honors `commit.gpgsign = true` by delegating lock-free commit creation to `git commit-tree -S`
    /// without touching `.git/index`.
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] or [`CoreError::InvalidGitObject`] if writing or signing the commit fails.
    pub fn write_commit(
        &self,
        tree_id: &ObjectId,
        parents: &[ObjectId],
        author_ident: &str,
        message: &str,
    ) -> CoreResult<ObjectId> {
        let secs = now_millis() / 1000;
        let raw_clean: String = author_ident
            .chars()
            .filter(|c| !matches!(c, '\n' | '\r' | '\0'))
            .collect();
        let clean_author = match raw_clean.rfind('>') {
            Some(idx) => raw_clean[..=idx].trim(),
            None => raw_clean.trim(),
        };

        if self
            .git_config_value("commit.gpgsign")
            .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"))
        {
            if let Some(signed_oid) =
                self.try_write_signed_commit(tree_id, parents, clean_author, secs, message)?
            {
                return Ok(signed_oid);
            }
        }

        let mut body = String::new();
        body.push_str(&format!("tree {}\n", tree_id.as_str()));

        for parent in parents {
            body.push_str(&format!("parent {}\n", parent.as_str()));
        }

        body.push_str(&format!("author {clean_author} {secs} +0000\n"));
        body.push_str(&format!("committer {clean_author} {secs} +0000\n\n"));
        body.push_str(message.trim_end());
        body.push('\n');

        self.write_raw_object("commit", body.as_bytes())
    }

    /// Creates a GPG/SSH-signed commit object via `git commit-tree -S` without touching `.git/index`.
    fn try_write_signed_commit(
        &self,
        tree_id: &ObjectId,
        parents: &[ObjectId],
        clean_author: &str,
        secs: u64,
        message: &str,
    ) -> CoreResult<Option<ObjectId>> {
        let (author_name, author_email) = match clean_author.split_once('<') {
            Some((name_part, rest)) => (
                name_part.trim().to_string(),
                rest.trim_end_matches('>').trim().to_string(),
            ),
            None => (clean_author.to_string(), "developer@localhost".to_string()),
        };

        let mut cmd = Command::new("git");
        cmd.env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_AUTHOR_NAME", &author_name)
            .env("GIT_AUTHOR_EMAIL", &author_email)
            .env("GIT_AUTHOR_DATE", format!("{secs} +0000"))
            .env("GIT_COMMITTER_NAME", &author_name)
            .env("GIT_COMMITTER_EMAIL", &author_email)
            .env("GIT_COMMITTER_DATE", format!("{secs} +0000"))
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("commit-tree")
            .arg("-S")
            .arg(tree_id.as_str());

        for parent in parents {
            cmd.arg("-p").arg(parent.as_str());
        }

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| CoreError::Io {
                path: "git commit-tree -S".to_string(),
                source,
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            let mut payload = message.trim_end().to_string();
            payload.push('\n');
            stdin
                .write_all(payload.as_bytes())
                .map_err(|source| CoreError::Io {
                    path: "git commit-tree -S stdin".to_string(),
                    source,
                })?;
        }

        let out = child.wait_with_output().map_err(|source| CoreError::Io {
            path: "git commit-tree -S".to_string(),
            source,
        })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(CoreError::InvalidGitObject {
                id: tree_id.to_string(),
                reason: format!("commit signing failed (commit.gpgsign=true): {stderr}"),
            });
        }

        let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(Some(ObjectId::from_hex(&hex)?))
    }

    /// Reads and parses a native Git `commit` object by its [`ObjectId`].
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidGitObject`] if `commit_id` is not a valid commit object.
    pub fn read_commit(&self, commit_id: &ObjectId) -> CoreResult<GitCommit> {
        if let Ok(guard) = self.cache.lock() {
            if let Some(cached) = guard.commits.get(commit_id) {
                return Ok(cached.clone());
            }
        }

        let (kind, body) = self.read_raw_object(commit_id)?;

        if kind != "commit" {
            return Err(CoreError::InvalidGitObject {
                id: commit_id.to_string(),
                reason: format!("expected commit, found {kind}"),
            });
        }

        let text = String::from_utf8_lossy(&body);
        let (headers, message) = text.split_once("\n\n").unwrap_or((&text, ""));

        let mut tree_id: Option<ObjectId> = None;
        let mut parents = Vec::new();
        let mut author = String::new();
        let mut committer = String::new();

        for line in headers.lines() {
            if let Some(rest) = line.strip_prefix("tree ") {
                tree_id = Some(ObjectId::from_hex(rest.trim())?);
            } else if let Some(rest) = line.strip_prefix("parent ") {
                parents.push(ObjectId::from_hex(rest.trim())?);
            } else if let Some(rest) = line.strip_prefix("author ") {
                author = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("committer ") {
                committer = rest.to_string();
            }
        }

        let tree_id = tree_id.ok_or_else(|| CoreError::InvalidGitObject {
            id: commit_id.to_string(),
            reason: "commit missing tree header".to_string(),
        })?;

        let commit = GitCommit {
            tree_id,
            parents,
            author,
            committer,
            message: message.to_string(),
        };

        if let Ok(mut guard) = self.cache.lock() {
            if guard.commits.len() >= MAX_CACHED_OBJECTS {
                guard.commits.clear();
            }
            guard.commits.insert(*commit_id, commit.clone());
        }

        Ok(commit)
    }

    /// Reads the symbolic branch name pointed to by `.git/HEAD` (e.g. `"main"`).
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] if `.git/HEAD` cannot be read, or [`CoreError::NotFound`] if `HEAD` is detached.
    pub fn current_branch_name(&self) -> CoreResult<String> {
        let head_path = self.git_dir.join("HEAD");
        let content = fs::read_to_string(&head_path).map_err(|source| CoreError::Io {
            path: head_path.display().to_string(),
            source,
        })?;
        let trimmed = content.trim();

        if let Some(sym) = trimmed.strip_prefix("ref: refs/heads/") {
            if validate_ref_name(sym).is_ok() {
                return Ok(sym.to_string());
            }
        }

        Err(CoreError::NotFound(
            "HEAD is detached or does not point to a local branch".to_string(),
        ))
    }

    /// Returns `true` if `.git/HEAD` points directly to a commit SHA rather than a symbolic branch ref.
    #[must_use]
    pub fn is_detached_head(&self) -> bool {
        let head_path = self.git_dir.join("HEAD");
        fs::read_to_string(&head_path)
            .map(|raw| !raw.trim().starts_with("ref: "))
            .unwrap_or(false)
    }

    /// Resolves a Git reference (such as `"refs/heads/main"`) to its [`ObjectId`],
    /// checking both loose refs and `.git/packed-refs` with bounded symbolic-ref indirection.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `ref_name` is unsafe, or
    /// [`CoreError::InvalidGitObject`] if a ref file contains malformed hex or a symref cycle.
    pub fn resolve_ref(&self, ref_name: &str) -> CoreResult<Option<ObjectId>> {
        self.resolve_ref_inner(ref_name, 0)
    }

    /// Internal recursive reference resolver with symref loop protection.
    fn resolve_ref_inner(&self, ref_name: &str, depth: usize) -> CoreResult<Option<ObjectId>> {
        if depth > 16 {
            return Err(CoreError::InvalidGitObject {
                id: ref_name.to_string(),
                reason: "symbolic ref cycle detected".to_string(),
            });
        }

        let is_per_worktree =
            matches!(ref_name, "HEAD" | "FETCH_HEAD" | "ORIG_HEAD" | "MERGE_HEAD");

        if !is_per_worktree {
            validate_ref_name(ref_name)?;
        }

        let base_dir = if is_per_worktree {
            &self.git_dir
        } else {
            &self.common_git_dir
        };
        let ref_path = join_rel_segments(base_dir, ref_name);

        if ref_path.is_file() {
            let raw = fs::read_to_string(&ref_path).map_err(|source| CoreError::Io {
                path: ref_path.display().to_string(),
                source,
            })?;
            let trimmed = raw.trim();

            if ref_name == "FETCH_HEAD" {
                let first_token = trimmed.split_whitespace().next().unwrap_or("");

                if first_token.is_empty() {
                    return Ok(None);
                }

                return Ok(Some(ObjectId::from_hex(first_token)?));
            }

            if let Some(target_ref) = trimmed.strip_prefix("ref: ") {
                return self.resolve_ref_inner(target_ref.trim(), depth + 1);
            }

            return Ok(Some(ObjectId::from_hex(trimmed)?));
        }

        let packed_refs = self.common_git_dir.join("packed-refs");

        if packed_refs.is_file() {
            let raw = fs::read_to_string(&packed_refs).map_err(|source| CoreError::Io {
                path: packed_refs.display().to_string(),
                source,
            })?;

            for line in raw.lines() {
                if line.starts_with('#') || line.starts_with('^') {
                    continue;
                }

                if let Some((sha, rname)) = line.split_once(' ') {
                    if rname.trim() == ref_name {
                        return Ok(Some(ObjectId::from_hex(sha.trim())?));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Resolves `.git/HEAD` to the current commit [`ObjectId`], if any.
    ///
    /// # Errors
    /// Returns [`CoreError`] if `.git/HEAD` is corrupt.
    pub fn resolve_head_commit(&self) -> CoreResult<Option<ObjectId>> {
        let head_path = self.git_dir.join("HEAD");

        if !head_path.exists() {
            return Ok(None);
        }

        let raw = fs::read_to_string(&head_path).map_err(|source| CoreError::Io {
            path: head_path.display().to_string(),
            source,
        })?;
        let trimmed = raw.trim();

        if let Some(sym_ref) = trimmed.strip_prefix("ref: ") {
            self.resolve_ref(sym_ref.trim())
        } else {
            Ok(Some(ObjectId::from_hex(trimmed)?))
        }
    }

    /// Atomically updates a Git reference (e.g., `"refs/heads/main"` or `"refs/tessera/agents/a1"`)
    /// to point to `commit_id`.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `ref_name` violates reference naming rules,
    /// or [`CoreError::Io`] if writing the ref fails.
    pub fn update_ref(&self, ref_name: &str, commit_id: &ObjectId) -> CoreResult<()> {
        validate_ref_name(ref_name)?;
        let ref_path = join_rel_segments(&self.common_git_dir, ref_name);
        let content = format!("{}\n", commit_id.as_str());
        atomic_write_file(&ref_path, content.as_bytes())
    }

    /// Removes a loose or packed Git reference if it exists and passes reference validation,
    /// preserving `packed-refs` peeled-tag integrity and pruning empty loose ref parent directories.
    pub fn delete_ref(&self, ref_name: &str) {
        if validate_ref_name(ref_name).is_err() {
            return;
        }

        let refs_root = self.common_git_dir.join("refs");
        let ref_path = join_rel_segments(&self.common_git_dir, ref_name);
        let _ = fs::remove_file(&ref_path);

        let mut parent_opt = ref_path.parent();

        while let Some(parent) = parent_opt {
            if parent == refs_root || !parent.starts_with(&refs_root) {
                break;
            }

            if fs::remove_dir(parent).is_err() {
                break;
            }

            parent_opt = parent.parent();
        }

        let packed_refs = self.common_git_dir.join("packed-refs");

        if packed_refs.is_file() {
            let lock_path = self.common_git_dir.join("tessera").join("packed-refs.lock");
            let lock_file = lock_path.parent().and_then(|p| {
                let _ = fs::create_dir_all(p);
                fs::OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .truncate(false)
                    .open(&lock_path)
                    .ok()
            });

            if let Some(ref f) = lock_file {
                let _ = f.lock();
            }

            if let Ok(raw) = fs::read_to_string(&packed_refs) {
                let mut changed = false;
                let mut skip_peeled = false;
                let mut kept = String::with_capacity(raw.len());

                for line in raw.lines() {
                    if skip_peeled {
                        skip_peeled = false;

                        if line.starts_with('^') {
                            continue;
                        }
                    }

                    if !line.starts_with('#') && !line.starts_with('^') {
                        if let Some((_, rname)) = line.split_once(' ') {
                            if rname.trim() == ref_name {
                                changed = true;
                                skip_peeled = true;
                                continue;
                            }
                        }
                    }

                    kept.push_str(line);
                    kept.push('\n');
                }

                if changed {
                    let _ = atomic_write_file(&packed_refs, kept.as_bytes());
                }
            }
        }
    }

    /// Returns `true` if `ancestor` is reachable from `descendant` (or equal to it) in the commit DAG.
    #[must_use]
    pub fn is_ancestor(&self, ancestor: &ObjectId, descendant: &ObjectId) -> bool {
        if ancestor == descendant {
            return true;
        }

        let mut visited = BTreeSet::new();
        let mut queue = std::collections::VecDeque::new();
        let mut hit_limit_or_error = false;
        queue.push_back(*descendant);

        while let Some(curr) = queue.pop_front() {
            if &curr == ancestor {
                return true;
            }

            if visited.contains(&curr) {
                continue;
            }

            if visited.len() >= 4096 {
                hit_limit_or_error = true;
                break;
            }

            visited.insert(curr);

            match self.read_commit(&curr) {
                Ok(commit) => {
                    for p in commit.parents {
                        queue.push_back(p);
                    }
                }
                Err(_) => {
                    hit_limit_or_error = true;
                }
            }
        }

        if !hit_limit_or_error {
            return false;
        }

        Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args([
                "merge-base",
                "--is-ancestor",
                ancestor.as_str(),
                descendant.as_str(),
            ])
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Finds the lowest common ancestor (merge-base) between commits `a` and `b` in the commit DAG,
    /// falling back to `git merge-base` on deep or merge-heavy histories for exact LCA accuracy.
    #[must_use]
    pub fn find_merge_base(&self, a: &ObjectId, b: &ObjectId) -> Option<ObjectId> {
        if a == b {
            return Some(*a);
        }

        let mut ancestors_a = BTreeSet::new();
        let mut queue_a = std::collections::VecDeque::new();
        let mut complex_or_deep = false;
        queue_a.push_back(*a);

        while let Some(curr) = queue_a.pop_front() {
            if ancestors_a.contains(&curr) {
                continue;
            }

            if ancestors_a.len() >= 4096 {
                complex_or_deep = true;
                break;
            }

            ancestors_a.insert(curr);

            match self.read_commit(&curr) {
                Ok(commit) => {
                    if commit.parents.len() > 1 {
                        complex_or_deep = true;
                    }

                    for p in commit.parents {
                        queue_a.push_back(p);
                    }
                }
                Err(_) => {
                    complex_or_deep = true;
                }
            }
        }

        if !complex_or_deep {
            let mut visited_b = BTreeSet::new();
            let mut queue_b = std::collections::VecDeque::new();
            queue_b.push_back(*b);

            while let Some(curr) = queue_b.pop_front() {
                if ancestors_a.contains(&curr) {
                    return Some(curr);
                }

                if visited_b.contains(&curr) {
                    continue;
                }

                if visited_b.len() >= 4096 {
                    complex_or_deep = true;
                    break;
                }

                visited_b.insert(curr);

                match self.read_commit(&curr) {
                    Ok(commit) => {
                        if commit.parents.len() > 1 {
                            complex_or_deep = true;
                            break;
                        }

                        for p in commit.parents {
                            queue_b.push_back(p);
                        }
                    }
                    Err(_) => {
                        complex_or_deep = true;
                        break;
                    }
                }
            }

            if !complex_or_deep {
                return None;
            }
        }

        let out = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["merge-base", a.as_str(), b.as_str()])
            .output()
            .ok()?;

        if !out.status.success() {
            return None;
        }

        let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
        ObjectId::from_hex(&hex).ok()
    }

    /// Updates `.git/HEAD` to point symbolically at `refs/heads/<branch>`.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `branch` is invalid, or [`CoreError::Io`] if writing `.git/HEAD` fails.
    pub fn set_head_branch(&self, branch: &str) -> CoreResult<()> {
        validate_ref_name(branch.trim())?;
        let head_path = self.git_dir.join("HEAD");
        let content = format!("ref: refs/heads/{}\n", branch.trim());
        atomic_write_file(&head_path, content.as_bytes())
    }

    /// Resolves the Git author identity (`Name <email>`) from `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`,
    /// `git config user.name` / `git config user.email`, or the active OS user account.
    pub fn resolve_git_author(&self) -> String {
        let sanitize_field = |raw: &str| -> String {
            raw.chars()
                .filter(|c| !matches!(c, '\n' | '\r' | '\0' | '<' | '>'))
                .collect::<String>()
                .trim()
                .to_string()
        };

        let raw_name = std::env::var("GIT_AUTHOR_NAME")
            .ok()
            .filter(|s| !sanitize_field(s).is_empty())
            .or_else(|| self.git_config_value("user.name"))
            .or_else(Self::os_user_full_name)
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|s| !sanitize_field(s).is_empty())
            })
            .or_else(|| {
                std::env::var("LOGNAME")
                    .ok()
                    .filter(|s| !sanitize_field(s).is_empty())
            })
            .or_else(|| {
                std::env::var("USERNAME")
                    .ok()
                    .filter(|s| !sanitize_field(s).is_empty())
            })
            .unwrap_or_else(|| "Developer".to_string());
        let name = {
            let s = sanitize_field(&raw_name);
            if s.is_empty() {
                "Developer".to_string()
            } else {
                s
            }
        };

        let raw_email = std::env::var("GIT_AUTHOR_EMAIL")
            .ok()
            .filter(|s| !sanitize_field(s).is_empty())
            .or_else(|| self.git_config_value("user.email"))
            .or_else(|| {
                std::env::var("EMAIL")
                    .ok()
                    .filter(|s| !sanitize_field(s).is_empty())
            })
            .unwrap_or_else(|| format!("{}@localhost", name.to_lowercase().replace(' ', ".")));
        let email = {
            let s = sanitize_field(&raw_email);
            if s.is_empty() {
                format!("{}@localhost", name.to_lowercase().replace(' ', "."))
            } else {
                s
            }
        };

        format!("{name} <{email}>")
    }

    /// Queries the current OS user's full display name (e.g., via `id -F` on macOS/BSD) when Git config is unset.
    fn os_user_full_name() -> Option<String> {
        #[cfg(unix)]
        {
            let out = Command::new("id").arg("-F").output().ok()?;

            if out.status.success() {
                let s = String::from_utf8(out.stdout).ok()?;
                let trimmed = s.trim();

                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }

        None
    }

    /// Queries a single configuration value from `.git/config` or global Git configuration.
    pub fn git_config_value(&self, key: &str) -> Option<String> {
        let out = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["config", "--get", key])
            .output()
            .ok()?;

        if !out.status.success() {
            return None;
        }

        let s = String::from_utf8(out.stdout).ok()?;
        let trimmed = s.trim();

        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Resolves the user's global Git ignore file (`core.excludesFile`, `$XDG_CONFIG_HOME/git/ignore`,
    /// or `~/.config/git/ignore`) if one exists on disk.
    #[must_use]
    pub fn global_excludes_file(&self) -> Option<PathBuf> {
        if let Some(configured) = self.git_config_value("core.excludesFile") {
            let expanded = if let Some(rest) = configured.strip_prefix("~/") {
                std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .map(|h| join_rel_segments(Path::new(&h), rest))
                    .unwrap_or_else(|_| PathBuf::from(&configured))
            } else {
                PathBuf::from(&configured)
            };

            if expanded.is_file() {
                return Some(expanded);
            }
        }

        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.trim().is_empty() {
                let cand = PathBuf::from(xdg).join("git").join("ignore");

                if cand.is_file() {
                    return Some(cand);
                }
            }
        }

        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            if !home.trim().is_empty() {
                let cand = PathBuf::from(home)
                    .join(".config")
                    .join("git")
                    .join("ignore");

                if cand.is_file() {
                    return Some(cand);
                }
            }
        }

        None
    }

    /// Synchronizes `.git/index` to match `tree_or_commit_id` (when `.git/index.lock` is not held)
    /// and refreshes the index stat cache (`ctime`, `mtime`, `ino`) so `git status`, `git diff`,
    /// and read-only prompt daemons (`gitstatusd` / Powerlevel10k / Starship) report a clean
    /// working tree after `tes commit`, `tes merge`, `tes restore`, or `tes undo`.
    pub fn sync_git_index(&self, tree_or_commit_id: &ObjectId) {
        if self.git_dir.join("index.lock").exists() {
            return;
        }

        let _ = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.repo_root)
            .args(["read-tree", "--reset", tree_or_commit_id.as_str()])
            .output();

        let _ = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.repo_root)
            .args(["update-index", "-q", "--refresh", "--unmerged"])
            .output();
    }

    /// Peels an annotated Git `tag` object (possibly nested) down to its target commit [`ObjectId`].
    fn peel_to_commit(&self, oid: ObjectId) -> CoreResult<ObjectId> {
        let mut curr = oid;

        for _ in 0..8 {
            let (kind, body) = self.read_raw_object(&curr)?;

            if kind == "commit" {
                return Ok(curr);
            }

            if kind != "tag" {
                return Err(CoreError::InvalidGitObject {
                    id: curr.to_string(),
                    reason: format!("expected commit or tag, found {kind}"),
                });
            }

            let text = String::from_utf8_lossy(&body);
            let target_hex = text
                .lines()
                .find_map(|l| l.strip_prefix("object "))
                .map(str::trim)
                .ok_or_else(|| CoreError::InvalidGitObject {
                    id: curr.to_string(),
                    reason: "annotated tag missing object header".to_string(),
                })?;
            curr = ObjectId::from_hex(target_hex)?;
        }

        Err(CoreError::InvalidGitObject {
            id: oid.to_string(),
            reason: "annotated tag indirection depth exceeded".to_string(),
        })
    }

    /// Resolves any Git revision expression (`HEAD`, `HEAD~1`, short commit SHA, branch name, or tag)
    /// to its full 40-char commit [`ObjectId`], peeling annotated tags to commits.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `rev_spec` starts with `-` or is empty, or
    /// [`CoreError::NotFound`] if the revision cannot be resolved to a valid commit.
    pub fn resolve_revision(&self, rev_spec: &str) -> CoreResult<ObjectId> {
        let trimmed = rev_spec.trim();

        if trimmed.is_empty()
            || trimmed.starts_with('-')
            || trimmed.contains("..")
            || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(CoreError::InvalidArgument(format!(
                "invalid revision specification '{trimmed}'"
            )));
        }

        if matches!(trimmed, "HEAD" | "FETCH_HEAD" | "ORIG_HEAD" | "MERGE_HEAD")
            || trimmed.starts_with("refs/")
        {
            if let Ok(Some(oid)) = self.resolve_ref(trimmed) {
                if let Ok(cid) = self.peel_to_commit(oid) {
                    return Ok(cid);
                }
            }
        }

        if let Ok(Some(oid)) = self.resolve_ref(&format!("refs/heads/{trimmed}")) {
            if let Ok(cid) = self.peel_to_commit(oid) {
                return Ok(cid);
            }
        }

        if let Ok(Some(oid)) = self.resolve_ref(&format!("refs/tags/{trimmed}")) {
            if let Ok(cid) = self.peel_to_commit(oid) {
                return Ok(cid);
            }
        }

        if let Ok(Some(oid)) = self.resolve_ref(&format!("refs/remotes/{trimmed}")) {
            if let Ok(cid) = self.peel_to_commit(oid) {
                return Ok(cid);
            }
        }

        if let Ok(Some(oid)) = self.resolve_ref(&format!("refs/remotes/origin/{trimmed}")) {
            if let Ok(cid) = self.peel_to_commit(oid) {
                return Ok(cid);
            }
        }

        let spec_commit = format!("{trimmed}^{{commit}}");

        if let Ok(out) = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["rev-parse", "--verify", "--quiet", &spec_commit])
            .output()
        {
            if out.status.success() {
                let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();

                if let Ok(oid) = ObjectId::from_hex(&hex) {
                    return Ok(oid);
                }
            }
        }

        Err(CoreError::NotFound(format!("revision '{trimmed}'")))
    }

    /// Lists all local Git branch names under `refs/heads/` and packed-refs.
    #[must_use]
    pub fn list_local_branches(&self) -> Vec<String> {
        let mut branches = BTreeSet::new();

        if let Ok(out) = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
            .output()
        {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let b = line.trim();

                    if !b.is_empty() && validate_ref_name(b).is_ok() {
                        branches.insert(b.to_string());
                    }
                }
            }
        }

        if let Ok(curr) = self.current_branch_name() {
            if !curr.is_empty() && validate_ref_name(&curr).is_ok() {
                branches.insert(curr);
            }
        }

        branches.into_iter().collect()
    }

    /// Lists recent commits `(short_sha, subject)` for dynamic shell tab completion.
    #[must_use]
    pub fn list_recent_commits(&self, limit: usize) -> Vec<(String, String)> {
        let mut commits = Vec::new();
        let max_count = format!("-n{limit}");

        if let Ok(out) = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["log", "--all", &max_count, "--format=%h\t%s"])
            .output()
        {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    if let Some((sha, subj)) = line.split_once('\t') {
                        let s = sha.trim();

                        if !s.is_empty() {
                            commits.push((s.to_string(), subj.trim().to_string()));
                        }
                    }
                }
            }
        }

        commits
    }

    /// Updates `.git/HEAD` to point symbolically at `refs/heads/<branch>`.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `branch` is invalid, or [`CoreError::Io`] if writing `.git/HEAD` fails.
    pub fn set_symbolic_head(&self, branch: &str) -> CoreResult<()> {
        let clean = branch.trim();
        validate_ref_name(clean)?;
        let content = format!("ref: refs/heads/{clean}\n");
        atomic_write_file(&self.git_dir.join("HEAD"), content.as_bytes())
    }

    /// Returns a structured commit DAG across all branches for visual graph rendering.
    #[must_use]
    pub fn list_commit_graph(&self, limit: usize) -> Vec<CommitGraphNode> {
        let mut nodes = Vec::new();
        let max_count = format!("-n{limit}");

        if let Ok(out) = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args([
                "log",
                "--branches",
                "--tags",
                "--remotes",
                "--topo-order",
                &max_count,
                "--format=%H\t%h\t%P\t%an\t%ct\t%D\t%s",
            ])
            .output()
        {
            if out.status.success() {
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
                            .map(|r| r.strip_prefix("HEAD -> ").unwrap_or(r).to_string())
                            .collect();
                        let subject = parts[6].trim().to_string();

                        nodes.push(CommitGraphNode {
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
            }
        }

        nodes
    }
}

/// Structured commit node for visual branch and commit graph rendering.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitGraphNode {
    /// Full 40-char commit SHA-1.
    pub sha: String,
    /// Short commit SHA-1.
    pub short_sha: String,
    /// Parent commit SHA-1s.
    pub parents: Vec<String>,
    /// Commit author name.
    pub author: String,
    /// Commit unix timestamp in seconds.
    pub timestamp: i64,
    /// Commit subject line.
    pub subject: String,
    /// Branch and tag labels pointing at this commit.
    pub refs: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_git_blobs_hierarchical_trees_and_commits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let b1 = git.write_blob(b"fn alpha() {}\n").expect("write blob 1");
        let b2 = git
            .write_blob(b"export const x = 42;\n")
            .expect("write blob 2");

        let mut files = BTreeMap::new();
        files.insert(
            "crates/core/src/lib.rs".to_string(),
            FileEntry::regular(b1, false),
        );
        files.insert(
            "packages/web/index.ts".to_string(),
            FileEntry::regular(b2, false),
        );

        let root_tree = git.write_tree_from_map(&files).expect("write tree");
        let flattened = git.read_tree_to_map(&root_tree).expect("read tree");
        assert_eq!(files, flattened);

        // Verify O(depth) single-path lookup and incremental spine update.
        let single = git
            .read_tree_entry_at_path(&root_tree, "crates/core/src/lib.rs")
            .expect("single path lookup");
        assert_eq!(single, Some(FileEntry::regular(b1, false)));
        assert!(
            git.read_tree_entry_at_path(&root_tree, "crates/core/src/missing.rs")
                .expect("missing lookup")
                .is_none()
        );

        let b3 = git.write_blob(b"fn updated() {}\n").expect("write blob 3");
        let mut updates = BTreeMap::new();
        updates.insert(
            "crates/core/src/lib.rs".to_string(),
            Some(FileEntry::regular(b3, false)),
        );
        updates.insert("packages/web/index.ts".to_string(), None);

        let updated_tree = git
            .update_tree_entries(&root_tree, &updates)
            .expect("update_tree_entries");
        let mut expected_files = BTreeMap::new();
        expected_files.insert(
            "crates/core/src/lib.rs".to_string(),
            FileEntry::regular(b3, false),
        );
        assert_eq!(
            git.read_tree_to_map(&updated_tree).expect("read updated"),
            expected_files
        );

        let head = git
            .resolve_head_commit()
            .expect("resolve head")
            .expect("head exists");
        let commit_id = git
            .write_commit(
                &root_tree,
                &[head],
                "Agent Alpha <alpha@tes>",
                "feat(core): add alpha and web index",
            )
            .expect("write commit");
        let parsed = git.read_commit(&commit_id).expect("read commit");
        assert_eq!(parsed.tree_id, root_tree);
        assert_eq!(
            git.read_blob(&b1).expect("read b1"),
            b"fn alpha() {}\n".to_vec()
        );
    }

    #[test]
    fn rejects_flag_injection_in_revisions_and_traversal_in_refs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");
        let head = git
            .resolve_head_commit()
            .expect("resolve head")
            .expect("head");

        assert!(git.resolve_revision("--output=/tmp/pwn").is_err());
        assert!(git.update_ref("refs/heads/../config", &head).is_err());
        assert!(git.set_head_branch("--bad-branch").is_err());
    }

    #[test]
    fn preserves_symlinks_gitlinks_dir_to_file_and_head_resolution() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let b1 = git.write_blob(b"hello\n").expect("blob");
        let link_blob = git.write_blob(b"target/path").expect("link blob");
        let sub_commit =
            ObjectId::from_hex("1234567890abcdef1234567890abcdef12345678").expect("oid");

        let mut files = BTreeMap::new();
        files.insert("src/a.rs".to_string(), FileEntry::regular(b1, false));
        files.insert(
            "links/latest".to_string(),
            FileEntry::from_mode(link_blob, EntryMode::Symlink),
        );
        files.insert(
            "vendor/submod".to_string(),
            FileEntry::from_mode(sub_commit, EntryMode::Gitlink),
        );

        let tree_id = git.write_tree_from_map(&files).expect("write tree");
        let roundtrip = git.read_tree_to_map(&tree_id).expect("read tree");
        assert_eq!(roundtrip, files);
        assert!(roundtrip["links/latest"].is_symlink());
        assert!(roundtrip["vendor/submod"].is_gitlink());

        // Replace directory `src` (`src/a.rs`) with a regular file named `src`.
        let mut dir_to_file = BTreeMap::new();
        dir_to_file.insert("src".to_string(), Some(FileEntry::regular(b1, true)));
        dir_to_file.insert("src/a.rs".to_string(), None);
        let replaced_tree = git
            .update_tree_entries(&tree_id, &dir_to_file)
            .expect("replace dir with file");
        let replaced_map = git.read_tree_to_map(&replaced_tree).expect("read replaced");
        assert_eq!(replaced_map.get("src"), Some(&FileEntry::regular(b1, true)));
        assert!(!replaced_map.contains_key("src/a.rs"));

        // Verify resolve_revision("HEAD") prefers local HEAD over refs/remotes/origin/HEAD.
        let initial = git
            .resolve_head_commit()
            .expect("head")
            .expect("head exists");
        let next_commit = git
            .write_commit(&tree_id, &[initial], "Dev <dev@example.com>", "feat: next")
            .expect("commit");
        git.update_ref("refs/heads/main", &next_commit)
            .expect("advance main");
        git.update_ref("refs/remotes/origin/main", &initial)
            .expect("origin main");
        atomic_write_file(
            &join_rel_segments(git.common_git_dir(), "refs/remotes/origin/HEAD"),
            b"ref: refs/remotes/origin/main\n",
        )
        .expect("write origin/HEAD");

        assert_eq!(
            git.resolve_revision("HEAD").expect("resolve HEAD"),
            next_commit
        );

        // Verify annotated tag peeling.
        let tag_body = format!(
            "object {}\ntype commit\ntag v1.0\ntagger Dev <dev@example.com> 1700000000 +0000\n\nrelease\n",
            next_commit.as_str()
        );
        let tag_oid = git
            .write_raw_object("tag", tag_body.as_bytes())
            .expect("write tag");
        git.update_ref("refs/tags/v1.0", &tag_oid)
            .expect("update tag");
        assert_eq!(
            git.resolve_revision("v1.0").expect("resolve annotated tag"),
            next_commit
        );

        // Verify packed-refs peeled line removal.
        let packed_content = format!(
            "# pack-refs with: peeled fully-peeled sorted\n{} refs/heads/keep\n{} refs/tags/v1.0\n^{}\n",
            initial.as_str(),
            tag_oid.as_str(),
            next_commit.as_str()
        );
        atomic_write_file(
            &git.common_git_dir().join("packed-refs"),
            packed_content.as_bytes(),
        )
        .expect("write packed-refs");
        git.delete_ref("refs/tags/v1.0");
        let after_packed =
            fs::read_to_string(git.common_git_dir().join("packed-refs")).expect("read packed-refs");
        assert!(!after_packed.contains("refs/tags/v1.0"));
        assert!(!after_packed.contains(&format!("^{}", next_commit.as_str())));
        assert!(after_packed.contains("refs/heads/keep"));
    }

    #[test]
    fn discovers_git_worktree_pointer_files_and_rejects_detached_branch_lie() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main_dir = tmp.path().join("main-repo");
        fs::create_dir_all(&main_dir).expect("mkdir main");
        let main_git = GitBackend::init_or_open(&main_dir).expect("init main");
        let head = main_git
            .resolve_head_commit()
            .expect("head")
            .expect("commit");

        let wt_git_dir = main_git.git_dir().join("worktrees").join("wt1");
        fs::create_dir_all(&wt_git_dir).expect("mkdir wt gitdir");
        atomic_write_file(&wt_git_dir.join("commondir"), b"../..\n").expect("commondir");
        atomic_write_file(
            &wt_git_dir.join("HEAD"),
            format!("{}\n", head.as_str()).as_bytes(),
        )
        .expect("detached HEAD");

        let wt_dir = tmp.path().join("wt1");
        fs::create_dir_all(&wt_dir).expect("mkdir wt");
        atomic_write_file(
            &wt_dir.join(".git"),
            format!("gitdir: {}\n", wt_git_dir.display()).as_bytes(),
        )
        .expect("write .git pointer");

        let discovered = GitBackend::discover(&wt_dir).expect("discover worktree");
        assert_eq!(discovered.git_dir(), wt_git_dir.as_path());
        assert_eq!(
            discovered.common_git_dir().canonicalize().ok(),
            main_git.git_dir().canonicalize().ok()
        );
        assert!(discovered.is_detached_head());
        assert!(discovered.current_branch_name().is_err());
        assert_eq!(
            discovered.resolve_head_commit().expect("resolve"),
            Some(head)
        );
    }

    #[test]
    fn validate_tree_entry_name_rejects_ntfs_ads_and_colons() {
        let dummy_tree =
            ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");

        assert!(validate_tree_entry_name(&dummy_tree, "lib.rs").is_ok());
        assert!(validate_tree_entry_name(&dummy_tree, ".git::$INDEX_ALLOCATION").is_err());
        assert!(validate_tree_entry_name(&dummy_tree, ".git:$I30:$INDEX_ALLOCATION").is_err());
        assert!(validate_tree_entry_name(&dummy_tree, ".tessera:stream").is_err());
        assert!(validate_tree_entry_name(&dummy_tree, "git~1:stream").is_err());
        assert!(validate_tree_entry_name(&dummy_tree, "file.txt:Zone.Identifier").is_err());
    }

    #[test]
    fn discover_env_fallback_supports_relative_dot_and_canonical_sandbox_prefixes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).expect("mkdir repo");
        let repo_git = GitBackend::init_or_open(&repo_dir).expect("init repo");
        fs::create_dir_all(repo_git.common_git_dir().join("tessera")).expect("mkdir tessera");

        let ws_dir = tmp.path().join("external-ws");
        let ws_sub = ws_dir.join("nested").join("pkg");
        let outside_dir = tmp.path().join("outside");
        fs::create_dir_all(&ws_sub).expect("mkdir ws_sub");
        fs::create_dir_all(&outside_dir).expect("mkdir outside");

        let repo_str = repo_dir.display().to_string();
        let ws_str = ws_dir.display().to_string();
        let ws_canon_str = ws_dir
            .canonicalize()
            .expect("canon ws")
            .display()
            .to_string();

        let discovered_sub = GitBackend::discover_with_context(
            Path::new("."),
            Some(&ws_sub),
            Some(&repo_str),
            Some(&ws_str),
        )
        .expect("discover relative '.' inside TES_WORKSPACE");
        assert_eq!(discovered_sub.repo_root(), repo_dir.as_path());

        let discovered_canon = GitBackend::discover_with_context(
            Path::new("."),
            Some(&ws_dir),
            Some(&repo_str),
            Some(&ws_canon_str),
        )
        .expect("discover relative '.' with canonicalized TES_WORKSPACE");
        assert_eq!(discovered_canon.repo_root(), repo_dir.as_path());

        let outside_res = GitBackend::discover_with_context(
            Path::new("."),
            Some(&outside_dir),
            Some(&repo_str),
            Some(&ws_str),
        );
        assert!(matches!(outside_res, Err(CoreError::RepoNotFound(_))));

        // Verify canonicalized sandbox paths inside `.tessera/workspaces` match non-canonical repo_root.
        let sandbox_dir = repo_dir
            .join(".tessera")
            .join("workspaces")
            .join("agent-alpha");
        fs::create_dir_all(sandbox_dir.join(".tessera")).expect("mkdir sandbox .tessera");
        fs::write(sandbox_dir.join(".tessera").join("repo_root"), &repo_str)
            .expect("write repo_root");

        let canon_sandbox = sandbox_dir.canonicalize().expect("canon sandbox");
        assert_eq!(
            is_valid_sandbox_repo_root(&canon_sandbox, Some(&repo_dir)),
            Some(repo_dir.clone())
        );

        // Verify `.tessera/tmp` blocks an enclosing host `TES_WORKSPACE` while allowing a nested one.
        let host_ws = tmp.path().join("host-sandbox");
        let nested_tmp = host_ws.join(".tessera").join("tmp").join("job-1");
        let nested_ws = nested_tmp.join("inner-ws");
        let nested_sub = nested_ws.join("pkg");
        fs::create_dir_all(&nested_sub).expect("mkdir nested_sub");

        let host_ws_str = host_ws.display().to_string();
        let nested_ws_str = nested_ws.display().to_string();

        let blocked_host = GitBackend::discover_with_context(
            Path::new("."),
            Some(&nested_sub),
            Some(&repo_str),
            Some(&host_ws_str),
        );
        assert!(matches!(blocked_host, Err(CoreError::RepoNotFound(_))));

        let allowed_nested = GitBackend::discover_with_context(
            Path::new("."),
            Some(&nested_sub),
            Some(&repo_str),
            Some(&nested_ws_str),
        )
        .expect("nested TES_WORKSPACE inside .tessera/tmp should resolve");
        assert_eq!(allowed_nested.repo_root(), repo_dir.as_path());
    }

    #[test]
    fn write_tree_from_map_rejects_empty_segments_and_leaf_directory_collisions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");
        let blob = git.write_blob(b"hello\n").expect("write blob");

        let mut empty_seg = BTreeMap::new();
        empty_seg.insert("a//b".to_string(), FileEntry::regular(blob, false));
        assert!(matches!(
            git.write_tree_from_map(&empty_seg),
            Err(CoreError::InvalidArgument(_))
        ));

        let mut dup_norm = BTreeMap::new();
        dup_norm.insert("a/b".to_string(), FileEntry::regular(blob, false));
        dup_norm.insert("./a/b".to_string(), FileEntry::regular(blob, false));
        assert!(matches!(
            git.write_tree_from_map(&dup_norm),
            Err(CoreError::InvalidArgument(_))
        ));

        let mut direct_collision = BTreeMap::new();
        direct_collision.insert("foo".to_string(), FileEntry::regular(blob, false));
        direct_collision.insert("foo/bar".to_string(), FileEntry::regular(blob, false));
        assert!(matches!(
            git.write_tree_from_map(&direct_collision),
            Err(CoreError::InvalidArgument(_))
        ));

        let mut non_adjacent_collision = BTreeMap::new();
        non_adjacent_collision.insert("foo".to_string(), FileEntry::regular(blob, false));
        non_adjacent_collision.insert("foo-extra".to_string(), FileEntry::regular(blob, false));
        non_adjacent_collision.insert("foo/bar".to_string(), FileEntry::regular(blob, false));
        assert!(matches!(
            git.write_tree_from_map(&non_adjacent_collision),
            Err(CoreError::InvalidArgument(_))
        ));

        let mut nested_collision = BTreeMap::new();
        nested_collision.insert("a/b".to_string(), FileEntry::regular(blob, false));
        nested_collision.insert("a/b/c".to_string(), FileEntry::regular(blob, false));
        assert!(matches!(
            git.write_tree_from_map(&nested_collision),
            Err(CoreError::InvalidArgument(_))
        ));
    }
}
