//! Domain data models, content-addressed identifiers, algebraic conflict representations,
//! and crash-safe atomic filesystem primitives.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Process-local monotonic sequence number used to guarantee unique temporary file names
/// across concurrent threads invoking [`atomic_write_file`].
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Domain errors emitted by `core` storage, object database, and operation log operations.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Underlying filesystem I/O failure.
    #[error("filesystem I/O error at {path}: {source}")]
    Io {
        /// Path associated with the I/O failure.
        path: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// Git repository or `.git/tessera` directory was not found.
    #[error("repository not found or not initialized with Tessera at {0}")]
    RepoNotFound(String),

    /// Corrupt or malformed Git object encountered in `.git/objects`.
    #[error("invalid git object {id}: {reason}")]
    InvalidGitObject {
        /// Object identifier.
        id: String,
        /// Explanation of format violation.
        reason: String,
    },

    /// Serialization or deserialization error for `.git/tessera` metadata.
    #[error("metadata serialization error: {0}")]
    Serialization(String),

    /// Requested operation ID, agent session, or conflict ID does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Invalid arguments or invariant violation.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

/// Result alias for `core` operations.
pub type CoreResult<T> = Result<T, CoreError>;

/// Shared implementation for crash-safe sibling temporary file creation and atomic rename,
/// optionally issuing a hardware `sync_all` barrier before renaming.
///
/// # Errors
/// Returns [`CoreError::Io`] if parent directory creation, temporary file creation/write,
/// optional `sync_all`, or `fs::rename` fails.
fn atomic_write_file_inner(target_path: &Path, bytes: &[u8], sync_data: bool) -> CoreResult<()> {
    let parent = target_path.parent().ok_or_else(|| CoreError::Io {
        path: target_path.display().to_string(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target path has no parent directory",
        ),
    })?;

    fs::create_dir_all(parent).map_err(|source| CoreError::Io {
        path: parent.display().to_string(),
        source,
    })?;

    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = target_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state");
    let tmp_name = format!(".{file_name}.tmp.{pid}.{seq}");
    let tmp_path = parent.join(tmp_name);

    {
        let mut file = File::create(&tmp_path).map_err(|source| CoreError::Io {
            path: tmp_path.display().to_string(),
            source,
        })?;

        file.write_all(bytes).map_err(|source| CoreError::Io {
            path: tmp_path.display().to_string(),
            source,
        })?;

        if sync_data {
            file.sync_all().map_err(|source| CoreError::Io {
                path: tmp_path.display().to_string(),
                source,
            })?;
        }
    }

    fs::rename(&tmp_path, target_path).map_err(|source| {
        let _ = fs::remove_file(&tmp_path);
        CoreError::Io {
            path: target_path.display().to_string(),
            source,
        }
    })
}

/// Atomically writes `bytes` to `target_path` by writing to a sibling temporary file
/// on the same filesystem mount, flushing hardware buffers via `sync_all`, and executing
/// an atomic `fs::rename`.
///
/// # Preconditions
/// - The parent directory of `target_path` must exist or be creatable.
///
/// # Errors
/// Returns [`CoreError::Io`] if parent directory creation, temporary file write, `sync_all`,
/// or `fs::rename` fails.
pub fn atomic_write_file(target_path: &Path, bytes: &[u8]) -> CoreResult<()> {
    atomic_write_file_inner(target_path, bytes, true)
}

/// Atomically writes `bytes` to `target_path` via a sibling temporary file and atomic
/// `fs::rename` without issuing a per-file hardware `sync_all` barrier.
///
/// Intended for high-frequency content-addressed loose objects and non-authoritative caches
/// where atomic visibility is required without per-file hardware `F_FULLFSYNC` stalls.
///
/// # Errors
/// Returns [`CoreError::Io`] if parent directory creation, temporary file write, or `fs::rename` fails.
pub fn atomic_write_file_relaxed(target_path: &Path, bytes: &[u8]) -> CoreResult<()> {
    atomic_write_file_inner(target_path, bytes, false)
}

/// Updates an existing working-tree file in-place using `O_NOFOLLOW` on Unix so file-watcher
/// inodes (`kqueue` / `inotify`) are preserved while atomically preventing TOCTOU symlink races,
/// falling back to [`atomic_write_file_relaxed`] when creating a new file.
///
/// # Errors
/// Returns [`CoreError::InvalidArgument`] if `target_path` is a symbolic link, or [`CoreError::Io`]
/// if writing fails.
pub fn write_working_tree_file_safe(target_path: &Path, bytes: &[u8]) -> CoreResult<()> {
    if let Ok(meta) = fs::symlink_metadata(target_path) {
        if meta.file_type().is_symlink() {
            return Err(CoreError::InvalidArgument(format!(
                "refusing to overwrite symbolic link at '{}'",
                target_path.display()
            )));
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            match fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(false)
                .custom_flags(libc::O_NOFOLLOW)
                .open(target_path)
            {
                Ok(mut f) => {
                    return f.write_all(bytes).map_err(|source| CoreError::Io {
                        path: target_path.display().to_string(),
                        source,
                    });
                }
                Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
                    return Err(CoreError::InvalidArgument(format!(
                        "refusing to follow symbolic link at '{}'",
                        target_path.display()
                    )));
                }
                Err(_) => {}
            }
        }
    }

    atomic_write_file_relaxed(target_path, bytes)
}

/// Validates and normalizes a repository-relative path, rejecting traversal (`..`),
/// `.git` and `.tessera` directory access, NUL bytes, and Windows drive prefixes.
///
/// # Errors
/// Returns [`CoreError::InvalidArgument`] if `raw` is empty or contains forbidden components.
pub fn validate_repo_rel_path(raw: &str) -> CoreResult<String> {
    if raw.contains('\0') {
        return Err(CoreError::InvalidArgument(
            "path must not contain NUL bytes".to_string(),
        ));
    }

    let unified = raw.replace('\\', "/");
    let mut trimmed = unified.trim();

    while let Some(rest) = trimmed.strip_prefix("./") {
        trimmed = rest;
    }

    if trimmed.starts_with('/') || trimmed.contains(':') {
        return Err(CoreError::InvalidArgument(format!(
            "path '{raw}' must be repository-relative"
        )));
    }

    let mut clean_segments = Vec::new();

    for seg in trimmed.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }

        if seg == ".."
            || seg.eq_ignore_ascii_case(".git")
            || seg.eq_ignore_ascii_case(".tessera")
            || seg.eq_ignore_ascii_case("git~1")
        {
            return Err(CoreError::InvalidArgument(format!(
                "path '{raw}' contains forbidden component '{seg}'"
            )));
        }

        clean_segments.push(seg);
    }

    if clean_segments.is_empty() {
        return Err(CoreError::InvalidArgument(format!(
            "path '{raw}' resolves to an empty repository path"
        )));
    }

    Ok(clean_segments.join("/"))
}

/// Resolves an input path (either repository-relative or an absolute path inside `repo_root`)
/// into a validated `(clean_rel_path, full_path)` pair while verifying that no existing
/// directory component or target path is a symbolic link escaping `repo_root`.
///
/// # Errors
/// Returns [`CoreError::InvalidArgument`] if the path traverses outside `repo_root`, targets
/// `.git`, or traverses through a symbolic link.
pub fn resolve_confined_repo_path(
    repo_root: &Path,
    raw_path: &str,
) -> CoreResult<(String, PathBuf)> {
    if raw_path.contains('\0') {
        return Err(CoreError::InvalidArgument(
            "path must not contain NUL bytes".to_string(),
        ));
    }

    let trimmed = raw_path.trim();

    if trimmed.is_empty() {
        return Err(CoreError::InvalidArgument("empty path".to_string()));
    }

    let candidate_rel = if Path::new(trimmed).is_absolute() {
        let input_path = Path::new(trimmed);

        if let Ok(stripped) = input_path.strip_prefix(repo_root) {
            stripped.to_string_lossy().replace('\\', "/")
        } else if let Ok(canon_root) = repo_root.canonicalize() {
            if let Ok(stripped) = input_path.strip_prefix(&canon_root) {
                stripped.to_string_lossy().replace('\\', "/")
            } else {
                // Canonicalize the deepest existing ancestor to handle OS symlinked temp roots (e.g., `/var` -> `/private/var` on macOS).
                let mut cur = input_path;
                let mut suffix_parts = Vec::new();
                let mut resolved_rel = None;

                while let Some(parent) = cur.parent() {
                    if let Some(name) = cur.file_name() {
                        suffix_parts.push(name.to_string_lossy().to_string());
                    }

                    if let Ok(canon_parent) = parent.canonicalize() {
                        if let Ok(base_rel) = canon_parent.strip_prefix(&canon_root) {
                            let mut combined = base_rel.to_path_buf();

                            for part in suffix_parts.iter().rev() {
                                combined.push(part);
                            }

                            resolved_rel = Some(combined.to_string_lossy().replace('\\', "/"));
                        }

                        break;
                    }

                    cur = parent;
                }

                resolved_rel.ok_or_else(|| {
                    CoreError::InvalidArgument(format!(
                        "path '{raw_path}' is outside repository root '{}'",
                        repo_root.display()
                    ))
                })?
            }
        } else {
            return Err(CoreError::InvalidArgument(format!(
                "path '{raw_path}' is outside repository root '{}'",
                repo_root.display()
            )));
        }
    } else {
        let mut s = trimmed.replace('\\', "/");

        while let Some(rest) = s.strip_prefix("./") {
            s = rest.to_string();
        }

        s
    };

    for comp in Path::new(&candidate_rel).components() {
        if matches!(
            comp,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(CoreError::InvalidArgument(format!(
                "path '{raw_path}' contains forbidden traversal component"
            )));
        }
    }

    let clean_rel = validate_repo_rel_path(&candidate_rel)?;
    let mut current = repo_root.to_path_buf();

    for seg in clean_rel.split('/') {
        current.push(seg);

        if let Ok(meta) = fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(CoreError::InvalidArgument(format!(
                    "refusing to traverse or overwrite symbolic link at '{}'",
                    current.display()
                )));
            }
        }
    }

    Ok((clean_rel, current))
}

/// Validates that an agent/session identifier contains only safe characters (`[A-Za-z0-9._-]`)
/// and cannot traverse directories or inject CLI flags.
///
/// # Errors
/// Returns [`CoreError::InvalidArgument`] if `id` is empty, exceeds 128 bytes, starts with `-` or `.`,
/// or contains disallowed characters.
pub fn validate_safe_identifier(kind: &str, id: &str) -> CoreResult<()> {
    if id.is_empty()
        || id.len() > 128
        || id.starts_with('-')
        || id.starts_with('.')
        || id.contains("..")
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(CoreError::InvalidArgument(format!(
            "invalid {kind} identifier '{id}'"
        )));
    }

    Ok(())
}

/// Validates a Git branch, remote, or reference name against `git check-ref-format` rules,
/// shell metacharacter injection, and leading `-` option injection.
///
/// # Errors
/// Returns [`CoreError::InvalidArgument`] if `name` violates ref naming invariants.
pub fn validate_ref_name(name: &str) -> CoreResult<()> {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.contains("..")
        || name.contains("@{")
        || name.contains("//")
        || name.contains('\\')
    {
        return Err(CoreError::InvalidArgument(format!(
            "invalid git reference or branch name '{name}'"
        )));
    }

    for byte in name.bytes() {
        if byte <= 0x20
            || byte == 0x7f
            || matches!(
                byte,
                b'~'
                    | b'^'
                    | b':'
                    | b'?'
                    | b'*'
                    | b'['
                    | b';'
                    | b'&'
                    | b'|'
                    | b'$'
                    | b'`'
                    | b'\''
                    | b'"'
                    | b'('
                    | b')'
                    | b'<'
                    | b'>'
                    | b'!'
                    | b'{'
                    | b'}'
            )
        {
            return Err(CoreError::InvalidArgument(format!(
                "invalid character in git reference or branch name '{name}'"
            )));
        }
    }

    for seg in name.split('/') {
        if seg.is_empty()
            || seg.starts_with('.')
            || seg.ends_with(".lock")
            || seg.eq_ignore_ascii_case(".git")
            || seg.eq_ignore_ascii_case(".tessera")
        {
            return Err(CoreError::InvalidArgument(format!(
                "invalid path component '{seg}' in git reference '{name}'"
            )));
        }
    }

    Ok(())
}

/// Validates that a content-addressed hexadecimal identifier (`OpId` or `ConflictId`)
/// contains strictly ASCII hexadecimal characters (`[0-9a-fA-F]{1,64}`).
///
/// # Errors
/// Returns [`CoreError::InvalidArgument`] if `id` is empty, exceeds 64 chars, or contains non-hex characters.
pub fn validate_hex_id(kind: &str, id: &str) -> CoreResult<()> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CoreError::InvalidArgument(format!(
            "invalid hexadecimal {kind} '{id}'"
        )));
    }

    Ok(())
}

/// Returns the current Unix epoch timestamp in milliseconds.
///
/// # Panics
/// Never panics; falls back to `0` if the system clock is set prior to `UNIX_EPOCH`.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A 20-byte (40-hex-character) SHA-1 Git object identifier (blob, tree, or commit),
/// stored inline on the stack with zero heap allocations.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId([u8; 40]);

impl Serialize for ObjectId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

impl ObjectId {
    /// Creates an [`ObjectId`] from a 40-character lowercase hexadecimal SHA-1 string.
    ///
    /// # Preconditions
    /// `hex_str` must be exactly 40 ASCII hexadecimal characters.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `hex_str` is not a valid 40-char hex digest.
    pub fn from_hex(hex_str: &str) -> CoreResult<Self> {
        let trimmed = hex_str.trim();

        if trimmed.len() != 40 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CoreError::InvalidArgument(format!(
                "expected 40-char hex SHA-1 ObjectId, got '{hex_str}'"
            )));
        }

        let mut buf = [0u8; 40];
        for (dst, src) in buf.iter_mut().zip(trimmed.bytes()) {
            *dst = src.to_ascii_lowercase();
        }

        Ok(Self(buf))
    }

    /// Creates an [`ObjectId`] from a raw 20-byte SHA-1 digest.
    pub fn from_bytes(bytes: &[u8; 20]) -> Self {
        const HEX_LUT: &[u8; 16] = b"0123456789abcdef";
        let mut buf = [0u8; 40];

        for (i, &b) in bytes.iter().enumerate() {
            buf[i * 2] = HEX_LUT[(b >> 4) as usize];
            buf[i * 2 + 1] = HEX_LUT[(b & 0x0f) as usize];
        }

        Self(buf)
    }

    /// Decodes this [`ObjectId`] into its raw 20-byte SHA-1 representation.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidGitObject`] if the stored hex string cannot be decoded.
    pub fn to_bytes(&self) -> CoreResult<[u8; 20]> {
        let mut out = [0u8; 20];
        let hex_err = || CoreError::InvalidGitObject {
            id: self.as_str().to_string(),
            reason: "invalid hex digit".to_string(),
        };

        for (i, slot) in out.iter_mut().enumerate() {
            let hi = (self.0[i * 2] as char).to_digit(16).ok_or_else(hex_err)? as u8;
            let lo = (self.0[i * 2 + 1] as char)
                .to_digit(16)
                .ok_or_else(hex_err)? as u8;
            *slot = (hi << 4) | lo;
        }

        Ok(out)
    }

    /// Returns the 40-character hexadecimal string slice.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("")
    }

    /// Returns a short 8-character prefix for human-readable output.
    pub fn short(&self) -> &str {
        &self.as_str()[..8]
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.as_str())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable Change Identifier encoded using reverse-hex (`k` through `z`) so it never
/// visually collides with Git hexadecimal SHA prefixes (`0`-`9`, `a`-`f`).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChangeId(String);

impl ChangeId {
    /// Deterministically derives a 24-character reverse-hex [`ChangeId`] from a seed string.
    pub fn from_seed(seed: &str) -> Self {
        let hash = blake3::hash(seed.as_bytes());
        let bytes = hash.as_bytes();
        let mut out = String::with_capacity(24);

        for &byte in &bytes[..12] {
            let hi = (byte >> 4) & 0x0f;
            let lo = byte & 0x0f;
            out.push(char::from(b'z' - hi));
            out.push(char::from(b'z' - lo));
        }

        Self(out)
    }

    /// Wraps an existing reverse-hex string as a [`ChangeId`].
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if the string is empty or contains non-reverse-hex characters.
    pub fn from_string(raw: String) -> CoreResult<Self> {
        if raw.is_empty() || !raw.bytes().all(|b| (b'k'..=b'z').contains(&b)) {
            return Err(CoreError::InvalidArgument(format!(
                "invalid reverse-hex ChangeId '{raw}' (expected characters in 'k'..='z')"
            )));
        }

        Ok(Self(raw))
    }

    /// Returns the full reverse-hex string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns an 8-character short prefix for CLI display.
    pub fn short(&self) -> &str {
        &self.0[..8.min(self.0.len())]
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({})", self.0)
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content-addressed 64-character Blake3 Operation Identifier in the Lock-Free `OpLog`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OpId(pub String);

impl OpId {
    /// Returns the full hex string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns a 12-character short prefix for display.
    pub fn short(&self) -> &str {
        &self.0[..12.min(self.0.len())]
    }
}

impl fmt::Debug for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OpId({})", self.0)
    }
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Content-addressed 64-character Blake3 Conflict Identifier.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConflictId(pub String);

impl ConflictId {
    /// Returns an 8-character short prefix for display.
    pub fn short(&self) -> &str {
        &self.0[..8.min(self.0.len())]
    }
}

impl fmt::Debug for ConflictId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConflictId({})", self.0)
    }
}

impl fmt::Display for ConflictId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// First-class algebraic conflict container representing the alternating sum:
/// `Value = adds[0] - removes[0] + adds[1] - removes[1] + ... + adds[N]`.
///
/// # Invariants
/// `adds.len() == removes.len() + 1` is maintained across all constructors and transformations.
///
/// ```text
///   Base (removes[0])
///    /            \
/// Left (adds[0])  Right (adds[1])
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Merge<T> {
    /// Base versions subtracted in the algebraic sum (`len == N`).
    pub removes: Vec<T>,
    /// Side versions added in the algebraic sum (`len == N + 1`).
    pub adds: Vec<T>,
}

impl<T: Clone + PartialEq> Merge<T> {
    /// Creates a resolved [`Merge`] containing a single non-conflicting value.
    pub fn resolved(val: T) -> Self {
        Self {
            removes: Vec::new(),
            adds: vec![val],
        }
    }

    /// Creates a 3-way [`Merge`] from `base`, `left`, and `right` values and immediately
    /// applies algebraic cancellation (`simplify`).
    pub fn from_3way(base: T, left: T, right: T) -> Self {
        let mut m = Self {
            removes: vec![base],
            adds: vec![left, right],
        };
        m.simplify();
        m
    }

    /// Returns `true` if this [`Merge`] has simplified to a single resolved value.
    pub fn is_resolved(&self) -> bool {
        self.removes.is_empty() && self.adds.len() == 1
    }

    /// Returns `true` if this [`Merge`] represents an unresolved conflict (`adds.len() > 1`).
    pub fn is_conflict(&self) -> bool {
        !self.is_resolved()
    }

    /// Returns `Some(&T)` if resolved, or `None` if conflicted.
    pub fn as_resolved(&self) -> Option<&T> {
        if self.is_resolved() {
            self.adds.first()
        } else {
            None
        }
    }

    /// Simplifies the algebraic conflict by canceling matching `(remove, add)` pairs
    /// and deduplicating identical `add` sides when both sides made the exact same change.
    pub fn simplify(&mut self) {
        let mut r_idx = 0;

        while r_idx < self.removes.len() {
            if let Some(a_idx) = self.adds.iter().position(|a| *a == self.removes[r_idx]) {
                self.removes.remove(r_idx);
                self.adds.remove(a_idx);
            } else {
                r_idx += 1;
            }
        }

        if self.adds.len() > 1 && self.adds.windows(2).all(|w| w[0] == w[1]) {
            self.adds.truncate(1);
            self.removes.clear();
        }
    }
}

/// Git tree entry file mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EntryMode {
    /// Standard non-executable file (`100644`).
    NormalFile,
    /// Executable file (`100755`).
    ExecutableFile,
    /// Symbolic link (`120000`).
    Symlink,
    /// Subdirectory tree (`040000`).
    Directory,
}

impl EntryMode {
    /// Returns the octal mode string used in native Git tree objects.
    pub fn as_git_octal(self) -> &'static str {
        match self {
            Self::NormalFile => "100644",
            Self::ExecutableFile => "100755",
            Self::Symlink => "120000",
            Self::Directory => "40000",
        }
    }

    /// Parses a Git tree octal mode string into an [`EntryMode`].
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidGitObject`] if `mode` is not a recognized Git tree mode.
    pub fn from_git_octal(mode: &str) -> CoreResult<Self> {
        match mode {
            "100644" | "0100644" => Ok(Self::NormalFile),
            "100755" | "0100755" => Ok(Self::ExecutableFile),
            "120000" | "0120000" => Ok(Self::Symlink),
            "40000" | "040000" => Ok(Self::Directory),
            other => Err(CoreError::InvalidGitObject {
                id: String::new(),
                reason: format!("unrecognized git tree mode '{other}'"),
            }),
        }
    }
}

/// Flat representation of a file blob in a materialized tree map.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileEntry {
    /// SHA-1 Git blob ID.
    pub blob_id: ObjectId,
    /// Executable bit (`100755` vs `100644`).
    pub executable: bool,
}

/// Classification of a first-class conflict in `.git/tessera/conflicts/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictKind {
    /// Overlapping modifications to the same AST declaration or statement.
    AstNode,
    /// Overlapping textual hunks in a non-AST file.
    TextHunk,
    /// One side deleted a file while the other side modified it (and no rename match was found).
    DeleteModify,
    /// Merge succeeded structurally, but the speculative verification gate (`--verify`) failed compilation.
    SemanticBuild,
}

/// First-class conflict record persisted in `.git/tessera/conflicts/<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRecord {
    /// Deterministic Blake3 identifier of this conflict.
    pub id: ConflictId,
    /// Repository-relative path of the conflicted file (or `<build>` for `SemanticBuild`).
    pub path: String,
    /// Scoped AST symbol name (e.g., `fn verify_jwt` or `impl AuthService`) if applicable.
    pub symbol: Option<String>,
    /// Classification of the conflict.
    pub kind: ConflictKind,
    /// Algebraic `Merge<Option<ObjectId>>` recording base (`removes`) and competing (`adds`) blob IDs.
    pub merge: Merge<Option<ObjectId>>,
    /// Map of `agent_id -> intent` for all agents involved in this conflict.
    pub agent_intents: BTreeMap<String, String>,
    /// Base text snippet for structured JSON inspection by AI agents.
    pub base_snippet: String,
    /// Left (ours / first agent) text snippet.
    pub left_snippet: String,
    /// Right (theirs / second agent) text snippet.
    pub right_snippet: String,
    /// Optional compiler/test diagnostics when `kind == ConflictKind::SemanticBuild`.
    pub diagnostics: Option<String>,
    /// Timestamp when the conflict was recorded.
    pub created_at_ms: u64,
}

impl ConflictRecord {
    /// Computes a deterministic [`ConflictId`] from the path, symbol, and participating blob IDs.
    pub fn compute_id(
        path: &str,
        symbol: Option<&str>,
        merge: &Merge<Option<ObjectId>>,
    ) -> ConflictId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(path.as_bytes());

        if let Some(sym) = symbol {
            hasher.update(b"::");
            hasher.update(sym.as_bytes());
        }

        for r in &merge.removes {
            hasher.update(b"-");
            if let Some(id) = r {
                hasher.update(id.as_str().as_bytes());
            }
        }

        for a in &merge.adds {
            hasher.update(b"+");
            if let Some(id) = a {
                hasher.update(id.as_str().as_bytes());
            }
        }

        ConflictId(hasher.finalize().to_hex().to_string())
    }
}

/// Navigation waypoint recording the originating AI harness, terminal emulator, TTY device,
/// and process hierarchy for an active session so the IDE extension or `tes session focus`
/// can jump directly to the session's live terminal tab or harness window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWaypoint {
    /// Harness or environment kind (`"antigravity"`, `"claude-code"`, `"cursor"`, `"windsurf"`,
    /// `"kiro"`, `"opencode"`, `"codex"`, `"copilot"`, `"vscode"`, or `"terminal"`).
    pub harness: String,
    /// Full untruncated conversation or session identifier from the harness environment
    /// (`ANTIGRAVITY_CONVERSATION_ID`, `CLAUDE_SESSION_ID`, `CURSOR_SESSION_ID`, etc.).
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Terminal emulator program (`Apple_Terminal`, `iTerm.app`, `vscode`, `WezTerm`, `ghostty`,
    /// `kitty`, `Alacritty`, `tmux`, `zellij`, `WindowsTerminal`).
    #[serde(default)]
    pub term_program: Option<String>,
    /// Terminal-specific session or pane identifier (`TERM_SESSION_ID`, `ITERM_SESSION_ID`,
    /// `TMUX_PANE`, `ZELLIJ_PANE_ID`, `WEZTERM_PANE`, `KITTY_WINDOW_ID`, `WT_SESSION`).
    #[serde(default)]
    pub term_session_id: Option<String>,
    /// Controlling POSIX TTY device path (e.g., `/dev/ttys014`).
    #[serde(default)]
    pub tty: Option<String>,
    /// Stable ancestor shell or harness process ID (`find_stable_session_ancestor_pid`).
    #[serde(default)]
    pub shell_pid: Option<u32>,
    /// Host IDE process ID when running inside an editor (`VSCODE_PID`).
    #[serde(default)]
    pub ide_pid: Option<u32>,
}

/// State and provenance metadata for an active AI agent session (either auto-detected via
/// Zero-Command Auto-Pilot or explicitly spawned via `tes agent spawn`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMetadata {
    /// Unique identifier for the session/agent (e.g., `antigravity-d19f7d`, `claude-8a2b`, or `auth-agent`).
    pub agent_id: String,
    /// Deterministic integer slot (`1..=N`) used for `PORT` (`3000 + slot * 100`), `DATABASE_URL`, and `TMPDIR` isolation.
    pub slot: u16,
    /// Stable reverse-hex Change-ID tracking this agent's logical change across auto-rebases.
    pub change_id: ChangeId,
    /// Base trunk commit ID from which this agent's change branched.
    pub base_commit: ObjectId,
    /// Current head commit ID of this agent's working snapshot.
    pub head_commit: ObjectId,
    /// Current root Git tree ID of this agent's working snapshot.
    pub head_tree: ObjectId,
    /// Natural-language intent captured automatically from prompt hooks or synthesized from AST diffs.
    pub intent: String,
    /// `true` if `intent` was auto-synthesized from AST diffs rather than an explicit prompt hook.
    pub intent_inferred: bool,
    /// AST symbols or file contracts touched or claimed by this agent (used by `tes radar`).
    pub claimed_symbols: BTreeSet<String>,
    /// Optional directory prefixes (`--scope`) materialized for this workspace.
    pub scopes: Vec<String>,
    /// Optional isolated Copy-on-Write workspace path (`.tessera/workspaces/<agent_id>`).
    /// `None` when operating in-place in the main repository root via Auto-Pilot hooks.
    pub workspace_path: Option<String>,
    /// Per-file baseline `BlobId` last read or written by this specific session in Auto-Pilot mode.
    /// Enables accurate 3-way AST reconciliation when two sessions edit the same file in the shared root.
    pub read_blobs: BTreeMap<String, ObjectId>,
    /// Per-file unmerged `BlobId` authored specifically by this session (prior to cross-session shared-root merge),
    /// allowing sessions on distinct branches to commit only their own edits.
    #[serde(default)]
    pub own_blobs: BTreeMap<String, ObjectId>,
    /// First-class deletion tombstones authored by this session: repository-relative paths that exist in
    /// `base_commit` but were explicitly deleted (or moved away) by this session.
    ///
    /// Invariant: a path is never present in both `own_blobs` and `deleted_paths`. Every tree rebuilt from
    /// `base_commit + own_blobs` must also remove every path in `deleted_paths`, otherwise the deletion
    /// would be silently resurrected in `head_tree`, the branch tip, or `repo_root`.
    #[serde(default)]
    pub deleted_paths: BTreeSet<String>,
    /// Repository-relative paths that already held uncommitted changes owned by no session (typically a
    /// human's in-progress edits) when this AI session started or claimed its work.
    ///
    /// Snapshots of this session never attribute these paths to it unless the session explicitly
    /// authors them (via `post-tool`, which records them in `own_blobs` or `deleted_paths`), so a
    /// user's pre-existing edits are never committed under an agent's name.
    #[serde(default)]
    pub foreign_baseline: BTreeSet<String>,
    /// Per repository-relative path, the own blob of each same-branch peer session (`peer_id ->
    /// blob`) that this session's last clean `post-tool` reconcile already merged into its file.
    ///
    /// A peer whose blob is unchanged since then contributes nothing new when the peer's edits are
    /// already present in `own_blobs`, and a peer that changed contributes only its delta from the
    /// recorded blob. Without this, a stale or idle peer's older blob looks like an edit that
    /// reverts this session's earlier hooked changes whenever the path has no entry in the base
    /// tree to anchor the 3-way merge.
    #[serde(default)]
    pub integrated_peer_blobs: BTreeMap<String, BTreeMap<String, ObjectId>>,
    /// Optional target branch associated with this session (e.g., set via `tes claim --branch feat/x`).
    ///
    /// `None` means "follow the currently checked-out `HEAD` branch". Callers must compare branches via
    /// [`AgentMetadata::effective_branch`] so `None` and `Some(current_head_branch)` behave identically.
    #[serde(default)]
    pub target_branch: Option<String>,
    /// Named operation checkpoints (`label -> OpId`) created via `tes checkpoint` or MCP.
    pub checkpoints: BTreeMap<String, OpId>,
    /// Optional navigation waypoint linking this session to its originating terminal tab or AI IDE harness.
    #[serde(default)]
    pub waypoint: Option<SessionWaypoint>,
    /// Last activity timestamp in Unix epoch milliseconds.
    pub updated_at_ms: u64,
}

impl AgentMetadata {
    /// Returns the branch this session effectively targets: its explicit `target_branch`, or
    /// `head_branch` (the branch checked out in `repo_root`) when `target_branch` is `None`.
    pub fn effective_branch<'a>(&'a self, head_branch: &'a str) -> &'a str {
        self.target_branch.as_deref().unwrap_or(head_branch)
    }

    /// Returns `true` if this session targets `head_branch`, treating `target_branch == None`
    /// and `target_branch == Some(head_branch)` identically.
    pub fn targets_head_branch(&self, head_branch: &str) -> bool {
        self.effective_branch(head_branch) == head_branch
    }

    /// Returns `true` if this session holds any uncommitted authored change (an edited/added blob
    /// in `own_blobs` or a deletion tombstone in `deleted_paths`).
    pub fn has_own_changes(&self) -> bool {
        !self.own_blobs.is_empty() || !self.deleted_paths.is_empty()
    }

    /// Returns `true` if `rel_path` is authored by this session, either as an edit or a deletion.
    pub fn owns_path(&self, rel_path: &str) -> bool {
        self.own_blobs.contains_key(rel_path) || self.deleted_paths.contains(rel_path)
    }

    /// Iterates every repository-relative path authored by this session (edits and deletions).
    pub fn owned_paths(&self) -> impl Iterator<Item = &String> {
        self.own_blobs.keys().chain(self.deleted_paths.iter())
    }

    /// Applies this session's authored changes (`own_blobs` edits and `deleted_paths` tombstones)
    /// onto `tree`. Executable bits for edited paths are taken from `mode_source` first, then from
    /// the existing `tree` entry, defaulting to non-executable.
    pub fn apply_own_changes(
        &self,
        tree: &mut BTreeMap<String, FileEntry>,
        mode_source: &BTreeMap<String, FileEntry>,
    ) {
        for (rel_path, blob_id) in &self.own_blobs {
            let executable = mode_source
                .get(rel_path)
                .or_else(|| tree.get(rel_path))
                .is_some_and(|e| e.executable);
            tree.insert(
                rel_path.clone(),
                FileEntry {
                    blob_id: *blob_id,
                    executable,
                },
            );
        }

        for rel_path in &self.deleted_paths {
            tree.remove(rel_path);
        }
    }
}

/// A session-isolated or branch-scoped stash entry stored in the lock-free `OpLog`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashEntry {
    /// Deterministic stash identifier (e.g., `stash-1729200000-1`).
    pub id: String,
    /// Session or agent identifier that created this stash.
    pub session_id: String,
    /// Branch name on which the changes were stashed.
    pub branch: String,
    /// Base commit [`ObjectId`] of the session when the stash was created.
    pub base_commit: ObjectId,
    /// Human-readable stash description.
    pub message: String,
    /// Map of repository-relative file paths to their stashed blob [`ObjectId`]s.
    pub own_blobs: BTreeMap<String, ObjectId>,
    /// Repository-relative paths deleted by the session at stash time (restored as tombstones on pop).
    #[serde(default)]
    pub deleted_paths: BTreeSet<String>,
    /// Repository-relative paths in `own_blobs` that had the executable (`0o755`) bit set at stash
    /// time. Every other path in `own_blobs` is restored as non-executable. `None` marks a stash
    /// written before modes were recorded, whose files keep the target branch's mode on pop.
    #[serde(default)]
    pub executable_paths: Option<BTreeSet<String>>,
    /// Claimed AST symbols associated with the stashed changes.
    #[serde(default)]
    pub claimed_symbols: BTreeSet<String>,
    /// Timestamp when the stash was recorded in Unix epoch milliseconds.
    pub created_at_ms: u64,
}

/// Repository-wide configuration stored at `.git/tessera/config.toml` (and optionally overlaid by `tessera.toml` in the repository root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoConfig {
    /// Primary trunk branch name (defaults to `"main"`).
    pub trunk_branch: String,
    /// Base port for deterministic agent slot offsets (`PORT = base_port + slot * port_stride`).
    pub base_port: u16,
    /// Port increment per session/branch slot (defaults to `100`).
    pub port_stride: u16,
    /// Untracked dependency and build cache directories to Copy-on-Write clone and preserve across all languages.
    pub cow_cache_dirs: Vec<String>,
    /// Relative binary directories inside a workspace to automatically prepend to `PATH` during `tes exec`.
    pub extra_bin_dirs: Vec<String>,
    /// Optional workspace or package glob patterns (`crates/*`, `packages/*`, `apps/*`).
    pub package_globs: Vec<String>,
    /// Default speculative verification command for `tes merge --verify`.
    pub verify_command: Option<String>,
    /// Polling interval in milliseconds for the automatic background write watcher (defaults to `75`).
    pub watch_interval_ms: u64,
    /// Idle timeout in seconds for the automatic background write watcher (defaults to `90`).
    pub watch_timeout_secs: u64,
    /// Whether commits created by Tessera automatically include a `Co-authored-by` trailer (defaults to `false`).
    pub co_author: bool,
    /// Identity formatted in the `Co-authored-by:` commit trailer when `co_author` is enabled (defaults to `"Tessera <noreply@tessera.getlunaris.dev>"`).
    pub co_author_trailer: String,
    /// Optional custom pager command for interactive CLI output (`tes diff`, `tes log`, `tes oplog`, `tes branch`).
    pub pager: Option<String>,
    /// Custom environment variable templates injected into `tes exec` (supports `{slot}`, `{port}`, `{branch}`, `{agent_id}` placeholders).
    pub custom_env: BTreeMap<String, String>,
    /// When `true` (default), `tes claim` without `--branch` binds a fresh AI session to its own branch
    /// (`agent/<session_id>`) if any claimed path already holds uncommitted changes from the user or a peer
    /// session on the checked-out branch, instead of mixing its edits into that in-progress work.
    pub auto_isolate_dirty_claims: bool,
    /// When `true`, `tes env` and `tes exec` opt into polyglot port isolation (`PORT`, framework port
    /// environment variables, and CLI port-wrapper shims in `TES_SHIM_BIN`). Defaults to `false`.
    #[serde(default)]
    pub port_isolation: bool,
}

impl RepoConfig {
    /// Returns `true` when polyglot port isolation is enabled via [`Self::port_isolation`] or
    /// `TES_PORT_ISOLATION=1` in the environment.
    #[must_use]
    pub fn is_port_isolation_enabled(&self) -> bool {
        self.port_isolation
            || std::env::var("TES_PORT_ISOLATION").is_ok_and(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
    }
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            trunk_branch: "main".to_string(),
            base_port: 3000,
            port_stride: 100,
            cow_cache_dirs: vec![
                "target".to_string(),
                "node_modules".to_string(),
                ".pnpm-store".to_string(),
                ".turbo".to_string(),
                ".next".to_string(),
                ".nuxt".to_string(),
                ".svelte-kit".to_string(),
                "dist".to_string(),
                "build".to_string(),
                ".venv".to_string(),
                "venv".to_string(),
                "__pycache__".to_string(),
                ".pytest_cache".to_string(),
                ".mypy_cache".to_string(),
                ".ruff_cache".to_string(),
                "vendor".to_string(),
                "_build".to_string(),
                "deps".to_string(),
                ".gradle".to_string(),
                ".build".to_string(),
                ".zig-cache".to_string(),
                "zig-out".to_string(),
                ".dart_tool".to_string(),
                "cmake-build-debug".to_string(),
                "cmake-build-release".to_string(),
            ],
            extra_bin_dirs: vec![
                ".venv/bin".to_string(),
                "venv/bin".to_string(),
                "node_modules/.bin".to_string(),
                "vendor/bin".to_string(),
                "bin".to_string(),
                ".bin".to_string(),
            ],
            package_globs: vec![
                "crates/*".to_string(),
                "packages/*".to_string(),
                "apps/*".to_string(),
                "services/*".to_string(),
            ],
            verify_command: None,
            watch_interval_ms: 75,
            watch_timeout_secs: 90,
            co_author: false,
            co_author_trailer: "Tessera <noreply@tessera.getlunaris.dev>".to_string(),
            pager: None,
            custom_env: BTreeMap::new(),
            auto_isolate_dirty_claims: true,
            port_isolation: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_3way_simplifies_when_one_side_unchanged() {
        let m = Merge::from_3way("base", "base", "right_modified");
        assert!(m.is_resolved());
        assert_eq!(m.as_resolved(), Some(&"right_modified"));
    }

    #[test]
    fn merge_3way_simplifies_when_both_sides_make_identical_edit() {
        let m = Merge::from_3way("base", "same_edit", "same_edit");
        assert!(m.is_resolved());
        assert_eq!(m.as_resolved(), Some(&"same_edit"));
    }

    #[test]
    fn merge_3way_retains_conflict_on_divergent_edits() {
        let m = Merge::from_3way("base", "left_edit", "right_edit");
        assert!(m.is_conflict());
        assert_eq!(m.as_resolved(), None);
        assert_eq!(m.removes, vec!["base"]);
        assert_eq!(m.adds, vec!["left_edit", "right_edit"]);
    }

    #[test]
    fn change_id_uses_strictly_reverse_hex_alphabet() {
        let cid = ChangeId::from_seed("agent-session-alpha-12345");
        assert_eq!(cid.as_str().len(), 24);
        assert!(cid.as_str().bytes().all(|b| (b'k'..=b'z').contains(&b)));
    }

    #[test]
    fn validate_repo_rel_path_rejects_traversal_and_git_dir() {
        assert_eq!(
            validate_repo_rel_path("./src/lib.rs").expect("valid"),
            "src/lib.rs"
        );
        assert!(validate_repo_rel_path("../outside.rs").is_err());
        assert!(validate_repo_rel_path("src/../../outside.rs").is_err());
        assert!(validate_repo_rel_path(".git/config").is_err());
        assert!(validate_repo_rel_path("sub/.GIT/hooks/pre-commit").is_err());
        assert!(validate_repo_rel_path("/etc/passwd").is_err());
    }

    #[test]
    fn validate_ref_name_and_identifiers_reject_injection_and_traversal() {
        assert!(validate_ref_name("feat/auth-flow").is_ok());
        assert!(validate_ref_name("--upload-pack=sh").is_err());
        assert!(validate_ref_name("../HEAD").is_err());
        assert!(validate_ref_name("refs/heads/main.lock").is_err());

        assert!(validate_safe_identifier("session", "antigravity-123_a.b").is_ok());
        assert!(validate_safe_identifier("session", "../escape").is_err());
        assert!(validate_safe_identifier("session", "-flag").is_err());
    }
}
