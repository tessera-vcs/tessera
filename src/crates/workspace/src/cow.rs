//! Copy-on-Write filesystem cloning (`clonefile` on macOS, `FICLONE` on Linux, and portable fallback),
//! build cache path sanitization, and working-tree materialization.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

use core::{
    CoreError, CoreResult, FileEntry, GitBackend, ObjectId, resolve_confined_repo_path,
    resolve_dot_git, validate_repo_rel_path, write_working_tree_file_safe,
};
use walkdir::WalkDir;

use crate::snapshot::{
    ALWAYS_IGNORED_DIRS, IgnoreMatcher, is_vcs_internal_segment, join_rel_segments,
};

/// Maximum symlink resolution depth permitted when verifying confinement inside a workspace root.
#[cfg(unix)]
const MAX_SYMLINK_HOPS: usize = 16;

/// Returns `true` if `link_dest` resolved from `link_path` (including any intermediate symbolic
/// links on disk inside `root_dir`, up to [`MAX_SYMLINK_HOPS`]) stays strictly within `root_dir`.
///
/// Rejects absolute targets, root/prefix components, traversal above `root_dir`, multi-hop
/// escapes (e.g. `escape -> a/b/up/../../../etc/passwd` where `a/b/up -> ../..`), and symlink loops.
#[cfg(unix)]
fn is_safe_symlink_within_root(link_path: &Path, link_dest: &Path, root_dir: &Path) -> bool {
    if link_dest.is_absolute() {
        return false;
    }

    let Some(parent) = link_path.parent() else {
        return false;
    };

    let rel_parent = if let Ok(rel) = parent.strip_prefix(root_dir) {
        rel.to_path_buf()
    } else if let (Ok(canon_root), Ok(canon_parent)) =
        (root_dir.canonicalize(), parent.canonicalize())
    {
        match canon_parent.strip_prefix(&canon_root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => return false,
        }
    } else {
        return false;
    };

    let mut pending: VecDeque<OsString> = VecDeque::new();

    for comp in rel_parent.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => pending.push_back(seg.to_os_string()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }

    for comp in link_dest.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => pending.push_back(seg.to_os_string()),
            Component::ParentDir => pending.push_back(OsString::from("..")),
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }

    let mut cur_rel: Vec<OsString> = Vec::new();
    let mut hops: usize = 0;

    while let Some(seg) = pending.pop_front() {
        if seg == "." {
            continue;
        }

        if seg == ".." {
            if cur_rel.pop().is_none() {
                return false;
            }

            continue;
        }

        cur_rel.push(seg);

        let mut candidate = root_dir.to_path_buf();

        for part in &cur_rel {
            candidate.push(part);
        }

        if let Ok(meta) = fs::symlink_metadata(&candidate) {
            if meta.file_type().is_symlink() {
                hops += 1;

                if hops > MAX_SYMLINK_HOPS {
                    return false;
                }

                let Ok(next_target) = fs::read_link(&candidate) else {
                    return false;
                };

                if next_target.is_absolute() {
                    return false;
                }

                // Symlink resolves relative to its parent directory.
                cur_rel.pop();

                let mut expanded: Vec<OsString> = Vec::new();

                for comp in next_target.components() {
                    match comp {
                        Component::CurDir => {}
                        Component::Normal(s) => expanded.push(s.to_os_string()),
                        Component::ParentDir => expanded.push(OsString::from("..")),
                        Component::RootDir | Component::Prefix(_) => return false,
                    }
                }

                for item in expanded.into_iter().rev() {
                    pending.push_front(item);
                }
            }
        }
    }

    true
}

/// Removes `path`, temporarily adding owner write permission to its parent directory on Unix if
/// the parent directory is read-only.
#[cfg(unix)]
fn remove_file_handling_readonly_parent(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if fs::remove_file(path).is_ok() {
        return;
    }

    let Some(parent) = path.parent() else {
        return;
    };

    let orig_mode = fs::metadata(parent).ok().map(|m| m.permissions().mode());

    if let Some(mode) = orig_mode {
        if (mode & 0o200) == 0 {
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(mode | 0o200));
            let _ = fs::remove_file(path);
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(mode));
        }
    }
}

/// Walks `dir` after a directory clone and removes any symbolic links that resolve outside
/// `confinement_root`, skipping only Cargo-specific `target/**/incremental` and `.fingerprint`
/// directories.
#[cfg(unix)]
fn prune_unsafe_symlinks_in_dir(dir: &Path, confinement_root: &Path) {
    let walker = WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    if matches!(name, "incremental" | ".fingerprint") {
                        let inside_target = e
                            .path()
                            .strip_prefix(confinement_root)
                            .ok()
                            .is_some_and(|rel| rel.components().any(|c| c.as_os_str() == "target"));

                        if inside_target {
                            return false;
                        }
                    }
                }
            }

            true
        });

    for entry in walker.flatten() {
        if entry.file_type().is_symlink() {
            let link_path = entry.path();
            let keep = fs::read_link(link_path)
                .map(|dest| is_safe_symlink_within_root(link_path, &dest, confinement_root))
                .unwrap_or(false);

            if !keep {
                remove_file_handling_readonly_parent(link_path);
            }
        }
    }
}

/// Strategy used when cloning a file or workspace directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CowStrategy {
    /// Native macOS APFS `clonefile(2)` zero-copy clone.
    ApfsClonefile,
    /// Native Linux `ioctl(FICLONE)` reflink zero-copy clone.
    LinuxReflink,
    /// Portable filesystem copy fallback.
    PortableCopy,
}

/// Attempts an OS-level Copy-on-Write clone of `src` to `dst`, falling back to a portable copy
/// if the underlying filesystem does not support COW.
///
/// # Preconditions
/// `src` must exist and `dst` must not exist prior to invocation.
///
/// # Errors
/// Returns [`CoreError::Io`] if both COW and portable copy fail.
pub fn clone_path_cow(src: &Path, dst: &Path) -> CoreResult<CowStrategy> {
    clone_path_cow_within(src, dst, dst)
}

/// Clones `src` to `dst` via Copy-on-Write while confining any internal relative symlinks within `confinement_root`.
fn clone_path_cow_within(
    src: &Path,
    dst: &Path,
    confinement_root: &Path,
) -> CoreResult<CowStrategy> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }

    let src_meta = fs::symlink_metadata(src).map_err(|source| CoreError::Io {
        path: src.display().to_string(),
        source,
    })?;

    if src_meta.file_type().is_symlink() {
        let _ = copy_recursive_fallback(src, dst, confinement_root)?;
        return Ok(CowStrategy::PortableCopy);
    }

    #[cfg(target_os = "macos")]
    {
        if try_macos_clonefile(src, dst).is_ok() {
            if src_meta.file_type().is_dir() {
                prune_unsafe_symlinks_in_dir(dst, confinement_root);
            }

            return Ok(CowStrategy::ApfsClonefile);
        }
    }

    #[cfg(target_os = "linux")]
    {
        if src_meta.file_type().is_file() && try_linux_reflink_file(src, dst).is_ok() {
            return Ok(CowStrategy::LinuxReflink);
        }
    }

    let used_reflink = copy_recursive_fallback(src, dst, confinement_root)?;

    if used_reflink {
        Ok(CowStrategy::LinuxReflink)
    } else {
        Ok(CowStrategy::PortableCopy)
    }
}

/// Invokes the macOS `clonefile(2)` syscall for zero-copy APFS cloning.
#[cfg(target_os = "macos")]
fn try_macos_clonefile(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src_c = CString::new(src.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "interior NUL in src")
    })?;
    let dst_c = CString::new(dst.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "interior NUL in dst")
    })?;

    // SAFETY: `src_c` and `dst_c` are valid, NUL-terminated C strings guaranteed by `CString::new`
    // to contain no interior NUL bytes, and both outlive the synchronous `libc::clonefile` syscall.
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };

    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Invokes the Linux `ioctl(FICLONE)` syscall for zero-copy Btrfs/XFS/bcachefs reflinking.
#[cfg(target_os = "linux")]
fn try_linux_reflink_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let src_file = fs::File::open(src)?;
    let dst_file = fs::File::create(dst)?;

    // SAFETY: `src_file` and `dst_file` are valid, open file descriptors owned by this stack frame,
    // and `libc::FICLONE` takes an integer source fd by value without dereferencing user pointers.
    let rc = unsafe { libc::ioctl(dst_file.as_raw_fd(), libc::FICLONE, src_file.as_raw_fd()) };

    if rc == 0 {
        if let Ok(meta) = src_file.metadata() {
            let _ = dst_file.set_permissions(meta.permissions());
        }

        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        let _ = fs::remove_file(dst);
        Err(err)
    }
}

/// Copies a single file from `src` to `dst`, attempting a Linux `FICLONE` reflink first when
/// `*reflink_supported` is `true`, and caching `EOPNOTSUPP`/`EXDEV`/`ENOSYS` to avoid redundant
/// failed syscalls on non-reflink filesystems (ext4, tmpfs).
fn copy_or_reflink_file(src: &Path, dst: &Path, reflink_supported: &mut bool) -> CoreResult<bool> {
    #[cfg(target_os = "linux")]
    if *reflink_supported {
        match try_linux_reflink_file(src, dst) {
            Ok(()) => return Ok(true),
            Err(err) => {
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EOPNOTSUPP | libc::EXDEV | libc::ENOSYS | libc::EINVAL)
                ) {
                    *reflink_supported = false;
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = reflink_supported;

    fs::copy(src, dst).map_err(|source| CoreError::Io {
        path: dst.display().to_string(),
        source,
    })?;

    Ok(false)
}

/// Recursively copies `src` to `dst` when a whole-directory `clonefile` syscall is unavailable,
/// preserving directory permissions and pruning any multi-hop escaping symlinks.
fn copy_recursive_fallback(src: &Path, dst: &Path, confinement_root: &Path) -> CoreResult<bool> {
    let src_meta = fs::symlink_metadata(src).map_err(|source| CoreError::Io {
        path: src.display().to_string(),
        source,
    })?;

    if src_meta.file_type().is_symlink() {
        #[cfg(unix)]
        {
            if let Ok(target_link) = fs::read_link(src) {
                if is_safe_symlink_within_root(dst, &target_link, confinement_root) {
                    let _ = fs::remove_file(dst);
                    std::os::unix::fs::symlink(&target_link, dst).map_err(|source| {
                        CoreError::Io {
                            path: dst.display().to_string(),
                            source,
                        }
                    })?;
                }
            }
        }

        #[cfg(not(unix))]
        let _ = confinement_root;

        return Ok(false);
    }

    let mut reflink_supported = true;

    if src_meta.file_type().is_file() {
        return copy_or_reflink_file(src, dst, &mut reflink_supported);
    }

    fs::create_dir_all(dst).map_err(|source| CoreError::Io {
        path: dst.display().to_string(),
        source,
    })?;

    let mut any_reflinked = false;
    #[cfg(unix)]
    let mut created_symlinks = false;
    let mut dir_perms: Vec<(PathBuf, fs::Permissions)> =
        vec![(dst.to_path_buf(), src_meta.permissions())];

    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry.map_err(|e| CoreError::Io {
            path: src.display().to_string(),
            source: std::io::Error::other(e.to_string()),
        })?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| CoreError::InvalidArgument(e.to_string()))?;
        let target = dst.join(rel);

        if entry.file_type().is_symlink() {
            #[cfg(unix)]
            {
                if let Some(p) = target.parent() {
                    let _ = fs::create_dir_all(p);
                }

                if let Ok(link_dest) = fs::read_link(entry.path()) {
                    if is_safe_symlink_within_root(&target, &link_dest, confinement_root) {
                        let _ = fs::remove_file(&target);

                        if std::os::unix::fs::symlink(&link_dest, &target).is_ok() {
                            created_symlinks = true;
                        }
                    }
                }
            }
        } else if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|source| CoreError::Io {
                path: target.display().to_string(),
                source,
            })?;

            if let Ok(meta) = entry.metadata() {
                dir_perms.push((target, meta.permissions()));
            }
        } else if entry.file_type().is_file() {
            if let Some(p) = target.parent() {
                fs::create_dir_all(p).map_err(|source| CoreError::Io {
                    path: p.display().to_string(),
                    source,
                })?;
            }

            if copy_or_reflink_file(entry.path(), &target, &mut reflink_supported)? {
                any_reflinked = true;
            }
        }
    }

    // Re-check copied symlinks after the full tree exists so order-dependent multi-hop chains are pruned.
    #[cfg(unix)]
    if created_symlinks {
        prune_unsafe_symlinks_in_dir(dst, confinement_root);
    }

    #[cfg(not(unix))]
    let _ = confinement_root;

    // Apply directory permissions bottom-up so read-only directories do not block child creation.
    for (dir_path, perms) in dir_perms.into_iter().rev() {
        let _ = fs::set_permissions(&dir_path, perms);
    }

    Ok(any_reflinked)
}

/// Returns `true` if `dir` exists and contains at least one directory entry.
fn dir_has_any_entries(dir: &Path) -> bool {
    fs::read_dir(dir)
        .ok()
        .and_then(|mut it| it.next())
        .is_some()
}

/// Ensures any warm dependency or build caches (`node_modules`, `target`, `.venv`, `vendor`, plus
/// configured `cow_cache_dirs`), as well as untracked `.gitignore`d and secret files, that exist in
/// `src_root` are COW-cloned into `dst_root` if missing.
///
/// This handles the common workflow where a branch sandbox is created before `npm install` / `pnpm install`
/// or `.env` / local SQLite setup is executed in `repo_root` (or vice versa).
///
/// # Errors
/// Returns [`CoreError::Io`] if cloning a cache directory or ignored file fails.
pub fn sync_missing_cow_caches(
    src_root: &Path,
    dst_root: &Path,
    cow_cache_dirs: &[String],
) -> CoreResult<usize> {
    if !src_root.is_dir() || !dst_root.is_dir() || src_root == dst_root {
        return Ok(0);
    }

    let defaults = [
        "node_modules",
        "target",
        ".venv",
        "venv",
        "vendor",
        ".pnpm-store",
    ];
    let mut synced = 0usize;

    for name in defaults
        .iter()
        .copied()
        .chain(cow_cache_dirs.iter().map(String::as_str))
    {
        let Ok((_, src_cache)) = resolve_confined_repo_path(src_root, name) else {
            continue;
        };
        let Ok((_, dst_cache)) = resolve_confined_repo_path(dst_root, name) else {
            continue;
        };

        if !src_cache.exists() {
            continue;
        }

        let needs_clone = if !dst_cache.exists() {
            true
        } else if name == "node_modules" {
            // Preserve non-empty branch `node_modules` even when no `.bin/` directory is present.
            !dir_has_any_entries(&dst_cache) && dir_has_any_entries(&src_cache)
        } else {
            false
        };

        if needs_clone {
            if dst_cache.exists() {
                let _ = fs::remove_dir_all(&dst_cache);
            }

            clone_path_cow_within(&src_cache, &dst_cache, dst_root)?;
            synced += 1;
        }
    }

    let src_git = resolve_dot_git(src_root);
    let dst_git = resolve_dot_git(dst_root);
    let is_sandbox_to_repo = src_git.is_none() && dst_git.is_some();

    if !is_sandbox_to_repo {
        let git_dir = src_git
            .as_ref()
            .or(dst_git.as_ref())
            .map(|(_, common)| common.clone())
            .unwrap_or_else(|| src_root.join(".git"));

        let mut init_matcher = IgnoreMatcher::load(src_root, &git_dir, None);

        if !src_root.join(".gitignore").is_file() && dst_root.join(".gitignore").is_file() {
            init_matcher.add_dir_file(&dst_root.join(".gitignore"), "");
        }

        let ignore_matcher = std::cell::RefCell::new(init_matcher);
        let pending_clones: std::cell::RefCell<Vec<(PathBuf, PathBuf)>> =
            std::cell::RefCell::new(Vec::new());

        let walker = WalkDir::new(src_root)
            .max_depth(5)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }

                let Some(name) = e.file_name().to_str() else {
                    return false;
                };

                if is_vcs_internal_segment(name) || name.eq_ignore_ascii_case(".ds_store") {
                    return false;
                }

                if e.file_type().is_dir() && fs::symlink_metadata(e.path().join(".git")).is_ok() {
                    return false;
                }

                let Ok(rel) = e.path().strip_prefix(src_root) else {
                    return false;
                };
                let rel_str = rel.to_string_lossy().replace('\\', "/");

                if e.file_type().is_dir() {
                    let is_cache_or_ignored = PRESERVED_CACHE_DIRS.contains(&name)
                        || cow_cache_dirs
                            .iter()
                            .any(|d| d == name || d.trim_matches('/') == rel_str)
                        || ignore_matcher.borrow().is_ignored(&rel_str, true);

                    if is_cache_or_ignored {
                        if let Ok((_, dst_dir)) = resolve_confined_repo_path(dst_root, &rel_str) {
                            let needs_clone = if !dst_dir.exists() {
                                true
                            } else if name == "node_modules" {
                                !dir_has_any_entries(&dst_dir) && dir_has_any_entries(e.path())
                            } else {
                                false
                            };

                            if needs_clone {
                                pending_clones
                                    .borrow_mut()
                                    .push((e.path().to_path_buf(), dst_dir));
                            }
                        }

                        return false;
                    }

                    let nested_gi = e.path().join(".gitignore");

                    if nested_gi.is_file() {
                        ignore_matcher
                            .borrow_mut()
                            .add_dir_file(&nested_gi, &rel_str);
                    }
                }

                true
            });

        for entry in walker.flatten() {
            if entry.depth() == 0 || entry.file_type().is_dir() {
                continue;
            }

            let Ok(rel) = entry.path().strip_prefix(src_root) else {
                continue;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            if rel_str.is_empty() {
                continue;
            }

            if PRESERVED_ROOT_FILES.contains(&rel_str.as_str())
                || ignore_matcher.borrow().is_ignored(&rel_str, false)
            {
                if let Ok((_, dst_file)) = resolve_confined_repo_path(dst_root, &rel_str) {
                    if !dst_file.exists() {
                        pending_clones
                            .borrow_mut()
                            .push((entry.path().to_path_buf(), dst_file));
                    }
                }
            }
        }

        for (src_item, dst_item) in pending_clones.into_inner() {
            if dst_item.exists() {
                let _ = fs::remove_dir_all(&dst_item);
            }

            clone_path_cow_within(&src_item, &dst_item, dst_root)?;
            synced += 1;
        }
    }

    if synced > 0 {
        let _ = sanitize_build_cache(dst_root, src_root);
    }

    Ok(synced)
}

/// Creates an isolated Copy-on-Write workspace at `workspace_dir` from `repo_root`.
///
/// - Skips `.git` and `.tessera` internal directories and pointer files.
/// - Respects optional directory `scopes` (always including root config files plus matching prefixes).
/// - Clones warm build caches (`cow_cache_dirs` like `target/` and `node_modules/`) via COW and
///   runs [`sanitize_build_cache`] to strip path-poisoned stamps and rewrite virtualenv shebangs.
///
/// # Errors
/// Returns [`CoreError::Io`] if directory traversal or cloning fails.
pub fn create_cow_workspace(
    repo_root: &Path,
    workspace_dir: &Path,
    scopes: &[String],
    cow_cache_dirs: &[String],
) -> CoreResult<usize> {
    if workspace_dir.exists() {
        fs::remove_dir_all(workspace_dir).map_err(|source| CoreError::Io {
            path: workspace_dir.display().to_string(),
            source,
        })?;
    }

    fs::create_dir_all(workspace_dir).map_err(|source| CoreError::Io {
        path: workspace_dir.display().to_string(),
        source,
    })?;

    let mut cloned_entries = 0usize;
    let mut root_symlinks: Vec<(PathBuf, PathBuf)> = Vec::new();
    let entries = fs::read_dir(repo_root).map_err(|source| CoreError::Io {
        path: repo_root.display().to_string(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| CoreError::Io {
            path: repo_root.display().to_string(),
            source,
        })?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };

        if is_vcs_internal_segment(name_str) {
            continue;
        }

        let src_path = entry.path();
        let dst_path = workspace_dir.join(name_str);
        let file_type = entry.file_type().map_err(|source| CoreError::Io {
            path: src_path.display().to_string(),
            source,
        })?;

        if file_type.is_symlink() {
            root_symlinks.push((src_path, dst_path));
            continue;
        }

        if file_type.is_file() {
            clone_path_cow_within(&src_path, &dst_path, workspace_dir)?;
            cloned_entries += 1;
            continue;
        }

        if cow_cache_dirs.iter().any(|c| c == name_str) {
            clone_path_cow_within(&src_path, &dst_path, workspace_dir)?;
            cloned_entries += 1;
            continue;
        }

        if scopes.is_empty() {
            clone_path_cow_within(&src_path, &dst_path, workspace_dir)?;
            cloned_entries += 1;
        } else {
            for scope in scopes {
                let normalized = normalize_scope_prefix(scope);

                if normalized.is_empty() {
                    continue;
                }

                if normalized == name_str || normalized.starts_with(&format!("{name_str}/")) {
                    let Ok((clean_rel, sub_src)) =
                        resolve_confined_repo_path(repo_root, &normalized)
                    else {
                        continue;
                    };
                    let sub_dst = join_rel_segments(workspace_dir, &clean_rel);

                    if sub_src.exists() && !sub_dst.exists() {
                        clone_path_cow_within(&sub_src, &sub_dst, workspace_dir)?;
                        cloned_entries += 1;
                    }
                }
            }
        }
    }

    // Validate and recreate root-level symlinks after subdirectories are cloned so multi-hop
    // targets pointing through cloned subdirectories are verified against the real workspace tree.
    #[cfg(unix)]
    for (src_path, dst_path) in root_symlinks {
        if let Ok(link_dest) = fs::read_link(&src_path) {
            if is_safe_symlink_within_root(&dst_path, &link_dest, workspace_dir) {
                if std::os::unix::fs::symlink(&link_dest, &dst_path).is_ok() {
                    cloned_entries += 1;
                }
            }
        }
    }

    #[cfg(not(unix))]
    let _ = root_symlinks;

    let _ = sanitize_build_cache(workspace_dir, repo_root)?;
    Ok(cloned_entries)
}

/// Normalizes a workspace scope prefix by converting backslashes to forward slashes and stripping
/// leading `./` or `/` and trailing `/` segments.
fn normalize_scope_prefix(scope: &str) -> String {
    let unified = scope.replace('\\', "/");
    let mut trimmed = unified.as_str();

    loop {
        if let Some(rest) = trimmed.strip_prefix("./") {
            trimmed = rest;
        } else if let Some(rest) = trimmed.strip_prefix('/') {
            trimmed = rest;
        } else {
            break;
        }
    }

    trimmed.trim_end_matches('/').to_string()
}

/// Converts `path` to a UTF-8 lossy string while stripping any Windows verbatim path prefix
/// (`\\?\UNC\server\share` -> `\\server\share`, `\\?\C:\...` -> `C:\...`) and trailing
/// directory separators so sanitized shebangs and `VIRTUAL_ENV=` assignments stay portable.
fn strip_windows_verbatim_prefix(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let stripped = if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw.into_owned()
    };

    stripped.trim_end_matches(['/', '\\']).to_string()
}

/// Collects both the raw and canonicalized string representations of `parent_repo_root` so path
/// matching handles OS symlinked temp roots (such as `/var` vs `/private/var` on macOS) and
/// Windows `\\?\` verbatim prefixes.
fn collect_parent_prefixes(parent_repo_root: &Path) -> Vec<String> {
    let mut prefixes = Vec::new();
    let mut push_unique = |candidate: String| {
        if !candidate.is_empty() && !prefixes.contains(&candidate) {
            prefixes.push(candidate);
        }
    };

    let raw = parent_repo_root
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_string();
    push_unique(raw);
    push_unique(strip_windows_verbatim_prefix(parent_repo_root));

    if let Ok(canon) = parent_repo_root.canonicalize() {
        let canon_str = canon
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        push_unique(canon_str);
        push_unique(strip_windows_verbatim_prefix(&canon));
    }

    // Match longer canonical prefixes (e.g. `/private/var/...` or `\\?\C:\...`) before shorter aliases.
    prefixes.sort_by_key(|p| std::cmp::Reverse(p.len()));
    prefixes
}

/// Returns `true` if `text` references any path prefix in `prefixes`.
fn contains_any_prefix(text: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| text.contains(p.as_str()))
}

/// Replaces occurrences of any prefix in `prefixes` (ordered longest-first) with `replacement` in a
/// single left-to-right pass so overlapping aliases (`/private/var` vs `/var`) never double-replace.
fn replace_any_prefix(text: &str, prefixes: &[String], replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while !rest.is_empty() {
        if let Some(matched) = prefixes.iter().find(|p| rest.starts_with(p.as_str())) {
            out.push_str(replacement);
            rest = &rest[matched.len()..];
        } else if let Some(ch) = rest.chars().next() {
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }

    out
}

/// Rewrites `#!` shebangs and `VIRTUAL_ENV=` assignments inside a cloned Python virtualenv's `bin`
/// (Unix) and `Scripts` (Windows) directories that still reference `parent_prefixes` so scripts
/// invoke the sandbox interpreter.
fn sanitize_python_venv(
    venv_dir: &Path,
    parent_prefixes: &[String],
    workspace_replacement: &str,
) -> usize {
    let mut rewritten = 0usize;

    for sub in ["bin", "Scripts"] {
        let scripts_dir = venv_dir.join(sub);

        if !scripts_dir.is_dir() {
            continue;
        }

        let Ok(entries) = fs::read_dir(&scripts_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };

            if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 2 * 1024 * 1024 {
                continue;
            }

            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };

            if bytes.starts_with(b"#!") {
                let line_end = bytes
                    .iter()
                    .position(|&b| b == b'\n')
                    .unwrap_or(bytes.len());

                if let Ok(shebang) = std::str::from_utf8(&bytes[..line_end]) {
                    if contains_any_prefix(shebang, parent_prefixes) {
                        let updated_shebang =
                            replace_any_prefix(shebang, parent_prefixes, workspace_replacement);

                        let mut new_bytes = Vec::with_capacity(
                            updated_shebang.len() + bytes.len().saturating_sub(line_end),
                        );
                        new_bytes.extend_from_slice(updated_shebang.as_bytes());
                        new_bytes.extend_from_slice(&bytes[line_end..]);

                        if write_working_tree_file_safe(&path, &new_bytes).is_ok() {
                            rewritten += 1;
                        }
                    }
                }
            } else if name_str.to_ascii_lowercase().starts_with("activate") {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    if contains_any_prefix(text, parent_prefixes) {
                        let updated =
                            replace_any_prefix(text, parent_prefixes, workspace_replacement);

                        if write_working_tree_file_safe(&path, updated.as_bytes()).is_ok() {
                            rewritten += 1;
                        }
                    }
                }
            }
        }
    }

    rewritten
}

/// Sanitizes cloned build caches (`target/`, `.turbo/`, `build/`, `cmake-build-*`, and monorepo
/// subdirectory equivalents such as `src/target/`) and Python virtualenvs (`.venv/`, `venv/`)
/// inside `workspace_dir` so hardcoded absolute paths to `parent_repo_root` do not poison builds.
///
/// Removes path-poisoned `.d` dependency-info stamps, Cargo `target/*/build/*/output` and
/// `root-output` scripts, and stale `CMakeCache.txt` files while preserving compiled `.rlib`/`.rmeta`
/// artifacts, and rewrites Python `.venv/bin/*` and `.venv/Scripts/*` shebangs and activation
/// scripts to point at `workspace_dir`.
///
/// # Errors
/// Returns [`CoreError::Io`] on directory walk errors.
pub fn sanitize_build_cache(workspace_dir: &Path, parent_repo_root: &Path) -> CoreResult<usize> {
    let parent_prefixes = collect_parent_prefixes(parent_repo_root);

    if parent_prefixes.is_empty() {
        return Ok(0);
    }

    let workspace_canon = workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.to_path_buf());
    let workspace_replacement = strip_windows_verbatim_prefix(&workspace_canon);

    let mut sanitized_count = 0usize;
    let mut build_cache_dirs: Vec<(PathBuf, bool)> = Vec::new();
    let mut venv_dirs: Vec<PathBuf> = Vec::new();

    // Remove top-level in-source `CMakeCache.txt` if it embeds parent repository paths.
    let root_cmake = workspace_dir.join("CMakeCache.txt");

    if root_cmake.is_file() {
        if let Ok(content) = fs::read_to_string(&root_cmake) {
            if contains_any_prefix(&content, &parent_prefixes)
                && fs::remove_file(&root_cmake).is_ok()
            {
                sanitized_count += 1;
            }
        }
    }

    let discovery = WalkDir::new(workspace_dir)
        .max_depth(4)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }

            if !e.file_type().is_dir() {
                return false;
            }

            let Some(name) = e.file_name().to_str() else {
                return false;
            };

            if is_vcs_internal_segment(name)
                || matches!(name, "node_modules" | ".pnpm-store" | "vendor")
            {
                return false;
            }

            if matches!(
                name,
                "target"
                    | ".turbo"
                    | "build"
                    | ".build"
                    | "cmake-build-debug"
                    | "cmake-build-release"
            ) {
                build_cache_dirs.push((e.path().to_path_buf(), name == "target"));
                return false;
            }

            if matches!(name, ".venv" | "venv") {
                venv_dirs.push(e.path().to_path_buf());
                return false;
            }

            true
        });

    for _ in discovery.flatten() {}

    for (cache_dir, is_cargo_target) in build_cache_dirs {
        let walker = WalkDir::new(&cache_dir).into_iter().filter_entry(|e| {
            if e.file_type().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    if matches!(name, "incremental" | ".fingerprint" | "doc" | "package") {
                        return false;
                    }
                }
            }

            true
        });

        for entry in walker.flatten() {
            let path = entry.path();

            if !entry.file_type().is_file() {
                continue;
            }

            let file_name = entry.file_name().to_str().unwrap_or("");
            let is_dep_info = path.extension().and_then(|e| e.to_str()) == Some("d");
            let is_cmake_cache = file_name == "CMakeCache.txt";
            let is_cargo_build_output = is_cargo_target
                && matches!(file_name, "output" | "root-output")
                && path
                    .strip_prefix(&cache_dir)
                    .ok()
                    .is_some_and(|rel| rel.components().any(|c| c.as_os_str() == "build"));

            if is_dep_info || is_cmake_cache || is_cargo_build_output {
                if let Ok(content) = fs::read_to_string(path) {
                    if contains_any_prefix(&content, &parent_prefixes)
                        && fs::remove_file(path).is_ok()
                    {
                        sanitized_count += 1;
                    }
                }
            }
        }
    }

    for venv_dir in venv_dirs {
        sanitized_count +=
            sanitize_python_venv(&venv_dir, &parent_prefixes, &workspace_replacement);
    }

    Ok(sanitized_count)
}

/// Build and dependency cache directory names cloned wholesale across workspaces without descending inside.
const PRESERVED_CACHE_DIRS: &[&str] = &[
    "target",
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
    ".cache",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "vendor",
    "_build",
    "deps",
    ".gradle",
    ".build",
    ".zig-cache",
    "zig-out",
    ".dart_tool",
    "cmake-build-debug",
    "cmake-build-release",
];

/// Top-level dependency, build-cache, and IDE/agent configuration directories preserved across tree materializations.
const PRESERVED_TOP_DIRS: &[&str] = &[
    ".git",
    ".tessera",
    ".agents",
    ".claude",
    ".cursor",
    ".windsurf",
    ".kiro",
    ".opencode",
    ".codex",
    ".vscode",
    "target",
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
    ".cache",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "vendor",
    "_build",
    "deps",
    ".gradle",
    ".build",
    ".zig-cache",
    "zig-out",
    ".dart_tool",
    "cmake-build-debug",
    "cmake-build-release",
];

/// Root-level agent harness and MCP configuration files preserved when switching or materializing trees.
const PRESERVED_ROOT_FILES: &[&str] = &[
    "AGENTS.md",
    "AGENTS.override.md",
    "CLAUDE.md",
    ".mcp.json",
    ".windsurfrules",
    "opencode.json",
];

/// Validates `rel_path` inside `target_dir`, ensuring no existing parent directory component is a
/// symbolic link while allowing the leaf itself to be a symlink, regular file, or directory.
fn resolve_confined_materialize_path(
    target_dir: &Path,
    rel_path: &str,
) -> CoreResult<(String, PathBuf)> {
    let clean_rel = validate_repo_rel_path(rel_path)?;
    let segments: Vec<&str> = clean_rel.split('/').collect();
    let mut current = target_dir.to_path_buf();

    for seg in &segments[..segments.len().saturating_sub(1)] {
        current.push(seg);

        if let Ok(meta) = fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(CoreError::InvalidArgument(format!(
                    "refusing to traverse symbolic link directory '{}'",
                    current.display()
                )));
            }
        }
    }

    if let Some(last) = segments.last() {
        current.push(last);
    }

    Ok((clean_rel, current))
}

/// Removes empty ancestor directories starting from `path.parent()` up to (but not including) `stop_dir`.
fn prune_empty_parents(path: &Path, stop_dir: &Path) {
    let mut parent_opt = path.parent();

    while let Some(parent) = parent_opt {
        if parent == stop_dir {
            break;
        }

        if fs::remove_dir(parent).is_err() {
            break;
        }

        parent_opt = parent.parent();
    }
}

/// Materializes all files from `tree_id` into `target_dir` atomically, removing tracked files
/// that are no longer present in `tree_id` while preserving untracked `.gitignore`d and secret files.
///
/// # Errors
/// Returns [`CoreError`] if any path escapes `target_dir` or if reading blobs or writing files fails.
pub fn materialize_tree_to_dir(
    git: &GitBackend,
    tree_id: &ObjectId,
    target_dir: &Path,
) -> CoreResult<()> {
    materialize_tree_to_dir_with_baseline(git, tree_id, target_dir, None)
}

/// Materializes all entries from `tree_id` into `target_dir` atomically, preserving `.git`
/// directories and worktree pointer files, tracked symbolic links (`120000`), and submodule
/// gitlinks (`160000`).
///
/// When `prev_tracked` is `Some(prev)`, only paths present in `prev` and absent from `tree_id`
/// are removed, ensuring untracked user files are never deleted. When `prev_tracked` is `None`,
/// stale non-ignored files and symlinks in `target_dir` are pruned while preserving top-level
/// caches, `.git`, `.tessera`, `.gitignore`d paths, and built-in secret files.
///
/// # Errors
/// Returns [`CoreError`] if any path or symlink escapes `target_dir`, or if reading Git blobs
/// or writing working-tree entries fails.
pub fn materialize_tree_to_dir_with_baseline(
    git: &GitBackend,
    tree_id: &ObjectId,
    target_dir: &Path,
    prev_tracked: Option<&BTreeMap<String, FileEntry>>,
) -> CoreResult<()> {
    let tree_files = git.read_tree_to_map(tree_id)?;
    fs::create_dir_all(target_dir).map_err(|source| CoreError::Io {
        path: target_dir.display().to_string(),
        source,
    })?;

    if let Some(prev) = prev_tracked {
        for (rel_path, prev_entry) in prev {
            if tree_files.contains_key(rel_path) || prev_entry.is_gitlink() {
                continue;
            }

            let Ok((_, full_path)) = resolve_confined_materialize_path(target_dir, rel_path) else {
                continue;
            };

            if let Ok(meta) = fs::symlink_metadata(&full_path) {
                if meta.file_type().is_symlink() || meta.is_file() {
                    let _ = fs::remove_file(&full_path);
                    prune_empty_parents(&full_path, target_dir);
                }
            }
        }
    } else {
        let extra_preserved =
            core::OpLogStore::new_with_repo_root(git.common_git_dir(), git.repo_root())
                .load_config()
                .map(|c| c.cow_cache_dirs)
                .unwrap_or_default();

        let global_excludes = git.global_excludes_file();
        let mut init_matcher =
            IgnoreMatcher::load(target_dir, git.common_git_dir(), global_excludes.as_deref());

        if target_dir != git.repo_root() {
            init_matcher.add_dir_file(&git.repo_root().join(".gitignore"), "");
        }

        if let Some(gi_entry) = tree_files.get(".gitignore") {
            if let Ok(bytes) = git.read_blob(&gi_entry.blob_id) {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    init_matcher.add_dir_str(text, "");
                }
            }
        }

        let ignore_matcher = std::cell::RefCell::new(init_matcher);

        let walker = WalkDir::new(target_dir).into_iter().filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }

            // Never touch or delete `.git` (directory OR worktree/submodule regular file) or `.tessera`.
            if let Some(name) = e.file_name().to_str() {
                if is_vcs_internal_segment(name) {
                    return false;
                }
            }

            if e.file_type().is_dir() {
                // Preserve nested Git submodules and sub-repositories untouched.
                if fs::symlink_metadata(e.path().join(".git")).is_ok() {
                    return false;
                }

                if let Ok(rel) = e.path().strip_prefix(target_dir) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    let prefix = format!("{rel_str}/");
                    let has_tracked_descendant = tree_files
                        .range(prefix.clone()..)
                        .next()
                        .is_some_and(|(k, _)| k.starts_with(&prefix));

                    if let Some(name) = e.file_name().to_str() {
                        if e.depth() == 1
                            && (PRESERVED_TOP_DIRS.contains(&name)
                                || extra_preserved.iter().any(|d| d.trim_matches('/') == name))
                        {
                            return false;
                        }

                        if !has_tracked_descendant
                            && (ALWAYS_IGNORED_DIRS.contains(&name)
                                || extra_preserved
                                    .iter()
                                    .any(|d| d.trim_matches('/') == rel_str))
                        {
                            return false;
                        }
                    }

                    if !has_tracked_descendant && ignore_matcher.borrow().is_ignored(&rel_str, true)
                    {
                        return false;
                    }

                    let nested_gi = e.path().join(".gitignore");

                    if nested_gi.is_file() {
                        ignore_matcher
                            .borrow_mut()
                            .add_dir_file(&nested_gi, &rel_str);
                    }

                    if let Some(tree_gi) = tree_files.get(&format!("{rel_str}/.gitignore")) {
                        if let Ok(bytes) = git.read_blob(&tree_gi.blob_id) {
                            if let Ok(text) = std::str::from_utf8(&bytes) {
                                ignore_matcher.borrow_mut().add_dir_str(text, &rel_str);
                            }
                        }
                    }
                }
            }

            true
        });

        let mut stale_files = Vec::new();

        for entry in walker.flatten() {
            let ft = entry.file_type();

            if !ft.is_file() && !ft.is_symlink() {
                continue;
            }

            if let Ok(rel) = entry.path().strip_prefix(target_dir) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");

                if rel_str.is_empty()
                    || rel_str.split('/').any(is_vcs_internal_segment)
                    || tree_files.contains_key(&rel_str)
                    || PRESERVED_ROOT_FILES.contains(&rel_str.as_str())
                    || ignore_matcher.borrow().is_ignored(&rel_str, false)
                {
                    continue;
                }

                stale_files.push(entry.path().to_path_buf());
            }
        }

        for stale in stale_files {
            let _ = fs::remove_file(&stale);
            prune_empty_parents(&stale, target_dir);
        }
    }

    #[cfg(unix)]
    let mut materialized_symlinks: Vec<(PathBuf, PathBuf)> = Vec::new();

    for (rel_path, entry) in &tree_files {
        let (_, full_path) = resolve_confined_materialize_path(target_dir, rel_path)?;

        if entry.is_gitlink() {
            if fs::symlink_metadata(&full_path).is_err() {
                let _ = fs::create_dir_all(&full_path);
            }

            continue;
        }

        // Resolve file -> directory transitions when an ancestor is currently a regular file.
        if let Some(parent) = full_path.parent() {
            let mut cursor = target_dir.to_path_buf();

            if let Ok(rel_parent) = parent.strip_prefix(target_dir) {
                for comp in rel_parent.components() {
                    if let Component::Normal(seg) = comp {
                        cursor.push(seg);

                        if let Ok(meta) = fs::symlink_metadata(&cursor) {
                            if meta.is_file() {
                                fs::remove_file(&cursor).map_err(|source| CoreError::Io {
                                    path: cursor.display().to_string(),
                                    source,
                                })?;
                            }
                        }
                    }
                }
            }
        }

        if entry.is_symlink() {
            let target_bytes = git.read_blob(&entry.blob_id)?;

            if target_bytes.is_empty() || target_bytes.contains(&0) {
                return Err(CoreError::InvalidArgument(format!(
                    "invalid symlink target blob for '{rel_path}'"
                )));
            }

            #[cfg(unix)]
            {
                use std::ffi::OsStr;
                use std::os::unix::ffi::OsStrExt;

                let link_target = PathBuf::from(OsStr::from_bytes(&target_bytes));

                if !is_safe_symlink_within_root(&full_path, &link_target, target_dir) {
                    return Err(CoreError::InvalidArgument(format!(
                        "refusing to materialize symlink '{rel_path}' pointing outside workspace"
                    )));
                }

                if let Ok(meta) = fs::symlink_metadata(&full_path) {
                    if meta.file_type().is_symlink()
                        && fs::read_link(&full_path).ok().as_deref() == Some(link_target.as_path())
                    {
                        materialized_symlinks.push((full_path, link_target));
                        continue;
                    }

                    if meta.is_dir() {
                        fs::remove_dir_all(&full_path).map_err(|source| CoreError::Io {
                            path: full_path.display().to_string(),
                            source,
                        })?;
                    } else {
                        fs::remove_file(&full_path).map_err(|source| CoreError::Io {
                            path: full_path.display().to_string(),
                            source,
                        })?;
                    }
                }

                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).map_err(|source| CoreError::Io {
                        path: parent.display().to_string(),
                        source,
                    })?;
                }

                std::os::unix::fs::symlink(&link_target, &full_path).map_err(|source| {
                    CoreError::Io {
                        path: full_path.display().to_string(),
                        source,
                    }
                })?;
                materialized_symlinks.push((full_path, link_target));
            }

            #[cfg(not(unix))]
            {
                if let Ok(meta) = fs::symlink_metadata(&full_path) {
                    if meta.is_dir() {
                        fs::remove_dir_all(&full_path).map_err(|source| CoreError::Io {
                            path: full_path.display().to_string(),
                            source,
                        })?;
                    } else if meta.file_type().is_symlink() {
                        fs::remove_file(&full_path).map_err(|source| CoreError::Io {
                            path: full_path.display().to_string(),
                            source,
                        })?;
                    }
                }

                write_working_tree_file_safe(&full_path, &target_bytes)?;
            }

            continue;
        }

        if let Ok(meta) = fs::symlink_metadata(&full_path) {
            if meta.file_type().is_symlink() {
                fs::remove_file(&full_path).map_err(|source| CoreError::Io {
                    path: full_path.display().to_string(),
                    source,
                })?;
            } else if meta.is_dir() {
                fs::remove_dir_all(&full_path).map_err(|source| CoreError::Io {
                    path: full_path.display().to_string(),
                    source,
                })?;
            } else if meta.is_file() {
                if let Ok(existing) = fs::read(&full_path) {
                    if GitBackend::hash_blob_in_memory(&existing) == entry.blob_id {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let target_mode = if entry.executable { 0o755 } else { 0o644 };

                            if ((meta.permissions().mode() & 0o111) != 0) != entry.executable {
                                let _ = fs::set_permissions(
                                    &full_path,
                                    fs::Permissions::from_mode(target_mode),
                                );
                            }
                        }

                        continue;
                    }
                }
            }
        }

        let blob_bytes = git.read_blob(&entry.blob_id)?;
        write_working_tree_file_safe(&full_path, &blob_bytes)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let target_mode = if entry.executable { 0o755 } else { 0o644 };

            if let Ok(meta) = fs::metadata(&full_path) {
                if ((meta.permissions().mode() & 0o111) != 0) != entry.executable {
                    let _ =
                        fs::set_permissions(&full_path, fs::Permissions::from_mode(target_mode));
                }
            }
        }
    }

    // Re-validate all materialized symlinks once the full tree is present to block multi-hop escapes.
    #[cfg(unix)]
    for (link_path, link_target) in materialized_symlinks {
        if !is_safe_symlink_within_root(&link_path, &link_target, target_dir) {
            let _ = fs::remove_file(&link_path);
            return Err(CoreError::InvalidArgument(format!(
                "removed multi-hop escaping symlink '{}'",
                link_path.display()
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::EntryMode;

    #[test]
    fn cow_workspace_clones_files_and_sanitizes_d_files() {
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_parent = tempfile::tempdir().expect("dst tempdir");
        let dst_dir = dst_parent.path().join("agent-ws");

        fs::create_dir_all(src_dir.path().join("packages/api")).expect("mkdir");
        fs::write(src_dir.path().join("Cargo.toml"), "[workspace]\n").expect("write root");
        fs::write(
            src_dir.path().join("packages/api/lib.rs"),
            "pub fn api() {}\n",
        )
        .expect("write lib");

        let deps_dir = src_dir.path().join("target/debug/deps");
        fs::create_dir_all(&deps_dir).expect("mkdir deps");
        let poisoned_d = format!(
            "{}/packages/api/lib.rs: src/lib.rs\n",
            src_dir.path().display()
        );
        fs::write(deps_dir.join("app.d"), poisoned_d).expect("write .d");
        fs::write(deps_dir.join("libapp.rlib"), b"compiled_artifact").expect("write .rlib");

        let count = create_cow_workspace(src_dir.path(), &dst_dir, &[], &["target".to_string()])
            .expect("create cow workspace");
        assert!(count >= 3);
        assert!(dst_dir.join("packages/api/lib.rs").is_file());
        assert!(dst_dir.join("target/debug/deps/libapp.rlib").is_file());
        assert!(!dst_dir.join("target/debug/deps/app.d").exists());
    }

    #[test]
    fn sanitize_build_cache_handles_subdirectories_cargo_build_output_cmake_and_venv() {
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let ws_dir = tempfile::tempdir().expect("ws tempdir");
        let src_canon = src_dir.path().canonicalize().expect("canonicalize src_dir");

        // Monorepo subdirectory Cargo target build output + .d stamp.
        let build_out = ws_dir.path().join("src/target/debug/build/foo-123");
        fs::create_dir_all(&build_out).expect("mkdir build_out");
        fs::write(
            build_out.join("output"),
            format!(
                "cargo:rerun-if-changed={}/src/build.rs\n",
                src_canon.display()
            ),
        )
        .expect("write output");
        fs::write(
            build_out.join("root-output"),
            format!(
                "{}/src/target/debug/build/foo-123/out\n",
                src_canon.display()
            ),
        )
        .expect("write root-output");
        fs::write(
            ws_dir.path().join("src/target/debug/app.d"),
            format!("{}/src/main.rs: src/main.rs\n", src_canon.display()),
        )
        .expect("write app.d");

        // Stale CMakeCache.txt referencing parent repo.
        let cmake_dir = ws_dir.path().join("cmake-build-debug");
        fs::create_dir_all(&cmake_dir).expect("mkdir cmake");
        fs::write(
            cmake_dir.join("CMakeCache.txt"),
            format!("CMAKE_HOME_DIRECTORY:INTERNAL={}\n", src_canon.display()),
        )
        .expect("write CMakeCache.txt");

        // Python `.venv/bin/pytest` shebang pointing at parent repo.
        let venv_bin = ws_dir.path().join(".venv/bin");
        fs::create_dir_all(&venv_bin).expect("mkdir venv bin");
        fs::write(
            venv_bin.join("pytest"),
            format!("#!{}/.venv/bin/python3\nprint('ok')\n", src_canon.display()),
        )
        .expect("write pytest");

        let count = sanitize_build_cache(ws_dir.path(), src_dir.path()).expect("sanitize");
        assert!(count >= 5, "expected >= 5 sanitized items, got {count}");
        assert!(!build_out.join("output").exists());
        assert!(!build_out.join("root-output").exists());
        assert!(!ws_dir.path().join("src/target/debug/app.d").exists());
        assert!(!cmake_dir.join("CMakeCache.txt").exists());

        let rewritten = fs::read_to_string(venv_bin.join("pytest")).expect("read pytest");
        let ws_canon = ws_dir.path().canonicalize().expect("canon ws");
        let ws_clean = strip_windows_verbatim_prefix(&ws_canon);
        assert!(
            rewritten.starts_with(&format!("#!{ws_clean}/.venv/bin/python3\n")),
            "unexpected shebang: {rewritten}"
        );
    }

    #[test]
    fn materialize_and_sync_preserve_gitignored_secrets_and_monorepo_symlinks() {
        let repo_dir = tempfile::tempdir().expect("repo tempdir");
        let ws_dir = tempfile::tempdir().expect("ws tempdir");

        let git = GitBackend::init_or_open(repo_dir.path()).expect("init git");

        let gitignore_content = b"*.db\n.dev.vars\nlocal.properties\nstorage/\n";
        let gi_blob = git.write_blob(gitignore_content).expect("write .gitignore");
        let main_blob = git.write_blob(b"fn main() {}\n").expect("write main.rs");

        let mut tree_map = BTreeMap::new();
        tree_map.insert(".gitignore".to_string(), FileEntry::regular(gi_blob, false));
        tree_map.insert(
            "src/main.rs".to_string(),
            FileEntry::regular(main_blob, false),
        );
        let tree_id = git.write_tree_from_map(&tree_map).expect("write tree");

        fs::write(repo_dir.path().join(".gitignore"), gitignore_content).expect("write gi");
        fs::write(repo_dir.path().join(".env"), "SECRET=1\n").expect("write .env");
        fs::write(repo_dir.path().join(".env.local"), "LOCAL=1\n").expect("write .env.local");
        fs::write(repo_dir.path().join(".dev.vars"), "CF_KEY=1\n").expect("write .dev.vars");
        fs::write(repo_dir.path().join("local.properties"), "sdk.dir=/sdk\n")
            .expect("write local.properties");
        fs::create_dir_all(repo_dir.path().join("prisma")).expect("mkdir prisma");
        fs::write(repo_dir.path().join("prisma/dev.db"), b"sqlite3_data").expect("write dev.db");
        fs::create_dir_all(repo_dir.path().join("packages/ui")).expect("mkdir packages/ui");
        fs::create_dir_all(repo_dir.path().join("packages/web/node_modules/pkg"))
            .expect("mkdir nested node_modules");
        fs::write(
            repo_dir
                .path()
                .join("packages/web/node_modules/pkg/index.js"),
            "module.exports = {};\n",
        )
        .expect("write nested pkg");

        #[cfg(unix)]
        {
            fs::create_dir_all(repo_dir.path().join("node_modules/@scope")).expect("mkdir scope");
            std::os::unix::fs::symlink(
                Path::new("../../packages/ui"),
                repo_dir.path().join("node_modules/@scope/ui"),
            )
            .expect("create monorepo symlink");
        }

        let synced =
            sync_missing_cow_caches(repo_dir.path(), ws_dir.path(), &[]).expect("sync caches");
        assert!(synced >= 5);
        assert!(ws_dir.path().join(".env").is_file());
        assert!(ws_dir.path().join(".env.local").is_file());
        assert!(ws_dir.path().join(".dev.vars").is_file());
        assert!(ws_dir.path().join("local.properties").is_file());
        assert!(ws_dir.path().join("prisma/dev.db").is_file());
        assert!(
            ws_dir
                .path()
                .join("packages/web/node_modules/pkg/index.js")
                .is_file()
        );

        #[cfg(unix)]
        {
            let link_meta = fs::symlink_metadata(ws_dir.path().join("node_modules/@scope/ui"))
                .expect("monorepo symlink must survive sync_missing_cow_caches");
            assert!(link_meta.file_type().is_symlink());
        }

        fs::write(ws_dir.path().join("obsolete.txt"), "remove me\n").expect("write obsolete");
        materialize_tree_to_dir(&git, &tree_id, ws_dir.path()).expect("materialize tree");

        assert!(!ws_dir.path().join("obsolete.txt").exists());
        assert!(ws_dir.path().join("src/main.rs").is_file());
        assert!(ws_dir.path().join(".env").is_file());
        assert!(ws_dir.path().join(".env.local").is_file());
        assert!(ws_dir.path().join(".dev.vars").is_file());
        assert!(ws_dir.path().join("local.properties").is_file());
        assert!(ws_dir.path().join("prisma/dev.db").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_multihop_symlink_escape_and_scans_doc_directory() {
        let src = tempfile::tempdir().expect("src");
        let dst_parent = tempfile::tempdir().expect("dst");
        let dst = dst_parent.path().join("ws");

        fs::create_dir_all(src.path().join("a/b")).expect("mkdir a/b");
        fs::create_dir_all(src.path().join("doc")).expect("mkdir doc");
        std::os::unix::fs::symlink(Path::new("../.."), src.path().join("a/b/up"))
            .expect("create up link");
        std::os::unix::fs::symlink(
            Path::new("a/b/up/../../../etc/passwd"),
            src.path().join("escape"),
        )
        .expect("create 2-hop escape");
        std::os::unix::fs::symlink(Path::new("/etc/passwd"), src.path().join("doc/leak"))
            .expect("create doc escape");
        std::os::unix::fs::symlink(Path::new("../a/b"), src.path().join("doc/safe"))
            .expect("create doc safe link");

        create_cow_workspace(src.path(), &dst, &[], &[]).expect("create cow workspace");

        assert!(
            fs::symlink_metadata(dst.join("escape")).is_err(),
            "2-hop symlink escape must be rejected"
        );
        assert!(
            fs::symlink_metadata(dst.join("doc/leak")).is_err(),
            "unsafe symlink inside doc/ must be pruned"
        );
        assert!(
            fs::symlink_metadata(dst.join("a/b/up")).is_ok(),
            "safe internal symlink staying at root must be preserved"
        );
        assert!(
            fs::symlink_metadata(dst.join("doc/safe")).is_ok(),
            "safe symlink inside doc/ must be preserved"
        );
    }

    #[test]
    fn materialize_preserves_worktree_git_file_untracked_files_symlinks_and_gitlinks() {
        let tmp = tempfile::tempdir().expect("tmp");
        let main_dir = tmp.path().join("main");
        fs::create_dir_all(&main_dir).expect("mkdir main");
        let main_git = GitBackend::init_or_open(&main_dir).expect("init main");

        let wt_git_dir = main_git.git_dir().join("worktrees/wt1");
        fs::create_dir_all(&wt_git_dir).expect("mkdir wt gitdir");
        fs::write(wt_git_dir.join("commondir"), "../..\n").expect("commondir");
        fs::write(wt_git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");

        let wt_dir = tmp.path().join("wt1");
        fs::create_dir_all(&wt_dir).expect("mkdir wt");
        let dot_git_content = format!("gitdir: {}\n", wt_git_dir.display());
        fs::write(wt_dir.join(".git"), &dot_git_content).expect("write .git file");

        let wt_git = GitBackend::discover(&wt_dir).expect("discover wt");

        let keep_blob = wt_git.write_blob(b"keep\n").expect("blob");
        let old_blob = wt_git.write_blob(b"old\n").expect("old blob");
        let link_blob = wt_git.write_blob(b"keep.txt").expect("link blob");
        let sub_commit =
            ObjectId::from_hex("1234567890abcdef1234567890abcdef12345678").expect("oid");

        let mut prev_tree_map = BTreeMap::new();
        prev_tree_map.insert("keep.txt".to_string(), FileEntry::regular(keep_blob, false));
        prev_tree_map.insert(
            "removed.txt".to_string(),
            FileEntry::regular(old_blob, false),
        );
        prev_tree_map.insert(
            "src/cache/ghost.rs".to_string(),
            FileEntry::regular(old_blob, false),
        );

        let mut next_tree_map = BTreeMap::new();
        next_tree_map.insert("keep.txt".to_string(), FileEntry::regular(keep_blob, false));
        next_tree_map.insert(
            "alias.txt".to_string(),
            FileEntry::from_mode(link_blob, EntryMode::Symlink),
        );
        next_tree_map.insert(
            "vendor/submod".to_string(),
            FileEntry::from_mode(sub_commit, EntryMode::Gitlink),
        );
        let next_tree = wt_git
            .write_tree_from_map(&next_tree_map)
            .expect("write next tree");

        fs::write(wt_dir.join("removed.txt"), "old\n").expect("write removed");
        fs::write(wt_dir.join("untracked_notes.md"), "user notes\n").expect("write untracked");
        fs::create_dir_all(wt_dir.join("src/cache")).expect("mkdir src/cache");
        fs::write(wt_dir.join("src/cache/ghost.rs"), "ghost\n").expect("write ghost");
        fs::create_dir_all(wt_dir.join("vendor/submod")).expect("mkdir submod");
        fs::write(wt_dir.join("vendor/submod/lib.rs"), "sub\n").expect("write submod lib");

        // Baseline-aware materialization deletes only removed tracked files and preserves untracked files.
        materialize_tree_to_dir_with_baseline(&wt_git, &next_tree, &wt_dir, Some(&prev_tree_map))
            .expect("materialize with baseline");

        assert_eq!(
            fs::read_to_string(wt_dir.join(".git")).expect(".git pointer preserved"),
            dot_git_content
        );
        assert!(!wt_dir.join("removed.txt").exists());
        assert!(!wt_dir.join("src/cache/ghost.rs").exists());
        assert!(wt_dir.join("untracked_notes.md").is_file());
        assert!(wt_dir.join("keep.txt").is_file());
        assert!(wt_dir.join("vendor/submod/lib.rs").is_file());

        #[cfg(unix)]
        {
            let link_meta = fs::symlink_metadata(wt_dir.join("alias.txt")).expect("symlink meta");
            assert!(link_meta.file_type().is_symlink());
            assert_eq!(
                fs::read_link(wt_dir.join("alias.txt")).expect("read link"),
                PathBuf::from("keep.txt")
            );
        }

        // Non-baseline mode must ALSO preserve worktree `.git` pointer file and clean nested `src/cache/` ghosts.
        fs::create_dir_all(wt_dir.join("src/cache")).expect("mkdir src/cache 2");
        fs::write(wt_dir.join("src/cache/ghost2.rs"), "ghost\n").expect("write ghost2");
        materialize_tree_to_dir(&wt_git, &next_tree, &wt_dir).expect("materialize scan mode");

        assert_eq!(
            fs::read_to_string(wt_dir.join(".git")).expect(".git pointer still preserved"),
            dot_git_content
        );
        assert!(!wt_dir.join("src/cache/ghost2.rs").exists());
        assert!(wt_dir.join("vendor/submod/lib.rs").is_file());
    }

    #[test]
    fn sync_does_not_wipe_existing_branch_node_modules_without_dot_bin() {
        let src = tempfile::tempdir().expect("src");
        let dst = tempfile::tempdir().expect("dst");

        fs::create_dir_all(src.path().join("node_modules/.bin")).expect("mkdir src .bin");
        fs::write(src.path().join("node_modules/.bin/tsc"), "#!/bin/sh\n").expect("write tsc");
        fs::create_dir_all(dst.path().join("node_modules/branch-only-pkg"))
            .expect("mkdir branch pkg");
        fs::write(
            dst.path().join("node_modules/branch-only-pkg/index.js"),
            "module.exports = 1;\n",
        )
        .expect("write branch pkg");

        sync_missing_cow_caches(src.path(), dst.path(), &[]).expect("sync");

        assert!(
            dst.path()
                .join("node_modules/branch-only-pkg/index.js")
                .is_file(),
            "existing branch node_modules must not be wiped when .bin is absent"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_handles_drive_unc_and_posix_paths() {
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\C:\Users\dev\repo")),
            r"C:\Users\dev\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\C:\Users\dev\repo\")),
            r"C:\Users\dev\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\UNC\server\share\repo")),
            r"\\server\share\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\UNC\server\share\repo\")),
            r"\\server\share\repo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new("/Users/dev/repo/")),
            "/Users/dev/repo"
        );
    }

    #[test]
    fn create_cow_workspace_normalizes_dot_slash_and_backslash_scopes() {
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_parent = tempfile::tempdir().expect("dst tempdir");
        let dst_dir = dst_parent.path().join("scoped-ws");

        fs::create_dir_all(src_dir.path().join("packages/api")).expect("mkdir api");
        fs::create_dir_all(src_dir.path().join("packages/web")).expect("mkdir web");
        fs::create_dir_all(src_dir.path().join("packages/other")).expect("mkdir other");
        fs::write(src_dir.path().join("Cargo.toml"), "[workspace]\n").expect("write root");
        fs::write(
            src_dir.path().join("packages/api/lib.rs"),
            "pub fn api() {}\n",
        )
        .expect("write api");
        fs::write(
            src_dir.path().join("packages/web/index.ts"),
            "export const ok = true;\n",
        )
        .expect("write web");
        fs::write(
            src_dir.path().join("packages/other/skip.rs"),
            "pub fn skip() {}\n",
        )
        .expect("write other");

        let scopes = vec!["./packages/api".to_string(), r"packages\web\".to_string()];
        create_cow_workspace(src_dir.path(), &dst_dir, &scopes, &[]).expect("create scoped ws");

        assert!(dst_dir.join("Cargo.toml").is_file());
        assert!(
            dst_dir
                .join("packages")
                .join("api")
                .join("lib.rs")
                .is_file()
        );
        assert!(
            dst_dir
                .join("packages")
                .join("web")
                .join("index.ts")
                .is_file()
        );
        assert!(!dst_dir.join("packages").join("other").exists());
    }

    #[test]
    fn sanitize_build_cache_rewrites_windows_venv_scripts_directory() {
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let ws_dir = tempfile::tempdir().expect("ws tempdir");
        let src_prefix =
            strip_windows_verbatim_prefix(&src_dir.path().canonicalize().expect("canon src"));
        let ws_prefix =
            strip_windows_verbatim_prefix(&ws_dir.path().canonicalize().expect("canon ws"));

        let venv_scripts = ws_dir.path().join(".venv").join("Scripts");
        fs::create_dir_all(&venv_scripts).expect("mkdir Scripts");
        fs::write(
            venv_scripts.join("pytest"),
            format!("#!{src_prefix}/.venv/Scripts/python.exe\nprint('ok')\n"),
        )
        .expect("write pytest");
        fs::write(
            venv_scripts.join("activate.bat"),
            format!("set \"VIRTUAL_ENV={src_prefix}/.venv\"\n"),
        )
        .expect("write activate.bat");
        fs::write(
            venv_scripts.join("Activate.ps1"),
            format!("$env:VIRTUAL_ENV = \"{src_prefix}/.venv\"\n"),
        )
        .expect("write Activate.ps1");

        let count = sanitize_build_cache(ws_dir.path(), src_dir.path()).expect("sanitize");
        assert!(count >= 3, "expected >= 3 sanitized files, got {count}");

        let pytest = fs::read_to_string(venv_scripts.join("pytest")).expect("read pytest");
        assert!(pytest.starts_with(&format!("#!{ws_prefix}/.venv/Scripts/python.exe\n")));
        assert!(!pytest.contains(r"\\?\"));

        let activate_bat =
            fs::read_to_string(venv_scripts.join("activate.bat")).expect("read activate.bat");
        assert!(activate_bat.contains(&ws_prefix));
        assert!(!activate_bat.contains(r"\\?\"));

        let activate_ps1 =
            fs::read_to_string(venv_scripts.join("Activate.ps1")).expect("read Activate.ps1");
        assert!(activate_ps1.contains(&ws_prefix));
        assert!(!activate_ps1.contains(r"\\?\"));
    }
}
