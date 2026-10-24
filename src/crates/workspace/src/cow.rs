//! Copy-on-Write filesystem cloning (`clonefile` on macOS, `FICLONE` on Linux, and portable fallback),
//! build cache path sanitization, and working-tree materialization.

use std::fs;
use std::path::{Component, Path};

use core::{
    resolve_confined_repo_path, write_working_tree_file_safe, CoreError, CoreResult, GitBackend,
    ObjectId,
};
use walkdir::WalkDir;

/// Returns `true` if `link_dest` is a relative path that does not contain `..` or root prefixes.
fn is_safe_relative_symlink_target(link_dest: &Path) -> bool {
    !link_dest.is_absolute()
        && !link_dest.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

/// Returns `true` if `link_dest` resolved from `link_path` stays strictly within `root_dir`.
fn is_safe_symlink_within_root(link_path: &Path, link_dest: &Path, root_dir: &Path) -> bool {
    if link_dest.is_absolute() {
        return false;
    }

    if is_safe_relative_symlink_target(link_dest) {
        return true;
    }

    let Some(parent) = link_path.parent() else {
        return false;
    };
    let Ok(rel_parent) = parent.strip_prefix(root_dir) else {
        return false;
    };

    let mut depth: isize = 0;

    for comp in rel_parent.components() {
        if matches!(comp, Component::Normal(_)) {
            depth += 1;
        }
    }

    for comp in link_dest.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(_) => {
                depth += 1;
            }
            Component::ParentDir => {
                depth -= 1;

                if depth < 0 {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return false;
            }
        }
    }

    true
}

/// Walks `dir` after a kernel-level directory clone and removes any symbolic links that
/// point outside `confinement_root`.
fn prune_unsafe_symlinks_in_dir(dir: &Path, confinement_root: &Path) {
    let walker = WalkDir::new(dir).min_depth(1).into_iter().filter_entry(|e| {
        if e.file_type().is_dir() {
            if let Some(name) = e.file_name().to_str() {
                if matches!(name, "incremental" | ".fingerprint" | "doc") {
                    return false;
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
                let _ = fs::remove_file(link_path);
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

    let src_c = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "interior NUL in src"))?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "interior NUL in dst"))?;

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
    const FICLONE: libc::c_ulong = 0x4004_9409;

    let src_file = fs::File::open(src)?;
    let dst_file = fs::File::create(dst)?;

    // SAFETY: `src_file` and `dst_file` are valid, open file descriptors owned by this stack frame,
    // and `FICLONE` takes an integer source fd by value without dereferencing user pointers.
    let rc = unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) };

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

/// Copies a single file from `src` to `dst`, attempting a Linux `FICLONE` reflink first when on Linux.
fn copy_or_reflink_file(src: &Path, dst: &Path) -> CoreResult<bool> {
    #[cfg(target_os = "linux")]
    {
        if try_linux_reflink_file(src, dst).is_ok() {
            return Ok(true);
        }
    }

    fs::copy(src, dst).map_err(|source| CoreError::Io {
        path: dst.display().to_string(),
        source,
    })?;

    Ok(false)
}

/// Recursively copies `src` to `dst` when a whole-directory `clonefile` syscall is unavailable.
fn copy_recursive_fallback(
    src: &Path,
    dst: &Path,
    confinement_root: &Path,
) -> CoreResult<bool> {
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

        return Ok(false);
    }

    if src_meta.file_type().is_file() {
        return copy_or_reflink_file(src, dst);
    }

    fs::create_dir_all(dst).map_err(|source| CoreError::Io {
        path: dst.display().to_string(),
        source,
    })?;

    let mut any_reflinked = false;

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
                        let _ = std::os::unix::fs::symlink(&link_dest, &target);
                    }
                }
            }
        } else if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(|source| CoreError::Io {
                path: target.display().to_string(),
                source,
            })?;
        } else if entry.file_type().is_file() {
            if let Some(p) = target.parent() {
                fs::create_dir_all(p).map_err(|source| CoreError::Io {
                    path: p.display().to_string(),
                    source,
                })?;
            }

            if copy_or_reflink_file(entry.path(), &target)? {
                any_reflinked = true;
            }
        }
    }

    Ok(any_reflinked)
}

/// Ensures any warm dependency or build caches (`node_modules`, `target`, `.venv`, `vendor`, plus
/// configured `cow_cache_dirs`) that exist in `src_root` are COW-cloned into `dst_root` if missing.
///
/// This handles the common workflow where a branch sandbox is created before `npm install` / `pnpm install`
/// is executed in `repo_root` (or vice versa).
///
/// # Errors
/// Returns [`CoreError::Io`] if cloning a cache directory fails.
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
            src_cache.join(".bin").is_dir() && !dst_cache.join(".bin").exists()
        } else {
            false
        };

        if needs_clone {
            if dst_cache.exists() {
                let _ = fs::remove_dir_all(&dst_cache);
            }
            clone_path_cow(&src_cache, &dst_cache)?;
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
/// - Skips `.git` and `.tessera` internal directories.
/// - Respects optional directory `scopes` (always including root config files plus matching prefixes).
/// - Clones warm build caches (`cow_cache_dirs` like `target/` and `node_modules/`) via COW and
///   runs [`sanitize_build_cache`] to strip path-poisoned `.d` stamp files.
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

        if name_str == ".git" || name_str == ".tessera" {
            continue;
        }

        let src_path = entry.path();
        let dst_path = workspace_dir.join(name_str);
        let file_type = entry.file_type().map_err(|source| CoreError::Io {
            path: src_path.display().to_string(),
            source,
        })?;

        if file_type.is_symlink() {
            #[cfg(unix)]
            {
                if let Ok(link_dest) = fs::read_link(&src_path) {
                    if is_safe_symlink_within_root(&dst_path, &link_dest, workspace_dir) {
                        let _ = std::os::unix::fs::symlink(&link_dest, &dst_path);
                        cloned_entries += 1;
                    }
                }
            }
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
            // Selective directory scope cloning
            for scope in scopes {
                let normalized = scope.trim_matches('/');

                if normalized == name_str || normalized.starts_with(&format!("{name_str}/")) {
                    let Ok((_, sub_src)) = resolve_confined_repo_path(repo_root, normalized) else {
                        continue;
                    };
                    let sub_dst = workspace_dir.join(normalized);

                    if sub_src.exists() && !sub_dst.exists() {
                        clone_path_cow_within(&sub_src, &sub_dst, workspace_dir)?;
                        cloned_entries += 1;
                    }
                }
            }
        }
    }

    let _ = sanitize_build_cache(workspace_dir, repo_root)?;
    Ok(cloned_entries)
}

/// Sanitizes cloned build caches (`target/`, `.turbo/`) by removing `.d` compiler dependency-info
/// stamp files that embed hardcoded absolute paths to `parent_repo_root`.
///
/// This prevents `rustc`/`cargo`/bundlers inside an agent's COW workspace from reading or
/// invalidating files in the parent workspace while preserving compiled `.rlib`/`.rmeta` artifacts.
///
/// # Errors
/// Returns [`CoreError::Io`] on directory walk errors.
pub fn sanitize_build_cache(workspace_dir: &Path, parent_repo_root: &Path) -> CoreResult<usize> {
    let parent_str = parent_repo_root.to_string_lossy();
    let mut sanitized_count = 0usize;

    for cache_name in [
        "target",
        ".turbo",
        "build",
        ".build",
        "cmake-build-debug",
        "cmake-build-release",
    ] {
        let cache_dir = workspace_dir.join(cache_name);

        if !cache_dir.is_dir() {
            continue;
        }

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

            if path.extension().and_then(|e| e.to_str()) == Some("d") && path.is_file() {
                if let Ok(content) = fs::read_to_string(path) {
                    if content.contains(parent_str.as_ref()) && fs::remove_file(path).is_ok() {
                        sanitized_count += 1;
                    }
                }
            }
        }
    }

    Ok(sanitized_count)
}

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

/// Materializes all files from `tree_id` into `target_dir` atomically, removing tracked files
/// that are no longer present in `tree_id`.
///
/// # Errors
/// Returns [`CoreError`] if any path escapes `target_dir` or if reading blobs or writing files fails.
pub fn materialize_tree_to_dir(
    git: &GitBackend,
    tree_id: &ObjectId,
    target_dir: &Path,
) -> CoreResult<()> {
    let tree_files = git.read_tree_to_map(tree_id)?;
    fs::create_dir_all(target_dir).map_err(|source| CoreError::Io {
        path: target_dir.display().to_string(),
        source,
    })?;

    let extra_preserved = core::OpLogStore::new(git.git_dir())
        .load_config()
        .map(|c| c.cow_cache_dirs)
        .unwrap_or_default();

    // Remove stale files on disk that are not present in `tree_files` (skipping `.git`, `.tessera`, harness configs, and build caches).
    let walker = WalkDir::new(target_dir).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }

        if e.file_type().is_dir() {
            if let Some(name) = e.file_name().to_str() {
                if PRESERVED_TOP_DIRS.contains(&name)
                    || name == "cache"
                    || name == "dist"
                    || extra_preserved.iter().any(|d| d == name)
                {
                    return false;
                }
            }
        }

        true
    });

    let mut stale_files = Vec::new();

    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }

        if let Ok(rel) = entry.path().strip_prefix(target_dir) {
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            if !rel_str.is_empty()
                && !tree_files.contains_key(&rel_str)
                && !PRESERVED_ROOT_FILES.contains(&rel_str.as_str())
            {
                stale_files.push(entry.path().to_path_buf());
            }
        }
    }

    for stale in stale_files {
        let _ = fs::remove_file(&stale);
        let mut parent_opt = stale.parent();

        while let Some(parent) = parent_opt {
            if parent == target_dir {
                break;
            }

            if fs::remove_dir(parent).is_err() {
                break;
            }

            parent_opt = parent.parent();
        }
    }

    for (rel_path, entry) in &tree_files {
        let (_, full_path) = resolve_confined_repo_path(target_dir, rel_path)?;

        if full_path.is_file() {
            if let Ok(existing) = fs::read(&full_path) {
                if GitBackend::hash_blob_in_memory(&existing) == entry.blob_id {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let target_mode = if entry.executable { 0o755 } else { 0o644 };
                        if let Ok(meta) = fs::metadata(&full_path) {
                            if ((meta.permissions().mode() & 0o111) != 0) != entry.executable {
                                let _ = fs::set_permissions(
                                    &full_path,
                                    fs::Permissions::from_mode(target_mode),
                                );
                            }
                        }
                    }
                    continue;
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Create a simulated warm `target/debug/deps/app.d` file containing parent absolute path
        let deps_dir = src_dir.path().join("target/debug/deps");
        fs::create_dir_all(&deps_dir).expect("mkdir deps");
        let poisoned_d = format!(
            "{}/packages/api/lib.rs: src/lib.rs\n",
            src_dir.path().display()
        );
        fs::write(deps_dir.join("app.d"), poisoned_d).expect("write .d");
        fs::write(deps_dir.join("libapp.rlib"), b"compiled_artifact").expect("write .rlib");

        let count = create_cow_workspace(
            src_dir.path(),
            &dst_dir,
            &[],
            &["target".to_string()],
        )
        .expect("create cow workspace");
        assert!(count >= 3);
        assert!(dst_dir.join("packages/api/lib.rs").is_file());
        // Warm `.rlib` is preserved while poisoned `.d` file is sanitized!
        assert!(dst_dir.join("target/debug/deps/libapp.rlib").is_file());
        assert!(!dst_dir.join("target/debug/deps/app.d").exists());
    }
}
