//! Parallel stat-cached working-copy snapshotting with `.gitignore` and `.git/info/exclude` filtering.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use core::{
    atomic_write_file_relaxed, validate_safe_identifier, CoreError, CoreResult, FileEntry,
    GitBackend, ObjectId,
};
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
    /// File size in bytes.
    pub size: u64,
    /// Filesystem inode number (0 on non-Unix platforms).
    #[serde(default)]
    pub inode: u64,
    /// Executable permission bit.
    pub executable: bool,
    /// Previously written Git blob [`ObjectId`].
    pub blob_id: ObjectId,
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

/// Default build-artifact and VCS directories always excluded from working-copy snapshots.
const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".tessera",
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
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".gradle",
    ".zig-cache",
    ".dart_tool",
];

/// Returns `true` if `file_name` matches a built-in secret or credential file pattern.
///
/// Built-in secret patterns protect untracked `.env`, `.envrc`, `.env.*` (excluding `.env.example`,
/// `.env.sample`, and `.env.template`), `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`,
/// `id_rsa`, `id_ed25519`, `id_ecdsa`, `id_dsa`, `.npmrc`, `.pypirc`, `.netrc`, and `credentials.json`
/// files from accidental ingestion into `.git/objects`.
fn is_builtin_secret_filename(file_name: &str) -> bool {
    if matches!(
        file_name,
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
    ) || file_name.ends_with(".pem")
        || file_name.ends_with(".key")
        || file_name.ends_with(".p12")
        || file_name.ends_with(".pfx")
        || file_name.ends_with(".jks")
        || file_name.ends_with(".keystore")
    {
        return true;
    }

    if file_name.starts_with(".env.") {
        return !matches!(
            file_name,
            ".env.example" | ".env.sample" | ".env.template"
        );
    }

    false
}

/// Evaluates whether a repository-relative path should be excluded from automatic ingestion
/// into the Git object store.
///
/// When `is_tracked` is `true` (the path already exists in the base commit tree), this function
/// only rejects `.git` and `.tessera` internals so intentionally tracked files continue to update.
/// When `is_tracked` is `false`, this function rejects paths inside [`IGNORED_DIRS`], built-in
/// secret filenames, and paths matching `.gitignore` or `.git/info/exclude`.
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

    for segment in trimmed.split('/') {
        if segment == ".git" || segment == ".tessera" || segment.eq_ignore_ascii_case("git~1") {
            return true;
        }

        if !is_tracked && IGNORED_DIRS.contains(&segment) {
            return true;
        }
    }

    if is_tracked {
        return false;
    }

    let mut rules = load_ignore_rules(working_dir, git_dir);
    let mut prefix = String::new();
    if let Some((parent, _)) = trimmed.rsplit_once('/') {
        for segment in parent.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            let nested_gitignore = working_dir.join(&prefix).join(".gitignore");
            if nested_gitignore.is_file() {
                append_ignore_rules_from_file(&nested_gitignore, &prefix, &mut rules);
            }
        }
    }

    is_path_ignored(trimmed, false, &rules)
}

/// Parsed `.gitignore` or `.git/info/exclude` rule entry.
#[derive(Debug, Clone)]
pub(crate) struct IgnoreRule {
    /// Relative directory prefix where this rule was defined (empty for repository root).
    base_dir: String,
    /// `true` if the rule starts with `!` and re-includes previously ignored paths.
    negated: bool,
    /// `true` if the rule ends with `/` and matches only directories.
    dir_only: bool,
    /// Normalized glob or path pattern.
    pattern: String,
}

/// Loads ignore rules from `.git/info/exclude` and `<working_dir>/.gitignore`.
pub(crate) fn load_ignore_rules(working_dir: &Path, git_dir: &Path) -> Vec<IgnoreRule> {
    let mut rules = Vec::new();
    for path in [
        git_dir.join("info/exclude"),
        working_dir.join(".gitignore"),
    ] {
        append_ignore_rules_from_file(&path, "", &mut rules);
    }
    rules
}

/// Appends parsed `.gitignore` rules from `path` scoped to `base_dir`.
pub(crate) fn append_ignore_rules_from_file(
    path: &Path,
    base_dir: &str,
    rules: &mut Vec<IgnoreRule>,
) {
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };

    append_ignore_rules_from_str(&raw, base_dir, rules);
}

/// Appends parsed `.gitignore` rules from `raw` text scoped to `base_dir`.
pub(crate) fn append_ignore_rules_from_str(
    raw: &str,
    base_dir: &str,
    rules: &mut Vec<IgnoreRule>,
) {
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (negated, rest) = if let Some(stripped) = trimmed.strip_prefix('!') {
            (true, stripped.trim())
        } else {
            (false, trimmed)
        };

        let (dir_only, pat) = if let Some(stripped) = rest.strip_suffix('/') {
            (true, stripped.trim_start_matches('/').to_string())
        } else {
            (false, rest.trim_start_matches('/').to_string())
        };

        if !pat.is_empty() {
            rules.push(IgnoreRule {
                base_dir: base_dir.to_string(),
                negated,
                dir_only,
                pattern: pat,
            });
        }
    }
}

/// Evaluates whether `rel_path` is ignored according to `rules` (and built-in OS metadata and secret exclusions).
pub(crate) fn is_path_ignored(rel_path: &str, is_dir: bool, rules: &[IgnoreRule]) -> bool {
    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    if file_name == ".DS_Store" {
        return true;
    }

    let mut ignored = !is_dir && is_builtin_secret_filename(file_name);
    for rule in rules {
        let scoped_rel = if rule.base_dir.is_empty() {
            rel_path
        } else if let Some(rest) = rel_path
            .strip_prefix(&rule.base_dir)
            .and_then(|s| s.strip_prefix('/'))
        {
            rest
        } else {
            continue;
        };

        if rule.dir_only && !is_dir {
            let has_matching_parent = if rule.pattern.contains('/') {
                scoped_rel.starts_with(&format!("{}/", rule.pattern))
            } else {
                scoped_rel.rsplit_once('/').is_some_and(|(parent, _)| {
                    parent
                        .split('/')
                        .any(|seg| glob_match(&rule.pattern, seg))
                })
            };
            if !has_matching_parent {
                continue;
            }
        }

        let matched = if rule.pattern.contains('/') {
            glob_match(&rule.pattern, scoped_rel)
                || scoped_rel.starts_with(&format!("{}/", rule.pattern))
        } else {
            glob_match(&rule.pattern, file_name)
                || glob_match(&rule.pattern, scoped_rel)
                || scoped_rel
                    .split('/')
                    .any(|segment| glob_match(&rule.pattern, segment))
        };

        if matched {
            ignored = !rule.negated;
        }
    }
    ignored
}

/// Evaluates a wildcard glob pattern (supporting multiple `*` and `**/` segments) against `candidate`.
fn glob_match(pattern: &str, candidate: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix("**/") {
        if glob_match(rest, candidate) {
            return true;
        }

        for (idx, ch) in candidate.char_indices() {
            if ch == '/' && glob_match(rest, &candidate[idx + 1..]) {
                return true;
            }
        }
    }

    if let Some((head, tail)) = pattern.split_once("/**/") {
        if glob_match(&format!("{head}/{tail}"), candidate) {
            return true;
        }
    }

    if !pattern.contains('*') {
        return pattern == candidate;
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rem = candidate;

    if let Some(first) = parts.first() {
        if !first.is_empty() {
            let Some(after_first) = rem.strip_prefix(first) else {
                return false;
            };
            rem = after_first;
        }
    }

    if let Some(last) = parts.last() {
        if !last.is_empty() && parts.len() > 1 {
            let Some(before_last) = rem.strip_suffix(last) else {
                return false;
            };
            rem = before_last;
        }
    }

    for part in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        if part.is_empty() {
            continue;
        }

        let Some(pos) = rem.find(part) else {
            return false;
        };
        rem = &rem[pos + part.len()..];
    }

    true
}

/// Incrementally snapshots `working_dir` into `git` using the stat cache identified by `cache_key`.
///
/// If `base_tree_for_scoped` is provided alongside non-empty `scopes`, any paths outside `scopes`
/// are preserved from `base_tree_for_scoped` so scoped agent workspaces do not delete unmaterialized
/// directories or packages.
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
    let prev_cache: BTreeMap<String, CachedStat> = if cache_path.is_file() {
        fs::read(&cache_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };

    let ignore_rules = std::cell::RefCell::new(load_ignore_rules(working_dir, git.git_dir()));
    let base_map_opt = match base_tree_for_scoped {
        Some(base_tid) => Some(git.read_tree_to_map(base_tid)?),
        None => None,
    };

    let mut discovered: Vec<(String, PathBuf, u64, u32, u64, u64, bool)> = Vec::new();
    let walker = WalkDir::new(working_dir).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }

        if e.file_type().is_dir() {
            if let Some(name) = e.file_name().to_str() {
                if IGNORED_DIRS.contains(&name) {
                    return false;
                }
            }

            if let Ok(rel) = e.path().strip_prefix(working_dir) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if is_path_ignored(&rel_str, true, &ignore_rules.borrow()) {
                    return false;
                }

                let nested_gitignore = e.path().join(".gitignore");
                if nested_gitignore.is_file() {
                    append_ignore_rules_from_file(
                        &nested_gitignore,
                        &rel_str,
                        &mut ignore_rules.borrow_mut(),
                    );
                }
            }
        }
        true
    });

    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }

        let Ok(rel) = entry.path().strip_prefix(working_dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() || is_path_ignored(&rel_str, false, &ignore_rules.borrow()) {
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let (mtime_secs, mtime_nanos) = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));

        #[cfg(unix)]
        let (executable, inode) = {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            (meta.permissions().mode() & 0o111 != 0, meta.ino())
        };
        #[cfg(not(unix))]
        let (executable, inode) = (false, 0u64);

        discovered.push((
            rel_str,
            entry.path().to_path_buf(),
            mtime_secs,
            mtime_nanos,
            meta.len(),
            inode,
            executable,
        ));
    }

    if let Some(ref base_map) = base_map_opt {
        let discovered_paths: std::collections::BTreeSet<String> =
            discovered.iter().map(|(p, ..)| p.clone()).collect();
        for tracked_path in base_map.keys() {
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
            let full_path = working_dir.join(tracked_path);
            let Ok(meta) = fs::symlink_metadata(&full_path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let (mtime_secs, mtime_nanos) = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| (d.as_secs(), d.subsec_nanos()))
                .unwrap_or((0, 0));

            #[cfg(unix)]
            let (executable, inode) = {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                (meta.permissions().mode() & 0o111 != 0, meta.ino())
            };
            #[cfg(not(unix))]
            let (executable, inode) = (false, 0u64);

            discovered.push((
                tracked_path.clone(),
                full_path,
                mtime_secs,
                mtime_nanos,
                meta.len(),
                inode,
                executable,
            ));
        }
    }

    let results: Vec<CoreResult<(String, CachedStat, bool)>> = discovered
        .into_par_iter()
        .map(
            |(rel_str, full_path, mtime_secs, mtime_nanos, size, inode, executable)| {
                if let Some(cached) = prev_cache.get(&rel_str) {
                    if cached.mtime_secs == mtime_secs
                        && cached.mtime_nanos == mtime_nanos
                        && cached.size == size
                        && cached.inode == inode
                        && cached.executable == executable
                    {
                        return Ok((rel_str, cached.clone(), false));
                    }
                }

                let bytes = fs::read(&full_path).map_err(|source| CoreError::Io {
                    path: full_path.display().to_string(),
                    source,
                })?;
                let blob_id = git.write_blob(&bytes)?;
                Ok((
                    rel_str,
                    CachedStat {
                        mtime_secs,
                        mtime_nanos,
                        size,
                        inode,
                        executable,
                        blob_id,
                    },
                    true,
                ))
            },
        )
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

    let mut rehashed_count = 0usize;
    let mut cached_count = 0usize;

    for res in results {
        let (rel_str, stat, was_rehashed) = res?;
        if was_rehashed {
            rehashed_count += 1;
        } else {
            cached_count += 1;
        }
        files.insert(
            rel_str.clone(),
            FileEntry {
                blob_id: stat.blob_id,
                executable: stat.executable,
            },
        );
        new_cache.insert(rel_str, stat);
    }

    if rehashed_count > 0 || new_cache.len() != prev_cache.len() {
        if let Ok(serialized) = serde_json::to_vec(&new_cache) {
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
        fs::write(tmp.path().join(".env.production"), "PROD_KEY=secret\n").expect("write .env.prod");
        fs::write(tmp.path().join(".env.example"), "API_KEY=\n").expect("write .env.example");
        fs::write(tmp.path().join("cert.pem"), "PRIVATE KEY\n").expect("write pem");

        let snap = snapshot_working_dir(&git, tmp.path(), "test-secrets", &[], None).expect("snap");
        assert!(!snap.files.contains_key(".env"));
        assert!(!snap.files.contains_key(".env.production"));
        assert!(!snap.files.contains_key("cert.pem"));
        assert!(snap.files.contains_key(".env.example"));

        assert!(is_ignored_or_secret_path(
            tmp.path(),
            git.git_dir(),
            ".env",
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
}
