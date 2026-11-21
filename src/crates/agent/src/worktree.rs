//! Working-tree file I/O helpers, cross-process advisory file locking, binary detection,
//! conflict marker scanning, and session attribution predicates.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use core::{
    AgentMetadata, CoreError, CoreResult, FileEntry, resolve_confined_repo_path,
    write_working_tree_file_safe,
};

/// Acquires an exclusive cross-process OS file lock on `lock_path` (with `EINTR` retry)
/// for the lifetime of the returned [`fs::File`] handle.
///
/// # Errors
/// Returns [`CoreError::Io`] if parent directory creation, opening, or locking `lock_path` fails.
pub(crate) fn acquire_file_lock(lock_path: &Path) -> CoreResult<fs::File> {
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
        .open(lock_path)
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

/// Returns `true` if `bytes` appear to be binary (contain a NUL byte or invalid UTF-8).
#[must_use]
pub(crate) fn is_binary_bytes(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

/// Returns `true` if `a` and `b` refer to the same filesystem path (accounting for OS symlinks
/// such as `/var` vs `/private/var` on macOS).
#[must_use]
pub(crate) fn same_canonical_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }

    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Writes `bytes` safely to the working-tree file `path` (refusing symlinks), creating parent
/// directories and setting the executable bit to match `executable`.
///
/// # Errors
/// Returns [`CoreError`] if `path` is a symlink or cannot be written.
pub(crate) fn write_worktree_file_checked(
    path: &Path,
    bytes: &[u8],
    executable: bool,
) -> CoreResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| CoreError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }

    write_working_tree_file_safe(path, bytes)?;
    set_worktree_executable(path, executable);
    Ok(())
}

/// Writes `bytes` to the working-tree file `path`, creating parent directories, and sets the
/// executable bit to match `executable` so `git status` never reports `100644`/`100755` drift.
///
/// Errors are ignored for best-effort callers that re-snapshot afterwards.
pub(crate) fn write_worktree_file(path: &Path, bytes: &[u8], executable: bool) {
    let _ = write_worktree_file_checked(path, bytes, executable);
}

/// Writes `bytes` to `rel_path` under `root` via [`write_worktree_file`] after confining the path
/// to `root` (rejecting traversal and symlinks). Invalid paths are ignored.
pub(crate) fn write_worktree_rel(root: &Path, rel_path: &str, bytes: &[u8], executable: bool) {
    if let Ok((_, full_path)) = resolve_confined_repo_path(root, rel_path) {
        write_worktree_file(&full_path, bytes, executable);
    }
}

/// Sets or clears the executable bits of `path` the way Git does on checkout (every readable
/// class gains or loses `x`), leaving the file untouched if it already matches.
pub(crate) fn set_worktree_executable(path: &Path, executable: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Ok(meta) = fs::metadata(path) {
            let mode = meta.permissions().mode();
            let new_mode = if executable {
                mode | ((mode & 0o444) >> 2)
            } else {
                mode & !0o111
            };

            if new_mode != mode {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(new_mode));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
}

/// Returns `true` if the working-tree file at `path` has any executable bit set.
#[must_use]
pub(crate) fn worktree_is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Removes the working-tree file `rel_path` under `root` (if present) and prunes any parent
/// directories left empty, stopping at `root`. Paths escaping `root` are ignored.
pub(crate) fn remove_worktree_file(root: &Path, rel_path: &str) {
    let Ok((_, full_path)) = resolve_confined_repo_path(root, rel_path) else {
        return;
    };

    if !full_path.is_file() {
        return;
    }

    let _ = fs::remove_file(&full_path);
    let mut parent_opt = full_path.parent();

    while let Some(parent) = parent_opt {
        if parent == root || !parent.starts_with(root) || fs::remove_dir(parent).is_err() {
            break;
        }

        parent_opt = parent.parent();
    }
}

/// Returns `true` if `session_id` identifies an AI agent session rather than a human terminal
/// (`term-*`, `session-*`) or editor session.
#[must_use]
pub(crate) fn is_ai_session_id(session_id: &str) -> bool {
    !session_id.starts_with("term-")
        && !session_id.starts_with("session-")
        && session_id != "editor"
}

/// Returns `true` if an unhooked working-tree change at `rel_path` may be attributed to `meta`'s
/// session by a snapshot (`tes status` / `tes commit`) or by the background watcher.
///
/// Attribution rules, in precedence order:
/// - Paths the session authored (`own_blobs` / `deleted_paths`) always belong to it.
/// - Paths in the session's `foreign_baseline` (changes nobody owned when an AI session started)
///   never do, so a user's in-progress edits are not committed under an agent's name.
/// - Human terminal/editor sessions sweep every remaining unowned change, like `git add -A`.
/// - AI sessions with explicit claims only sweep paths they claimed, read, or created under a
///   claimed directory; AI sessions without any claim sweep every remaining unowned change.
#[must_use]
pub(crate) fn session_may_sweep(
    meta: &AgentMetadata,
    rel_path: &str,
    base_files: &BTreeMap<String, FileEntry>,
) -> bool {
    if meta.owns_path(rel_path) {
        return true;
    }

    if meta.foreign_baseline.contains(rel_path) {
        return false;
    }

    if !is_ai_session_id(&meta.agent_id) || meta.claimed_symbols.is_empty() {
        return true;
    }

    session_claims_path(meta, rel_path, base_files)
}

/// Returns `true` if `rel_path` is an AI harness or IDE configuration file that should be
/// excluded from passive working-tree session ownership.
#[must_use]
pub(crate) fn is_harness_config_path(rel_path: &str) -> bool {
    matches!(
        rel_path,
        "AGENTS.md"
            | "AGENTS.override.md"
            | "CLAUDE.md"
            | ".mcp.json"
            | ".windsurfrules"
            | "opencode.json"
    ) || rel_path.starts_with(".agents/")
        || rel_path.starts_with(".claude/")
        || rel_path.starts_with(".cursor/")
        || rel_path.starts_with(".windsurf/")
        || rel_path.starts_with(".kiro/")
        || rel_path.starts_with(".opencode/")
        || rel_path.starts_with(".codex/")
        || rel_path.starts_with(".vscode/")
}

/// Returns `true` if `agent` owns, read, or claimed `rel_path` (including directory prefix claims
/// or newly created sibling files inside a brand-new top-level directory).
#[must_use]
pub(crate) fn session_claims_path(
    agent: &AgentMetadata,
    rel_path: &str,
    base_files: &BTreeMap<String, FileEntry>,
) -> bool {
    if agent.owns_path(rel_path) || agent.read_blobs.contains_key(rel_path) {
        return true;
    }

    for s in &agent.claimed_symbols {
        let target = s
            .split("::")
            .next()
            .unwrap_or(s.as_str())
            .trim()
            .trim_end_matches('/');

        if target == rel_path || (!target.is_empty() && rel_path.starts_with(&format!("{target}/")))
        {
            return true;
        }
    }

    if !base_files.contains_key(rel_path) {
        if let Some((top_dir, _)) = rel_path.split_once('/') {
            let dir_prefix = format!("{top_dir}/");
            let existed_in_base = base_files.keys().any(|k| k.starts_with(&dir_prefix));

            if !existed_in_base {
                let claimed_same_new_top_dir = agent.claimed_symbols.iter().any(|s| {
                    let target = s
                        .split("::")
                        .next()
                        .unwrap_or(s.as_str())
                        .trim()
                        .trim_end_matches('/');
                    target == top_dir || target.starts_with(&dir_prefix)
                }) || agent
                    .own_blobs
                    .keys()
                    .any(|k| k == top_dir || k.starts_with(&dir_prefix));

                if claimed_same_new_top_dir {
                    return true;
                }
            }
        }
    }

    false
}

/// Validates that the label suffix following a column-0 `<<<<<<< ` or `>>>>>>> ` marker matches
/// standard Git conflict marker label formatting.
fn is_valid_conflict_label(label: &str) -> bool {
    let trimmed = label.trim();

    if trimmed.is_empty() {
        return false;
    }

    !trimmed.starts_with('<')
        && !trimmed.starts_with('>')
        && !trimmed.contains("<<<<<<<")
        && !trimmed.contains(">>>>>>>")
}

/// Returns `true` if `text` contains a complete standard Git conflict marker block starting at
/// column 0: `<<<<<<< <label>`, followed by exact column-0 `=======`, followed by column-0
/// `>>>>>>> <label>`.
///
/// Strict column-0 matching and non-empty label validation prevent Markdown Setext heading
/// underlines (`=======`) or indented documentation examples from triggering false positives.
#[must_use]
pub(crate) fn has_git_conflict_markers(text: &str) -> bool {
    let mut state: u8 = 0;

    for line in text.lines() {
        if let Some(label) = line.strip_prefix("<<<<<<< ") {
            if is_valid_conflict_label(label) {
                state = 1;
            }
        } else if state == 1 && line == "=======" {
            state = 2;
        } else if state == 2 {
            if let Some(label) = line.strip_prefix(">>>>>>> ") {
                if is_valid_conflict_label(label) {
                    return true;
                }
            }
        }
    }

    false
}
