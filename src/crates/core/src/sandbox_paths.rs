//! Canonical external sandbox root resolution, temporary-repository isolation, and OS cache
//! hygiene markers (`CACHEDIR.TAG`, macOS Spotlight `.metadata_never_index`, and Time Machine
//! exclusion xattrs).

#[cfg(any(unix, test))]
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::atomic_write_file_relaxed;

/// Environment variable allowing callers and tests to override the base directory where external
/// Tessera branch sandboxes are stored.
pub const WORKSPACES_ROOT_ENV: &str = "TES_WORKSPACES_ROOT";

/// Standard `CACHEDIR.TAG` header defined by <https://bford.info/cachedir/>.
pub const CACHEDIR_TAG_CONTENT: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by Tessera.\n# For information about cache directory tags, see:\n#   https://bford.info/cachedir/\n";

/// macOS extended attribute name marking a directory as excluded from Time Machine backups.
#[cfg(target_os = "macos")]
const MACOS_TM_EXCLUDE_ATTR: &str = "com.apple.metadata:com_apple_backup_excludeItem";

/// Standard Apple binary plist payload encoding `"com.apple.backupd"` used by `tmutil addexclusion`.
#[cfg(target_os = "macos")]
const MACOS_TM_EXCLUDE_PLIST: &[u8] = &[
    98, 112, 108, 105, 115, 116, 48, 48, 95, 16, 17, 99, 111, 109, 46, 97, 112, 112, 108, 101, 46,
    98, 97, 99, 107, 117, 112, 100, 8, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 28,
];

/// Returns the effective Unix user ID (`geteuid`), or `0` on non-Unix platforms.
#[must_use]
fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: `libc::geteuid()` is a pure POSIX syscall taking no pointers or arguments.
        unsafe { libc::geteuid() }
    }

    #[cfg(not(unix))]
    {
        0
    }
}

/// Computes the deterministic external sandbox directory slug (`<sanitized_name>-<32_hex_blake3>`)
/// for `repo_root`.
#[must_use]
pub fn compute_repo_slug(repo_root: &Path) -> String {
    let repo_name = repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    let full_hash = blake3::hash(repo_root.display().to_string().as_bytes()).to_hex();

    format!("{repo_name}-{}", &full_hash[..32])
}

/// Returns `true` if `path` equals or resides inside `prefix`, comparing both lexical paths and
/// canonicalized forms so symlinked OS roots (`/var` vs `/private/var`) and Windows verbatim
/// prefixes match reliably.
#[must_use]
pub fn path_is_within(path: &Path, prefix: &Path) -> bool {
    if path.starts_with(prefix) {
        return true;
    }

    if let (Ok(canon_path), Ok(canon_prefix)) = (path.canonicalize(), prefix.canonicalize()) {
        return canon_path.starts_with(&canon_prefix);
    }

    false
}

/// Returns `true` if `a` and `b` represent the same filesystem location lexically or canonically.
#[must_use]
fn paths_equal_canonical(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }

    if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
        return ca == cb;
    }

    false
}

/// Collects OS temporary root directories (`std::env::temp_dir()` plus `/tmp` and `/var/tmp` on Unix).
fn os_temp_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let std_tmp = std::env::temp_dir();

    if !std_tmp.as_os_str().is_empty() {
        roots.push(std_tmp.clone());

        if let Ok(canon) = std_tmp.canonicalize() {
            if !roots.contains(&canon) {
                roots.push(canon);
            }
        }
    }

    #[cfg(unix)]
    for extra in ["/tmp", "/private/tmp", "/var/tmp", "/private/var/tmp"] {
        let p = PathBuf::from(extra);

        if !roots.contains(&p) {
            roots.push(p);
        }
    }

    roots
}

/// Returns `true` when `path` resides strictly inside an OS temporary directory (`std::env::temp_dir`,
/// `/tmp`, or `/var/tmp`).
///
/// Used to keep ephemeral test repositories (`tempfile::tempdir()`, `mktemp -d`) self-contained
/// inside the temp hierarchy instead of leaking directories into the user's persistent cache root.
#[must_use]
pub fn is_path_inside_temp_dir(path: &Path) -> bool {
    os_temp_roots()
        .iter()
        .any(|root| path_is_within(path, root) && !paths_equal_canonical(path, root))
}

/// Returns the validated path from `TES_WORKSPACES_ROOT` if set to a non-empty absolute path
/// owned by the current user (and not a symlink when already present).
#[must_use]
pub fn configured_workspaces_root() -> Option<PathBuf> {
    let raw = std::env::var(WORKSPACES_ROOT_ENV).ok()?;
    let trimmed = raw.trim();

    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }

    let path = PathBuf::from(trimmed);

    if !path.is_absolute() {
        return None;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let uid = current_uid();

        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() || meta.uid() != uid {
                return None;
            }
        }
    }

    Some(path)
}

/// Returns the ordered list of persistent global OS cache bases where Tessera stores external
/// repository sandboxes (`~/Library/Caches/tessera/workspaces` on macOS, `%LOCALAPPDATA%\tessera\workspaces`
/// on Windows, and `$XDG_CACHE_HOME`/`~/.cache`/`/var/tmp` on Linux and BSD).
#[must_use]
pub fn global_workspaces_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    let uid = current_uid();

    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                bases.push(
                    PathBuf::from(home)
                        .join("Library")
                        .join("Caches")
                        .join("tessera")
                        .join("workspaces"),
                );
            }
        }
    } else if cfg!(windows) {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            if !local_app_data.trim().is_empty() {
                bases.push(
                    PathBuf::from(local_app_data)
                        .join("tessera")
                        .join("workspaces"),
                );
            }
        }

        bases.push(std::env::temp_dir().join("tessera-workspaces"));
    } else {
        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            if !xdg.trim().is_empty() {
                bases.push(PathBuf::from(xdg).join("tessera").join("workspaces"));
            }
        }

        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                bases.push(
                    PathBuf::from(home)
                        .join(".cache")
                        .join("tessera")
                        .join("workspaces"),
                );
            }
        }

        bases.push(PathBuf::from("/var/tmp").join(format!("tessera-workspaces-{uid}")));
        bases.push(std::env::temp_dir().join(format!("tessera-workspaces-{uid}")));
    }

    bases
}

/// Returns the primary global workspaces base directory for the active OS, honoring
/// `TES_WORKSPACES_ROOT` if explicitly configured.
#[must_use]
pub fn primary_global_workspaces_base() -> Option<PathBuf> {
    configured_workspaces_root().or_else(|| global_workspaces_bases().into_iter().next())
}

/// Computes the isolated temp-contained sandbox base for a repository living inside an OS temp
/// directory.
///
/// - If `repo_root`'s parent is itself a subdirectory inside the temp root (e.g. `WORK_ROOT/project`),
///   returns `<parent>/tessera-workspaces` so deleting `WORK_ROOT` cleans up all sandboxes.
/// - If `repo_root` is directly a child of the OS temp root (e.g. `tempfile::tempdir()`), returns
///   `<repo_root>/.tessera/sandboxes` so dropping the `TempDir` removes the sandboxes automatically
///   without leaking into `$HOME/Library/Caches` or the shared OS temp root.
#[must_use]
fn temp_contained_workspaces_base(repo_root: &Path) -> Option<PathBuf> {
    if !is_path_inside_temp_dir(repo_root) {
        return None;
    }

    if let Some(parent) = repo_root.parent() {
        if is_path_inside_temp_dir(parent) {
            return Some(parent.join("tessera-workspaces"));
        }
    }

    Some(repo_root.join(".tessera").join("sandboxes"))
}

/// Returns ordered candidate base directories for storing `repo_root`'s external sandbox slug.
///
/// Priority order:
/// 1. Explicit `TES_WORKSPACES_ROOT` override (when set and valid).
/// 2. Temp-contained base when `repo_root` resides inside an OS temp directory.
/// 3. Standard OS cache directories (`~/Library/Caches/tessera/workspaces`, etc.).
/// 4. Sibling `<repo_parent>/tessera-workspaces` fallback.
#[must_use]
pub fn candidate_workspaces_bases(repo_root: &Path) -> Vec<PathBuf> {
    let mut bases = Vec::new();

    if let Some(override_base) = configured_workspaces_root() {
        bases.push(override_base);
    }

    if let Some(temp_base) = temp_contained_workspaces_base(repo_root) {
        if !bases.contains(&temp_base) {
            bases.push(temp_base);
        }
    }

    for global_base in global_workspaces_bases() {
        if !bases.contains(&global_base) {
            bases.push(global_base);
        }
    }

    if let Some(parent) = repo_root.parent() {
        let sibling = parent.join("tessera-workspaces");

        if !bases.contains(&sibling) {
            bases.push(sibling);
        }
    }

    bases
}

/// Returns all authorized parent base directories accepted by [`crate::is_valid_sandbox_repo_root`]
/// for external branch sandboxes belonging to `target_root`.
#[must_use]
pub fn authorized_sandbox_bases(target_root: &Path) -> Vec<PathBuf> {
    let mut allowed = Vec::new();

    let mut push_unique = |p: PathBuf| {
        if !allowed.contains(&p) {
            allowed.push(p);
        }
    };

    if let Some(override_base) = configured_workspaces_root() {
        push_unique(override_base);
    }

    push_unique(target_root.join(".tessera").join("sandboxes"));

    if let Some(parent) = target_root.parent() {
        push_unique(parent.join("tessera-workspaces"));
    }

    if let Ok(canon_root) = target_root.canonicalize() {
        push_unique(canon_root.join(".tessera").join("sandboxes"));

        if let Some(parent) = canon_root.parent() {
            push_unique(parent.join("tessera-workspaces"));
        }
    }

    for global_base in global_workspaces_bases() {
        push_unique(global_base);
    }

    allowed
}

/// Writes OS cache hygiene markers into `workspaces_base` (best-effort, non-fatal):
/// - `CACHEDIR.TAG` on all platforms so backup tools (`tar --exclude-caches`, `restic`, `borg`)
///   skip disposable branch sandboxes.
/// - `.metadata_never_index` on macOS so Spotlight (`mds` / `mdworker`) does not index cloned
///   build trees.
/// - `com.apple.metadata:com_apple_backup_excludeItem` xattr on macOS so Time Machine excludes
///   the cache directory.
pub fn ensure_cache_dir_hygiene(workspaces_base: &Path) {
    if !workspaces_base.is_dir() {
        return;
    }

    let tag_path = workspaces_base.join("CACHEDIR.TAG");

    if !tag_path.is_file() {
        let _ = atomic_write_file_relaxed(&tag_path, CACHEDIR_TAG_CONTENT.as_bytes());
    }

    #[cfg(target_os = "macos")]
    {
        let noindex_path = workspaces_base.join(".metadata_never_index");

        if !noindex_path.is_file() {
            let _ = atomic_write_file_relaxed(&noindex_path, b"");
        }

        apply_macos_timemachine_exclusion(workspaces_base);
    }
}

/// Marks `dir` as excluded from macOS Time Machine backups via `setxattr(2)`.
#[cfg(target_os = "macos")]
fn apply_macos_timemachine_exclusion(dir: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path_c) = CString::new(dir.as_os_str().as_bytes()) else {
        return;
    };
    let Ok(attr_c) = CString::new(MACOS_TM_EXCLUDE_ATTR) else {
        return;
    };

    // SAFETY: `path_c` and `attr_c` are valid NUL-terminated C strings live across the syscall,
    // and `MACOS_TM_EXCLUDE_PLIST` is a valid `'static` byte slice of length `MACOS_TM_EXCLUDE_PLIST.len()`.
    unsafe {
        let _ = libc::setxattr(
            path_c.as_ptr(),
            attr_c.as_ptr(),
            MACOS_TM_EXCLUDE_PLIST.as_ptr().cast::<libc::c_void>(),
            MACOS_TM_EXCLUDE_PLIST.len(),
            0,
            0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_repos_prefer_contained_bases_before_global_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let expected_direct = tmp.path().join(".tessera").join("sandboxes");
        assert_eq!(
            temp_contained_workspaces_base(tmp.path()),
            Some(expected_direct.clone()),
            "repo directly at TempDir root must contain sandboxes inside itself"
        );

        let direct_bases = candidate_workspaces_bases(tmp.path());
        let direct_pos = direct_bases
            .iter()
            .position(|p| p == &expected_direct)
            .expect("contained direct base must appear in candidates");

        for global_base in global_workspaces_bases() {
            if let Some(global_pos) = direct_bases.iter().position(|p| p == &global_base) {
                assert!(
                    direct_pos < global_pos,
                    "temp-contained base must precede global cache base"
                );
            }
        }

        if configured_workspaces_root().is_none() {
            assert_eq!(direct_pos, 0);
        }

        let nested_repo = tmp.path().join("project");
        fs::create_dir_all(&nested_repo).expect("mkdir nested_repo");
        let expected_nested = tmp.path().join("tessera-workspaces");
        assert_eq!(
            temp_contained_workspaces_base(&nested_repo),
            Some(expected_nested.clone()),
            "nested repo inside TempDir must contain sandboxes in parent TempDir"
        );

        let nested_bases = candidate_workspaces_bases(&nested_repo);
        let nested_pos = nested_bases
            .iter()
            .position(|p| p == &expected_nested)
            .expect("contained nested base must appear in candidates");

        for global_base in global_workspaces_bases() {
            if let Some(global_pos) = nested_bases.iter().position(|p| p == &global_base) {
                assert!(
                    nested_pos < global_pos,
                    "nested temp-contained base must precede global cache base"
                );
            }
        }

        if configured_workspaces_root().is_none() {
            assert_eq!(nested_pos, 0);
        }
    }

    #[test]
    fn ensure_cache_dir_hygiene_creates_cachedir_tag_and_platform_markers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        ensure_cache_dir_hygiene(tmp.path());

        let tag_raw = fs::read_to_string(tmp.path().join("CACHEDIR.TAG")).expect("CACHEDIR.TAG");
        assert!(
            tag_raw.starts_with("Signature: 8a477f597d28d172789f06886806bc55"),
            "unexpected CACHEDIR.TAG content: {tag_raw}"
        );

        #[cfg(target_os = "macos")]
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;

            assert!(
                tmp.path().join(".metadata_never_index").is_file(),
                "macOS cache dir must contain .metadata_never_index"
            );

            let path_c = CString::new(tmp.path().as_os_str().as_bytes()).expect("path_c");
            let attr_c = CString::new(MACOS_TM_EXCLUDE_ATTR).expect("attr_c");
            let mut buf = vec![0u8; 128];

            // SAFETY: `path_c` and `attr_c` are valid NUL-terminated strings and `buf` is a
            // valid writable buffer of length 128 bytes.
            let n = unsafe {
                libc::getxattr(
                    path_c.as_ptr(),
                    attr_c.as_ptr(),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                    0,
                    0,
                )
            };
            assert_eq!(n as usize, MACOS_TM_EXCLUDE_PLIST.len());
            assert_eq!(&buf[..MACOS_TM_EXCLUDE_PLIST.len()], MACOS_TM_EXCLUDE_PLIST);
        }
    }

    #[test]
    fn authorized_bases_include_temp_sandboxes_and_sibling_workspaces() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bases = authorized_sandbox_bases(tmp.path());

        assert!(bases.contains(&tmp.path().join(".tessera").join("sandboxes")));

        if let Some(parent) = tmp.path().parent() {
            assert!(bases.contains(&parent.join("tessera-workspaces")));
        }
    }
}
