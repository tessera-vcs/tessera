//! Repository and environment health diagnostics for `tes doctor`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use agent::Coordinator;
use core::{GitBackend, OpLogStore, authorized_sandbox_bases, sandbox_paths};
use serde_json::{Value, json};
use workspace::{CowStrategy, DirDiskUsage, clone_path_cow, format_bytes, measure_dir_usage};

use crate::output::{emit_paged_output, use_color};

/// Known repository-local AI harness files inspected by `tes doctor`.
const KNOWN_HARNESS_FILES: &[&str] = &[
    ".agents/rules/tessera.md",
    ".claude/settings.json",
    ".mcp.json",
    ".cursor/mcp.json",
    ".cursor/rules/tessera.mdc",
    ".windsurf/rules/tessera.md",
    ".kiro/steering/tessera.md",
    ".kiro/settings/mcp.json",
    "opencode.json",
    ".codex/config.toml",
    ".codex/hooks.json",
    ".vscode/mcp.json",
    "AGENTS.md",
    "CLAUDE.md",
];

/// Git hooks installed by `tes init`.
const EXPECTED_GIT_HOOKS: &[&str] = &["post-commit", "post-merge", "post-checkout"];

/// Execution-sensitive keys in `tessera.toml` gated by `tes config trust`.
const PRIVILEGED_TOML_KEYS: &[&str] = &["verify_command", "extra_bin_dirs", "custom_env", "pager"];

/// Individual diagnostic check outcome produced by `tes doctor`.
#[derive(Debug, Clone)]
struct DoctorCheck {
    id: &'static str,
    name: &'static str,
    status: &'static str,
    summary: String,
    details: Value,
}

impl DoctorCheck {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "status": self.status,
            "summary": self.summary,
            "details": self.details,
        })
    }
}

/// Checks whether a numeric process ID is currently alive on the host OS.
fn is_pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }

    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .is_ok_and(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
            })
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// Parses `core.worktree` from a Git `config` file, if explicitly configured.
fn parse_core_worktree_from_config(raw: &str) -> Option<String> {
    let mut in_core = false;
    let mut result: Option<String> = None;

    for line in raw.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('[') {
            let section = rest.split(']').next().unwrap_or("").trim();
            in_core = section.eq_ignore_ascii_case("core");
            continue;
        }

        if !in_core {
            continue;
        }

        let Some((key, val)) = trimmed.split_once('=') else {
            continue;
        };

        if !key.trim().eq_ignore_ascii_case("worktree") {
            continue;
        }

        let cleaned = strip_git_config_value(val);

        if !cleaned.is_empty() {
            result = Some(cleaned);
        }
    }

    result
}

/// Strips surrounding quotes and trailing inline `#`/`;` comments from a Git config value.
fn strip_git_config_value(raw: &str) -> String {
    let trimmed = raw.trim();

    if let Some(after_open) = trimmed.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = after_open.chars();

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    match next {
                        '\\' => out.push('\\'),
                        '"' => out.push('"'),
                        other => {
                            out.push('\\');
                            out.push(other);
                        }
                    }
                }
            } else if ch == '"' {
                return out;
            } else {
                out.push(ch);
            }
        }

        return out;
    }

    for (idx, ch) in trimmed.char_indices() {
        if ch == '#' || ch == ';' {
            return trimmed[..idx].trim_end().to_string();
        }
    }

    trimmed.to_string()
}

/// Reads any configured `core.worktree` from `<git_dir>/config` or `<common_git_dir>/config` and
/// resolves relative values against the containing Git directory.
fn read_git_core_worktree(git_dir: &Path, common_git_dir: &Path) -> Option<(String, PathBuf)> {
    let candidates = if git_dir == common_git_dir {
        vec![git_dir]
    } else {
        vec![git_dir, common_git_dir]
    };

    for dir in candidates {
        let cfg_path = dir.join("config");

        if let Ok(raw) = fs::read_to_string(&cfg_path) {
            if let Some(val) = parse_core_worktree_from_config(&raw) {
                let path = PathBuf::from(&val);
                let resolved = if path.is_absolute() {
                    path
                } else {
                    dir.join(&path)
                };

                return Some((val, resolved));
            }
        }
    }

    None
}

/// Returns `true` if `worktree_path` points inside a Tessera sandbox base or `.tessera/workspaces`.
fn is_worktree_inside_sandbox(worktree_path: &Path, repo_root: &Path) -> bool {
    let in_repo_ws = repo_root.join(".tessera").join("workspaces");

    if sandbox_paths::path_is_within(worktree_path, &in_repo_ws) {
        return true;
    }

    authorized_sandbox_bases(repo_root)
        .iter()
        .any(|base| sandbox_paths::path_is_within(worktree_path, base))
}

/// Parses the owning PID from a `verify-sandbox-<pid>-<ms>-<seq>` directory name.
fn parse_verify_sandbox_pid(dir_name: &str) -> Option<i32> {
    let rest = dir_name.strip_prefix("verify-sandbox-")?;
    rest.split('-').next()?.parse::<i32>().ok()
}

/// Returns `true` if `name` ends with `-<32 lowercase hex chars>`, matching Tessera's slug format.
fn has_tessera_slug_suffix(name: &str) -> bool {
    let Some((_, suffix)) = name.rsplit_once('-') else {
        return false;
    };

    suffix.len() == 32
        && suffix
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Returns `true` if `a` and `b` refer to the same canonical path.
fn same_canonical_path(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// Probes Copy-on-Write cloning behavior on the filesystem hosting `probe_root`.
fn probe_cow_strategy(probe_root: &Path) -> Result<CowStrategy, String> {
    let probe_dir = probe_root.join(format!(".doctor-cow-probe-{}", std::process::id()));

    if let Err(err) = fs::create_dir_all(&probe_dir) {
        return Err(format!("unable to create CoW probe directory: {err}"));
    }

    let src = probe_dir.join("src.txt");
    let dst = probe_dir.join("dst.txt");

    if let Err(err) = fs::write(&src, b"tessera-cow-probe\n") {
        let _ = fs::remove_dir_all(&probe_dir);
        return Err(format!("unable to write CoW probe file: {err}"));
    }

    let result = clone_path_cow(&src, &dst).map_err(|err| err.to_string());
    let _ = fs::remove_dir_all(&probe_dir);
    result
}

/// Collects a full structured `tes doctor` diagnostic report for `cwd`.
#[must_use]
pub fn collect_doctor_report(cwd: &Path) -> Value {
    let mut checks: Vec<DoctorCheck> = Vec::new();

    let git_backend = match GitBackend::discover(cwd) {
        Ok(git) => {
            let repo_root = git.repo_root().to_path_buf();
            let git_dir = git.git_dir().to_path_buf();
            let common_git_dir = git.common_git_dir().to_path_buf();
            let is_worktree = git_dir != common_git_dir;
            let detached = git.is_detached_head();
            let branch = git.current_branch_name().ok();
            let head_commit = git
                .resolve_head_commit()
                .ok()
                .flatten()
                .map(|id| id.as_str().to_string());

            let core_worktree = read_git_core_worktree(&git_dir, &common_git_dir);
            let core_worktree_remediation = core_worktree.as_ref().map(|_| {
                format!(
                    "git --git-dir {} config --unset core.worktree",
                    git_dir.display()
                )
            });

            let (status, summary) = match &core_worktree {
                Some((raw, resolved)) if !resolved.is_dir() => (
                    "fail",
                    format!(
                        "Stale core.worktree '{}' does not exist and breaks native git commands (run `{}`)",
                        raw,
                        core_worktree_remediation.as_deref().unwrap_or("")
                    ),
                ),
                Some((raw, resolved)) if is_worktree_inside_sandbox(resolved, &repo_root) => (
                    "warn",
                    format!(
                        "core.worktree '{}' points inside a Tessera sandbox (run `{}`)",
                        raw,
                        core_worktree_remediation.as_deref().unwrap_or("")
                    ),
                ),
                _ if detached => (
                    "warn",
                    format!(
                        "Git repository detected at {} (detached HEAD{})",
                        repo_root.display(),
                        if is_worktree { ", linked worktree" } else { "" }
                    ),
                ),
                _ => (
                    "pass",
                    format!(
                        "Git repository detected at {} (branch: {}{})",
                        repo_root.display(),
                        branch.as_deref().unwrap_or("unborn"),
                        if is_worktree { ", linked worktree" } else { "" }
                    ),
                ),
            };

            checks.push(DoctorCheck {
                id: "git_repository",
                name: "Git Repository",
                status,
                summary,
                details: json!({
                    "repo_root": repo_root.display().to_string(),
                    "git_dir": git_dir.display().to_string(),
                    "common_git_dir": common_git_dir.display().to_string(),
                    "is_linked_worktree": is_worktree,
                    "detached_head": detached,
                    "branch": branch,
                    "head_commit": head_commit,
                    "core_worktree": core_worktree.as_ref().map(|(raw, _)| raw.as_str()),
                    "core_worktree_remediation": core_worktree_remediation,
                }),
            });

            Some(git)
        }
        Err(err) => {
            checks.push(DoctorCheck {
                id: "git_repository",
                name: "Git Repository",
                status: "fail",
                summary: format!("Not inside a valid Git repository: {err}"),
                details: json!({
                    "cwd": cwd.display().to_string(),
                    "error": err.to_string(),
                }),
            });

            None
        }
    };

    if let Some(git) = git_backend {
        let oplog = OpLogStore::new_with_repo_root(git.common_git_dir(), git.repo_root());
        let coord_opt = Coordinator::open(cwd).ok();

        let initialized = oplog.is_initialized();

        if !initialized {
            checks.push(DoctorCheck {
                id: "oplog_health",
                name: "OpLog & Locking",
                status: "fail",
                summary: "Tessera is not initialized in this repository (run `tes init`)"
                    .to_string(),
                details: json!({
                    "store_dir": oplog.store_dir().display().to_string(),
                    "initialized": false,
                }),
            });
        } else {
            let lock_ok = oplog.acquire_lock().is_ok();
            let op_heads = oplog.list_op_heads().unwrap_or_default();
            let reconciled = oplog.load_current_reconciled();

            match reconciled {
                Ok(op) => {
                    let status = if lock_ok { "pass" } else { "fail" };
                    let summary = if lock_ok {
                        format!(
                            "OpLog healthy ({} head(s), {} active session(s), lock OK)",
                            op_heads.len(),
                            op.view.agents.len()
                        )
                    } else {
                        "OpLog readable, but cross-process lock acquisition failed".to_string()
                    };

                    checks.push(DoctorCheck {
                        id: "oplog_health",
                        name: "OpLog & Locking",
                        status,
                        summary,
                        details: json!({
                            "store_dir": oplog.store_dir().display().to_string(),
                            "initialized": true,
                            "lock_healthy": lock_ok,
                            "op_heads": op_heads.len(),
                            "latest_op": op.id.as_str(),
                            "active_sessions": op.view.agents.len(),
                        }),
                    });
                }
                Err(err) => {
                    checks.push(DoctorCheck {
                        id: "oplog_health",
                        name: "OpLog & Locking",
                        status: "fail",
                        summary: format!("Failed to reconcile OpLog: {err}"),
                        details: json!({
                            "store_dir": oplog.store_dir().display().to_string(),
                            "initialized": true,
                            "lock_healthy": lock_ok,
                            "error": err.to_string(),
                        }),
                    });
                }
            }
        }

        let probe_root = coord_opt
            .as_ref()
            .map(Coordinator::sandboxes_root)
            .unwrap_or_else(|| oplog.store_dir().join("tmp"));

        match probe_cow_strategy(&probe_root) {
            Ok(strategy) => {
                let (status, strategy_name, summary) = match strategy {
                    CowStrategy::ApfsClonefile => (
                        "pass",
                        "apfs_clonefile",
                        "Native macOS APFS clonefile(2) zero-copy sandboxes available".to_string(),
                    ),
                    CowStrategy::LinuxReflink => (
                        "pass",
                        "linux_reflink",
                        "Native Linux FICLONE reflink zero-copy sandboxes available".to_string(),
                    ),
                    CowStrategy::PortableCopy => (
                        "warn",
                        "portable_copy",
                        "Filesystem does not support CoW reflinks; using portable copy fallback"
                            .to_string(),
                    ),
                };

                checks.push(DoctorCheck {
                    id: "cow_strategy",
                    name: "Copy-on-Write Filesystem",
                    status,
                    summary,
                    details: json!({
                        "strategy": strategy_name,
                        "probe_root": probe_root.display().to_string(),
                    }),
                });
            }
            Err(err) => {
                checks.push(DoctorCheck {
                    id: "cow_strategy",
                    name: "Copy-on-Write Filesystem",
                    status: "fail",
                    summary: format!("CoW sandbox probe failed: {err}"),
                    details: json!({
                        "probe_root": probe_root.display().to_string(),
                        "error": err,
                    }),
                });
            }
        }

        let repo_toml = git.repo_root().join("tessera.toml");
        let repo_toml_present = repo_toml.is_file();
        let repo_toml_trusted = oplog.is_repo_config_trusted();
        let local_cfg_res = oplog.load_local_config();
        let has_privileged_keys = repo_toml_present
            && fs::read_to_string(&repo_toml)
                .ok()
                .is_some_and(|raw| PRIVILEGED_TOML_KEYS.iter().any(|key| raw.contains(key)));

        let (cfg_status, cfg_summary) = if let Err(err) = &local_cfg_res {
            ("fail", format!("Local config.toml parse error: {err}"))
        } else if repo_toml_present && !repo_toml_trusted && has_privileged_keys {
            (
                "warn",
                "tessera.toml present with untrusted execution settings (run `tes config trust` to enable)"
                    .to_string(),
            )
        } else if repo_toml_present && repo_toml_trusted {
            (
                "pass",
                "tessera.toml present and trusted (Blake3 digest verified)".to_string(),
            )
        } else if repo_toml_present {
            (
                "pass",
                "tessera.toml present (declarative settings active, not trusted for execution)"
                    .to_string(),
            )
        } else {
            (
                "pass",
                "Local configuration healthy (no repo-tracked tessera.toml)".to_string(),
            )
        };

        checks.push(DoctorCheck {
            id: "repo_config",
            name: "Configuration & Trust",
            status: cfg_status,
            summary: cfg_summary,
            details: json!({
                "local_config_valid": local_cfg_res.is_ok(),
                "tessera_toml_present": repo_toml_present,
                "tessera_toml_trusted": repo_toml_trusted,
                "has_privileged_keys": has_privileged_keys,
            }),
        });

        let installed_harnesses: Vec<&str> = KNOWN_HARNESS_FILES
            .iter()
            .copied()
            .filter(|rel| git.repo_root().join(rel).is_file())
            .collect();

        let hooks_dir = git.common_git_dir().join("hooks");
        let mut installed_hooks = Vec::new();
        let mut missing_hooks = Vec::new();
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut non_exec_hooks: Vec<&str> = Vec::new();

        for hook_name in EXPECTED_GIT_HOOKS {
            let hook_path = hooks_dir.join(hook_name);

            if !hook_path.is_file() {
                missing_hooks.push(*hook_name);
                continue;
            }

            let content = fs::read_to_string(&hook_path).unwrap_or_default();

            if !content.contains("tes hook git-sync") {
                missing_hooks.push(*hook_name);
                continue;
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let is_exec = fs::metadata(&hook_path)
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false);

                if !is_exec {
                    non_exec_hooks.push(*hook_name);
                }
            }

            installed_hooks.push(*hook_name);
        }

        let (hook_status, hook_summary) = if !missing_hooks.is_empty() || !non_exec_hooks.is_empty()
        {
            (
                "warn",
                format!(
                    "{} harness file(s) found; Git hook issues (missing: {:?}, non-executable: {:?})",
                    installed_harnesses.len(),
                    missing_hooks,
                    non_exec_hooks
                ),
            )
        } else {
            (
                "pass",
                format!(
                    "{} harness file(s) and all {} Git sync hooks active",
                    installed_harnesses.len(),
                    installed_hooks.len()
                ),
            )
        };

        checks.push(DoctorCheck {
            id: "harness_and_hooks",
            name: "Harness & Git Hooks",
            status: hook_status,
            summary: hook_summary,
            details: json!({
                "harness_files": installed_harnesses,
                "git_hooks_installed": installed_hooks,
                "git_hooks_missing": missing_hooks,
                "git_hooks_non_executable": non_exec_hooks,
            }),
        });

        let info_exclude_path = git.common_git_dir().join("info/exclude");
        let info_exclude_exists = info_exclude_path.is_file();
        let info_exclude_lines: Vec<String> = fs::read_to_string(&info_exclude_path)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(ToString::to_string)
            .collect();
        let global_excludes = git.global_excludes_file();

        checks.push(DoctorCheck {
            id: "ignore_rules",
            name: "Git Exclude & Global Ignore",
            status: "pass",
            summary: format!(
                ".git/info/exclude ({} rule(s)); global gitignore: {}",
                info_exclude_lines.len(),
                global_excludes
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none".to_string())
            ),
            details: json!({
                "info_exclude_path": info_exclude_path.display().to_string(),
                "info_exclude_exists": info_exclude_exists,
                "info_exclude_rule_count": info_exclude_lines.len(),
                "global_excludes_file": global_excludes.map(|p| p.display().to_string()),
            }),
        });

        let mut orphaned_sandboxes: Vec<String> = Vec::new();
        let mut merged_sandboxes: Vec<String> = Vec::new();
        let mut crashed_verify_sandboxes: Vec<String> = Vec::new();
        let mut orphaned_agent_workspaces: Vec<String> = Vec::new();
        let mut stale_global_repos: Vec<String> = Vec::new();
        let mut dead_port_leases: Vec<Value> = Vec::new();
        let mut active_conflicts_count: usize = 0;
        let mut sandbox_disk_usage = DirDiskUsage::default();

        if let Some(coord) = &coord_opt {
            let cfg = oplog.load_config().unwrap_or_default();
            let current_head_branch = git
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());
            let local_git_branches: BTreeSet<String> =
                git.list_local_branches().into_iter().collect();
            let mut known_safe_branches: BTreeSet<String> = BTreeSet::new();
            let mut safe_name_to_branch: BTreeMap<String, String> = BTreeMap::new();
            let mut active_agent_ids: BTreeSet<String> = BTreeSet::new();
            let mut active_target_branches: BTreeSet<String> = BTreeSet::new();

            known_safe_branches.insert(Coordinator::sanitize_branch_name(&cfg.trunk_branch));
            known_safe_branches.insert(Coordinator::sanitize_branch_name(&current_head_branch));

            for br in &local_git_branches {
                let safe = Coordinator::sanitize_branch_name(br);
                known_safe_branches.insert(safe.clone());
                safe_name_to_branch.insert(safe, br.clone());
            }

            if let Ok(current) = oplog.load_current_reconciled() {
                active_conflicts_count = current.view.conflicts.len();

                for (aid, agent) in &current.view.agents {
                    active_agent_ids.insert(aid.clone());

                    if let Some(tb) = &agent.target_branch {
                        if aid != "editor" {
                            active_target_branches.insert(tb.clone());
                        }

                        if local_git_branches.contains(tb)
                            || tb == &cfg.trunk_branch
                            || tb == &current_head_branch
                            || agent.has_own_changes()
                        {
                            known_safe_branches.insert(Coordinator::sanitize_branch_name(tb));
                        }
                    }
                }

                for br in current.view.branches.keys() {
                    let has_dirty_session = current.view.agents.values().any(|a| {
                        a.target_branch.as_deref() == Some(br.as_str()) && a.has_own_changes()
                    });

                    if br == &cfg.trunk_branch
                        || br == &current_head_branch
                        || local_git_branches.contains(br)
                        || has_dirty_session
                    {
                        let safe = Coordinator::sanitize_branch_name(br);
                        known_safe_branches.insert(safe.clone());
                        safe_name_to_branch
                            .entry(safe)
                            .or_insert_with(|| br.clone());
                    }
                }
            }

            let trunk_cid = git
                .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))
                .ok()
                .flatten();
            let trunk_safe = Coordinator::sanitize_branch_name(&cfg.trunk_branch);
            let head_safe = Coordinator::sanitize_branch_name(&current_head_branch);
            let branches_root = coord.branches_root();
            sandbox_disk_usage = measure_dir_usage(&branches_root);

            if let Ok(entries) = fs::read_dir(&branches_root) {
                for entry in entries.flatten() {
                    let p = entry.path();

                    if !p.is_dir() {
                        continue;
                    }

                    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };

                    if !known_safe_branches.contains(name) {
                        orphaned_sandboxes.push(name.to_string());
                        continue;
                    }

                    if name == trunk_safe || name == head_safe {
                        continue;
                    }

                    if let Some(t_cid) = trunk_cid.as_ref() {
                        let raw_br = safe_name_to_branch
                            .get(name)
                            .map(String::as_str)
                            .unwrap_or(name);

                        if !active_target_branches.contains(raw_br) {
                            if let Ok(Some(b_cid)) =
                                git.resolve_ref(&format!("refs/heads/{raw_br}"))
                            {
                                if git.is_ancestor(&b_cid, t_cid) {
                                    merged_sandboxes.push(name.to_string());
                                }
                            }
                        }
                    }
                }
            }

            let sandboxes_root = coord.sandboxes_root();

            if let Ok(entries) = fs::read_dir(&sandboxes_root) {
                for entry in entries.flatten() {
                    let p = entry.path();

                    if !p.is_dir() {
                        continue;
                    }

                    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };

                    if !name.starts_with("verify-sandbox-") {
                        continue;
                    }

                    let alive = parse_verify_sandbox_pid(name).is_some_and(is_pid_alive);

                    if !alive {
                        crashed_verify_sandboxes.push(name.to_string());
                    }
                }
            }

            let in_repo_ws = git.repo_root().join(".tessera").join("workspaces");

            if let Ok(entries) = fs::read_dir(&in_repo_ws) {
                for entry in entries.flatten() {
                    let p = entry.path();

                    if !p.is_dir() {
                        continue;
                    }

                    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };

                    if !active_agent_ids.contains(name) {
                        orphaned_agent_workspaces.push(name.to_string());
                    }
                }
            }

            let mut global_bases: Vec<PathBuf> = Vec::new();
            let isolated_root = sandbox_paths::configured_workspaces_root().is_some()
                || sandbox_paths::is_path_inside_temp_dir(git.repo_root());

            if let Some(parent) = sandboxes_root.parent() {
                global_bases.push(parent.to_path_buf());
            }

            if !isolated_root {
                if let Some(primary) = core::primary_global_workspaces_base() {
                    if !global_bases
                        .iter()
                        .any(|b| same_canonical_path(b, &primary))
                    {
                        global_bases.push(primary);
                    }
                }
            }

            for base in global_bases {
                let Ok(entries) = fs::read_dir(&base) else {
                    continue;
                };

                for entry in entries.flatten() {
                    let p = entry.path();

                    if !p.is_dir() || same_canonical_path(&p, &sandboxes_root) {
                        continue;
                    }

                    let Some(slug) = p.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };

                    if !has_tessera_slug_suffix(slug) {
                        continue;
                    }

                    let marker = p.join(".repo_root");

                    if let Ok(raw) = fs::read_to_string(&marker) {
                        let owner = PathBuf::from(raw.trim());

                        if !raw.trim().is_empty() && !owner.join(".git").exists() {
                            stale_global_repos.push(slug.to_string());
                        }
                    }
                }
            }

            let leases_dir = sandboxes_root.join("bin").join(".leases");

            if let Ok(entries) = fs::read_dir(&leases_dir) {
                for entry in entries.flatten() {
                    let p = entry.path();

                    if !p.is_dir() {
                        continue;
                    }

                    let Some(port_name) = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .filter(|s| s.parse::<u16>().is_ok())
                        .map(ToString::to_string)
                    else {
                        continue;
                    };
                    let pid_raw = fs::read_to_string(p.join("pid")).unwrap_or_default();
                    let pid_opt = pid_raw.trim().parse::<i32>().ok();
                    let alive = pid_opt.is_some_and(is_pid_alive);

                    if !alive {
                        dead_port_leases.push(json!({
                            "port": port_name,
                            "pid": pid_opt,
                        }));
                    }
                }
            }
        }

        orphaned_sandboxes.sort();
        merged_sandboxes.sort();
        crashed_verify_sandboxes.sort();
        orphaned_agent_workspaces.sort();
        stale_global_repos.sort();

        let has_local_prunable = !orphaned_sandboxes.is_empty()
            || !merged_sandboxes.is_empty()
            || !crashed_verify_sandboxes.is_empty()
            || !orphaned_agent_workspaces.is_empty()
            || !dead_port_leases.is_empty();
        let usage_note = if sandbox_disk_usage.file_count > 0 {
            format!(
                " [sandboxes: {} unique / {} logical, {} files]",
                format_bytes(sandbox_disk_usage.unique_bytes),
                format_bytes(sandbox_disk_usage.logical_bytes),
                sandbox_disk_usage.file_count
            )
        } else {
            String::new()
        };

        let (rt_status, rt_summary) = if has_local_prunable
            || active_conflicts_count > 0
            || !stale_global_repos.is_empty()
        {
            let mut parts: Vec<String> = vec![
                format!("{} orphaned sandbox(es)", orphaned_sandboxes.len()),
                format!("{} dead port lease(s)", dead_port_leases.len()),
                format!("{} active conflict(s)", active_conflicts_count),
            ];

            if !merged_sandboxes.is_empty() {
                parts.push(format!("{} merged sandbox(es)", merged_sandboxes.len()));
            }

            if !crashed_verify_sandboxes.is_empty() {
                parts.push(format!(
                    "{} crashed verify sandbox(es)",
                    crashed_verify_sandboxes.len()
                ));
            }

            if !orphaned_agent_workspaces.is_empty() {
                parts.push(format!(
                    "{} orphaned agent workspace(s)",
                    orphaned_agent_workspaces.len()
                ));
            }

            if !stale_global_repos.is_empty() {
                parts.push(format!(
                    "{} deleted-repo sandbox cache(s)",
                    stale_global_repos.len()
                ));
            }

            let gc_hint = if has_local_prunable && !stale_global_repos.is_empty() {
                " (run 'tes gc' or 'tes gc --all' to clean up)"
            } else if has_local_prunable {
                " (run 'tes gc' to clean up)"
            } else if !stale_global_repos.is_empty() {
                " (run 'tes gc --all' to clean up)"
            } else {
                ""
            };

            ("warn", format!("{}{usage_note}{gc_hint}", parts.join(", ")))
        } else {
            (
                "pass",
                format!("No orphaned sandboxes, dead port leases, or active conflicts{usage_note}"),
            )
        };

        checks.push(DoctorCheck {
            id: "sandboxes_leases_conflicts",
            name: "Sandboxes, Leases & Conflicts",
            status: rt_status,
            summary: rt_summary,
            details: json!({
                "orphaned_sandboxes": orphaned_sandboxes,
                "merged_sandboxes": merged_sandboxes,
                "crashed_verify_sandboxes": crashed_verify_sandboxes,
                "orphaned_agent_workspaces": orphaned_agent_workspaces,
                "stale_global_repos": stale_global_repos,
                "dead_port_leases": dead_port_leases,
                "active_conflicts": active_conflicts_count,
                "sandbox_disk_usage": {
                    "logical_bytes": sandbox_disk_usage.logical_bytes,
                    "unique_bytes": sandbox_disk_usage.unique_bytes,
                    "file_count": sandbox_disk_usage.file_count,
                },
            }),
        });
    }

    let pass_count = checks.iter().filter(|c| c.status == "pass").count();
    let warn_count = checks.iter().filter(|c| c.status == "warn").count();
    let fail_count = checks.iter().filter(|c| c.status == "fail").count();
    let checks_json: Vec<Value> = checks.iter().map(DoctorCheck::to_json).collect();

    json!({
        "ok": fail_count == 0,
        "summary": {
            "pass": pass_count,
            "warn": warn_count,
            "fail": fail_count,
        },
        "checks": checks_json,
    })
}

/// Formats a `tes doctor` diagnostic JSON report as aligned human-readable text.
///
/// Pads check names to a uniform display width before applying ANSI color escapes so columns stay
/// aligned regardless of whether `colored` is enabled.
#[must_use]
pub fn format_doctor_report(report: &Value, colored: bool) -> String {
    let style = |code: &str, text: &str| {
        if colored {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };

    let mut buf = String::new();
    let _ = writeln!(buf, "{}", style("1", "Tessera Doctor Diagnostics"));

    if let Some(checks) = report.get("checks").and_then(Value::as_array) {
        for check in checks {
            let status = check
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("fail");
            let name = check.get("name").and_then(Value::as_str).unwrap_or("Check");
            let summary = check.get("summary").and_then(Value::as_str).unwrap_or("");
            let badge = match status {
                "pass" => style("1;32", "✓ PASS"),
                "warn" => style("1;33", "! WARN"),
                _ => style("1;31", "✗ FAIL"),
            };
            let padded_name = format!("{name:<30}");

            let _ = writeln!(buf, "  {badge}  {} {summary}", style("1;36", &padded_name));
        }
    }

    let pass = report["summary"]["pass"].as_u64().unwrap_or(0);
    let warn = report["summary"]["warn"].as_u64().unwrap_or(0);
    let fail = report["summary"]["fail"].as_u64().unwrap_or(0);

    let _ = writeln!(
        buf,
        "\n{} {} passed, {} warning(s), {} failed",
        style("2", "Summary:"),
        style("1;32", &pass.to_string()),
        style("1;33", &warn.to_string()),
        if fail == 0 {
            style("1;32", &fail.to_string())
        } else {
            style("1;31", &fail.to_string())
        }
    );

    buf
}

/// Runs `tes doctor`, printing either structured JSON or a formatted diagnostic report.
///
/// Exits with code 1 when any diagnostic check reports status `"fail"`.
///
/// # Errors
/// Returns an error if JSON serialization fails.
pub fn run_doctor(
    cwd: &Path,
    json_out: bool,
    no_pager: bool,
    paginate: bool,
) -> anyhow::Result<()> {
    let report = collect_doctor_report(cwd);
    let ok = report.get("ok").and_then(Value::as_bool).unwrap_or(false);

    if json_out {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let buf = format_doctor_report(&report, use_color());
        let coord_opt = Coordinator::open(cwd).ok();
        emit_paged_output(
            &buf,
            false,
            no_pager,
            paginate,
            json_out,
            coord_opt.as_ref(),
        );
    }

    if !ok {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strips ANSI CSI SGR escape sequences so visible character columns can be verified.
    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();

                for esc_ch in chars.by_ref() {
                    if esc_ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }

        out
    }

    #[test]
    fn doctor_human_output_aligns_summary_columns_with_and_without_ansi() {
        let sample = json!({
            "ok": true,
            "summary": { "pass": 6, "warn": 1, "fail": 0 },
            "checks": [
                { "name": "Git Repository", "status": "pass", "summary": "S1" },
                { "name": "Copy-on-Write Filesystem", "status": "pass", "summary": "S2" },
                { "name": "Sandboxes, Leases & Conflicts", "status": "warn", "summary": "S3" }
            ]
        });

        let char_col = |line: &str, needle: &str| -> usize {
            let byte_idx = line.find(needle).expect("needle column");
            line[..byte_idx].chars().count()
        };

        for colored in [false, true] {
            let rendered = format_doctor_report(&sample, colored);
            let visible_lines: Vec<String> = rendered.lines().map(strip_ansi).collect();
            let s1_col = char_col(&visible_lines[1], "S1");
            let s2_col = char_col(&visible_lines[2], "S2");
            let s3_col = char_col(&visible_lines[3], "S3");

            assert_eq!(
                s1_col, s2_col,
                "summary columns must align for short and long check names (colored={colored}):\n{rendered}"
            );
            assert_eq!(
                s2_col, s3_col,
                "summary columns must align for 29-char check names (colored={colored}):\n{rendered}"
            );
        }
    }

    #[test]
    fn doctor_detects_missing_and_sandbox_core_worktree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");
        let config_path = coord.git().common_git_dir().join("config");
        let missing_path = tmp.path().join("deleted-sandbox-dir");

        fs::write(
            &config_path,
            format!(
                "[core]\n\tbare = false\n\tworktree = {}\n",
                missing_path.display()
            ),
        )
        .expect("write missing core.worktree");

        let report_missing = collect_doctor_report(tmp.path());
        let git_check_missing = report_missing["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .find(|c| c["id"] == "git_repository")
            .expect("git_repository check");
        assert_eq!(git_check_missing["status"], "fail", "{git_check_missing}");
        assert!(
            git_check_missing["summary"]
                .as_str()
                .unwrap_or("")
                .contains("config --unset core.worktree"),
            "missing core.worktree summary must include remediation: {git_check_missing}"
        );

        let sandbox_branch = coord.sandboxes_root().join("branches").join("feat-x");
        fs::create_dir_all(&sandbox_branch).expect("mkdir sandbox_branch");
        let escaped_sandbox = sandbox_branch.display().to_string().replace('\\', "\\\\");
        fs::write(
            &config_path,
            format!("[core]\n\tbare = false\n\tworktree = \"{escaped_sandbox}\"\n"),
        )
        .expect("write sandbox core.worktree");

        let report_sandbox = collect_doctor_report(tmp.path());
        let git_check_sandbox = report_sandbox["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .find(|c| c["id"] == "git_repository")
            .expect("git_repository check");
        assert_eq!(git_check_sandbox["status"], "warn", "{git_check_sandbox}");
        assert!(
            git_check_sandbox["summary"]
                .as_str()
                .unwrap_or("")
                .contains("Tessera sandbox"),
            "sandbox core.worktree summary must warn: {git_check_sandbox}"
        );

        // Verify Windows escaped and forward-slash `core.worktree` values parse cleanly.
        assert_eq!(
            parse_core_worktree_from_config(
                "[core]\n\tworktree = \"C:\\\\Users\\\\dev\\\\repo\\\\branches\\\\feat-x\"\n"
            ),
            Some("C:\\Users\\dev\\repo\\branches\\feat-x".to_string())
        );
        assert_eq!(
            parse_core_worktree_from_config(
                "[core]\n\tworktree = \"C:\\Users\\dev\\repo\\branches\\feat-x\"\n"
            ),
            Some("C:\\Users\\dev\\repo\\branches\\feat-x".to_string())
        );
        assert_eq!(
            parse_core_worktree_from_config(
                "[core]\n\tworktree = \"C:/Users/dev/repo/branches/feat-x\"\n"
            ),
            Some("C:/Users/dev/repo/branches/feat-x".to_string())
        );
    }
}
