//! Commit, semantic merge, cherry-pick, and auto-rebase sync command handlers.

use agent::{CommitOptions, Coordinator, detect_session_id};

use crate::output::{CliContext, c_cyan, c_green, c_red, c_yellow};

/// Options for `tes commit`.
#[derive(Debug, Clone)]
pub struct CommitArgs {
    /// Optional explicit commit message.
    pub message: Option<String>,
    /// Target branch override.
    pub branch: Option<String>,
    /// Amend the tip commit.
    pub amend: bool,
    /// Reuse tip message without editing when amending.
    pub no_edit: bool,
    /// Speculative verification flag (`Some("")` = configured command, `Some(cmd)` = custom).
    pub verify: Option<String>,
    /// Explicit `--verify-cmd <CMD>` override.
    pub verify_cmd: Option<String>,
    /// Explicit `Co-authored-by` trailers.
    pub co_author: Vec<String>,
    /// Suppress `Co-authored-by` trailers.
    pub no_co_author: bool,
    /// Explicit author override.
    pub author: Option<String>,
    /// Optional explicit session ID.
    pub agent: Option<String>,
}

/// Options for `tes merge`.
#[derive(Debug, Clone)]
pub struct MergeArgs {
    /// Agent session IDs or branch names to merge.
    pub agents: Vec<String>,
    /// Target branch name.
    pub into: Option<String>,
    /// Optional custom commit message.
    pub message: Option<String>,
    /// Speculative verification flag (`Some("")` = configured command, `Some(cmd)` = custom).
    pub verify: Option<String>,
    /// Explicit `--verify-cmd <CMD>` override.
    pub verify_cmd: Option<String>,
    /// Simulate the merge without mutating repository state.
    pub dry_run: bool,
    /// Keep merged sessions open.
    pub keep_open: bool,
    /// Delete merged non-trunk source branches after a clean, verified merge.
    pub delete_branch: bool,
    /// Explicit `Co-authored-by` trailers.
    pub co_author: Vec<String>,
    /// Suppress `Co-authored-by` trailers.
    pub no_co_author: bool,
    /// Explicit author override.
    pub author: Option<String>,
}

/// Resolves `--verify` / `--verify-cmd` into an effective command string, returning an error
/// if bare `--verify` is passed when no `verify_command` is configured.
fn resolve_verify_command(
    verify: Option<&str>,
    verify_cmd: Option<&str>,
    configured: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if let Some(explicit) = verify_cmd {
        let trimmed = explicit.trim();

        if trimmed.is_empty() {
            anyhow::bail!("--verify-cmd requires a non-empty shell command");
        }

        return Ok(Some(trimmed.to_string()));
    }

    match verify {
        None => Ok(None),
        Some(raw) if raw.trim().is_empty() => {
            match configured.map(str::trim).filter(|s| !s.is_empty()) {
                Some(cmd) => Ok(Some(cmd.to_string())),
                None => anyhow::bail!(
                    "--verify was requested, but no verify_command is configured; set one via `tes config set verify_command \"<cmd>\"` or pass `--verify=\"<cmd>\"`"
                ),
            }
        }
        Some(raw) => Ok(Some(raw.trim().to_string())),
    }
}

/// Executes `tes commit`.
///
/// Exits with code 1 after emitting the report when verification fails or conflicts are recorded.
///
/// # Errors
/// Returns an error if the repository cannot be opened or commit creation fails.
pub fn run_commit(ctx: &CliContext, args: CommitArgs) -> anyhow::Result<()> {
    let _ = args.no_edit;
    let coord = Coordinator::open(&ctx.cwd)?;
    let cfg = coord.oplog().load_config()?;
    let eff_verify = resolve_verify_command(
        args.verify.as_deref(),
        args.verify_cmd.as_deref(),
        cfg.verify_command.as_deref(),
    )?;
    let session_id = detect_session_id(args.agent.as_deref());
    let _ = coord.watch_tick(&session_id);
    let commit_options = CommitOptions {
        message: args.message,
        author: args.author,
        co_author: if args.no_co_author {
            Some(false)
        } else if !args.co_author.is_empty() {
            Some(true)
        } else {
            None
        },
        co_author_trailers: args.co_author,
        amend: args.amend,
        dry_run: false,
        delete_branch: false,
    };

    let report = coord.merge_agents_with_options(
        std::slice::from_ref(&session_id),
        args.branch.as_deref(),
        eff_verify.as_deref(),
        true,
        &commit_options,
    )?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if !report.verification_passed {
        println!(
            "{} Verification failed; branch '{}' was not advanced",
            c_red("✗"),
            c_green(&report.target_branch)
        );

        if let Some(output) = report
            .verification_output
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            println!("{output}");
        }
    } else if report.created_commit {
        let badge = if report.conflicts.is_empty() {
            c_green("✓")
        } else {
            c_red("!")
        };
        println!(
            "{} Committed onto {} -> commit {} (conflicts={})",
            badge,
            c_green(&report.target_branch),
            c_yellow(report.merged_commit_id.short()),
            if report.conflicts.is_empty() {
                c_green("0")
            } else {
                c_red(report.conflicts.len())
            }
        );
    } else if !report.conflicts.is_empty() {
        println!(
            "{} Commit encountered {} conflict(s) on {} (branch tip {})",
            c_red("!"),
            c_red(report.conflicts.len()),
            c_green(&report.target_branch),
            c_yellow(report.merged_commit_id.short())
        );
    } else {
        println!(
            "Nothing to commit on {} (working tree matches commit {})",
            c_green(&report.target_branch),
            c_yellow(report.merged_commit_id.short())
        );
    }

    if !report.conflicts.is_empty() || !report.verification_passed {
        std::process::exit(1);
    }

    Ok(())
}

/// Executes `tes merge`.
///
/// Exits with code 1 after emitting the report when verification fails or conflicts are recorded.
///
/// # Errors
/// Returns an error if the repository cannot be opened or the merge operation fails.
pub fn run_merge(ctx: &CliContext, mut args: MergeArgs) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let cfg = coord.oplog().load_config()?;
    let eff_verify = resolve_verify_command(
        args.verify.as_deref(),
        args.verify_cmd.as_deref(),
        cfg.verify_command.as_deref(),
    )?;

    let mut normalized_trailers = Vec::new();

    for raw in args.co_author {
        let trimmed = raw.trim();

        if !trimmed.is_empty() && !trimmed.contains('<') && !trimmed.contains('@') {
            args.agents.push(trimmed.to_string());
            normalized_trailers.push(String::new());
        } else {
            normalized_trailers.push(raw);
        }
    }

    let commit_options = CommitOptions {
        message: args.message,
        author: args.author,
        co_author: if args.no_co_author {
            Some(false)
        } else if !normalized_trailers.is_empty() {
            Some(true)
        } else {
            None
        },
        co_author_trailers: normalized_trailers,
        amend: false,
        dry_run: args.dry_run,
        delete_branch: args.delete_branch,
    };

    let report = coord.merge_agents_with_options(
        &args.agents,
        args.into.as_deref(),
        eff_verify.as_deref(),
        args.keep_open,
        &commit_options,
    )?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if !report.verification_passed {
        println!(
            "{} Verification failed; branch '{}' was not advanced",
            c_red("✗"),
            c_green(&report.target_branch)
        );

        if let Some(output) = report
            .verification_output
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            println!("{output}");
        }
    } else if args.dry_run {
        let badge = if report.conflicts.is_empty() {
            c_green("✓")
        } else {
            c_red("!")
        };
        println!(
            "{} Dry-run merge into {} (verified={}, conflicts={}, skipped_subtrees={}, renames={}, migrations_resequenced={})",
            badge,
            c_green(&report.target_branch),
            c_green("true"),
            if report.conflicts.is_empty() {
                c_green("0")
            } else {
                c_red(report.conflicts.len())
            },
            report.skipped_subtrees,
            report.renames_applied.len(),
            report.migrations_resequenced.len()
        );
    } else if report.created_commit {
        let badge = if report.conflicts.is_empty() {
            c_green("✓")
        } else {
            c_red("!")
        };
        println!(
            "{} Merged into {} -> commit {} (verified={}, conflicts={}, skipped_subtrees={}, renames={}, migrations_resequenced={})",
            badge,
            c_green(&report.target_branch),
            c_yellow(report.merged_commit_id.short()),
            c_green("true"),
            if report.conflicts.is_empty() {
                c_green("0")
            } else {
                c_red(report.conflicts.len())
            },
            report.skipped_subtrees,
            report.renames_applied.len(),
            report.migrations_resequenced.len()
        );

        if !report.deleted_branches.is_empty() {
            println!(
                "  Deleted merged branch(es): {}",
                report.deleted_branches.join(", ")
            );
        }
    } else if !report.conflicts.is_empty() {
        println!(
            "{} Merge encountered {} conflict(s) targeting {} (branch tip {})",
            c_red("!"),
            c_red(report.conflicts.len()),
            c_green(&report.target_branch),
            c_yellow(report.merged_commit_id.short())
        );
    } else {
        println!(
            "Nothing to merge into {} (already up to date at commit {})",
            c_green(&report.target_branch),
            c_yellow(report.merged_commit_id.short())
        );
    }

    if !report.conflicts.is_empty() || !report.verification_passed {
        std::process::exit(1);
    }

    Ok(())
}

/// Executes `tes cherry-pick`.
///
/// Exits with code 1 after emitting the report when conflicts are recorded.
///
/// # Errors
/// Returns an error if the repository cannot be opened or cherry-pick fails.
pub fn run_cherry_pick(
    ctx: &CliContext,
    commit: String,
    branch: Option<String>,
    agent: Option<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(agent.as_deref());
    let report = coord.cherry_pick(&session_id, &commit, branch.as_deref())?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let badge = if report.conflicts.is_empty() {
            c_green("✓")
        } else {
            c_red("!")
        };
        println!(
            "{} Cherry-picked {} onto '{}' -> commit {} ({} file(s) touched)",
            badge,
            c_yellow(report.source_commit.short()),
            c_green(&report.target_branch),
            c_yellow(report.new_commit_id.short()),
            c_cyan(report.touched_paths.len())
        );

        if !report.conflicts.is_empty() {
            println!(
                "  {} {} conflict(s) recorded.",
                c_red("!"),
                report.conflicts.len()
            );
        }
    }

    if !report.conflicts.is_empty() {
        std::process::exit(1);
    }

    Ok(())
}

/// Executes `tes sync`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or auto-rebase fails.
pub fn run_sync(ctx: &CliContext) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let rebased = coord.sync_all_agents()?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&rebased)?);
    } else {
        println!(
            "{} Synced and auto-rebased {} active session(s).",
            c_green("✓"),
            c_cyan(rebased.len())
        );
    }

    Ok(())
}
