//! Lock-free native Git object database (`blob`, `tree`, `commit`) and reference backend.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Digest, Sha1};

use crate::model::{
    atomic_write_file, atomic_write_file_relaxed, now_millis, validate_ref_name,
    validate_repo_rel_path, CoreError, CoreResult, EntryMode, FileEntry, ObjectId,
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

/// Lock-free bridge to a colocated `.git` directory.
///
/// Reads and writes native Git objects (`blob`, `tree`, `commit`) directly in `.git/objects`
/// without ever acquiring `.git/index.lock`.
#[derive(Clone)]
pub struct GitBackend {
    repo_root: PathBuf,
    git_dir: PathBuf,
    cache: Arc<Mutex<GitObjectCache>>,
}

impl fmt::Debug for GitBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitBackend")
            .field("repo_root", &self.repo_root)
            .field("git_dir", &self.git_dir)
            .finish()
    }
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

/// Validates that a single Git tree entry name cannot escape its directory or target `.git` / `.tessera`.
fn validate_tree_entry_name(tree_id: &ObjectId, name: &str) -> CoreResult<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.eq_ignore_ascii_case(".git")
        || name.eq_ignore_ascii_case(".tessera")
        || name.eq_ignore_ascii_case("git~1")
    {
        return Err(CoreError::InvalidGitObject {
            id: tree_id.to_string(),
            reason: format!("unsafe or invalid tree entry name '{name}'"),
        });
    }

    Ok(())
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

    if !target_root.is_absolute() || !target_root.join(".git").join("tessera").is_dir() {
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

    let is_inside_repo = sandbox_dir.starts_with(target_root.join(".tessera").join("workspaces"))
        || sandbox_dir.starts_with(target_root.join(".git").join("tessera"));

    if is_inside_repo {
        return Some(target_root);
    }

    let parent = sandbox_dir.parent()?;

    if parent.file_name().and_then(|n| n.to_str()) != Some("branches") {
        return None;
    }

    let slug_dir = parent.parent()?.file_name()?.to_str()?;
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
    /// Constructs a [`GitBackend`] handle for `(repo_root, git_dir)` with an initialized cache.
    fn with_paths(repo_root: PathBuf, git_dir: PathBuf) -> Self {
        Self {
            repo_root,
            git_dir,
            cache: Arc::new(Mutex::new(GitObjectCache::default())),
        }
    }

    /// Discovers a Git repository starting at `start_dir` (walking upward if needed) and returns
    /// a [`GitBackend`] handle.
    ///
    /// # Errors
    /// Returns [`CoreError::RepoNotFound`] if no `.git` directory exists in `start_dir` or its ancestors.
    pub fn discover(start_dir: &Path) -> CoreResult<Self> {
        let mut curr = start_dir.to_path_buf();

        loop {
            let dot_git = curr.join(".git");

            if dot_git.is_dir() {
                return Ok(Self::with_paths(curr, dot_git));
            }

            if let Some(target_root) = is_valid_sandbox_repo_root(&curr, None) {
                let target_git = target_root.join(".git");

                if target_git.is_dir() {
                    return Ok(Self::with_paths(target_root, target_git));
                }
            }

            if !curr.pop() {
                if let (Ok(env_root), Ok(env_ws)) = (
                    std::env::var("TES_REPO_ROOT"),
                    std::env::var("TES_WORKSPACE"),
                ) {
                    let ws_path = PathBuf::from(env_ws.trim());
                    let target_root = PathBuf::from(env_root.trim());
                    let target_git = target_root.join(".git");

                    if !ws_path.as_os_str().is_empty()
                        && start_dir.starts_with(&ws_path)
                        && target_git.is_dir()
                    {
                        return Ok(Self::with_paths(target_root, target_git));
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
        let git_dir = repo_root.join(".git");

        if !git_dir.exists() {
            fs::create_dir_all(git_dir.join("objects/info")).map_err(|source| CoreError::Io {
                path: git_dir.display().to_string(),
                source,
            })?;
            fs::create_dir_all(git_dir.join("objects/pack")).map_err(|source| CoreError::Io {
                path: git_dir.display().to_string(),
                source,
            })?;
            fs::create_dir_all(git_dir.join("refs/heads")).map_err(|source| CoreError::Io {
                path: git_dir.display().to_string(),
                source,
            })?;
            fs::create_dir_all(git_dir.join("refs/tags")).map_err(|source| CoreError::Io {
                path: git_dir.display().to_string(),
                source,
            })?;

            atomic_write_file(&git_dir.join("HEAD"), b"ref: refs/heads/main\n")?;
        }

        let backend = Self::with_paths(repo_root.to_path_buf(), git_dir);

        // Ensure at least one commit exists so agents always have a valid base commit.
        if backend.resolve_head_commit()?.is_none() {
            let empty_tree = backend.write_tree_from_map(&BTreeMap::new())?;
            let author = backend.resolve_git_author();
            let root_commit =
                backend.write_commit(&empty_tree, &[], &author, "Initial commit")?;
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

    /// Returns the `.git` directory path.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Computes the loose object filesystem path `.git/objects/<2-hex>/<38-hex>` for `id`.
    fn loose_object_path(&self, id: &ObjectId) -> PathBuf {
        let hex_str = id.as_str();
        self.git_dir
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

        atomic_write_file_relaxed(&path, &compressed)?;
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

        if path.exists() {
            let compressed = fs::read(&path).map_err(|source| CoreError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let mut decoder = ZlibDecoder::new(&compressed[..]).take(MAX_GIT_OBJECT_BYTES + 64);
            let mut decompressed = Vec::new();
            decoder
                .read_to_end(&mut decompressed)
                .map_err(|source| CoreError::Io {
                    path: path.display().to_string(),
                    source,
                })?;

            if (decompressed.len() as u64) > MAX_GIT_OBJECT_BYTES + 64 {
                return Err(CoreError::InvalidGitObject {
                    id: id.to_string(),
                    reason: "decompressed git object exceeds maximum size limit".to_string(),
                });
            }

            let nul_pos = decompressed
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| CoreError::InvalidGitObject {
                    id: id.to_string(),
                    reason: "missing NUL byte in object header".to_string(),
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

            if let Some(size_str) = header_parts.next() {
                if let Ok(expected_size) = size_str.parse::<usize>() {
                    let actual_size = decompressed.len() - (nul_pos + 1);

                    if expected_size != actual_size {
                        return Err(CoreError::InvalidGitObject {
                            id: id.to_string(),
                            reason: format!(
                                "header size {expected_size} does not match payload size {actual_size}"
                            ),
                        });
                    }
                }
            }

            decompressed.drain(..=nul_pos);
            return Ok((kind, decompressed));
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
            let space_offset = body[pos..]
                .iter()
                .position(|&b| b == b' ')
                .ok_or_else(|| CoreError::InvalidGitObject {
                    id: tree_id.to_string(),
                    reason: "missing space in tree entry".to_string(),
                })?;
            let mode_str =
                std::str::from_utf8(&body[pos..pos + space_offset]).map_err(|e| {
                    CoreError::InvalidGitObject {
                        id: tree_id.to_string(),
                        reason: e.to_string(),
                    }
                })?;
            let mode = EntryMode::from_git_octal(mode_str)?;
            pos += space_offset + 1;

            let nul_offset = body[pos..]
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| CoreError::InvalidGitObject {
                    id: tree_id.to_string(),
                    reason: "missing NUL after tree entry name".to_string(),
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

    /// Recursively builds hierarchical Git `tree` objects from a flat `path -> FileEntry` map
    /// using zero-allocation slice partitioning and returns the root tree [`ObjectId`].
    ///
    /// # Errors
    /// Returns [`CoreError`] if writing any tree object fails.
    pub fn write_tree_from_map(
        &self,
        files: &BTreeMap<String, FileEntry>,
    ) -> CoreResult<ObjectId> {
        let items: Vec<(&str, &FileEntry)> = files
            .iter()
            .map(|(k, v)| (k.trim_matches('/'), v))
            .filter(|(k, _)| !k.is_empty())
            .collect();

        self.write_tree_from_slice(&items)
    }

    /// Recursively partitions a sorted `(rel_path, &FileEntry)` slice by directory prefix
    /// without allocating intermediate `BTreeMap`s or suffix `String`s.
    fn write_tree_from_slice(&self, items: &[(&str, &FileEntry)]) -> CoreResult<ObjectId> {
        let mut nodes = Vec::new();
        let mut idx = 0usize;

        while idx < items.len() {
            let (rel_path, entry) = items[idx];

            if let Some((dir_name, _rest)) = rel_path.split_once('/') {
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
                let mode = if entry.executable {
                    EntryMode::ExecutableFile
                } else {
                    EntryMode::NormalFile
                };
                nodes.push(GitTreeNode {
                    mode,
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
                    EntryMode::ExecutableFile => Ok(Some(FileEntry {
                        blob_id: found.id,
                        executable: true,
                    })),
                    EntryMode::NormalFile | EntryMode::Symlink => Ok(Some(FileEntry {
                        blob_id: found.id,
                        executable: false,
                    })),
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

        let mut clean_updates: Vec<(&str, &Option<FileEntry>)> = Vec::with_capacity(updates.len());

        for (raw_path, entry_opt) in updates {
            let trimmed = raw_path.trim_matches('/');

            if !trimmed.is_empty() {
                let _ = validate_repo_rel_path(trimmed)?;
                clean_updates.push((trimmed, entry_opt));
            }
        }

        clean_updates.sort_by(|a, b| a.0.cmp(b.0));
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
                } else {
                    existing.remove(dir_name);
                }

                idx = end;
            } else {
                match entry_opt {
                    Some(entry) => {
                        let mode = if entry.executable {
                            EntryMode::ExecutableFile
                        } else {
                            EntryMode::NormalFile
                        };
                        existing.insert(
                            rel_path.to_string(),
                            GitTreeNode {
                                mode,
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
    pub fn read_tree_to_map(
        &self,
        tree_id: &ObjectId,
    ) -> CoreResult<BTreeMap<String, FileEntry>> {
        let mut out = BTreeMap::new();
        self.collect_tree_recursive(tree_id, "", &mut out)?;
        Ok(out)
    }

    /// Recursively traverses `tree_id` under `prefix` and populates `out` with leaf file entries.
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
                EntryMode::ExecutableFile => {
                    out.insert(
                        rel_path,
                        FileEntry {
                            blob_id: node.id,
                            executable: true,
                        },
                    );
                }
                EntryMode::NormalFile | EntryMode::Symlink => {
                    out.insert(
                        rel_path,
                        FileEntry {
                            blob_id: node.id,
                            executable: false,
                        },
                    );
                }
            }
        }

        Ok(())
    }

    /// Writes a native Git `commit` object and returns its [`ObjectId`].
    ///
    /// # Errors
    /// Returns [`CoreError::Io`] if writing the commit object fails.
    pub fn write_commit(
        &self,
        tree_id: &ObjectId,
        parents: &[ObjectId],
        author_ident: &str,
        message: &str,
    ) -> CoreResult<ObjectId> {
        let secs = now_millis() / 1000;
        let clean_author: String = author_ident
            .chars()
            .filter(|c| !matches!(c, '\n' | '\r' | '\0'))
            .collect();
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
    /// Returns [`CoreError::Io`] if `.git/HEAD` cannot be read.
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

        Ok("main".to_string())
    }

    /// Resolves a Git reference (such as `"refs/heads/main"`) to its [`ObjectId`],
    /// checking both loose refs and `.git/packed-refs`.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `ref_name` is unsafe, or
    /// [`CoreError::InvalidGitObject`] if a ref file contains malformed hex.
    pub fn resolve_ref(&self, ref_name: &str) -> CoreResult<Option<ObjectId>> {
        if ref_name != "HEAD" && ref_name != "FETCH_HEAD" {
            validate_ref_name(ref_name)?;
        }

        let ref_path = self.git_dir.join(ref_name);

        if ref_path.is_file() {
            let raw = fs::read_to_string(&ref_path).map_err(|source| CoreError::Io {
                path: ref_path.display().to_string(),
                source,
            })?;
            let trimmed = raw.trim();

            if let Some(target_ref) = trimmed.strip_prefix("ref: ") {
                return self.resolve_ref(target_ref.trim());
            }

            return Ok(Some(ObjectId::from_hex(trimmed)?));
        }

        let packed_refs = self.git_dir.join("packed-refs");

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
        let ref_path = self.git_dir.join(ref_name);
        let content = format!("{}\n", commit_id.as_str());
        atomic_write_file(&ref_path, content.as_bytes())
    }

    /// Removes a loose or packed Git reference if it exists and passes reference validation.
    pub fn delete_ref(&self, ref_name: &str) {
        if validate_ref_name(ref_name).is_err() {
            return;
        }

        let ref_path = self.git_dir.join(ref_name);
        let _ = fs::remove_file(ref_path);

        let packed_refs = self.git_dir.join("packed-refs");

        if packed_refs.is_file() {
            if let Ok(raw) = fs::read_to_string(&packed_refs) {
                let mut changed = false;
                let mut kept = String::with_capacity(raw.len());

                for line in raw.lines() {
                    if let Some((_, rname)) = line.split_once(' ') {
                        if rname.trim() == ref_name {
                            changed = true;
                            continue;
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
        let mut queue = vec![*descendant];

        while let Some(curr) = queue.pop() {
            if &curr == ancestor {
                return true;
            }

            if !visited.insert(curr) || visited.len() > 4096 {
                continue;
            }

            if let Ok(commit) = self.read_commit(&curr) {
                for p in commit.parents {
                    queue.push(p);
                }
            }
        }

        Command::new("git")
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
            .or_else(|| std::env::var("USER").ok().filter(|s| !sanitize_field(s).is_empty()))
            .or_else(|| std::env::var("LOGNAME").ok().filter(|s| !sanitize_field(s).is_empty()))
            .or_else(|| std::env::var("USERNAME").ok().filter(|s| !sanitize_field(s).is_empty()))
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
            .or_else(|| std::env::var("EMAIL").ok().filter(|s| !sanitize_field(s).is_empty()))
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
    fn git_config_value(&self, key: &str) -> Option<String> {
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

    /// Resolves any Git revision expression (`HEAD`, `HEAD~1`, short commit SHA, branch name, or tag)
    /// to its full 40-char commit [`ObjectId`].
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `rev_spec` starts with `-` or is empty, or
    /// [`CoreError::NotFound`] if the revision cannot be resolved to a valid commit.
    pub fn resolve_revision(&self, rev_spec: &str) -> CoreResult<ObjectId> {
        let trimmed = rev_spec.trim();

        if trimmed.is_empty() || trimmed.starts_with('-') || trimmed.contains('\0') {
            return Err(CoreError::InvalidArgument(format!(
                "invalid revision specification '{trimmed}'"
            )));
        }

        if let Ok(Some(cid)) = self.resolve_ref(&format!("refs/heads/{trimmed}")) {
            return Ok(cid);
        }

        if let Ok(Some(cid)) = self.resolve_ref(&format!("refs/tags/{trimmed}")) {
            return Ok(cid);
        }

        if let Ok(Some(cid)) = self.resolve_ref(trimmed) {
            return Ok(cid);
        }

        let spec_commit = format!("{trimmed}^{{commit}}");

        if let Ok(out) = Command::new("git")
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
        let b2 = git.write_blob(b"export const x = 42;\n").expect("write blob 2");

        let mut files = BTreeMap::new();
        files.insert(
            "crates/core/src/lib.rs".to_string(),
            FileEntry {
                blob_id: b1,
                executable: false,
            },
        );
        files.insert(
            "packages/web/index.ts".to_string(),
            FileEntry {
                blob_id: b2,
                executable: false,
            },
        );

        let root_tree = git.write_tree_from_map(&files).expect("write tree");
        let flattened = git.read_tree_to_map(&root_tree).expect("read tree");
        assert_eq!(files, flattened);

        // Verify O(depth) single-path lookup and incremental spine update.
        let single = git
            .read_tree_entry_at_path(&root_tree, "crates/core/src/lib.rs")
            .expect("single path lookup");
        assert_eq!(
            single,
            Some(FileEntry {
                blob_id: b1,
                executable: false,
            })
        );
        assert!(git
            .read_tree_entry_at_path(&root_tree, "crates/core/src/missing.rs")
            .expect("missing lookup")
            .is_none());

        let b3 = git.write_blob(b"fn updated() {}\n").expect("write blob 3");
        let mut updates = BTreeMap::new();
        updates.insert(
            "crates/core/src/lib.rs".to_string(),
            Some(FileEntry {
                blob_id: b3,
                executable: false,
            }),
        );
        updates.insert("packages/web/index.ts".to_string(), None);

        let updated_tree = git
            .update_tree_entries(&root_tree, &updates)
            .expect("update_tree_entries");
        let mut expected_files = BTreeMap::new();
        expected_files.insert(
            "crates/core/src/lib.rs".to_string(),
            FileEntry {
                blob_id: b3,
                executable: false,
            },
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
}
