//! Read-only inspection commands: `tes status`, `tes diff`, `tes log`, `tes radar`, and `tes oplog`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::process::Command;

use agent::{Coordinator, detect_session_id};
use core::{GitBackend, ObjectId};
use serde_json::{Value, json};

use crate::output::{
    CliContext, c_bold, c_cyan, c_dim, c_green, c_orange, c_red, c_yellow, emit_paged_output,
    format_session_link, format_status_badge, is_existing_user_path, resolve_user_pathspecs,
    use_color,
};

/// Options for `tes diff`.
#[derive(Debug, Clone, Default)]
pub struct DiffArgs {
    /// Positional targets (revisions, ranges, or file paths).
    pub targets: Vec<String>,
    /// Show only staged/session changes (`--staged` / `--cached`).
    pub staged: bool,
    /// Include combined changes across all active sessions on the branch.
    pub all: bool,
    /// Show diffstat summary.
    pub stat: bool,
    /// Show shortstat aggregate line counts.
    pub shortstat: bool,
    /// Show numstat per-file counts.
    pub numstat: bool,
    /// Show only file names.
    pub name_only: bool,
    /// Show file names with status letters.
    pub name_status: bool,
    /// Unified diff context lines (`-U`).
    pub unified: Option<usize>,
    /// Ignore all whitespace (`-w`).
    pub ignore_all_space: bool,
    /// Ignore whitespace amount changes.
    pub ignore_space_change: bool,
    /// Explicit branch to compare against trunk.
    pub branch: Option<String>,
    /// Optional explicit session ID.
    pub agent: Option<String>,
    /// Explicit pathspecs after `--`.
    pub pathspecs: Vec<String>,
}

/// Options for `tes log`.
#[derive(Debug, Clone)]
pub struct LogArgs {
    /// Positional targets (branch, revision range, or file paths).
    pub targets: Vec<String>,
    /// Explicit `-b <branch>` override.
    pub branch_flag: Option<String>,
    /// Maximum commits to show.
    pub limit: usize,
    /// Single-line compact format.
    pub oneline: bool,
    /// Include patch output (`-p`).
    pub patch: bool,
    /// Include diffstat (`--stat`).
    pub stat: bool,
    /// Include changed file names (`--name-only`).
    pub name_only: bool,
    /// Include changed file names and status (`--name-status`).
    pub name_status: bool,
    /// Draw ASCII commit graph (`--graph`).
    pub graph: bool,
    /// Show commits across all refs (`--all`).
    pub all: bool,
    /// Filter by author (`--author`).
    pub author: Option<String>,
    /// Filter by commit message (`--grep`).
    pub grep: Option<String>,
    /// Filter by pickaxe string (`-S`).
    pub pickaxe: Option<String>,
    /// Filter commits after date (`--since`).
    pub since: Option<String>,
    /// Filter commits before date (`--until`).
    pub until: Option<String>,
    /// Reverse chronological order (`--reverse`).
    pub reverse: bool,
    /// Optional explicit session ID.
    pub agent: Option<String>,
    /// Explicit pathspecs after `--`.
    pub paths: Vec<String>,
}

/// Executes `tes status`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or snapshotted.
pub fn run_status(
    ctx: &CliContext,
    short: bool,
    porcelain: bool,
    branch: bool,
    agent: Option<String>,
) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(agent.as_deref());
    coord.reconcile_in_flight_sessions();

    if !session_id.starts_with("term-") {
        let _ = coord.watch_tick(&session_id);
    }

    let meta = coord.snapshot_agent(&session_id, true)?;
    let radar = coord.radar()?;
    let cfg = coord.oplog().load_config()?;
    let current = coord.oplog().load_current_reconciled()?;
    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| cfg.trunk_branch.clone());
    let active_branch = meta
        .target_branch
        .as_deref()
        .unwrap_or(&default_branch)
        .to_string();

    let base_commit = coord.git().read_commit(&meta.base_commit)?;
    let base_map = coord.git().read_tree_to_map(&base_commit.tree_id)?;
    let head_map = coord.git().read_tree_to_map(&meta.head_tree)?;

    let mut peer_changes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut peer_statuses: BTreeMap<String, &'static str> = BTreeMap::new();

    for (peer_id, peer_meta) in &current.view.agents {
        if peer_id == &meta.agent_id {
            continue;
        }

        if peer_meta.effective_branch(&default_branch) != active_branch {
            continue;
        }

        for (p, peer_blob) in &peer_meta.own_blobs {
            if workspace::is_ignored_or_secret_path(
                coord.git().repo_root(),
                coord.git().git_dir(),
                p,
                base_map.contains_key(p),
            ) {
                continue;
            }

            if base_map.get(p).map(|e| &e.blob_id) != Some(peer_blob) {
                peer_changes
                    .entry(p.clone())
                    .or_default()
                    .push(peer_id.clone());
                let st = if base_map.contains_key(p) { "M" } else { "A" };
                peer_statuses.insert(p.clone(), st);
            }
        }

        for p in &peer_meta.deleted_paths {
            if workspace::is_ignored_or_secret_path(
                coord.git().repo_root(),
                coord.git().git_dir(),
                p,
                base_map.contains_key(p),
            ) {
                continue;
            }

            peer_changes
                .entry(p.clone())
                .or_default()
                .push(peer_id.clone());
            peer_statuses.entry(p.clone()).or_insert("D");
        }
    }

    let mut staged_files: BTreeMap<String, &'static str> = BTreeMap::new();

    for (path, blob_id) in &meta.own_blobs {
        if workspace::is_ignored_or_secret_path(
            coord.git().repo_root(),
            coord.git().git_dir(),
            path,
            base_map.contains_key(path),
        ) {
            continue;
        }

        let status = match base_map.get(path) {
            None => "A",
            Some(entry) if entry.blob_id != *blob_id => "M",
            _ => continue,
        };
        staged_files.insert(path.clone(), status);
    }

    for path in &meta.deleted_paths {
        if base_map.contains_key(path) {
            staged_files.insert(path.clone(), "D");
        }
    }

    let mut unstaged_files: BTreeMap<String, &'static str> = BTreeMap::new();

    for (path, entry) in &head_map {
        if staged_files.contains_key(path) {
            continue;
        }

        match base_map.get(path) {
            None => {
                unstaged_files.insert(path.clone(), "A");
            }
            Some(base_entry)
                if base_entry.blob_id != entry.blob_id
                    || base_entry.executable != entry.executable
                    || base_entry.kind != entry.kind =>
            {
                unstaged_files.insert(path.clone(), "M");
            }
            _ => {}
        }
    }

    for path in base_map.keys() {
        if !head_map.contains_key(path) && !staged_files.contains_key(path) {
            unstaged_files.insert(path.clone(), "D");
        }
    }

    let mut branch_files: BTreeMap<String, &'static str> = BTreeMap::new();

    if active_branch != cfg.trunk_branch {
        if let (Ok(Some(br_cid)), Ok(Some(tr_cid))) = (
            coord
                .git()
                .resolve_ref(&format!("refs/heads/{active_branch}")),
            coord
                .git()
                .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch)),
        ) {
            let mb_cid = coord
                .git()
                .find_merge_base(&tr_cid, &br_cid)
                .unwrap_or(tr_cid);

            if br_cid != mb_cid {
                if let (Ok(br_c), Ok(mb_c)) = (
                    coord.git().read_commit(&br_cid),
                    coord.git().read_commit(&mb_cid),
                ) {
                    if let (Ok(br_m), Ok(mb_m)) = (
                        coord.git().read_tree_to_map(&br_c.tree_id),
                        coord.git().read_tree_to_map(&mb_c.tree_id),
                    ) {
                        for (p, e) in &br_m {
                            match mb_m.get(p) {
                                None => {
                                    branch_files.insert(p.clone(), "A");
                                }
                                Some(be)
                                    if be.blob_id != e.blob_id
                                        || be.executable != e.executable
                                        || be.kind != e.kind =>
                                {
                                    branch_files.insert(p.clone(), "M");
                                }
                                _ => {}
                            }
                        }

                        for p in mb_m.keys() {
                            if !br_m.contains_key(p) {
                                branch_files.insert(p.clone(), "D");
                            }
                        }
                    }
                }
            }
        }
    }

    let unattributed_files: BTreeSet<String> = if active_branch == default_branch {
        coord
            .unattributed_worktree_changes()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !staged_files.contains_key(p) && !unstaged_files.contains_key(p))
            .collect()
    } else {
        BTreeSet::new()
    };

    let session_conflicts: Vec<&core::ConflictRecord> = radar
        .active_conflicts
        .iter()
        .filter(|c| {
            c.agent_intents.contains_key(&meta.agent_id)
                || meta.owns_path(&c.path)
                || meta.read_blobs.contains_key(&c.path)
        })
        .collect();
    let peer_conflict_count = radar.active_conflicts.len() - session_conflicts.len();

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session": meta,
                "active_branch": active_branch,
                "staged_changes": staged_files,
                "unstaged_changes": unstaged_files,
                "peer_changes": peer_changes,
                "unattributed_changes": unattributed_files,
                "branch_changes": branch_files,
                "active_conflicts": radar.active_conflicts,
                "session_conflicts": session_conflicts,
                "collisions": radar.collisions,
            }))?
        );

        return Ok(());
    }

    if short || porcelain {
        let mut out_buf = String::new();
        let allow_ansi = !porcelain && use_color();

        if branch {
            if allow_ansi {
                let repo_root = coord.git().repo_root();
                let linked_id =
                    format_session_link(&meta.agent_id, repo_root, c_dim(&meta.agent_id));
                let _ = writeln!(
                    out_buf,
                    "## {} {}{}{}",
                    c_green(&active_branch),
                    c_dim("[session: "),
                    linked_id,
                    c_dim("]")
                );
            } else {
                let _ = writeln!(out_buf, "## {active_branch} [session: {}]", meta.agent_id);
            }
        }

        let mut all_paths: BTreeSet<&String> = BTreeSet::new();

        for p in staged_files
            .keys()
            .chain(unstaged_files.keys())
            .chain(unattributed_files.iter())
        {
            all_paths.insert(p);
        }

        for p in all_paths {
            let x_raw = staged_files.get(p).copied();
            let y_raw = unstaged_files.get(p).copied();
            let is_untracked = x_raw.is_none()
                && !base_map.contains_key(p)
                && (y_raw == Some("A") || unattributed_files.contains(p));

            if is_untracked {
                if allow_ansi {
                    let _ = writeln!(out_buf, "{} {p}", c_red("??"));
                } else {
                    let _ = writeln!(out_buf, "?? {p}");
                }

                continue;
            }

            let x = x_raw.unwrap_or(" ");
            let y = match y_raw {
                Some(st) => st,
                None if unattributed_files.contains(p) => {
                    if coord.git().repo_root().join(p).exists() {
                        "M"
                    } else {
                        "D"
                    }
                }
                None => " ",
            };

            if allow_ansi {
                let x_col = if x == " " {
                    " ".to_string()
                } else {
                    c_green(x)
                };
                let y_col = if y == " " {
                    " ".to_string()
                } else {
                    c_yellow(y)
                };
                let _ = writeln!(out_buf, "{x_col}{y_col} {p}");
            } else {
                let _ = writeln!(out_buf, "{x}{y} {p}");
            }
        }

        emit_paged_output(
            &out_buf,
            false,
            ctx.no_pager || porcelain,
            ctx.paginate && !porcelain,
            ctx.json,
            Some(&coord),
        );

        return Ok(());
    }

    let mut out_buf = String::new();
    let repo_root = coord.git().repo_root();

    let _ = writeln!(
        out_buf,
        "{} {} (slot {}, change: {}, commit: {})",
        c_bold("Session:"),
        format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
        c_yellow(format!("#{}", meta.slot)),
        c_yellow(meta.change_id.short()),
        c_yellow(meta.head_commit.short())
    );

    let _ = writeln!(
        out_buf,
        "{}  {}",
        c_bold("Branch:"),
        c_green(&active_branch)
    );
    let _ = writeln!(out_buf, "{}  {}", c_bold("Intent:"), meta.intent);

    if !staged_files.is_empty() {
        let _ = writeln!(out_buf, "\n{}", c_green("Staged / Session Changes:"));

        for (p, st) in &staged_files {
            let _ = writeln!(
                out_buf,
                "  {}  {}",
                format_status_badge(st, true),
                c_green(p)
            );
        }
    }

    if !unstaged_files.is_empty() {
        let _ = writeln!(out_buf, "\n{}", c_yellow("Unstaged Changes:"));

        for (p, st) in &unstaged_files {
            let _ = writeln!(
                out_buf,
                "  {}  {}",
                format_status_badge(st, false),
                c_yellow(p)
            );
        }
    }

    if !peer_changes.is_empty() {
        let _ = writeln!(
            out_buf,
            "\n{} '{}':",
            c_cyan("Peer Session Changes on"),
            c_green(&active_branch)
        );

        for (p, peers) in &peer_changes {
            let badge = peer_statuses.get(p).copied().unwrap_or("M");
            let linked_peers: Vec<String> = peers
                .iter()
                .map(|peer_id| format_session_link(peer_id, repo_root, c_dim(peer_id)))
                .collect();

            let _ = writeln!(
                out_buf,
                "  {}  {} {}{}{}",
                format_status_badge(badge, false),
                c_cyan(p),
                c_dim("("),
                linked_peers.join(&c_dim(", ")),
                c_dim(")")
            );
        }
    }

    if !unattributed_files.is_empty() {
        let _ = writeln!(
            out_buf,
            "\n{}",
            c_yellow("Unattributed Working-Tree Changes (not committed by this session):")
        );

        for p in &unattributed_files {
            let badge = if !base_map.contains_key(p) {
                "A"
            } else if coord.git().repo_root().join(p).exists() {
                "M"
            } else {
                "D"
            };
            let _ = writeln!(
                out_buf,
                "  {}  {}",
                format_status_badge(badge, false),
                c_yellow(p)
            );
        }
    }

    if !branch_files.is_empty() {
        let _ = writeln!(
            out_buf,
            "\n{} '{}' (ahead of '{}'):",
            c_cyan("Committed on"),
            c_green(&active_branch),
            c_dim(&cfg.trunk_branch)
        );

        for (p, st) in &branch_files {
            let _ = writeln!(
                out_buf,
                "  {}  {}",
                format_status_badge(st, true),
                c_cyan(p)
            );
        }
    }

    if staged_files.is_empty()
        && unstaged_files.is_empty()
        && branch_files.is_empty()
        && unattributed_files.is_empty()
        && peer_changes.is_empty()
    {
        let _ = writeln!(
            out_buf,
            "{} {}",
            c_bold("Changes:"),
            c_green("✓ working tree clean")
        );
    }

    if !meta.claimed_symbols.is_empty() {
        let _ = writeln!(
            out_buf,
            "{} {}",
            c_bold("Symbols:"),
            c_cyan(
                meta.claimed_symbols
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        );
    }

    let conflict_str = if session_conflicts.is_empty() {
        c_green("0")
    } else {
        c_red(session_conflicts.len())
    };

    if peer_conflict_count > 0 {
        let _ = writeln!(
            out_buf,
            "{} {} {}",
            c_bold("Conflicts:"),
            conflict_str,
            c_dim(format!(
                "({peer_conflict_count} in peer sessions, not blocking this session; see `tes conflicts`)"
            ))
        );
    } else {
        let _ = writeln!(out_buf, "{} {}", c_bold("Conflicts:"), conflict_str);
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

/// Computes diff trees, labels, and resolved pathspecs shared by `tes diff` and the MCP `diff` tool.
pub struct ComputedDiff {
    /// Human-readable base tree/revision label.
    pub base_label: String,
    /// Human-readable target tree/revision label.
    pub target_label: String,
    /// Left Git tree [`ObjectId`].
    pub left_tree: ObjectId,
    /// Right Git tree [`ObjectId`].
    pub right_tree: ObjectId,
    /// Normalized repo-relative pathspecs.
    pub effective_paths: Vec<String>,
}

/// Computes the left/right Git trees for a diff invocation without mutating untouched ODB objects.
///
/// # Errors
/// Returns an error if revisions or trees cannot be resolved.
pub fn compute_diff_trees(
    coord: &Coordinator,
    invocation_cwd: &std::path::Path,
    session_id: &str,
    args: &DiffArgs,
) -> anyhow::Result<ComputedDiff> {
    let cfg = coord.oplog().load_config()?;

    if !args.staged && !session_id.starts_with("term-") {
        let _ = coord.watch_tick(session_id);
    }

    let meta = coord.snapshot_agent(session_id, true)?;
    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| cfg.trunk_branch.clone());
    let active_branch = args
        .branch
        .as_deref()
        .or(meta.target_branch.as_deref())
        .unwrap_or(&default_branch)
        .to_string();
    let repo_root = coord.git().repo_root();
    let base_tree = coord.git().read_commit(&meta.base_commit)?.tree_id;

    let session_staged_tree = |coord: &Coordinator| -> anyhow::Result<ObjectId> {
        if !meta.has_own_changes() {
            return Ok(base_tree);
        }

        let mut staged_map = coord.git().read_tree_to_map(&base_tree)?;
        let head_map = coord
            .git()
            .read_tree_to_map(&meta.head_tree)
            .unwrap_or_default();
        meta.apply_own_changes(&mut staged_map, &head_map);
        Ok(coord.git().write_tree_from_map(&staged_map)?)
    };

    enum DiffTargetSpec {
        ThreeDot(String, String),
        TwoDot(String, String),
        SingleRev(String),
        Default,
    }

    let (spec, mut raw_paths) = match args.targets.first().map(String::as_str) {
        Some(p) if p.contains("...") => {
            let (left_rev, right_rev) = p.split_once("...").unwrap_or(("", ""));
            let l = if left_rev.trim().is_empty() {
                "HEAD"
            } else {
                left_rev.trim()
            };
            let r = if right_rev.trim().is_empty() {
                "HEAD"
            } else {
                right_rev.trim()
            };
            (
                DiffTargetSpec::ThreeDot(l.to_string(), r.to_string()),
                args.targets[1..].to_vec(),
            )
        }
        Some(p) if p.contains("..") => {
            let (left_rev, right_rev) = p.split_once("..").unwrap_or(("", ""));
            let l = if left_rev.trim().is_empty() {
                "HEAD"
            } else {
                left_rev.trim()
            };
            let r = if right_rev.trim().is_empty() {
                "HEAD"
            } else {
                right_rev.trim()
            };
            (
                DiffTargetSpec::TwoDot(l.to_string(), r.to_string()),
                args.targets[1..].to_vec(),
            )
        }
        Some(first)
            if args.targets.len() >= 2
                && !is_existing_user_path(invocation_cwd, repo_root, first)
                && coord.git().resolve_revision(first).is_ok()
                && !is_existing_user_path(invocation_cwd, repo_root, &args.targets[1])
                && coord.git().resolve_revision(&args.targets[1]).is_ok() =>
        {
            (
                DiffTargetSpec::TwoDot(first.to_string(), args.targets[1].clone()),
                args.targets[2..].to_vec(),
            )
        }
        Some(first)
            if !is_existing_user_path(invocation_cwd, repo_root, first)
                && coord.git().resolve_revision(first).is_ok() =>
        {
            (
                DiffTargetSpec::SingleRev(first.to_string()),
                args.targets[1..].to_vec(),
            )
        }
        _ => (DiffTargetSpec::Default, args.targets.clone()),
    };

    raw_paths.extend(args.pathspecs.iter().cloned());
    let effective_paths = resolve_user_pathspecs(invocation_cwd, repo_root, &raw_paths);

    let (base_label, target_label, left_tree, right_tree) = match spec {
        DiffTargetSpec::ThreeDot(l_rev, r_rev) => {
            let l_cid = coord.git().resolve_revision(&l_rev)?;
            let r_cid = coord.git().resolve_revision(&r_rev)?;
            let mb_cid = coord.git().find_merge_base(&l_cid, &r_cid).unwrap_or(l_cid);
            (
                format!("merge-base({l_rev},{r_rev}):{}", mb_cid.short()),
                r_rev,
                coord.git().read_commit(&mb_cid)?.tree_id,
                coord.git().read_commit(&r_cid)?.tree_id,
            )
        }
        DiffTargetSpec::TwoDot(l_rev, r_rev) => {
            let l_cid = coord.git().resolve_revision(&l_rev)?;
            let r_cid = coord.git().resolve_revision(&r_rev)?;
            (
                l_rev,
                r_rev,
                coord.git().read_commit(&l_cid)?.tree_id,
                coord.git().read_commit(&r_cid)?.tree_id,
            )
        }
        DiffTargetSpec::SingleRev(rev) => {
            let rev_cid = coord.git().resolve_revision(&rev)?;
            let left = coord.git().read_commit(&rev_cid)?.tree_id;
            let right = if args.staged {
                session_staged_tree(coord)?
            } else {
                meta.head_tree
            };
            let right_label = if args.staged {
                format!("staged:{session_id}")
            } else {
                format!("working:{active_branch}")
            };
            (rev, right_label, left, right)
        }
        DiffTargetSpec::Default => {
            if args.all {
                let mut combined_map = coord.git().read_tree_to_map(&base_tree)?;
                let current = coord.oplog().load_current_reconciled()?;
                let mut touched_paths: BTreeSet<String> = BTreeSet::new();

                for a in current.view.agents.values() {
                    if a.effective_branch(&default_branch) == active_branch {
                        let a_head = coord
                            .git()
                            .read_tree_to_map(&a.head_tree)
                            .unwrap_or_default();

                        for p in a.own_blobs.keys() {
                            touched_paths.insert(p.clone());
                        }

                        for p in &a.deleted_paths {
                            touched_paths.insert(p.clone());
                        }

                        a.apply_own_changes(&mut combined_map, &a_head);
                    }
                }

                let ws_dir = if active_branch == default_branch {
                    repo_root.to_path_buf()
                } else {
                    let sb = coord.branch_sandbox_dir(&active_branch);

                    if sb.is_dir() {
                        sb
                    } else {
                        repo_root.to_path_buf()
                    }
                };

                // Only inspect paths actually touched by active sessions, hashing in memory before
                // writing to the ODB so `tes diff --all` never writes blobs for untouched files.
                for p in &touched_paths {
                    if let Some(entry) = combined_map.get_mut(p) {
                        let disk_file = ws_dir.join(p);

                        if disk_file.is_file() {
                            if let Ok(bytes) = std::fs::read(&disk_file) {
                                let in_mem_id = GitBackend::hash_blob_in_memory(&bytes);

                                if in_mem_id != entry.blob_id {
                                    if let Ok(disk_bid) = coord.git().write_blob(&bytes) {
                                        entry.blob_id = disk_bid;
                                    }
                                }
                            }
                        }
                    }
                }

                let combined_tree = if touched_paths.is_empty() {
                    base_tree
                } else {
                    coord.git().write_tree_from_map(&combined_map)?
                };

                (
                    meta.base_commit.short().to_string(),
                    format!("all:{active_branch}"),
                    base_tree,
                    combined_tree,
                )
            } else if args.staged {
                let staged_tree = session_staged_tree(coord)?;
                (
                    meta.base_commit.short().to_string(),
                    format!("staged:{session_id}"),
                    base_tree,
                    staged_tree,
                )
            } else if args.branch.is_some()
                || (meta.head_tree == base_tree && active_branch != cfg.trunk_branch)
            {
                let tr_cid = coord
                    .git()
                    .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))?
                    .unwrap_or(meta.base_commit);
                let br_cid = coord
                    .git()
                    .resolve_ref(&format!("refs/heads/{active_branch}"))?
                    .unwrap_or(meta.head_commit);
                let mb_cid = coord
                    .git()
                    .find_merge_base(&tr_cid, &br_cid)
                    .unwrap_or(tr_cid);
                (
                    format!(
                        "merge-base({},{}):{}",
                        cfg.trunk_branch,
                        active_branch,
                        mb_cid.short()
                    ),
                    active_branch.clone(),
                    coord.git().read_commit(&mb_cid)?.tree_id,
                    coord.git().read_commit(&br_cid)?.tree_id,
                )
            } else {
                (
                    meta.base_commit.short().to_string(),
                    format!("working:{active_branch}"),
                    base_tree,
                    meta.head_tree,
                )
            }
        }
    };

    Ok(ComputedDiff {
        base_label,
        target_label,
        left_tree,
        right_tree,
        effective_paths,
    })
}

/// Runs `git diff` against a [`ComputedDiff`] and returns the stdout text.
///
/// # Errors
/// Returns an error if `git diff` fails to execute or exits with a non-zero status.
pub fn render_git_diff(
    coord: &Coordinator,
    computed: &ComputedDiff,
    args: &DiffArgs,
    color: bool,
) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(coord.git().repo_root())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("--git-dir")
        .arg(coord.git().git_dir())
        .arg("diff")
        .arg(if color {
            "--color=always"
        } else {
            "--color=never"
        });

    if args.stat {
        cmd.arg("--stat");
    }

    if args.shortstat {
        cmd.arg("--shortstat");
    }

    if args.numstat {
        cmd.arg("--numstat");
    }

    if args.name_only {
        cmd.arg("--name-only");
    }

    if args.name_status {
        cmd.arg("--name-status");
    }

    if let Some(lines) = args.unified {
        cmd.arg(format!("-U{lines}"));
    }

    if args.ignore_all_space {
        cmd.arg("-w");
    }

    if args.ignore_space_change {
        cmd.arg("--ignore-space-change");
    }

    cmd.arg(computed.left_tree.as_str())
        .arg(computed.right_tree.as_str());

    if !computed.effective_paths.is_empty() {
        cmd.arg("--").args(&computed.effective_paths);
    }

    let out = cmd.output()?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git diff failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Collects structured `{ "status", "path" }` file entries for JSON diff output.
pub fn collect_diff_files_json(coord: &Coordinator, computed: &ComputedDiff) -> Vec<Value> {
    let mut cmd = Command::new("git");
    cmd.current_dir(coord.git().repo_root())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("--git-dir")
        .arg(coord.git().git_dir())
        .args(["diff", "--color=never", "--name-status"])
        .arg(computed.left_tree.as_str())
        .arg(computed.right_tree.as_str());

    if !computed.effective_paths.is_empty() {
        cmd.arg("--").args(&computed.effective_paths);
    }

    let Ok(out) = cmd.output() else {
        return Vec::new();
    };

    if !out.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let status = parts.next()?.trim();
            let path = parts.next()?.trim();

            if status.is_empty() || path.is_empty() {
                None
            } else {
                Some(json!({
                    "status": status,
                    "path": path,
                }))
            }
        })
        .collect()
}

/// Executes `tes diff`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or `git diff` fails.
pub fn run_diff(ctx: &CliContext, args: DiffArgs) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let session_id = detect_session_id(args.agent.as_deref());
    let computed = compute_diff_trees(&coord, &ctx.cwd, &session_id, &args)?;
    let diff_text = render_git_diff(&coord, &computed, &args, use_color() && !ctx.json)?;

    if ctx.json {
        let files = collect_diff_files_json(&coord, &computed);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "base": computed.base_label,
                "target": computed.target_label,
                "files": files,
                "diff": diff_text,
            }))?
        );
    } else {
        emit_paged_output(
            &diff_text,
            true,
            ctx.no_pager,
            ctx.paginate,
            ctx.json,
            Some(&coord),
        );
    }

    Ok(())
}

/// Executes `tes log`.
///
/// # Errors
/// Returns an error if the repository cannot be opened, an invalid revision is specified, or `git log` fails.
pub fn run_log(ctx: &CliContext, args: LogArgs) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;

    if args.limit == 0 {
        if ctx.json {
            println!("[]");
        }

        return Ok(());
    }

    let repo_root = coord.git().repo_root();
    let head_tree_paths: BTreeSet<String> = coord
        .git()
        .resolve_head_commit()
        .ok()
        .flatten()
        .and_then(|cid| coord.git().read_commit(&cid).ok())
        .and_then(|c| coord.git().read_tree_to_map(&c.tree_id).ok())
        .map(|m| m.into_keys().collect())
        .unwrap_or_default();

    let mut rev_arg: Option<String> = args.branch_flag;
    let mut raw_path_args: Vec<String> = Vec::new();

    for t in args.targets {
        let exists_on_disk = is_existing_user_path(&ctx.cwd, repo_root, &t);
        let is_in_head = head_tree_paths.contains(&t);

        if rev_arg.is_none()
            && !exists_on_disk
            && (t.contains("..") || coord.git().resolve_revision(&t).is_ok())
        {
            rev_arg = Some(t);
        } else if exists_on_disk
            || is_in_head
            || rev_arg.is_some()
            || t.contains('/')
            || t.contains('.')
            || t.contains('*')
        {
            raw_path_args.push(t);
        } else {
            anyhow::bail!("unknown revision or path not in the working tree: '{t}'");
        }
    }

    raw_path_args.extend(args.paths);
    let path_args = resolve_user_pathspecs(&ctx.cwd, repo_root, &raw_path_args);

    let eff_branch = rev_arg.or_else(|| {
        if args.all {
            return None;
        }

        let sid = detect_session_id(args.agent.as_deref());
        coord.oplog().load_current_reconciled().ok().and_then(|op| {
            op.view
                .agents
                .get(&sid)
                .and_then(|a| a.target_branch.clone())
        })
    });

    let use_git_log = args.patch
        || args.stat
        || args.name_only
        || args.name_status
        || args.graph
        || args.all
        || args.author.is_some()
        || args.grep.is_some()
        || args.pickaxe.is_some()
        || args.since.is_some()
        || args.until.is_some()
        || args.reverse
        || !path_args.is_empty()
        || eff_branch.as_deref().is_some_and(|r| r.contains(".."));

    if !use_git_log {
        let entries = coord.commit_log(eff_branch.as_deref(), args.limit)?;

        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&entries)?);
        } else {
            let mut out = String::new();

            for entry in &entries {
                let refs_badge = if entry.refs.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", c_green(entry.refs.join(", ")))
                };

                if args.oneline {
                    let _ = writeln!(
                        out,
                        "{}{} {}",
                        c_yellow(&entry.short_sha),
                        refs_badge,
                        entry.subject
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "{} {}{}",
                        c_yellow("commit"),
                        c_yellow(&entry.sha),
                        refs_badge
                    );
                    let _ = writeln!(out, "{} {}", c_bold("Author:"), c_cyan(&entry.author));
                    let _ = writeln!(out, "    {}\n", entry.subject);
                }
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

        return Ok(());
    }

    let mut cmd = Command::new("git");
    cmd.current_dir(coord.git().repo_root())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("--git-dir")
        .arg(coord.git().git_dir())
        .arg("log");

    if ctx.json {
        cmd.arg("--topo-order")
            .arg(format!("-n{}", args.limit))
            .arg("--format=%H\t%h\t%P\t%an\t%ct\t%D\t%s");
    } else {
        cmd.arg(if use_color() {
            "--color=always"
        } else {
            "--color=never"
        })
        .arg("--decorate=short")
        .arg(format!("-n{}", args.limit));

        if args.oneline {
            cmd.arg("--oneline");
        }

        if args.patch {
            cmd.arg("-p");
        }

        if args.stat {
            cmd.arg("--stat");
        }

        if args.name_only {
            cmd.arg("--name-only");
        }

        if args.name_status {
            cmd.arg("--name-status");
        }

        if args.graph {
            cmd.arg("--graph");
        }
    }

    if args.all {
        cmd.arg("--all");
    }

    if let Some(a) = &args.author {
        cmd.arg(format!("--author={a}"));
    }

    if let Some(g) = &args.grep {
        cmd.arg(format!("--grep={g}"));
    }

    if let Some(s) = &args.pickaxe {
        cmd.arg(format!("-S{s}"));
    }

    if let Some(s) = &args.since {
        cmd.arg(format!("--since={s}"));
    }

    if let Some(u) = &args.until {
        cmd.arg(format!("--until={u}"));
    }

    if args.reverse {
        cmd.arg("--reverse");
    }

    cmd.arg("--end-of-options");

    if let Some(b) = eff_branch
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cmd.arg(b);
    } else if !args.all {
        cmd.arg("HEAD");
    }

    if !path_args.is_empty() {
        cmd.arg("--").args(&path_args);
    }

    let out = cmd.output()?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        anyhow::bail!("git log failed: {stderr}");
    }

    if ctx.json {
        let mut entries = Vec::new();

        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts: Vec<&str> = line.splitn(7, '\t').collect();

            if parts.len() == 7 {
                let sha = parts[0].trim().to_string();
                let short_sha = parts[1].trim().to_string();
                let parents: Vec<String> = parts[2]
                    .split_whitespace()
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string)
                    .collect();
                let entry_author = parts[3].trim().to_string();
                let timestamp = parts[4].trim().parse::<i64>().unwrap_or(0);
                let refs: Vec<String> = parts[5]
                    .split(',')
                    .map(str::trim)
                    .filter(|r| !r.is_empty() && !r.starts_with("tessera/"))
                    .map(ToString::to_string)
                    .collect();
                let subject = parts[6].trim().to_string();

                entries.push(json!({
                    "sha": sha,
                    "short_sha": short_sha,
                    "parents": parents,
                    "author": entry_author,
                    "timestamp": timestamp,
                    "subject": subject,
                    "refs": refs,
                }));
            }
        }

        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        let log_text = String::from_utf8_lossy(&out.stdout);
        emit_paged_output(
            &log_text,
            true,
            ctx.no_pager,
            ctx.paginate,
            ctx.json,
            Some(&coord),
        );
    }

    Ok(())
}

/// Executes `tes radar`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or radar computation fails.
pub fn run_radar(ctx: &CliContext) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let report = coord.radar()?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| "main".to_string());
    let mut out_buf = String::new();
    let _ = writeln!(
        out_buf,
        "{} ({}):",
        c_bold("Active Agent Sessions"),
        c_cyan(report.active_agents.len())
    );
    let repo_root = coord.git().repo_root();

    for a in &report.active_agents {
        let br = a.target_branch.as_deref().unwrap_or(&default_branch);
        let _ = writeln!(
            out_buf,
            "  {} {} [branch: {}, slot {}, change {}]: \"{}\"",
            c_orange("•"),
            format_session_link(&a.agent_id, repo_root, c_cyan(&a.agent_id)),
            c_green(br),
            c_yellow(format!("#{}", a.slot)),
            c_yellow(a.change_id.short()),
            a.intent
        );
    }

    if !report.collisions.is_empty() {
        let _ = writeln!(
            out_buf,
            "\n{} ({}):",
            c_yellow("Live Overlaps"),
            c_red(report.collisions.len())
        );

        for col in &report.collisions {
            let pairs: Vec<String> = col
                .competing_agents
                .iter()
                .map(|(k, v)| format!("{} (\"{v}\")", format_session_link(k, repo_root, c_cyan(k))))
                .collect();

            let _ = writeln!(
                out_buf,
                "  {} {} -> {}",
                c_red("!"),
                c_bold(&col.target),
                pairs.join(" vs ")
            );
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

/// Executes `tes oplog`.
///
/// # Errors
/// Returns an error if the repository cannot be opened or the `OpLog` cannot be read.
pub fn run_oplog(ctx: &CliContext, limit: usize) -> anyhow::Result<()> {
    let coord = Coordinator::open(&ctx.cwd)?;
    let ops = coord.oplog().list_history(limit)?;

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&ops)?);
        return Ok(());
    }

    let mut out_buf = String::new();
    let repo_root = coord.git().repo_root();

    for op in &ops {
        let kind_label = if op.is_ephemeral {
            c_dim("ephemeral")
        } else {
            c_green("milestone")
        };

        let _ = writeln!(
            out_buf,
            "{} [{}] ({}): {}",
            c_yellow(op.id.short()),
            format_session_link(&op.agent_id, repo_root, c_cyan(&op.agent_id)),
            kind_label,
            op.description
        );
    }

    emit_paged_output(
        &out_buf,
        true,
        ctx.no_pager,
        ctx.paginate,
        ctx.json,
        Some(&coord),
    );

    Ok(())
}
