//! Branch listing, switching, deletion, and renaming command handlers (`tes branch`, `tes switch`).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::process::Command;

use agent::{Coordinator, detect_session_id};
use serde_json::json;

use crate::output::{
    CliContext, c_cyan, c_dim, c_green, c_yellow, emit_paged_output, format_session_link,
};

/// Options for `tes branch`.
#[derive(Debug, Clone)]
pub struct BranchArgs {
    /// Optional branch name, filter pattern (with `-l`), or rename target.
    pub branch: Option<String>,
    /// Explicitly list branches (`-l` / `--list`).
    pub list: bool,
    /// Print only the active branch name (`--show-current`).
    pub show_current: bool,
    /// List remote-tracking branches (`-r` / `--remotes`).
    pub remotes: bool,
    /// List both local and remote-tracking branches (`-a` / `--all`).
    pub all: bool,
    /// Show commit SHA and subject line in list mode (`-v` / `--verbose`).
    pub verbose: bool,
    /// Delete a merged branch (`-d <branch>`).
    pub delete: Option<String>,
    /// Force-delete a branch (`-D <branch>`).
    pub force_delete: Option<String>,
    /// Rename a branch (`-m <new>` or `-m <old> <new>`).
    pub rename: Option<String>,
    /// Switch the primary editor working tree (`-e` / `--editor`).
    pub editor: bool,
    /// Optional explicit session ID.
    pub agent: Option<String>,
}

/// Options for `tes switch`.
#[derive(Debug, Clone)]
pub struct SwitchArgs {
    /// Target branch name (or start-point when paired with `-b`/`-c`).
    pub branch: Option<String>,
    /// Optional start-point when creating a branch.
    pub start_point: Option<String>,
    /// Create or switch to a branch (`-b` / `-c`).
    pub create: Option<String>,
    /// Parent branch or revision to fork from (`--from`).
    pub from: Option<String>,
    /// Switch the primary editor working tree (`-e` / `--editor`).
    pub editor: bool,
    /// Optional explicit session ID.
    pub agent: Option<String>,
}

/// Options controlling shared branch listing output.
#[derive(Debug, Clone, Default)]
struct ListBranchesOptions<'a> {
    verbose: bool,
    remotes_only: bool,
    include_remotes: bool,
    pattern: Option<&'a str>,
}

/// Matches a string against a simple glob pattern supporting `*` and `?` wildcards.
#[must_use]
pub fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_idx, mut match_idx) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            match_idx = ti;
            pi += 1;
        } else if let Some(si) = star_idx {
            pi = si + 1;
            match_idx += 1;
            ti = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }

    pi == p.len()
}

/// Lists remote-tracking branches (`refs/remotes/...`) via `git for-each-ref`.
fn list_remote_branches(coord: &Coordinator) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(coord.git().repo_root())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("--git-dir")
        .arg(coord.git().git_dir())
        .args(["for-each-ref", "--format=%(refname:short)", "refs/remotes"])
        .output();

    let Ok(output) = out else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
        .map(ToString::to_string)
        .collect()
}

/// Switches the primary editor working tree to `target_br` and optionally binds the caller session.
fn switch_editor_and_bind(
    coord: &Coordinator,
    session_id: &str,
    explicit_agent: Option<&str>,
    target_br: &str,
    json_out: bool,
) -> anyhow::Result<()> {
    let switched = coord.switch_editor_branch(target_br)?;
    let should_bind_caller = session_id != "editor"
        && (explicit_agent.is_some()
            || std::env::var_os("TES_SESSION_ID").is_some()
            || coord
                .oplog()
                .load_current_reconciled()
                .ok()
                .is_some_and(|op| op.view.agents.contains_key(session_id)));

    if should_bind_caller {
        let _ = coord.switch_branch(session_id, &switched);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "editor_branch": switched,
            }))?
        );
    } else {
        println!(
            "{} Switched working tree to branch '{}'",
            c_green("✓"),
            c_green(&switched)
        );
    }

    Ok(())
}

/// Shared implementation for listing local and/or remote branches for `tes branch` and `tes switch`.
fn list_branches(
    ctx: &CliContext,
    coord: &Coordinator,
    session_id: &str,
    opts: ListBranchesOptions<'_>,
) -> anyhow::Result<()> {
    let cfg = coord.oplog().load_config()?;
    let current = coord.oplog().load_current_reconciled()?;
    let editor_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| cfg.trunk_branch.clone());
    let active_branch = current
        .view
        .agents
        .get(session_id)
        .and_then(|a| a.target_branch.clone())
        .unwrap_or_else(|| editor_branch.clone());

    let mut local_branches: BTreeSet<String> = current.view.branches.keys().cloned().collect();

    for git_br in coord.git().list_local_branches() {
        local_branches.insert(git_br);
    }

    local_branches.insert(editor_branch.clone());
    local_branches.insert(active_branch.clone());

    for a in current.view.agents.values() {
        if let Some(tb) = &a.target_branch {
            local_branches.insert(tb.clone());
        }
    }

    let remote_branches: BTreeSet<String> = if opts.remotes_only || opts.include_remotes {
        list_remote_branches(coord).into_iter().collect()
    } else {
        BTreeSet::new()
    };

    let matches_pattern = |name: &str| -> bool {
        match opts.pattern {
            Some(pat) if !pat.trim().is_empty() => glob_matches(pat.trim(), name),
            _ => true,
        }
    };

    let filtered_local: BTreeSet<String> = if opts.remotes_only {
        BTreeSet::new()
    } else {
        local_branches
            .into_iter()
            .filter(|b| matches_pattern(b))
            .collect()
    };
    let filtered_remotes: BTreeSet<String> = remote_branches
        .into_iter()
        .filter(|b| matches_pattern(b))
        .collect();

    if ctx.json {
        let trunk_cid_opt = coord
            .git()
            .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))
            .ok()
            .flatten();
        let main_files = trunk_cid_opt
            .and_then(|cid| coord.git().read_commit(&cid).ok())
            .and_then(|c| coord.git().read_tree_to_map(&c.tree_id).ok())
            .unwrap_or_default();
        let stride = if cfg.port_stride == 0 {
            100
        } else {
            cfg.port_stride
        };

        let mut branch_details = Vec::new();

        for br in &filtered_local {
            let cid_opt = coord
                .git()
                .resolve_ref(&format!("refs/heads/{br}"))
                .ok()
                .flatten()
                .or_else(|| current.view.branches.get(br).cloned());
            let commit_str = cid_opt
                .as_ref()
                .map(|c| c.as_str().to_string())
                .unwrap_or_default();
            let short_commit = cid_opt
                .as_ref()
                .map(|c| c.short().to_string())
                .unwrap_or_default();

            let mut modified_files = BTreeSet::new();

            if *br != cfg.trunk_branch {
                if let Some(cid) = &cid_opt {
                    if let Ok(c) = coord.git().read_commit(cid) {
                        let base_files = trunk_cid_opt
                            .and_then(|t_cid| coord.git().find_merge_base(&t_cid, cid))
                            .filter(|mb_cid| mb_cid != cid)
                            .or_else(|| c.parents.first().copied())
                            .and_then(|b_cid| coord.git().read_commit(&b_cid).ok())
                            .and_then(|bc| coord.git().read_tree_to_map(&bc.tree_id).ok())
                            .unwrap_or_else(|| main_files.clone());

                        if let Ok(br_files) = coord.git().read_tree_to_map(&c.tree_id) {
                            for (p, entry) in &br_files {
                                let changed = match base_files.get(p) {
                                    None => true,
                                    Some(be) => {
                                        be.blob_id != entry.blob_id
                                            || be.executable != entry.executable
                                            || be.kind != entry.kind
                                    }
                                };

                                if changed {
                                    modified_files.insert(p.clone());
                                }
                            }

                            for p in base_files.keys() {
                                if !br_files.contains_key(p) {
                                    modified_files.insert(p.clone());
                                }
                            }
                        }
                    }
                }
            }

            let mut sessions = Vec::new();

            for (aid, a) in &current.view.agents {
                let a_br = a.target_branch.as_deref().unwrap_or(&editor_branch);

                if a_br == br {
                    sessions.push(aid.clone());

                    for p in a.own_blobs.keys() {
                        modified_files.insert(p.clone());
                    }

                    for p in &a.deleted_paths {
                        modified_files.insert(p.clone());
                    }
                }
            }

            let br_slot = coord.branch_slot(&current.view, br, &cfg.trunk_branch);
            let port = coord
                .active_leased_port_for_branch(br, &cfg.trunk_branch)
                .unwrap_or_else(|| cfg.base_port.saturating_add(br_slot.saturating_mul(stride)));

            let ws_dir = coord.branch_sandbox_dir(br);
            branch_details.push(json!({
                "name": br,
                "commit": commit_str,
                "short_commit": short_commit,
                "is_editor_branch": *br == editor_branch,
                "workspace_path": ws_dir.display().to_string(),
                "port": port,
                "sessions": sessions,
                "modified_files": modified_files,
            }));
        }

        let commit_graph = coord.git().list_commit_graph(60);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session_id": session_id,
                "trunk_branch": cfg.trunk_branch,
                "editor_branch": editor_branch,
                "active_branch": active_branch,
                "branches": filtered_local,
                "remote_branches": filtered_remotes,
                "branch_details": branch_details,
                "commit_graph": commit_graph,
            }))?
        );

        return Ok(());
    }

    let mut out = String::new();
    let repo_root = coord.git().repo_root();
    let linked_session = format_session_link(session_id, repo_root, c_dim(session_id));

    for br in &filtered_local {
        if opts.verbose {
            let cid_opt = coord
                .git()
                .resolve_ref(&format!("refs/heads/{br}"))
                .ok()
                .flatten()
                .or_else(|| current.view.branches.get(br).cloned());
            let short_sha = cid_opt
                .as_ref()
                .map(|c| c.short().to_string())
                .unwrap_or_default();
            let subject = cid_opt
                .as_ref()
                .and_then(|c| coord.git().read_commit(c).ok())
                .and_then(|cm| cm.message.lines().next().map(ToString::to_string))
                .unwrap_or_default();

            if *br == active_branch {
                let _ = writeln!(
                    out,
                    "{} {:<20} {} {} {}{}{}",
                    c_green("*"),
                    c_green(br),
                    c_yellow(&short_sha),
                    subject,
                    c_dim("(current session: "),
                    linked_session,
                    c_dim(")")
                );
            } else {
                let _ = writeln!(out, "  {:<20} {} {}", br, c_yellow(&short_sha), subject);
            }
        } else if *br == active_branch {
            let _ = writeln!(
                out,
                "{} {} {}{}{}",
                c_green("*"),
                c_green(br),
                c_dim("(current session: "),
                linked_session,
                c_dim(")")
            );
        } else {
            let _ = writeln!(out, "  {br}");
        }
    }

    for rbr in &filtered_remotes {
        if opts.verbose {
            let cid_opt = coord
                .git()
                .resolve_ref(&format!("refs/remotes/{rbr}"))
                .ok()
                .flatten();
            let short_sha = cid_opt
                .as_ref()
                .map(|c| c.short().to_string())
                .unwrap_or_default();
            let subject = cid_opt
                .as_ref()
                .and_then(|c| coord.git().read_commit(c).ok())
                .and_then(|cm| cm.message.lines().next().map(ToString::to_string))
                .unwrap_or_default();

            let _ = writeln!(
                out,
                "  {:<20} {} {}",
                c_cyan(format!("remotes/{rbr}")),
                c_yellow(&short_sha),
                subject
            );
        } else {
            let _ = writeln!(out, "  {}", c_cyan(format!("remotes/{rbr}")));
        }
    }

    emit_paged_output(
        &out,
        true,
        ctx.no_pager,
        ctx.paginate,
        ctx.json,
        Some(coord),
    );

    Ok(())
}

/// Executes `tes branch`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or the branch mutation fails.
pub fn run_branch(ctx: &CliContext, args: BranchArgs) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(args.agent.as_deref());

    if args.show_current {
        let cfg = coord.oplog().load_config()?;
        let current = coord.oplog().load_current_reconciled()?;
        let editor_branch = coord
            .git()
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        let active_branch = current
            .view
            .agents
            .get(&session_id)
            .and_then(|a| a.target_branch.clone())
            .unwrap_or(editor_branch);

        if ctx.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "session_id": session_id,
                    "active_branch": active_branch,
                }))?
            );
        } else {
            println!("{active_branch}");
        }

        return Ok(());
    }

    if let Some(del_br) = args.delete.as_deref().or(args.force_delete.as_deref()) {
        let force = args.force_delete.is_some();
        let report = coord.delete_branch(&session_id, del_br, force)?;

        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "{} Deleted branch '{}' (was {})",
                c_green("✓"),
                c_green(&report.deleted_branch),
                c_yellow(
                    report
                        .previous_commit
                        .as_ref()
                        .map_or("uncommitted", |c| c.short())
                )
            );

            if !report.detached_sessions.is_empty() {
                println!(
                    "  {} {}",
                    c_dim("Moved sessions to trunk:"),
                    c_cyan(report.detached_sessions.join(", "))
                );
            }
        }

        return Ok(());
    }

    if let Some(rename_arg) = args.rename {
        let (old_name, new_name) = match args.branch.as_deref() {
            Some(second) => (rename_arg, second.to_string()),
            None => {
                let cfg = coord.oplog().load_config()?;
                let current = coord.oplog().load_current_reconciled()?;
                let editor_br = coord
                    .git()
                    .current_branch_name()
                    .unwrap_or_else(|_| cfg.trunk_branch.clone());
                let active_br = current
                    .view
                    .agents
                    .get(&session_id)
                    .and_then(|a| a.target_branch.clone())
                    .unwrap_or(editor_br);
                (active_br, rename_arg)
            }
        };

        let report = coord.rename_branch(&session_id, &old_name, &new_name)?;

        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "{} Renamed branch '{}' -> '{}'",
                c_green("✓"),
                c_yellow(&report.old_branch),
                c_green(&report.new_branch)
            );
        }

        return Ok(());
    }

    let is_list_mode = args.list || args.remotes || args.all || args.branch.is_none();

    if !is_list_mode {
        if let Some(target_br) = args.branch {
            if args.editor {
                return switch_editor_and_bind(
                    &coord,
                    &session_id,
                    args.agent.as_deref(),
                    &target_br,
                    ctx.json,
                );
            }

            let meta = coord.switch_branch(&session_id, &target_br)?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            } else {
                let repo_root = coord.git().repo_root();
                println!(
                    "{} Session '{}' switched to branch '{}' (slot {}, base: {})",
                    c_green("✓"),
                    format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
                    c_green(meta.target_branch.as_deref().unwrap_or(&target_br)),
                    c_yellow(format!("#{}", meta.slot)),
                    c_yellow(meta.base_commit.short())
                );
            }

            return Ok(());
        }
    }

    list_branches(
        ctx,
        &coord,
        &session_id,
        ListBranchesOptions {
            verbose: args.verbose,
            remotes_only: args.remotes,
            include_remotes: args.all,
            pattern: args.branch.as_deref(),
        },
    )
}

/// Executes `tes switch`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or branch switching fails.
pub fn run_switch(ctx: &CliContext, args: SwitchArgs) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(args.agent.as_deref());

    let (target_opt, base_opt) = match args.create {
        Some(new_br) => (Some(new_br), args.from.or(args.branch).or(args.start_point)),
        None => (args.branch, args.from.or(args.start_point)),
    };

    if let Some(target_br) = target_opt {
        if args.editor {
            if let Some(base_ref) = base_opt.as_deref() {
                let _ = coord.switch_branch_with_base(&session_id, &target_br, Some(base_ref))?;
            }

            return switch_editor_and_bind(
                &coord,
                &session_id,
                args.agent.as_deref(),
                &target_br,
                ctx.json,
            );
        }

        let meta = coord.switch_branch_with_base(&session_id, &target_br, base_opt.as_deref())?;

        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&meta)?);
        } else {
            let repo_root = coord.git().repo_root();
            println!(
                "{} Session '{}' switched to branch '{}' (slot {}, base: {})",
                c_green("✓"),
                format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
                c_green(meta.target_branch.as_deref().unwrap_or(&target_br)),
                c_yellow(format!("#{}", meta.slot)),
                c_yellow(meta.base_commit.short())
            );
        }

        return Ok(());
    }

    list_branches(ctx, &coord, &session_id, ListBranchesOptions::default())
}
