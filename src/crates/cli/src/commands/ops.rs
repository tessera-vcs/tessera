//! Operational command handlers: `init`, `claim`, `checkpoint`, `agent`, `stash`, `conflicts`,
//! `resolve`, `undo`, `restore`, `gc`, `watch`, `hook`, `exec`, `config`, `complete`, and passthrough.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use agent::{
    Coordinator, ResolveStrategy, detect_session_id, handle_post_tool_hook, handle_pre_tool_hook,
    run_auto_watch_daemon,
};
use core::ConflictKind;
use serde_json::{Value, json};

use crate::args::{AgentCommands, CompleteArgs, ConfigCommands, HookCommands, StashCommands};
use crate::output::{
    CliContext, c_bold, c_cyan, c_dim, c_green, c_red, c_yellow, emit_paged_output,
    format_session_link,
};

/// Parses a stash reference (`0` or `stash@{0}`) into its zero-based stack index.
///
/// # Errors
/// Returns [`core::CoreError::InvalidArgument`] if `raw` cannot be parsed as a non-negative integer.
pub fn parse_stash_index(raw: Option<&str>) -> core::CoreResult<usize> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };

    let inner = s
        .strip_prefix("stash@{")
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(s);

    inner.parse::<usize>().map_err(|_| {
        core::CoreError::InvalidArgument(format!(
            "invalid stash index '{s}' (expected 0 or stash@{{0}})"
        ))
    })
}

/// Formats a [`ConflictKind`] as a concise human-readable label.
fn format_conflict_kind(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::AstNode => "ast-node",
        ConflictKind::TextHunk => "text-hunk",
        ConflictKind::DeleteModify => "delete-modify",
        ConflictKind::Binary => "binary",
        ConflictKind::FileDirectory => "file-directory",
        ConflictKind::SemanticBuild => "semantic-build",
    }
}

/// Truncates a multiline conflict snippet to at most `max_lines` lines for clean terminal display.
fn snippet_preview(raw: &str, max_lines: usize) -> Vec<String> {
    let trimmed = raw.trim();

    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = trimmed
        .lines()
        .take(max_lines)
        .map(|l| {
            if l.len() > 120 {
                format!("{}...", &l[..117])
            } else {
                l.to_string()
            }
        })
        .collect();

    if trimmed.lines().count() > max_lines {
        lines.push("...".to_string());
    }

    lines
}

/// Executes `tes init`, resolving relative `<path>` against `-C <repo>` (`ctx.cwd`).
///
/// # Errors
/// Returns an error if repository or harness initialization fails.
pub fn run_init(
    ctx: &CliContext,
    path: Option<PathBuf>,
    branch: Option<String>,
    harness: Option<String>,
    local: bool,
) -> anyhow::Result<()> {
    let target = match path {
        Some(p) if p.is_absolute() => p,
        Some(p) => ctx.cwd.join(p),
        None => ctx.cwd.clone(),
    };

    let (_, report) = Coordinator::init_repo_with_full_options(
        &target,
        branch.as_deref(),
        harness.as_deref(),
        local,
    )?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{} Initialized Tessera in {} (branch: {}, head: {}, op: {})",
            c_green("✓"),
            c_bold(&report.repo_root),
            c_green(&report.trunk_branch),
            c_yellow(report.head_commit.short()),
            c_yellow(report.root_op_id.short())
        );

        if local {
            println!(
                "  {} {}",
                c_dim("Local-only mode:"),
                c_cyan("newly created harness files added to .git/info/exclude")
            );
        }

        println!(
            "  {} {}",
            c_dim("Installed harness integrations:"),
            c_cyan(report.harness_files_installed.join(", "))
        );
    }

    Ok(())
}

/// Executes `tes claim`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or the claim fails.
pub fn run_claim(
    ctx: &CliContext,
    intent: Option<String>,
    branch: Option<String>,
    from: Option<String>,
    agent: Option<String>,
    symbols: Vec<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(agent.as_deref());
    let meta = coord.claim_intent_and_symbols_with_branch_and_base(
        &session_id,
        intent.as_deref(),
        &symbols,
        branch.as_deref(),
        from.as_deref(),
    )?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&meta)?);
    } else {
        let repo_root = coord.git().repo_root();
        println!(
            "{} Updated session '{}': intent=\"{}\"",
            c_green("✓"),
            format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
            c_bold(&meta.intent)
        );
    }

    Ok(())
}

/// Executes `tes checkpoint`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or checkpoint creation fails.
pub fn run_checkpoint(
    ctx: &CliContext,
    label: String,
    agent: Option<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(agent.as_deref());
    let op_id = coord.create_checkpoint(&session_id, &label)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session_id": session_id,
                "label": label,
                "op_id": op_id,
            }))?
        );
    } else {
        println!(
            "{} Created checkpoint '{}' at operation {}",
            c_green("✓"),
            c_cyan(&label),
            c_yellow(op_id.short())
        );
    }

    Ok(())
}

/// Executes `tes agent` (`tes session`).
///
/// # Errors
/// Returns an error if the repository cannot be opened or the session operation fails.
pub fn run_agent(ctx: &CliContext, sub: AgentCommands) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;

    match sub {
        AgentCommands::Spawn {
            name,
            intent,
            scopes,
        } => {
            let meta = coord.spawn_cow_agent(&name, intent.as_deref(), scopes)?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            } else {
                let repo_root = coord.git().repo_root();
                println!(
                    "{} Spawned COW workspace for '{}' at {}",
                    c_green("✓"),
                    format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
                    c_bold(meta.workspace_path.as_deref().unwrap_or("."))
                );
            }
        }
        AgentCommands::List => {
            let report = coord.radar()?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&report.active_agents)?);
            } else {
                let repo_root = coord.git().repo_root();

                for a in &report.active_agents {
                    println!(
                        "{}\tslot={}\tchange={}\tcommit={}\tintent=\"{}\"",
                        format_session_link(&a.agent_id, repo_root, c_cyan(&a.agent_id)),
                        c_yellow(a.slot),
                        c_yellow(a.change_id.short()),
                        c_yellow(a.head_commit.short()),
                        a.intent
                    );
                }
            }
        }
        AgentCommands::Focus { name } => {
            #[allow(unused_mut)]
            let mut report = coord.focus_session(&name)?;

            #[cfg(target_os = "macos")]
            if !report.focused && std::env::var_os("TES_FROM_IDE_EXTENSION").is_none() {
                if let Some(wp) = &report.waypoint {
                    if wp.tty.is_none() && (wp.ide_pid.is_some() || wp.conversation_id.is_some()) {
                        let scheme = if wp.harness == "antigravity"
                            || report.session_id.starts_with("antigravity-")
                        {
                            "antigravity-ide"
                        } else if wp.harness == "cursor" || report.session_id.starts_with("cursor-")
                        {
                            "cursor"
                        } else if wp.harness == "windsurf"
                            || report.session_id.starts_with("windsurf-")
                        {
                            "windsurf"
                        } else {
                            "vscode"
                        };

                        let uri = format!(
                            "{scheme}://Lunaris.tessera/focus?session={}",
                            report.session_id
                        );

                        if Command::new("open")
                            .arg(&uri)
                            .output()
                            .is_ok_and(|o| o.status.success())
                        {
                            report.focused = true;
                            report.mechanism = format!("{scheme}-uri");
                            report.detail = format!(
                                "Focused IDE session via {scheme}:// for {}",
                                report.session_id
                            );
                        }
                    }
                }
            }

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let badge = if report.focused {
                    c_green("✓")
                } else {
                    c_cyan("ℹ")
                };
                println!(
                    "{} {} [harness: {}, mechanism: {}]",
                    badge,
                    report.detail,
                    c_cyan(&report.harness),
                    c_yellow(&report.mechanism)
                );
            }
        }
        AgentCommands::Exec { name, cmd } => {
            let code = coord.exec_in_agent(&name, &cmd)?;

            if code != 0 {
                std::process::exit(code);
            }
        }
        AgentCommands::Close { name, all } => {
            if all {
                let removed = coord.clear_agents(true)?;

                if ctx.json {
                    println!("{}", serde_json::to_string_pretty(&removed)?);
                } else {
                    println!(
                        "{} Closed {} session(s).",
                        c_green("✓"),
                        c_cyan(removed.len())
                    );
                }
            } else if let Some(target_name) = name {
                let closed = coord.close_agent(&target_name)?;

                if ctx.json {
                    println!("{}", serde_json::to_string_pretty(&closed)?);
                } else {
                    println!(
                        "{} Closed agent session '{}'.",
                        c_green("✓"),
                        c_cyan(&closed.agent_id)
                    );
                }
            } else {
                let sid = detect_session_id(None);
                let closed = coord.close_agent(&sid)?;

                if ctx.json {
                    println!("{}", serde_json::to_string_pretty(&closed)?);
                } else {
                    println!(
                        "{} Closed current session '{}'.",
                        c_green("✓"),
                        c_cyan(&closed.agent_id)
                    );
                }
            }
        }
        AgentCommands::Clear { force } => {
            let removed = coord.clear_agents(force)?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&removed)?);
            } else {
                println!(
                    "{} Cleared {} session(s).",
                    c_green("✓"),
                    c_cyan(removed.len())
                );
            }
        }
    }

    Ok(())
}

/// Executes `tes stash`.
///
/// Exits with code 1 after emitting the report when `pop` or `apply` records conflicts.
///
/// # Errors
/// Returns an error if the repository cannot be opened or the stash operation fails.
pub fn run_stash(
    ctx: &CliContext,
    sub: Option<StashCommands>,
    message: Option<String>,
    branch: Option<String>,
    agent: Option<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(agent.as_deref());
    let action = sub.unwrap_or(StashCommands::Push {
        message: message.clone(),
        branch: branch.clone(),
        paths: Vec::new(),
    });

    match action {
        StashCommands::Push {
            message: sub_msg,
            branch: sub_br,
            paths,
        } => {
            let mut eff_msg = sub_msg.or(message);
            let eff_br = sub_br.or(branch);
            let mut eff_paths = paths;

            if eff_msg.is_none() && eff_paths.len() == 1 {
                let candidate = &eff_paths[0];
                let cfg = coord.oplog().load_config()?;
                let current = coord.oplog().load_current_reconciled()?;
                let default_branch = coord
                    .git()
                    .current_branch_name()
                    .unwrap_or_else(|_| cfg.trunk_branch.clone());
                let target_branch = eff_br
                    .as_deref()
                    .map(ToString::to_string)
                    .or_else(|| {
                        current
                            .view
                            .agents
                            .get(&session_id)
                            .and_then(|a| a.target_branch.clone())
                    })
                    .unwrap_or(default_branch.clone());
                let branch_ws = if target_branch == default_branch {
                    coord.git().repo_root().to_path_buf()
                } else {
                    coord.branch_sandbox_dir(&target_branch)
                };
                let base_has_path = coord
                    .git()
                    .resolve_ref(&format!("refs/heads/{target_branch}"))
                    .ok()
                    .flatten()
                    .and_then(|cid| coord.git().read_commit(&cid).ok())
                    .and_then(|c| coord.git().read_tree_to_map(&c.tree_id).ok())
                    .is_some_and(|m| m.contains_key(candidate));
                let meta_owns = current
                    .view
                    .agents
                    .get(&session_id)
                    .is_some_and(|m| m.owns_path(candidate));

                let is_known_path = coord.git().repo_root().join(candidate).exists()
                    || branch_ws.join(candidate).exists()
                    || meta_owns
                    || base_has_path;

                if !is_known_path {
                    eff_msg = eff_paths.pop();
                }
            }

            let entry = coord.stash_push_with_paths(
                &session_id,
                eff_msg.as_deref(),
                eff_br.as_deref(),
                &eff_paths,
            )?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                let total_files = entry.own_blobs.len() + entry.deleted_paths.len();
                println!(
                    "{} Saved working state on '{}' ({} file(s)): {}",
                    c_green("✓"),
                    c_green(&entry.branch),
                    c_cyan(total_files),
                    c_bold(&entry.message)
                );
            }
        }
        StashCommands::Pop { index } => {
            let idx = parse_stash_index(index.as_deref())?;
            let report = coord.stash_pop(&session_id, idx, false)?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let badge = if report.has_conflicts {
                    c_red("!")
                } else {
                    c_green("✓")
                };
                println!(
                    "{} Popped stash@{{{}}} onto '{}' ({} file(s))",
                    badge,
                    idx,
                    c_green(&report.applied_branch),
                    c_cyan(report.restored_paths.len())
                );

                if report.has_conflicts {
                    println!(
                        "  {}",
                        c_red(
                            "Conflicts recorded during 3-way AST application (stash entry preserved)."
                        )
                    );
                }
            }

            if report.has_conflicts {
                std::process::exit(1);
            }
        }
        StashCommands::Apply { index } => {
            let idx = parse_stash_index(index.as_deref())?;
            let report = coord.stash_pop(&session_id, idx, true)?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let badge = if report.has_conflicts {
                    c_red("!")
                } else {
                    c_green("✓")
                };
                println!(
                    "{} Applied stash@{{{}}} onto '{}' ({} file(s))",
                    badge,
                    idx,
                    c_green(&report.applied_branch),
                    c_cyan(report.restored_paths.len())
                );

                if report.has_conflicts {
                    println!(
                        "  {}",
                        c_red("Conflicts recorded during 3-way AST application.")
                    );
                }
            }

            if report.has_conflicts {
                std::process::exit(1);
            }
        }
        StashCommands::List => {
            let stashes = coord.stash_list()?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&stashes)?);
            } else if stashes.is_empty() {
                println!("{}", c_dim("No stashed entries."));
            } else {
                let mut out = String::new();
                let repo_root = coord.git().repo_root();

                for (idx, s) in stashes.iter().enumerate() {
                    let total_files = s.own_blobs.len() + s.deleted_paths.len();
                    let _ = writeln!(
                        out,
                        "{}: on {} ({}, {} file(s)): {}",
                        c_yellow(format!("stash@{{{idx}}}")),
                        c_green(&s.branch),
                        format_session_link(&s.session_id, repo_root, c_cyan(&s.session_id)),
                        total_files,
                        s.message
                    );
                }

                emit_paged_output(
                    &out,
                    true,
                    ctx.no_pager,
                    ctx.paginate,
                    ctx.json,
                    Some(&coord),
                );
            }
        }
        StashCommands::Drop { index } => {
            let idx = parse_stash_index(index.as_deref())?;
            let dropped = coord.stash_drop(&session_id, Some(idx))?;

            if ctx.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "dropped": dropped }))?
                );
            } else {
                println!(
                    "{} Dropped stash@{{{}}} ({} entry removed)",
                    c_green("✓"),
                    idx,
                    dropped
                );
            }
        }
        StashCommands::Clear => {
            let dropped = coord.stash_drop(&session_id, None)?;

            if ctx.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "dropped": dropped }))?
                );
            } else {
                println!(
                    "{} Cleared {} stash entry/entries.",
                    c_green("✓"),
                    c_cyan(dropped)
                );
            }
        }
    }

    Ok(())
}

/// Executes `tes conflicts`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or conflicts cannot be loaded.
pub fn run_conflicts(ctx: &CliContext) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let radar = coord.radar()?;

    if ctx.json {
        let mut enriched = Vec::new();

        for c in &radar.active_conflicts {
            let mut val = serde_json::to_value(c).unwrap_or_else(|_| json!({}));

            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "ast_union_preview".to_string(),
                    json!(coord.compute_ast_union_preview(c)),
                );
            }

            enriched.push(val);
        }

        println!("{}", serde_json::to_string_pretty(&enriched)?);
        return Ok(());
    }

    if radar.active_conflicts.is_empty() {
        println!("{} No active conflicts.", c_green("✓"));
        return Ok(());
    }

    let mut out_buf = String::new();
    let repo_root = coord.git().repo_root();

    for c in &radar.active_conflicts {
        let kind_label = format_conflict_kind(c.kind);
        let symbol_suffix = match c.symbol.as_deref() {
            Some(sym) if !sym.trim().is_empty() => format!(", symbol: {}", c_cyan(sym)),
            _ => String::new(),
        };

        let _ = writeln!(
            out_buf,
            "{} Conflict [{}] at {} ({}{})",
            c_red("!"),
            c_yellow(c.id.short()),
            c_bold(&c.path),
            c_dim(kind_label),
            symbol_suffix
        );

        if !c.agent_intents.is_empty() {
            for (agent_id, intent) in &c.agent_intents {
                let _ = writeln!(
                    out_buf,
                    "    {} {}: \"{}\"",
                    c_dim("agent"),
                    format_session_link(agent_id, repo_root, c_cyan(agent_id)),
                    intent
                );
            }
        }

        if let Some(diag) = c
            .diagnostics
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let _ = writeln!(out_buf, "    {}", c_dim("diagnostics:"));

            for line in snippet_preview(diag, 4) {
                let _ = writeln!(out_buf, "      {line}");
            }
        } else {
            let left_lines = snippet_preview(&c.left_snippet, 3);
            let right_lines = snippet_preview(&c.right_snippet, 3);

            if !left_lines.is_empty() {
                let _ = writeln!(out_buf, "    {}", c_green("ours:"));

                for line in left_lines {
                    let _ = writeln!(out_buf, "      {line}");
                }
            }

            if !right_lines.is_empty() {
                let _ = writeln!(out_buf, "    {}", c_yellow("theirs:"));

                for line in right_lines {
                    let _ = writeln!(out_buf, "      {line}");
                }
            }
        }
    }

    emit_paged_output(
        &out_buf,
        false,
        ctx.no_pager,
        ctx.paginate,
        ctx.json,
        Some(&coord),
    );

    Ok(())
}

/// Executes `tes resolve`, rejecting invalid strategies and requiring explicit content when
/// `--strategy content` is chosen.
///
/// # Errors
/// Returns an error if the strategy is unknown, required content is missing, or resolution fails.
pub fn run_resolve(
    ctx: &CliContext,
    path: String,
    strategy: String,
    content: Option<String>,
    content_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let strat = match strategy.trim() {
        "ours" => ResolveStrategy::Ours,
        "theirs" => ResolveStrategy::Theirs,
        "ast-union" => ResolveStrategy::AstUnion,
        "working" => ResolveStrategy::WorkingFile,
        "content" => {
            let resolved_text = if let Some(cf) = content_file {
                let resolved_cf = if cf.is_absolute() {
                    cf
                } else {
                    ctx.cwd.join(cf)
                };
                fs::read_to_string(&resolved_cf).map_err(|err| {
                    anyhow::anyhow!(
                        "failed to read --content-file '{}': {err}",
                        resolved_cf.display()
                    )
                })?
            } else if let Some(text) = content {
                text
            } else {
                anyhow::bail!(
                    "--strategy content requires --content <TEXT> or --content-file <PATH>"
                );
            };

            ResolveStrategy::ExplicitContent(resolved_text)
        }
        other => anyhow::bail!(
            "unknown conflict resolution strategy '{other}' (expected: ours, theirs, ast-union, working, content)"
        ),
    };

    let blob_id = coord.resolve_conflict(&path, strat)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "resolved_path": path,
                "blob_id": blob_id,
            }))?
        );
    } else {
        println!(
            "{} Resolved conflict at {} (blob {})",
            c_green("✓"),
            c_bold(&path),
            c_yellow(blob_id.short())
        );
    }

    Ok(())
}

/// Executes `tes undo`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or rollback fails.
pub fn run_undo(
    ctx: &CliContext,
    op: Option<String>,
    checkpoint: Option<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let target = op.or(checkpoint);
    let undo_op = coord.undo(target.as_deref())?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&undo_op)?);
    } else {
        println!(
            "{} Restored state via operation {}",
            c_green("✓"),
            c_yellow(undo_op.id.short())
        );
    }

    Ok(())
}

/// Executes `tes restore`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or restoration fails.
pub fn run_restore(
    ctx: &CliContext,
    targets: Vec<String>,
    source: Option<String>,
    branch: Option<String>,
    agent: Option<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(agent.as_deref());
    let cfg = coord.oplog().load_config()?;
    let current = coord.oplog().load_current_reconciled()?;
    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| cfg.trunk_branch.clone());
    let eff_branch = branch
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            current
                .view
                .agents
                .get(&session_id)
                .and_then(|a| a.target_branch.clone())
        })
        .unwrap_or(default_branch);

    let (resolved_source, resolved_paths): (Option<String>, Vec<String>) = if let Some(src) = source
    {
        let paths = if targets.is_empty() {
            vec![".".to_string()]
        } else {
            targets
        };
        (Some(src), paths)
    } else if targets.is_empty() {
        (None, vec![".".to_string()])
    } else if targets.len() == 1 {
        let arg0 = &targets[0];
        let path_on_disk = coord.git().repo_root().join(arg0).exists();
        let is_rev = coord
            .resolve_checkpoint_or_revision(arg0, &eff_branch)
            .ok()
            .flatten()
            .is_some();

        if is_rev && !path_on_disk {
            (Some(arg0.clone()), Vec::new())
        } else if path_on_disk {
            (None, vec![arg0.clone()])
        } else if is_rev {
            (Some(arg0.clone()), Vec::new())
        } else {
            (None, vec![arg0.clone()])
        }
    } else {
        let arg0 = &targets[0];
        let path_on_disk = coord.git().repo_root().join(arg0).exists();
        let is_rev = coord
            .resolve_checkpoint_or_revision(arg0, &eff_branch)
            .ok()
            .flatten()
            .is_some();

        if is_rev && !path_on_disk {
            (Some(arg0.clone()), targets[1..].to_vec())
        } else {
            (None, targets)
        }
    };

    let report = coord.restore(
        &session_id,
        branch.as_deref(),
        resolved_source.as_deref(),
        &resolved_paths,
    )?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if report.branch_reset {
        println!(
            "{} Restored branch '{}' to commit {}",
            c_green("✓"),
            c_green(&report.branch),
            c_yellow(report.source_commit_id.short())
        );
    } else {
        println!(
            "{} Restored {} file(s) on branch '{}' from commit {}",
            c_green("✓"),
            c_cyan(report.restored_paths.len()),
            c_green(&report.branch),
            c_yellow(report.source_commit_id.short())
        );
    }

    Ok(())
}

/// Executes `tes gc`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or compaction fails.
pub fn run_gc(ctx: &CliContext) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let removed = coord.oplog().gc_compact()?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "compacted_operations": removed }))?
        );
    } else {
        println!(
            "{} Compacted {} ephemeral micro-snapshot operation(s).",
            c_green("✓"),
            c_cyan(removed)
        );
    }

    Ok(())
}

/// Executes `tes watch`, emitting compact flushed NDJSON lines when `--json` is enabled.
///
/// # Errors
/// Returns an error if the repository cannot be opened or watching fails.
pub fn run_watch(
    ctx: &CliContext,
    agent: Option<String>,
    interval_ms: u64,
    once: bool,
    auto_daemon: bool,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;

    if auto_daemon {
        run_auto_watch_daemon(&coord)?;
        return Ok(());
    }

    let session_id = detect_session_id(agent.as_deref());
    let mut stdout = io::stdout();

    loop {
        let outcomes = coord.watch_tick(&session_id)?;

        if ctx.json {
            if once || !outcomes.is_empty() {
                writeln!(stdout, "{}", serde_json::to_string(&outcomes)?)?;
                stdout.flush()?;
            }
        } else {
            for out in &outcomes {
                if let Some(notice) = &out.context_notice {
                    println!("{}", c_yellow(notice));
                } else {
                    println!(
                        "{} Snapshotted '{}' for session '{}'",
                        c_cyan("[Tessera Watch]:"),
                        c_bold(&out.path),
                        c_cyan(&out.session_id)
                    );
                }
            }
        }

        if once {
            break;
        }

        std::thread::sleep(std::time::Duration::from_millis(interval_ms.max(50)));
    }

    Ok(())
}

/// Executes `tes config`, always mutating ONLY `.git/tessera/config.toml` via
/// [`core::OpLogStore::load_local_config`] so trusted `tessera.toml` fields are never baked
/// permanently into local config.
///
/// # Errors
/// Returns an error if configuration parsing, validation, or saving fails.
pub fn run_config(
    ctx: &CliContext,
    sub: Option<ConfigCommands>,
    verify_cmd: Option<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;

    if let Some(vcmd) = verify_cmd {
        let mut local_cfg = coord.oplog().load_local_config()?;
        local_cfg.verify_command = if vcmd.trim().is_empty() {
            None
        } else {
            Some(vcmd)
        };
        coord.oplog().save_config(&local_cfg)?;
    }

    match sub.unwrap_or(ConfigCommands::Show) {
        ConfigCommands::Show => {}
        ConfigCommands::Trust => {
            let digest = coord.oplog().trust_repo_config()?;

            if !ctx.json {
                println!(
                    "{} Trusted repository tessera.toml (blake3: {})",
                    c_green("✓"),
                    c_yellow(&digest[..12.min(digest.len())])
                );
            }
        }
        ConfigCommands::Untrust => {
            coord.oplog().untrust_repo_config()?;

            if !ctx.json {
                println!(
                    "{} Revoked trust for repository tessera.toml execution settings",
                    c_green("✓")
                );
            }
        }
        ConfigCommands::AddCacheDir { dirs } => {
            let mut local_cfg = coord.oplog().load_local_config()?;

            for d in dirs {
                let clean = d.trim().trim_matches('/').to_string();

                if !clean.is_empty() && !local_cfg.cow_cache_dirs.contains(&clean) {
                    local_cfg.cow_cache_dirs.push(clean);
                }
            }

            coord.oplog().save_config(&local_cfg)?;
        }
        ConfigCommands::AddBinDir { dirs } => {
            let mut local_cfg = coord.oplog().load_local_config()?;

            for d in dirs {
                let clean = d.trim().trim_matches('/').to_string();

                if !clean.is_empty() && !local_cfg.extra_bin_dirs.contains(&clean) {
                    local_cfg.extra_bin_dirs.push(clean);
                }
            }

            coord.oplog().save_config(&local_cfg)?;
        }
        ConfigCommands::AddPackageGlob { globs } => {
            let mut local_cfg = coord.oplog().load_local_config()?;

            for g in globs {
                let clean = g.trim().to_string();

                if !clean.is_empty() && !local_cfg.package_globs.contains(&clean) {
                    local_cfg.package_globs.push(clean);
                }
            }

            coord.oplog().save_config(&local_cfg)?;
        }
        ConfigCommands::SetEnv { key, value } => {
            if workspace::is_forbidden_custom_env_key(&key) {
                anyhow::bail!(
                    "environment variable '{}' is restricted and cannot be set via custom_env",
                    key.trim()
                );
            }

            let mut local_cfg = coord.oplog().load_local_config()?;
            local_cfg.custom_env.insert(key.trim().to_string(), value);
            coord.oplog().save_config(&local_cfg)?;
        }
        ConfigCommands::Set { key, value } => {
            let mut local_cfg = coord.oplog().load_local_config()?;

            match key.trim() {
                "trunk_branch" => {
                    core::validate_ref_name(&value)?;
                    local_cfg.trunk_branch = value;
                }
                "base_port" => local_cfg.base_port = value.parse()?,
                "port_stride" => local_cfg.port_stride = value.parse()?,
                "port_isolation" | "isolate_ports" => {
                    local_cfg.port_isolation = match value.trim().to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" | "on" => true,
                        "false" | "0" | "no" | "off" => false,
                        other => anyhow::bail!(
                            "invalid value '{other}' for port_isolation (expected true or false)"
                        ),
                    };
                }
                "verify_command" => {
                    local_cfg.verify_command = if value.trim().is_empty() {
                        None
                    } else {
                        Some(value)
                    };
                }
                "watch_interval_ms" => local_cfg.watch_interval_ms = value.parse()?,
                "watch_timeout_secs" => local_cfg.watch_timeout_secs = value.parse()?,
                "co_author" => {
                    let v = value.trim();

                    match v.to_ascii_lowercase().as_str() {
                        "true" | "1" | "yes" | "on" => local_cfg.co_author = true,
                        "false" | "0" | "no" | "off" | "" => local_cfg.co_author = false,
                        _ => {
                            local_cfg.co_author = true;
                            local_cfg.co_author_trailer = v.to_string();
                        }
                    }
                }
                "co_author_trailer" => {
                    local_cfg.co_author_trailer = value.trim().to_string();
                }
                "auto_isolate_dirty_claims" => {
                    local_cfg.auto_isolate_dirty_claims = match value
                        .trim()
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "true" | "1" | "yes" | "on" => true,
                        "false" | "0" | "no" | "off" => false,
                        other => anyhow::bail!(
                            "invalid value '{other}' for auto_isolate_dirty_claims (expected true or false)"
                        ),
                    };
                }
                "pager" => {
                    let trimmed = value.trim();
                    local_cfg.pager = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    };
                }
                other => anyhow::bail!("unknown config key '{other}'"),
            }

            coord.oplog().save_config(&local_cfg)?;
        }
    }

    let effective_cfg = coord.oplog().load_config()?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&effective_cfg)?);
    } else {
        println!(
            "trunk_branch = {:?}\nbase_port = {}\nport_stride = {}\nport_isolation = {}\nauto_isolate_dirty_claims = {}\nwatch_interval_ms = {}\nwatch_timeout_secs = {}\nco_author = {}\nco_author_trailer = {:?}\npager = {:?}\nverify_command = {:?}\ncow_cache_dirs = {:?}\nextra_bin_dirs = {:?}\npackage_globs = {:?}\ncustom_env = {:?}",
            effective_cfg.trunk_branch,
            effective_cfg.base_port,
            effective_cfg.port_stride,
            effective_cfg.port_isolation,
            effective_cfg.auto_isolate_dirty_claims,
            effective_cfg.watch_interval_ms,
            effective_cfg.watch_timeout_secs,
            effective_cfg.co_author,
            effective_cfg.co_author_trailer,
            effective_cfg.pager,
            effective_cfg.verify_command,
            effective_cfg.cow_cache_dirs,
            effective_cfg.extra_bin_dirs,
            effective_cfg.package_globs,
            effective_cfg.custom_env
        );
    }

    Ok(())
}

/// Executes `tes exec`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or command execution fails to spawn.
pub fn run_exec(
    ctx: &CliContext,
    branch: Option<String>,
    agent: Option<String>,
    cmd: Vec<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(agent.as_deref());
    let code = coord.exec_on_branch(&session_id, branch.as_deref(), &cmd)?;
    std::process::exit(code);
}

/// Executes `tes <external-git-subcommand>` while preserving subdirectory `cwd`, stripping
/// `GIT_DIR` / `GIT_WORK_TREE`, and syncing Tessera agents afterward.
///
/// # Errors
/// Returns an error if `args` is empty or `git` cannot be spawned.
pub fn run_external(ctx: &CliContext, args: Vec<String>) -> anyhow::Result<()> {
    let Some(subcmd) = args.first() else {
        anyhow::bail!("missing subcommand");
    };

    let coord_opt = Coordinator::open(&ctx.cwd).ok();
    let exec_dir = if let Some(coord) = &coord_opt {
        if let Some(inv_dir) = coord.invocation_dir() {
            if let Ok(rel) = ctx.cwd.strip_prefix(inv_dir) {
                let mapped = coord.git().repo_root().join(rel);

                if mapped.is_dir() {
                    mapped
                } else {
                    coord.git().repo_root().to_path_buf()
                }
            } else {
                coord.git().repo_root().to_path_buf()
            }
        } else if ctx.cwd.is_dir() {
            ctx.cwd.clone()
        } else {
            coord.git().repo_root().to_path_buf()
        }
    } else {
        ctx.cwd.clone()
    };

    let status = Command::new("git")
        .current_dir(&exec_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(&args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to execute 'git {subcmd}': {e}"))?;

    if let Some(coord) = &coord_opt {
        let _ = coord.sync_all_agents();
    }

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

/// Dispatches AI harness lifecycle hooks (`prompt`, `pre-tool`, `post-tool`, `git-sync`) to the coordinator.
///
/// # Errors
/// Returns an error if session registration, pre/post-tool reconciliation, or JSON serialization fails.
pub fn run_hook(
    ctx: &CliContext,
    sub: HookCommands,
    session_flag: Option<String>,
) -> anyhow::Result<()> {
    let Ok(coord) = Coordinator::open(&ctx.cwd) else {
        return Ok(());
    };

    let (event, path_flags, prompt_flag) = match sub {
        HookCommands::Prompt { prompt } => ("prompt", Vec::new(), prompt),
        HookCommands::PreTool { mut path, paths } => {
            path.extend(paths);
            ("pre-tool", path, None)
        }
        HookCommands::PostTool { mut path, paths } => {
            path.extend(paths);
            ("post-tool", path, None)
        }
        HookCommands::GitSync => ("git-sync", Vec::new(), None),
        HookCommands::Other(_) => return Ok(()),
    };

    let stdin_json = if !path_flags.is_empty() || prompt_flag.is_some() || event == "git-sync" {
        None
    } else {
        read_nonblocking_stdin_json()
    };

    let payload_session = stdin_json
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let session_id = detect_session_id(session_flag.or(payload_session).as_deref());

    match event {
        "prompt" => {
            let extracted_prompt = prompt_flag.or_else(|| {
                stdin_json
                    .as_ref()
                    .and_then(|v| v.get("prompt"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            });
            let meta = coord.ensure_session(&session_id, extracted_prompt.as_deref())?;

            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            }
        }
        "pre-tool" => {
            let paths = if !path_flags.is_empty() {
                path_flags
            } else {
                extract_hook_file_paths(&stdin_json)
            };

            for target_path in paths {
                let outcome = handle_pre_tool_hook(&coord, &session_id, &target_path)?;

                if ctx.json {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else if let Some(warn) = outcome.radar_warning {
                    println!("{warn}");
                }
            }
        }
        "post-tool" => {
            let explicit_cli_path = !path_flags.is_empty();
            let paths = if !path_flags.is_empty() {
                path_flags
            } else {
                extract_hook_file_paths(&stdin_json)
            };

            for target_path in paths {
                let outcome = handle_post_tool_hook(&coord, &session_id, &target_path)?;

                if ctx.json {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else if let Some(notice) = outcome.context_notice {
                    println!("{notice}");
                } else if explicit_cli_path {
                    println!(
                        "{} Reconciled '{}' for session '{}'",
                        c_green("✓"),
                        outcome.path,
                        c_cyan(&outcome.session_id)
                    );
                }
            }
        }
        "git-sync" => {
            let _ = coord.sync_all_agents();
        }
        _ => {}
    }

    Ok(())
}

/// Reads a JSON hook payload (bounded at 10 MiB) from non-interactive stdin.
fn read_nonblocking_stdin_json() -> Option<Value> {
    if io::stdin().is_terminal() {
        return None;
    }

    let mut buf = String::new();
    let mut limited = io::stdin().take(10 * 1024 * 1024);

    if limited.read_to_string(&mut buf).is_ok() && !buf.trim().is_empty() {
        serde_json::from_str(buf.trim()).ok()
    } else {
        None
    }
}

/// Extracts target file path(s) from a JSON hook payload across Claude Code, Codex (`apply_patch`), Cursor, and Antigravity schemas.
#[must_use]
pub fn extract_hook_file_paths(stdin_json: &Option<Value>) -> Vec<String> {
    let Some(v) = stdin_json.as_ref() else {
        return Vec::new();
    };

    if let Some(tool_input) = v.get("tool_input") {
        for key in ["file_path", "path", "TargetFile", "AbsolutePath"] {
            if let Some(s) = tool_input.get(key).and_then(Value::as_str) {
                if !s.trim().is_empty() {
                    return vec![s.trim().to_string()];
                }
            }
        }

        if let Some(patch_str) = tool_input.as_str() {
            let paths = extract_apply_patch_paths(patch_str);

            if !paths.is_empty() {
                return paths;
            }
        }

        for key in ["patch", "input", "diff"] {
            if let Some(patch_str) = tool_input.get(key).and_then(Value::as_str) {
                let paths = extract_apply_patch_paths(patch_str);

                if !paths.is_empty() {
                    return paths;
                }
            }
        }
    }

    for key in ["file_path", "path"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return vec![s.trim().to_string()];
            }
        }
    }

    if let Some(patch_str) = v.get("patch").and_then(Value::as_str) {
        let paths = extract_apply_patch_paths(patch_str);

        if !paths.is_empty() {
            return paths;
        }
    }

    Vec::new()
}

/// Extracts modified/added file paths from an OpenAI Codex `apply_patch` body.
fn extract_apply_patch_paths(patch: &str) -> Vec<String> {
    let mut paths = Vec::new();

    for line in patch.lines() {
        let trimmed = line.trim();

        for prefix in [
            "*** Update File:",
            "*** Add File:",
            "*** Delete File:",
            "*** Move to:",
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let path = rest.trim();

                if !path.is_empty() && !paths.iter().any(|p| p == path) {
                    paths.push(path.to_string());
                }
            }
        }
    }

    paths
}

/// Generates shell-specific tab-completion candidates for `tes` subcommands, flags, branches, revisions, and active sessions.
pub fn run_complete(cwd: &Path, args: &CompleteArgs) {
    let stripped: Vec<String> = if args.words.first().map(String::as_str) == Some("--") {
        args.words[1..].to_vec()
    } else {
        args.words.clone()
    };

    let words: Vec<String> = if stripped
        .first()
        .is_none_or(|w| w != "tes" && !w.ends_with("/tes"))
    {
        let mut v = vec!["tes".to_string()];
        v.extend(stripped);
        v
    } else {
        stripped
    };

    let cword = args.cword.unwrap_or_else(|| words.len().saturating_sub(1));
    let cur = words.get(cword).map(String::as_str).unwrap_or("");
    let prev = if cword > 0 {
        words.get(cword - 1).map(String::as_str).unwrap_or("")
    } else {
        ""
    };

    let subcmd = words
        .iter()
        .enumerate()
        .skip(1)
        .take_while(|(idx, _)| *idx < cword)
        .find_map(|(_, w)| {
            if !w.starts_with('-') {
                Some(w.as_str())
            } else {
                None
            }
        });

    let coord_opt = Coordinator::open(cwd).ok();

    let collect_branches = || -> Vec<(String, String)> {
        let mut set = BTreeSet::new();

        if let Some(coord) = &coord_opt {
            for b in coord.git().list_local_branches() {
                set.insert(b);
            }

            if let Ok(current) = coord.oplog().load_current_reconciled() {
                for b in current.view.branches.keys() {
                    set.insert(b.clone());
                }

                for a in current.view.agents.values() {
                    if let Some(tb) = &a.target_branch {
                        set.insert(tb.clone());
                    }
                }
            }
        }

        set.into_iter().map(|b| (b, "branch".to_string())).collect()
    };

    let collect_revisions = || -> Vec<(String, String)> {
        let mut items = collect_branches();
        items.push(("HEAD".to_string(), "current branch tip".to_string()));
        items.push(("HEAD~1".to_string(), "1 commit ago".to_string()));
        items.push(("HEAD~2".to_string(), "2 commits ago".to_string()));

        if let Some(coord) = &coord_opt {
            if let Ok(current) = coord.oplog().load_current_reconciled() {
                for a in current.view.agents.values() {
                    for (cp_name, op_id) in &a.checkpoints {
                        items.push((cp_name.clone(), format!("checkpoint ({})", op_id.short())));
                    }
                }
            }

            for (sha, subj) in coord.git().list_recent_commits(25) {
                items.push((sha, format!("commit: {subj}")));
            }
        }

        items
    };

    let collect_agents = || -> Vec<(String, String)> {
        let mut items = Vec::new();

        if let Some(coord) = &coord_opt {
            if let Ok(current) = coord.oplog().load_current_reconciled() {
                for (id, a) in &current.view.agents {
                    let br = a.target_branch.as_deref().unwrap_or("main");
                    items.push((id.clone(), format!("session on {br} ({})", a.intent)));
                }
            }
        }

        items
    };

    let collect_checkpoints = || -> Vec<(String, String)> {
        let mut items = Vec::new();

        if let Some(coord) = &coord_opt {
            if let Ok(current) = coord.oplog().load_current_reconciled() {
                for a in current.view.agents.values() {
                    for (cp_name, op_id) in &a.checkpoints {
                        items.push((cp_name.clone(), format!("checkpoint ({})", op_id.short())));
                    }
                }
            }
        }

        items
    };

    let collect_ops = || -> Vec<(String, String)> {
        let mut items = Vec::new();

        if let Some(coord) = &coord_opt {
            if let Ok(ops) = coord.oplog().list_history(15) {
                for op in ops {
                    items.push((op.id.short().to_string(), op.description));
                }
            }
        }

        items
    };

    let mut candidates: Vec<(String, String)> = Vec::new();

    match prev {
        "-b" | "--branch" | "--into" | "-d" | "--delete" | "-D" | "--force-delete" | "-m"
        | "--move" | "--from" | "--base" => {
            candidates = collect_branches();
        }
        "-s" | "--source" => {
            candidates = collect_revisions();
        }
        "--strategy" => {
            candidates = vec![
                (
                    "ast-union".to_string(),
                    "Combine both sides' AST declarations".to_string(),
                ),
                ("ours".to_string(), "Keep the first side".to_string()),
                ("theirs".to_string(), "Take the second side".to_string()),
                (
                    "working".to_string(),
                    "Use the current file content on disk".to_string(),
                ),
                (
                    "content".to_string(),
                    "Use explicit content via --content or --content-file".to_string(),
                ),
            ];
        }
        "--harness" => {
            for h in [
                "all",
                "antigravity",
                "claude",
                "cursor",
                "windsurf",
                "kiro",
                "opencode",
                "codex",
                "vscode",
            ] {
                candidates.push((h.to_string(), "AI harness integration".to_string()));
            }
        }
        "--agent" | "--session" => {
            candidates = collect_agents();
        }
        "--checkpoint" => {
            candidates = collect_checkpoints();
        }
        "--op" => {
            candidates = collect_ops();
        }
        _ => match subcmd {
            None => {
                candidates = vec![
                    (
                        "init".to_string(),
                        "Initialize Tessera (.git/tessera) and AI harness hooks".to_string(),
                    ),
                    (
                        "doctor".to_string(),
                        "Diagnose repository, CoW, config trust, hooks, and sandboxes".to_string(),
                    ),
                    (
                        "upgrade".to_string(),
                        "Check for or install the latest Tessera CLI release".to_string(),
                    ),
                    (
                        "update".to_string(),
                        "Alias for upgrade: install the latest Tessera CLI release".to_string(),
                    ),
                    (
                        "status".to_string(),
                        "Show session status, branch, and staged/unstaged/peer changes".to_string(),
                    ),
                    (
                        "diff".to_string(),
                        "Show line-by-line unified diffs".to_string(),
                    ),
                    (
                        "switch".to_string(),
                        "Switch or bind current terminal/session to a branch".to_string(),
                    ),
                    (
                        "branch".to_string(),
                        "List, switch, delete (-d/-D), or rename (-m) branches".to_string(),
                    ),
                    (
                        "commit".to_string(),
                        "Commit current session changes onto its active branch".to_string(),
                    ),
                    (
                        "push".to_string(),
                        "Push branch to remote (auto-commits pending changes)".to_string(),
                    ),
                    (
                        "pull".to_string(),
                        "Fetch from remote and 3-way AST-merge into active branch".to_string(),
                    ),
                    (
                        "stash".to_string(),
                        "Stash and restore session-isolated uncommitted changes".to_string(),
                    ),
                    (
                        "log".to_string(),
                        "Show commit history for the current or specified branch".to_string(),
                    ),
                    (
                        "cherry-pick".to_string(),
                        "Apply a commit's delta onto the active branch via 3-way AST merge"
                            .to_string(),
                    ),
                    (
                        "restore".to_string(),
                        "Restore branch or files to a commit, ref, or checkpoint".to_string(),
                    ),
                    (
                        "claim".to_string(),
                        "Declare session intent, target branch, and claimed files".to_string(),
                    ),
                    (
                        "exec".to_string(),
                        "Execute command in isolated Copy-on-Write branch sandbox".to_string(),
                    ),
                    (
                        "merge".to_string(),
                        "Semantically AST-merge sessions or branches".to_string(),
                    ),
                    (
                        "radar".to_string(),
                        "Display live Symbol Collision Radar across active agents".to_string(),
                    ),
                    (
                        "conflicts".to_string(),
                        "List active first-class algebraic conflicts".to_string(),
                    ),
                    (
                        "resolve".to_string(),
                        "Resolve a conflict using ast-union, ours, theirs, working, or content"
                            .to_string(),
                    ),
                    (
                        "sync".to_string(),
                        "Auto-rebase active sessions onto latest trunk commit".to_string(),
                    ),
                    (
                        "checkpoint".to_string(),
                        "Save a named checkpoint in the Operation Log".to_string(),
                    ),
                    (
                        "undo".to_string(),
                        "Roll back repository state to previous operation or checkpoint"
                            .to_string(),
                    ),
                    (
                        "oplog".to_string(),
                        "Inspect lock-free Operation Log history".to_string(),
                    ),
                    (
                        "agent".to_string(),
                        "Manage isolated Copy-on-Write agent sandboxes".to_string(),
                    ),
                    (
                        "config".to_string(),
                        "View or modify repository configuration".to_string(),
                    ),
                    (
                        "env".to_string(),
                        "Emit or install shell prompt and tab-completion integration".to_string(),
                    ),
                    (
                        "watch".to_string(),
                        "Continuously watch and auto-reconcile working tree edits".to_string(),
                    ),
                    (
                        "gc".to_string(),
                        "Compact ephemeral snapshots in Operation Log".to_string(),
                    ),
                    (
                        "mcp".to_string(),
                        "Start Model Context Protocol JSON-RPC server".to_string(),
                    ),
                ];
            }
            Some("init") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-b".to_string(), "Initial trunk branch name".to_string()),
                        (
                            "--branch".to_string(),
                            "Initial trunk branch name".to_string(),
                        ),
                        (
                            "--harness".to_string(),
                            "AI harness integration(s) to install".to_string(),
                        ),
                        (
                            "--local".to_string(),
                            "Add new harness files to .git/info/exclude".to_string(),
                        ),
                    ];
                }
            }
            Some("upgrade" | "update" | "self-update") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        (
                            "--check".to_string(),
                            "Check for updates without replacing the binary".to_string(),
                        ),
                        (
                            "--version".to_string(),
                            "Install a specific release version (e.g. 0.0.3)".to_string(),
                        ),
                        (
                            "--force".to_string(),
                            "Reinstall even if already up to date".to_string(),
                        ),
                        (
                            "--json".to_string(),
                            "Emit machine-readable JSON output".to_string(),
                        ),
                    ];
                }
            }
            Some("status") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-s".to_string(), "Give output in short-format".to_string()),
                        (
                            "--short".to_string(),
                            "Give output in short-format".to_string(),
                        ),
                        (
                            "--porcelain".to_string(),
                            "Give output in machine-readable format (no ANSI)".to_string(),
                        ),
                        (
                            "-b".to_string(),
                            "Show branch and session header in short-format".to_string(),
                        ),
                        (
                            "--branch".to_string(),
                            "Show branch and session header in short-format".to_string(),
                        ),
                    ];
                }
            }
            Some("switch" | "checkout") => {
                candidates = collect_branches();
            }
            Some("branch" | "branches") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-l".to_string(), "List branches".to_string()),
                        ("--list".to_string(), "List branches".to_string()),
                        (
                            "--show-current".to_string(),
                            "Print active branch name".to_string(),
                        ),
                        (
                            "-r".to_string(),
                            "List remote-tracking branches".to_string(),
                        ),
                        (
                            "-a".to_string(),
                            "List local and remote-tracking branches".to_string(),
                        ),
                        (
                            "-v".to_string(),
                            "Show commit SHA and subject in list mode".to_string(),
                        ),
                        (
                            "--verbose".to_string(),
                            "Show commit SHA and subject in list mode".to_string(),
                        ),
                        ("-d".to_string(), "Delete merged branch".to_string()),
                        ("-D".to_string(), "Force-delete branch".to_string()),
                        ("-m".to_string(), "Rename branch".to_string()),
                        ("-e".to_string(), "Switch editor working tree".to_string()),
                    ];
                } else {
                    candidates = collect_branches();
                }
            }
            Some("stash") => {
                if cword == 2 {
                    candidates = vec![
                        (
                            "push".to_string(),
                            "Stash current session's uncommitted changes".to_string(),
                        ),
                        (
                            "pop".to_string(),
                            "Apply stash via 3-way AST merge and remove it".to_string(),
                        ),
                        (
                            "apply".to_string(),
                            "Apply stash via 3-way AST merge and keep it".to_string(),
                        ),
                        ("list".to_string(), "List stashed entries".to_string()),
                        (
                            "drop".to_string(),
                            "Remove a single stash entry".to_string(),
                        ),
                        ("clear".to_string(), "Remove all stash entries".to_string()),
                    ];
                }
            }
            Some("log") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-n".to_string(), "Number of commits to show".to_string()),
                        (
                            "--max-count".to_string(),
                            "Number of commits to show".to_string(),
                        ),
                        ("-b".to_string(), "Branch to inspect".to_string()),
                        (
                            "--oneline".to_string(),
                            "Compact one-line output".to_string(),
                        ),
                        (
                            "-p".to_string(),
                            "Generate patch (unified diff) for each commit".to_string(),
                        ),
                        (
                            "--patch".to_string(),
                            "Generate patch (unified diff) for each commit".to_string(),
                        ),
                        (
                            "--stat".to_string(),
                            "Show diffstat of modified files for each commit".to_string(),
                        ),
                        (
                            "--name-only".to_string(),
                            "Show only names of changed files".to_string(),
                        ),
                        (
                            "--name-status".to_string(),
                            "Show names and status of changed files".to_string(),
                        ),
                        (
                            "--graph".to_string(),
                            "Draw text-based commit graph".to_string(),
                        ),
                        (
                            "--all".to_string(),
                            "Show commits across all branches and refs".to_string(),
                        ),
                        (
                            "--author".to_string(),
                            "Filter commits by author pattern".to_string(),
                        ),
                        (
                            "--grep".to_string(),
                            "Filter commits by message pattern".to_string(),
                        ),
                        (
                            "-S".to_string(),
                            "Filter commits introducing or removing a string".to_string(),
                        ),
                        (
                            "--since".to_string(),
                            "Show commits more recent than date".to_string(),
                        ),
                        (
                            "--until".to_string(),
                            "Show commits older than date".to_string(),
                        ),
                        (
                            "--reverse".to_string(),
                            "Output chosen commits in reverse order".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_branches();
                }
            }
            Some("cherry-pick" | "restore" | "reset" | "discard") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-b".to_string(), "Target branch name".to_string()),
                        ("--branch".to_string(), "Target branch name".to_string()),
                        (
                            "-s".to_string(),
                            "Source commit, ref, or checkpoint".to_string(),
                        ),
                        (
                            "--source".to_string(),
                            "Source commit, ref, or checkpoint".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_revisions();
                }
            }
            Some("push") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-u".to_string(), "Set upstream tracking branch".to_string()),
                        ("-f".to_string(), "Force push".to_string()),
                        (
                            "--force-with-lease".to_string(),
                            "Safe force push with lease".to_string(),
                        ),
                        ("--tags".to_string(), "Push all tags".to_string()),
                        ("-d".to_string(), "Delete remote branch".to_string()),
                        ("-n".to_string(), "Dry run push".to_string()),
                        ("--dry-run".to_string(), "Dry run push".to_string()),
                        (
                            "--no-auto-commit".to_string(),
                            "Skip auto-committing pending session edits".to_string(),
                        ),
                    ];
                } else if cword == 2 {
                    candidates.push(("origin".to_string(), "default Git remote".to_string()));
                    candidates.extend(collect_branches());
                } else {
                    candidates = collect_branches();
                }
            }
            Some("pull") => {
                if cword == 2 && !cur.starts_with('-') {
                    candidates.push(("origin".to_string(), "default Git remote".to_string()));
                    candidates.extend(collect_branches());
                } else {
                    candidates = collect_branches();
                }
            }
            Some("commit" | "ci") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-m".to_string(), "Commit message".to_string()),
                        ("--message".to_string(), "Commit message".to_string()),
                        ("-b".to_string(), "Target branch name".to_string()),
                        ("--branch".to_string(), "Target branch name".to_string()),
                        (
                            "--amend".to_string(),
                            "Amend the tip commit of the target branch".to_string(),
                        ),
                        (
                            "--no-edit".to_string(),
                            "Reuse previous commit message when amending".to_string(),
                        ),
                        (
                            "--verify".to_string(),
                            "Run configured or explicit verification command".to_string(),
                        ),
                        (
                            "--verify-cmd".to_string(),
                            "Explicit verification shell command".to_string(),
                        ),
                        (
                            "--co-author".to_string(),
                            "Append Co-authored-by trailer (optional Name <email>)".to_string(),
                        ),
                        (
                            "--no-co-author".to_string(),
                            "Omit Co-authored-by trailer for this commit".to_string(),
                        ),
                        (
                            "--author".to_string(),
                            "Override commit author (Name <email>)".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_branches();
                }
            }
            Some("merge") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        (
                            "--into".to_string(),
                            "Target branch to merge into".to_string(),
                        ),
                        ("-m".to_string(), "Commit message".to_string()),
                        (
                            "--verify".to_string(),
                            "Run configured or explicit verification command".to_string(),
                        ),
                        (
                            "--verify-cmd".to_string(),
                            "Explicit verification shell command".to_string(),
                        ),
                        (
                            "--dry-run".to_string(),
                            "Simulate merge without mutating refs or OpLog".to_string(),
                        ),
                        (
                            "--keep-open".to_string(),
                            "Keep merged sessions open".to_string(),
                        ),
                        (
                            "--co-author".to_string(),
                            "Append Co-authored-by trailer (optional Name <email>)".to_string(),
                        ),
                        (
                            "--no-co-author".to_string(),
                            "Omit Co-authored-by trailer for this merge commit".to_string(),
                        ),
                        (
                            "--author".to_string(),
                            "Override merge commit author (Name <email>)".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_branches();
                    candidates.extend(collect_agents());
                }
            }
            Some("diff") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        (
                            "--staged".to_string(),
                            "Show only staged/session changes".to_string(),
                        ),
                        (
                            "--cached".to_string(),
                            "Show only staged/session changes".to_string(),
                        ),
                        (
                            "--all".to_string(),
                            "Include concurrent peer session changes on the branch".to_string(),
                        ),
                        (
                            "--stat".to_string(),
                            "Show diffstat summary of changed files".to_string(),
                        ),
                        (
                            "--shortstat".to_string(),
                            "Show aggregate line count summary".to_string(),
                        ),
                        (
                            "--numstat".to_string(),
                            "Show machine-friendly per-file line counts".to_string(),
                        ),
                        (
                            "--name-only".to_string(),
                            "Show only names of changed files".to_string(),
                        ),
                        (
                            "--name-status".to_string(),
                            "Show names and status of changed files".to_string(),
                        ),
                        (
                            "-U".to_string(),
                            "Number of context lines in unified diff".to_string(),
                        ),
                        (
                            "-w".to_string(),
                            "Ignore all whitespace when comparing lines".to_string(),
                        ),
                        (
                            "--ignore-space-change".to_string(),
                            "Ignore changes in amount of whitespace".to_string(),
                        ),
                        ("-b".to_string(), "Compare specific branch".to_string()),
                        (
                            "--branch".to_string(),
                            "Compare specific branch".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_branches();
                }
            }
            Some("resolve") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        (
                            "--strategy".to_string(),
                            "Resolution strategy (ast-union, ours, theirs, working, content)"
                                .to_string(),
                        ),
                        (
                            "--content".to_string(),
                            "Explicit resolved text content".to_string(),
                        ),
                        (
                            "--content-file".to_string(),
                            "Read explicit resolved content from file".to_string(),
                        ),
                    ];
                } else if let Some(coord) = &coord_opt {
                    if let Ok(current) = coord.oplog().load_current_reconciled() {
                        for path in current.view.conflicts.keys() {
                            candidates.push((path.clone(), "conflicted file".to_string()));
                        }
                    }
                }
            }
            Some("undo") => {
                candidates = vec![
                    (
                        "--checkpoint".to_string(),
                        "Restore to named checkpoint".to_string(),
                    ),
                    (
                        "--op".to_string(),
                        "Restore to operation ID prefix".to_string(),
                    ),
                ];
            }
            Some("agent" | "session" | "sessions") => {
                if cword == 2 {
                    candidates = vec![
                        ("list".to_string(), "List active agent sessions".to_string()),
                        ("spawn".to_string(), "Spawn COW agent workspace".to_string()),
                        (
                            "exec".to_string(),
                            "Execute command in agent workspace".to_string(),
                        ),
                        (
                            "close".to_string(),
                            "Close active agent session".to_string(),
                        ),
                        (
                            "clear".to_string(),
                            "Clear idle or finished agent sessions".to_string(),
                        ),
                        (
                            "focus".to_string(),
                            "Focus terminal or IDE harness for an active session".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_agents();
                }
            }
            Some("config") => {
                if cword == 2 {
                    candidates = vec![
                        (
                            "show".to_string(),
                            "Display effective configuration".to_string(),
                        ),
                        (
                            "add-cache-dir".to_string(),
                            "Add custom dependency/build cache directory".to_string(),
                        ),
                        (
                            "add-bin-dir".to_string(),
                            "Add relative binary directory to PATH".to_string(),
                        ),
                        (
                            "add-package-glob".to_string(),
                            "Add monorepo package glob".to_string(),
                        ),
                        (
                            "set-env".to_string(),
                            "Set custom environment variable template".to_string(),
                        ),
                        ("set".to_string(), "Set configuration property".to_string()),
                        (
                            "trust".to_string(),
                            "Trust repository tessera.toml execution settings".to_string(),
                        ),
                        (
                            "untrust".to_string(),
                            "Revoke trust for repository tessera.toml execution settings"
                                .to_string(),
                        ),
                    ];
                } else if words.get(2).map(String::as_str) == Some("set") && cword == 3 {
                    for k in [
                        "trunk_branch",
                        "base_port",
                        "port_stride",
                        "port_isolation",
                        "auto_isolate_dirty_claims",
                        "verify_command",
                        "watch_interval_ms",
                        "watch_timeout_secs",
                        "co_author",
                        "co_author_trailer",
                        "pager",
                    ] {
                        candidates.push((k.to_string(), "config key".to_string()));
                    }
                }
            }
            _ => {}
        },
    }

    for (val, desc) in candidates {
        if !cur.is_empty() && !val.starts_with(cur) {
            continue;
        }

        match args.shell.as_str() {
            "zsh" => {
                let escaped_val = val.replace(':', "\\:");
                let clean_desc = desc.replace('\n', " ");
                println!("{escaped_val}:{clean_desc}");
            }
            "fish" => {
                let clean_desc = desc.replace(['\n', '\t'], " ");
                println!("{val}\t{clean_desc}");
            }
            _ => {
                println!("{val}");
            }
        }
    }
}
