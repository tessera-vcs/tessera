//! Lightweight background micro-watcher daemon that observes intermediate disk blobs for
//! claimed files into `view.observed_blobs` without ever mutating or reverting working-tree files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use core::{CoreResult, ObjectId, atomic_write_file, resolve_confined_repo_path};
use workspace::is_ignored_or_secret_path;

use crate::Coordinator;

/// Maximum number of intermediate observed blobs retained per file path in the `OpLog` `View`.
pub(crate) const MAX_OBSERVED_BLOBS_PER_PATH: usize = 64;

/// Default polling interval in milliseconds for the background micro-watcher daemon.
const DEFAULT_WATCH_INTERVAL_MS: u64 = 75;

/// Attempts to acquire a non-blocking exclusive cross-platform advisory file lock on `lock_path`.
///
/// Returns `Some(File)` if this process acquired the lock, or `None` if another watcher
/// process currently holds the lock.
fn try_acquire_watcher_lock(lock_path: &Path) -> Option<fs::File> {
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;

    match file.try_lock() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

/// Ensures a background file watcher (`tes watch --auto-daemon`) is running for `repo_root`
/// so rapid concurrent file writes are captured in `.git/objects` before `post-tool` hooks execute.
///
/// `git_dir` should be the repository's shared common Git directory (`common_git_dir`).
/// On Windows, background daemon spawning is skipped because child processes inherit parent pipe
/// handles and hold open working-directory locks for the daemon lifetime.
pub fn ensure_background_watcher(repo_root: &Path, git_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let lock_file = git_dir.join("tessera").join("watch.lock");
        let probe = try_acquire_watcher_lock(&lock_file);

        if probe.is_none() {
            return;
        }

        drop(probe);

        let exe_opt = std::env::current_exe().ok().filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem == "tes")
        });

        if let Some(bin) = exe_opt {
            let mut cmd = std::process::Command::new(bin);
            cmd.args(["watch", "--auto-daemon"])
                .current_dir(repo_root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0);

            let _ = cmd.spawn();
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (repo_root, git_dir);
    }
}

/// Runs the lightweight background micro-watcher that records intermediate disk blobs for
/// claimed files into `view.observed_blobs`, exiting automatically once all active sessions
/// close or after the configured `watch_timeout_secs` (default 90 seconds).
///
/// This daemon is strictly read-only with respect to working-tree files on disk: it only writes
/// content-addressed blobs to `.git/objects` and records their IDs in `view.observed_blobs`.
///
/// # Errors
/// Returns [`core::CoreError`] if reading or updating the `OpLog` fails.
pub fn run_auto_watch_daemon(coord: &Coordinator) -> CoreResult<()> {
    let common_git_dir = coord.git().common_git_dir();
    let lock_file = common_git_dir.join("tessera").join("watch.lock");
    let Some(_lock_guard) = try_acquire_watcher_lock(&lock_file) else {
        return Ok(());
    };

    let pid_file = common_git_dir.join("tessera").join("watch.pid");
    let my_pid = std::process::id().to_string();
    let _ = atomic_write_file(&pid_file, my_pid.as_bytes());

    let cfg = coord.oplog().load_config().unwrap_or_default();
    let poll_ms = if cfg.watch_interval_ms == 0 {
        DEFAULT_WATCH_INTERVAL_MS
    } else {
        cfg.watch_interval_ms.max(15)
    };
    let timeout_ms = cfg.watch_timeout_secs.max(5).saturating_mul(1_000);
    let max_ticks = (timeout_ms / poll_ms).max(1);

    let mut last_stat: BTreeMap<String, (u64, u64, u32, ObjectId)> = BTreeMap::new();

    for _ in 0..max_ticks {
        let Ok(current) = coord.oplog().load_current_reconciled() else {
            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
            continue;
        };

        if current.view.agents.is_empty() {
            break;
        }

        let mut watched_paths = BTreeSet::new();

        for agent in current.view.agents.values() {
            for p in agent.read_blobs.keys() {
                watched_paths.insert(p.clone());
            }

            for sym in &agent.claimed_symbols {
                let file_part = sym.split("::").next().unwrap_or(sym.as_str()).trim();

                if !file_part.is_empty() {
                    watched_paths.insert(file_part.to_string());
                }
            }
        }

        for rel_path in watched_paths {
            let Ok((clean_rel, full_path)) =
                resolve_confined_repo_path(coord.git().repo_root(), &rel_path)
            else {
                continue;
            };

            let Ok(meta) = fs::symlink_metadata(&full_path) else {
                continue;
            };

            if !meta.file_type().is_file() {
                continue;
            }

            let size = meta.len();
            let (mtime_secs, mtime_nanos) = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| (d.as_secs(), d.subsec_nanos()))
                .unwrap_or((0, 0));

            if let Some(&(prev_size, prev_secs, prev_nanos, prev_blob)) = last_stat.get(&clean_rel)
            {
                if prev_size == size && prev_secs == mtime_secs && prev_nanos == mtime_nanos {
                    let already_recorded = current
                        .view
                        .observed_blobs
                        .get(&clean_rel)
                        .is_some_and(|list| list.contains(&prev_blob));

                    if already_recorded {
                        continue;
                    }
                }
            }

            let is_tracked = current
                .view
                .agents
                .values()
                .any(|a| a.read_blobs.contains_key(&clean_rel));

            if is_ignored_or_secret_path(
                coord.git().repo_root(),
                common_git_dir,
                &clean_rel,
                is_tracked,
            ) {
                continue;
            }

            let Ok(bytes) = fs::read(&full_path) else {
                continue;
            };

            let Ok(blob_id) = coord.git().write_blob(&bytes) else {
                continue;
            };

            last_stat.insert(clean_rel.clone(), (size, mtime_secs, mtime_nanos, blob_id));

            let already_recorded = current
                .view
                .observed_blobs
                .get(&clean_rel)
                .is_some_and(|list| list.contains(&blob_id));

            if !already_recorded {
                let _ = coord.oplog().record_tx(
                    "watcher",
                    format!("observe intermediate blob for {clean_rel}"),
                    true,
                    |view| {
                        let list = view.observed_blobs.entry(clean_rel.clone()).or_default();

                        if !list.contains(&blob_id) {
                            if list.len() >= MAX_OBSERVED_BLOBS_PER_PATH {
                                list.remove(0);
                            }

                            list.push(blob_id);
                        }

                        Ok(())
                    },
                );
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(poll_ms));
    }

    let _ = fs::remove_file(&pid_file);
    Ok(())
}
