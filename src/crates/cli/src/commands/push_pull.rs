//! Remote synchronization command handlers (`tes push`, `tes pull`).

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use agent::{Coordinator, detect_session_id};
use serde_json::{Value, json};

use crate::output::{CliContext, c_cyan, c_green, c_red, c_yellow};

/// Options for `tes push`.
#[derive(Debug, Clone, Default)]
pub struct PushArgs {
    /// Positional argument 1: remote name, URL, or branch/refspec (when single-arg).
    pub remote: Option<String>,
    /// Positional argument 2: branch name or refspec (`local:remote`).
    pub branch: Option<String>,
    /// Set upstream tracking reference (`-u` / `--set-upstream`).
    pub set_upstream: bool,
    /// Optional commit message for pending session auto-commit.
    pub message: Option<String>,
    /// Force push (`--force`).
    pub force: bool,
    /// Force push with lease (`--force-with-lease`).
    pub force_with_lease: bool,
    /// Push all tags (`--tags`).
    pub tags: bool,
    /// Delete the target remote branch/ref (`-d` / `--delete`).
    pub delete: bool,
    /// Simulate the push without updating remote refs (`-n` / `--dry-run`).
    pub dry_run: bool,
    /// Skip auto-committing uncommitted session changes before pushing.
    pub no_auto_commit: bool,
    /// Optional explicit session ID.
    pub agent: Option<String>,
}

/// Result returned by [`execute_push`] for both CLI and MCP callers.
#[derive(Debug, Clone)]
pub struct PushOutcome {
    /// Structured JSON report describing the push execution.
    pub payload: Value,
    /// Process exit code (`0` on success, non-zero on failure or aborted push).
    pub exit_code: i32,
}

/// Lists configured Git remote names via `git remote`.
fn list_git_remotes(coord: &Coordinator) -> BTreeSet<String> {
    let out = Command::new("git")
        .current_dir(coord.git().repo_root())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("--git-dir")
        .arg(coord.git().git_dir())
        .arg("remote")
        .output();

    let Ok(output) = out else {
        return BTreeSet::new();
    };

    if !output.status.success() {
        return BTreeSet::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Returns `true` when `token` looks like a Git remote URL or local bare repository path.
fn looks_like_remote_url(token: &str) -> bool {
    token.contains("://")
        || token.starts_with("git@")
        || token.starts_with("ssh://")
        || token.starts_with("http://")
        || token.starts_with("https://")
        || token.starts_with("file://")
        || (token.ends_with(".git") && Path::new(token).exists())
}

/// Resolves the configured default remote for `branch` (`branch.<branch>.remote`), falling back to `"origin"`.
fn default_remote_for_branch(coord: &Coordinator, branch: &str) -> String {
    coord
        .git()
        .git_config_value(&format!("branch.{branch}.remote"))
        .unwrap_or_else(|| "origin".to_string())
}

/// Resolves `(remote, branch_or_refspec)` with single-argument smart detection:
/// when only one positional argument is supplied and it is neither a configured remote nor a URL,
/// it is treated as `<branch>` paired with the branch's configured remote (or `"origin"`).
pub fn resolve_remote_and_branch(
    coord: &Coordinator,
    arg1: Option<&str>,
    arg2: Option<&str>,
    active_branch: &str,
) -> (String, String) {
    match (arg1, arg2) {
        (Some(r), Some(b)) => (r.to_string(), b.to_string()),
        (None, Some(b)) => (default_remote_for_branch(coord, b), b.to_string()),
        (None, None) => (
            default_remote_for_branch(coord, active_branch),
            active_branch.to_string(),
        ),
        (Some(only), None) => {
            let remotes = list_git_remotes(coord);

            if remotes.contains(only) || looks_like_remote_url(only) {
                (only.to_string(), active_branch.to_string())
            } else {
                let local_for_cfg = only
                    .split_once(':')
                    .map(|(l, _)| l)
                    .filter(|l| !l.is_empty())
                    .unwrap_or(active_branch);
                (
                    default_remote_for_branch(coord, local_for_cfg),
                    only.to_string(),
                )
            }
        }
    }
}

/// Validates a remote name or URL, rejecting option-injection arguments starting with `-`.
fn validate_remote_spec(remote: &str) -> anyhow::Result<()> {
    let trimmed = remote.trim();

    if trimmed.is_empty() || trimmed.starts_with('-') {
        anyhow::bail!("invalid remote '{remote}'");
    }

    if !looks_like_remote_url(trimmed) {
        core::validate_ref_name(trimmed)?;
    }

    Ok(())
}

/// Executes a push operation (shared by CLI `tes push` and MCP `push`).
///
/// When `capture_output` is `true` (e.g., `--json` or MCP), `git push` stdout/stderr are captured
/// cleanly into the returned [`PushOutcome::payload`].
///
/// # Errors
/// Returns an error if validation fails or Git cannot be spawned.
pub fn execute_push(
    coord: &Coordinator,
    session_id: &str,
    args: &PushArgs,
    capture_output: bool,
    human_stdout: bool,
) -> anyhow::Result<PushOutcome> {
    let cfg = coord.oplog().load_config()?;
    let _ = coord.watch_tick(session_id);
    let meta = coord.snapshot_agent(session_id, true)?;
    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| cfg.trunk_branch.clone());
    let fallback_branch = meta.target_branch.clone().unwrap_or(default_branch);

    let (remote, target_spec) = resolve_remote_and_branch(
        coord,
        args.remote.as_deref(),
        args.branch.as_deref(),
        &fallback_branch,
    );
    validate_remote_spec(&remote)?;

    let (local_branch, git_refspec, is_delete_refspec) = if args.delete {
        core::validate_ref_name(&target_spec)?;
        (target_spec.clone(), target_spec.clone(), true)
    } else if let Some((lhs, rhs)) = target_spec.split_once(':') {
        let lhs_t = lhs.trim();
        let rhs_t = rhs.trim();

        if rhs_t.is_empty() {
            anyhow::bail!("invalid push refspec '{target_spec}': missing destination ref");
        }

        core::validate_ref_name(rhs_t)?;

        if lhs_t.is_empty() {
            (String::new(), format!(":{rhs_t}"), true)
        } else {
            core::validate_ref_name(lhs_t)?;
            let lhs_full = if lhs_t.starts_with("refs/") {
                lhs_t.to_string()
            } else {
                format!("refs/heads/{lhs_t}")
            };
            let rhs_full = if rhs_t.starts_with("refs/") {
                rhs_t.to_string()
            } else {
                format!("refs/heads/{rhs_t}")
            };
            (lhs_t.to_string(), format!("{lhs_full}:{rhs_full}"), false)
        }
    } else {
        core::validate_ref_name(&target_spec)?;
        (
            target_spec.clone(),
            format!("refs/heads/{target_spec}:refs/heads/{target_spec}"),
            false,
        )
    };

    let mut auto_commit_json: Value = Value::Null;
    let should_auto_commit = !args.no_auto_commit
        && !args.delete
        && !args.dry_run
        && !is_delete_refspec
        && (!args.tags || args.branch.is_some())
        && !local_branch.is_empty()
        && (meta.head_commit != meta.base_commit
            || meta.has_own_changes()
            || args.message.is_some());

    if should_auto_commit {
        let report = coord.merge_agents_with_message(
            std::slice::from_ref(&session_id.to_string()),
            Some(&local_branch),
            None,
            true,
            args.message.as_deref(),
        )?;
        auto_commit_json = serde_json::to_value(&report)?;

        if !report.conflicts.is_empty() || !report.verification_passed {
            if human_stdout {
                println!(
                    "{} Aborted push of '{}': auto-commit encountered {} conflict(s) (verified={})",
                    c_red("✗"),
                    c_green(&local_branch),
                    c_red(report.conflicts.len()),
                    if report.verification_passed {
                        c_green("true")
                    } else {
                        c_red("false")
                    }
                );
            }

            return Ok(PushOutcome {
                payload: json!({
                    "remote": remote,
                    "branch": local_branch,
                    "refspec": git_refspec,
                    "dry_run": args.dry_run,
                    "tags": args.tags,
                    "delete": args.delete,
                    "auto_commit": auto_commit_json,
                    "ok": false,
                    "aborted": true,
                    "reason": "auto-commit produced conflicts or failed verification",
                    "exit_code": 1,
                }),
                exit_code: 1,
            });
        }

        if human_stdout && report.created_commit {
            println!(
                "{} Committed session changes onto {} -> commit {}",
                c_green("✓"),
                c_green(&report.target_branch),
                c_yellow(report.merged_commit_id.short())
            );
        }
    }

    let has_upstream = !local_branch.is_empty()
        && coord
            .git()
            .git_config_value(&format!("branch.{local_branch}.merge"))
            .is_some();
    let should_set_upstream =
        args.set_upstream || (!has_upstream && !args.delete && !args.tags && !is_delete_refspec);

    let mut cmd = Command::new("git");
    cmd.current_dir(coord.git().repo_root())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("--git-dir")
        .arg(coord.git().git_dir())
        .arg("push");

    if should_set_upstream {
        cmd.arg("-u");
    }

    if args.force {
        cmd.arg("--force");
    } else if args.force_with_lease {
        cmd.arg("--force-with-lease");
    }

    if args.tags {
        cmd.arg("--tags");
    }

    if args.delete {
        cmd.arg("--delete");
    }

    if args.dry_run {
        cmd.arg("--dry-run");
    }

    cmd.arg("--end-of-options").arg(&remote);

    if !args.tags || args.branch.is_some() || args.delete {
        cmd.arg(&git_refspec);
    }

    if capture_output {
        let output = cmd.output()?;
        let code = output
            .status
            .code()
            .unwrap_or(if output.status.success() { 0 } else { 1 });
        let stdout_s = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr_s = String::from_utf8_lossy(&output.stderr).into_owned();

        return Ok(PushOutcome {
            payload: json!({
                "remote": remote,
                "branch": local_branch,
                "refspec": git_refspec,
                "dry_run": args.dry_run,
                "tags": args.tags,
                "delete": args.delete,
                "auto_commit": auto_commit_json,
                "ok": output.status.success(),
                "exit_code": code,
                "stdout": stdout_s,
                "stderr": stderr_s,
            }),
            exit_code: code,
        });
    }

    let status = cmd.status()?;
    let code = status
        .code()
        .unwrap_or(if status.success() { 0 } else { 1 });

    if status.success() && human_stdout {
        if args.delete {
            println!(
                "{} Deleted remote branch '{}' on {}",
                c_green("✓"),
                c_yellow(&target_spec),
                c_cyan(&remote)
            );
        } else if args.dry_run {
            println!(
                "{} Dry-run push of '{}' to {} succeeded",
                c_green("✓"),
                c_green(&target_spec),
                c_cyan(&remote)
            );
        } else {
            println!(
                "{} Pushed branch {} to {}",
                c_green("✓"),
                c_green(&target_spec),
                c_cyan(&remote)
            );
        }
    }

    Ok(PushOutcome {
        payload: json!({
            "remote": remote,
            "branch": local_branch,
            "refspec": git_refspec,
            "ok": status.success(),
            "exit_code": code,
        }),
        exit_code: code,
    })
}

/// Executes `tes push`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or push arguments are invalid.
pub fn run_push(ctx: &CliContext, args: PushArgs) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(args.agent.as_deref());
    let outcome = execute_push(&coord, &session_id, &args, ctx.json, !ctx.json)?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&outcome.payload)?);
    }

    if outcome.exit_code != 0 {
        std::process::exit(outcome.exit_code);
    }

    Ok(())
}

/// Executes `tes pull`.
///
/// Exits with code 1 after emitting the report when 3-way pull merge records conflicts.
///
/// # Errors
/// Returns an error if the repository cannot be opened or pull fails.
pub fn run_pull(
    ctx: &CliContext,
    remote_arg: Option<String>,
    branch_arg: Option<String>,
    agent: Option<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let cfg = coord.oplog().load_config()?;
    let session_id = detect_session_id(agent.as_deref());
    let current = coord.oplog().load_current_reconciled()?;
    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| cfg.trunk_branch.clone());
    let active_branch = current
        .view
        .agents
        .get(&session_id)
        .and_then(|a| a.target_branch.clone())
        .unwrap_or(default_branch);

    let (remote, target_branch) = resolve_remote_and_branch(
        &coord,
        remote_arg.as_deref(),
        branch_arg.as_deref(),
        &active_branch,
    );

    let report = coord.pull_branch(&session_id, &remote, Some(&target_branch))?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if !report.conflicts.is_empty() {
        println!(
            "{} Pulled '{}/{}' ({} -> {}, {} session(s) rebased) with {} conflict(s) recorded:",
            c_red("!"),
            c_cyan(&report.remote),
            c_green(&report.branch),
            c_yellow(report.old_commit.short()),
            c_yellow(report.new_commit.short()),
            c_cyan(report.rebased_sessions),
            c_red(report.conflicts.len())
        );

        for conflict in &report.conflicts {
            println!("  {} {}", c_red("!"), conflict.path);
        }

        println!("  Run `tes conflicts` to inspect and `tes resolve <path>` to resolve.");
    } else {
        println!(
            "{} Pulled '{}/{}' ({} -> {}, {} session(s) rebased, fast-forward: {})",
            c_green("✓"),
            c_cyan(&report.remote),
            c_green(&report.branch),
            c_yellow(report.old_commit.short()),
            c_yellow(report.new_commit.short()),
            c_cyan(report.rebased_sessions),
            c_cyan(report.fast_forward)
        );
    }

    if !report.conflicts.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}
