//! Full 3-way repository tree merging with `O(1)` unchanged subtree skipping,
//! cross-directory rename routing, binary/symlink/gitlink/file-dir safety,
//! and sequential migration collision re-sequencing.

use std::collections::{BTreeMap, BTreeSet};

use core::{
    ConflictKind, ConflictRecord, CoreResult, EntryMode, FileEntry, GitBackend, GitTreeNode, Merge,
    ObjectId, now_millis,
};
use serde::{Deserialize, Serialize};

use crate::ast_merge::merge_file_3way;
use crate::drivers::{
    MigrationResequence, format_resequenced_filename, is_migration_dir, is_migration_file,
    parse_numeric_prefix,
};
use crate::rename::{RenameMatch, detect_renames};

/// Minimum byte length required for exact-OID binary rename matching so tiny placeholder
/// blobs are not falsely paired across unrelated paths.
const MIN_EXACT_BINARY_BYTES: usize = 16;

/// Result of a full 3-way repository tree merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeMergeOutcome {
    /// Root Git tree [`ObjectId`] of the merged tree written to `.git/objects`.
    pub merged_tree_id: ObjectId,
    /// Flattened file map of the active/merged paths inspected during the merge.
    pub merged_files: BTreeMap<String, FileEntry>,
    /// Number of unchanged directory subtrees skipped in `O(1)` without descending into child trees.
    pub skipped_subtrees: usize,
    /// First-class [`ConflictRecord`]s produced during the merge (empty if 100% clean).
    pub conflicts: Vec<ConflictRecord>,
    /// File moves or renames detected and automatically routed.
    pub renames_applied: Vec<RenameMatch>,
    /// Colliding sequential migrations automatically renumbered.
    pub migrations_resequenced: Vec<MigrationResequence>,
    /// List of file paths that were modified between `base_tree` and `merged_tree_id`.
    pub touched_paths: Vec<String>,
}

/// Performs a full 3-way repository tree merge between `base_tree`, `left_tree`, and `right_tree`.
///
/// Combines:
/// - `O(1)` directory-subtree and blob-hash fast-pathing for untouched subtrees and files;
/// - Cross-directory / cross-package rename detection and AST edit routing;
/// - Sequential migration collision re-sequencing (preserving paired `.up`/`.down` files and order);
/// - Binary, symbolic link (`120000`), submodule gitlink (`160000`), and file-vs-directory safety;
/// - Manifest & lockfile merge drivers (`Cargo.toml`, `Cargo.lock`, `package.json`);
/// - Tree-sitter structural 3-way AST merging and first-class [`ConflictRecord`] generation.
///
/// # Errors
/// Returns [`core::CoreError`] if reading or writing Git objects fails.
#[allow(clippy::too_many_arguments)]
pub fn merge_trees_3way(
    git: &GitBackend,
    base_tree: &ObjectId,
    left_tree: &ObjectId,
    right_tree: &ObjectId,
    left_agent_id: &str,
    left_intent: &str,
    right_agent_id: &str,
    right_intent: &str,
) -> CoreResult<TreeMergeOutcome> {
    if left_tree == right_tree || right_tree == base_tree {
        let mut base_files = BTreeMap::new();
        let mut merged_files = BTreeMap::new();
        let mut right_files = BTreeMap::new();
        let mut passthrough = BTreeMap::new();
        let skipped_subtrees = collect_selective_trees_3way(
            git,
            Some(base_tree),
            Some(left_tree),
            Some(left_tree),
            "",
            &mut base_files,
            &mut merged_files,
            &mut right_files,
            &mut passthrough,
        )?;
        let touched_paths = compute_touched_paths(&base_files, &merged_files);
        return Ok(TreeMergeOutcome {
            merged_tree_id: *left_tree,
            merged_files,
            skipped_subtrees,
            conflicts: Vec::new(),
            renames_applied: Vec::new(),
            migrations_resequenced: Vec::new(),
            touched_paths,
        });
    }

    if left_tree == base_tree {
        let mut base_files = BTreeMap::new();
        let mut left_files = BTreeMap::new();
        let mut merged_files = BTreeMap::new();
        let mut passthrough = BTreeMap::new();
        let skipped_subtrees = collect_selective_trees_3way(
            git,
            Some(base_tree),
            Some(right_tree),
            Some(right_tree),
            "",
            &mut base_files,
            &mut left_files,
            &mut merged_files,
            &mut passthrough,
        )?;
        let touched_paths = compute_touched_paths(&base_files, &merged_files);
        return Ok(TreeMergeOutcome {
            merged_tree_id: *right_tree,
            merged_files,
            skipped_subtrees,
            conflicts: Vec::new(),
            renames_applied: Vec::new(),
            migrations_resequenced: Vec::new(),
            touched_paths,
        });
    }

    let mut base_files = BTreeMap::new();
    let mut left_files = BTreeMap::new();
    let mut right_files = BTreeMap::new();
    let mut passthrough_subtrees = BTreeMap::new();

    let skipped_subtrees = collect_selective_trees_3way(
        git,
        Some(base_tree),
        Some(left_tree),
        Some(right_tree),
        "",
        &mut base_files,
        &mut left_files,
        &mut right_files,
        &mut passthrough_subtrees,
    )?;

    let mut renames_applied = Vec::new();
    let mut migrations_resequenced = Vec::new();
    let mut conflicts = Vec::new();

    // Detect renames on BOTH sides against the original unmutated maps first.
    let left_renames = find_side_renames(git, &base_files, &left_files)?;
    let right_renames = find_side_renames(git, &base_files, &right_files)?;

    let left_by_old: BTreeMap<String, RenameMatch> = left_renames
        .into_iter()
        .map(|rm| (rm.old_path.clone(), rm))
        .collect();
    let right_by_old: BTreeMap<String, RenameMatch> = right_renames
        .into_iter()
        .map(|rm| (rm.old_path.clone(), rm))
        .collect();

    let mut effective_base = base_files.clone();
    let mut handled_old_paths: BTreeSet<String> = BTreeSet::new();

    for (old_path, l_rm) in &left_by_old {
        if let Some(r_rm) = right_by_old.get(old_path) {
            handled_old_paths.insert(old_path.clone());

            if l_rm.new_path == r_rm.new_path {
                if let Some(orig_entry) = effective_base.remove(old_path) {
                    effective_base.insert(l_rm.new_path.clone(), orig_entry);
                }

                renames_applied.push(l_rm.clone());
            } else {
                // Rename/rename (1-to-2) conflict: preserve both destination files and record a conflict.
                let base_oid = base_files.get(old_path).map(|e| e.blob_id);
                let left_oid = left_files.get(&l_rm.new_path).map(|e| e.blob_id);
                let right_oid = right_files.get(&r_rm.new_path).map(|e| e.blob_id);
                let blob_merge = Merge {
                    removes: vec![base_oid],
                    adds: vec![left_oid, right_oid],
                };
                let symbol = Some("rename/rename".to_string());
                let cid = ConflictRecord::compute_id(old_path, symbol.as_deref(), &blob_merge);
                let mut intents = BTreeMap::new();
                intents.insert(left_agent_id.to_string(), left_intent.to_string());
                intents.insert(right_agent_id.to_string(), right_intent.to_string());

                conflicts.push(ConflictRecord {
                    id: cid,
                    path: old_path.clone(),
                    symbol,
                    kind: ConflictKind::DeleteModify,
                    merge: blob_merge,
                    agent_intents: intents,
                    base_snippet: format!("original path: {old_path}"),
                    left_snippet: format!("renamed to: {}", l_rm.new_path),
                    right_snippet: format!("renamed to: {}", r_rm.new_path),
                    diagnostics: None,
                    created_at_ms: now_millis(),
                });
                effective_base.remove(old_path);
            }
        }
    }

    for (old_path, rm) in &left_by_old {
        if handled_old_paths.contains(old_path) {
            continue;
        }

        // Guard against rename/add collision: do not drop right's edits to old_path if right already created new_path.
        if right_files.contains_key(&rm.new_path) {
            continue;
        }

        if let Some(base_entry) = base_files.get(old_path) {
            if let Some(right_old) = right_files.remove(old_path) {
                right_files.insert(rm.new_path.clone(), right_old);

                if let Some(orig_entry) = effective_base.remove(old_path) {
                    effective_base.insert(rm.new_path.clone(), orig_entry);
                }

                renames_applied.push(rm.clone());
            } else {
                // Rename/delete conflict: left renamed `old_path -> new_path` while right deleted `old_path`.
                let base_oid = Some(base_entry.blob_id);
                let left_oid = left_files.get(&rm.new_path).map(|e| e.blob_id);
                let blob_merge = Merge {
                    removes: vec![base_oid],
                    adds: vec![left_oid, None],
                };
                let symbol = Some("rename/delete".to_string());
                let cid = ConflictRecord::compute_id(old_path, symbol.as_deref(), &blob_merge);
                let mut intents = BTreeMap::new();
                intents.insert(left_agent_id.to_string(), left_intent.to_string());
                intents.insert(right_agent_id.to_string(), right_intent.to_string());

                conflicts.push(ConflictRecord {
                    id: cid,
                    path: old_path.clone(),
                    symbol,
                    kind: ConflictKind::DeleteModify,
                    merge: blob_merge,
                    agent_intents: intents,
                    base_snippet: format!("original path: {old_path}"),
                    left_snippet: format!("renamed to: {}", rm.new_path),
                    right_snippet: "<deleted>".to_string(),
                    diagnostics: Some(format!(
                        "Rename/delete conflict: '{old_path}' renamed to '{}' while deleted on the other side",
                        rm.new_path
                    )),
                    created_at_ms: now_millis(),
                });
                effective_base.remove(old_path);
            }
        }
    }

    for (old_path, rm) in &right_by_old {
        if handled_old_paths.contains(old_path) {
            continue;
        }

        // Guard against rename/add collision: do not drop left's edits to old_path if left already created new_path.
        if left_files.contains_key(&rm.new_path) {
            continue;
        }

        if let Some(base_entry) = base_files.get(old_path) {
            if let Some(left_old) = left_files.remove(old_path) {
                left_files.insert(rm.new_path.clone(), left_old);

                if let Some(orig_entry) = effective_base.remove(old_path) {
                    effective_base.insert(rm.new_path.clone(), orig_entry);
                }

                renames_applied.push(rm.clone());
            } else {
                // Rename/delete conflict: right renamed `old_path -> new_path` while left deleted `old_path`.
                let base_oid = Some(base_entry.blob_id);
                let right_oid = right_files.get(&rm.new_path).map(|e| e.blob_id);
                let blob_merge = Merge {
                    removes: vec![base_oid],
                    adds: vec![None, right_oid],
                };
                let symbol = Some("rename/delete".to_string());
                let cid = ConflictRecord::compute_id(old_path, symbol.as_deref(), &blob_merge);
                let mut intents = BTreeMap::new();
                intents.insert(left_agent_id.to_string(), left_intent.to_string());
                intents.insert(right_agent_id.to_string(), right_intent.to_string());

                conflicts.push(ConflictRecord {
                    id: cid,
                    path: old_path.clone(),
                    symbol,
                    kind: ConflictKind::DeleteModify,
                    merge: blob_merge,
                    agent_intents: intents,
                    base_snippet: format!("original path: {old_path}"),
                    left_snippet: "<deleted>".to_string(),
                    right_snippet: format!("renamed to: {}", rm.new_path),
                    diagnostics: Some(format!(
                        "Rename/delete conflict: '{old_path}' renamed to '{}' while deleted on the other side",
                        rm.new_path
                    )),
                    created_at_ms: now_millis(),
                });
                effective_base.remove(old_path);
            }
        }
    }

    resequence_migrations_in_place(
        &effective_base,
        &left_files,
        &mut right_files,
        &mut migrations_resequenced,
    );

    let mut all_paths = BTreeSet::new();

    for p in effective_base
        .keys()
        .chain(left_files.keys())
        .chain(right_files.keys())
    {
        all_paths.insert(p.clone());
    }

    let mut merged_files = BTreeMap::new();

    let left_label = format!("{left_agent_id} ({left_intent})");
    let right_label = format!("{right_agent_id} ({right_intent})");

    for path in all_paths {
        let b_entry = effective_base.get(&path).cloned();
        let l_entry = left_files.get(&path).cloned();
        let r_entry = right_files.get(&path).cloned();

        let alg_merge = Merge::from_3way(b_entry.clone(), l_entry.clone(), r_entry.clone());

        if let Some(resolved_opt) = alg_merge.as_resolved() {
            if let Some(entry) = resolved_opt {
                merged_files.insert(path, entry.clone());
            }

            continue;
        }

        // Unresolved algebraic entry: either both modified/added `path` with different entries,
        // or one deleted while the other modified.
        match (&b_entry, &l_entry, &r_entry) {
            (_, Some(le), Some(re)) => {
                if le.is_symlink()
                    || re.is_symlink()
                    || le.is_gitlink()
                    || re.is_gitlink()
                    || b_entry
                        .as_ref()
                        .is_some_and(|e| e.is_symlink() || e.is_gitlink())
                {
                    merged_files.insert(path.clone(), le.clone());
                    let blob_merge = Merge {
                        removes: vec![b_entry.as_ref().map(|e| e.blob_id)],
                        adds: vec![Some(le.blob_id), Some(re.blob_id)],
                    };
                    let cid = ConflictRecord::compute_id(&path, None, &blob_merge);
                    let mut intents = BTreeMap::new();
                    intents.insert(left_agent_id.to_string(), left_intent.to_string());
                    intents.insert(right_agent_id.to_string(), right_intent.to_string());

                    conflicts.push(ConflictRecord {
                        id: cid,
                        path,
                        symbol: None,
                        kind: ConflictKind::Binary,
                        merge: blob_merge,
                        agent_intents: intents,
                        base_snippet: format_special_entry_snippet(git, b_entry.as_ref())?,
                        left_snippet: format_special_entry_snippet(git, Some(le))?,
                        right_snippet: format_special_entry_snippet(git, Some(re))?,
                        diagnostics: None,
                        created_at_ms: now_millis(),
                    });
                    continue;
                }

                let base_exec = b_entry.as_ref().is_some_and(|e| e.executable);
                let merged_exec = Merge::from_3way(base_exec, le.executable, re.executable)
                    .as_resolved()
                    .copied()
                    .unwrap_or(le.executable || re.executable);
                let blob_alg = Merge::from_3way(
                    b_entry.as_ref().map(|e| e.blob_id),
                    Some(le.blob_id),
                    Some(re.blob_id),
                );

                if let Some(Some(resolved_blob_id)) = blob_alg.as_resolved() {
                    merged_files.insert(path, FileEntry::regular(*resolved_blob_id, merged_exec));
                    continue;
                }

                let base_bytes = match &b_entry {
                    Some(be) => git.read_blob(&be.blob_id)?,
                    None => Vec::new(),
                };
                let left_bytes = git.read_blob(&le.blob_id)?;
                let right_bytes = git.read_blob(&re.blob_id)?;

                if is_binary_bytes(&left_bytes)
                    || is_binary_bytes(&right_bytes)
                    || is_binary_bytes(&base_bytes)
                {
                    merged_files.insert(path.clone(), FileEntry::regular(le.blob_id, merged_exec));
                    let blob_merge = Merge {
                        removes: vec![b_entry.as_ref().map(|e| e.blob_id)],
                        adds: vec![Some(le.blob_id), Some(re.blob_id)],
                    };
                    let cid = ConflictRecord::compute_id(&path, None, &blob_merge);
                    let mut intents = BTreeMap::new();
                    intents.insert(left_agent_id.to_string(), left_intent.to_string());
                    intents.insert(right_agent_id.to_string(), right_intent.to_string());

                    conflicts.push(ConflictRecord {
                        id: cid,
                        path,
                        symbol: None,
                        kind: ConflictKind::Binary,
                        merge: blob_merge,
                        agent_intents: intents,
                        base_snippet: if b_entry.is_some() {
                            format!("<binary: {} bytes>", base_bytes.len())
                        } else {
                            "<absent>".to_string()
                        },
                        left_snippet: format!("<binary: {} bytes>", left_bytes.len()),
                        right_snippet: format!("<binary: {} bytes>", right_bytes.len()),
                        diagnostics: None,
                        created_at_ms: now_millis(),
                    });
                    continue;
                }

                let base_str = std::str::from_utf8(&base_bytes).unwrap_or("");
                let left_str = std::str::from_utf8(&left_bytes).unwrap_or("");
                let right_str = std::str::from_utf8(&right_bytes).unwrap_or("");

                let ast_out = merge_file_3way(
                    &path,
                    base_str,
                    left_str,
                    right_str,
                    &left_label,
                    &right_label,
                );
                let merged_blob_id = git.write_blob(ast_out.content.as_bytes())?;
                merged_files.insert(
                    path.clone(),
                    FileEntry::regular(merged_blob_id, merged_exec),
                );

                if ast_out.has_conflicts {
                    let symbol = ast_out.conflicted_symbols.first().cloned();
                    let kind = if symbol.is_some() {
                        ConflictKind::AstNode
                    } else {
                        ConflictKind::TextHunk
                    };
                    let blob_merge = Merge {
                        removes: vec![b_entry.map(|e| e.blob_id)],
                        adds: vec![Some(le.blob_id), Some(re.blob_id)],
                    };
                    let cid = ConflictRecord::compute_id(&path, symbol.as_deref(), &blob_merge);
                    let mut intents = BTreeMap::new();
                    intents.insert(left_agent_id.to_string(), left_intent.to_string());
                    intents.insert(right_agent_id.to_string(), right_intent.to_string());

                    conflicts.push(ConflictRecord {
                        id: cid,
                        path,
                        symbol,
                        kind,
                        merge: blob_merge,
                        agent_intents: intents,
                        base_snippet: ast_out.base_conflict_snippet,
                        left_snippet: ast_out.left_conflict_snippet,
                        right_snippet: ast_out.right_conflict_snippet,
                        diagnostics: None,
                        created_at_ms: now_millis(),
                    });
                }
            }
            (Some(be), Some(le), None) | (Some(be), None, Some(le)) => {
                merged_files.insert(path.clone(), le.clone());
                let blob_merge = Merge {
                    removes: vec![Some(be.blob_id)],
                    adds: vec![
                        l_entry.as_ref().map(|e| e.blob_id),
                        r_entry.as_ref().map(|e| e.blob_id),
                    ],
                };
                let cid = ConflictRecord::compute_id(&path, None, &blob_merge);
                let mut intents = BTreeMap::new();
                intents.insert(left_agent_id.to_string(), left_intent.to_string());
                intents.insert(right_agent_id.to_string(), right_intent.to_string());

                let base_snippet = format_entry_preview(git, be)?;
                let mod_snippet = format_entry_preview(git, le)?;

                conflicts.push(ConflictRecord {
                    id: cid,
                    path,
                    symbol: None,
                    kind: ConflictKind::DeleteModify,
                    merge: blob_merge,
                    agent_intents: intents,
                    base_snippet,
                    left_snippet: if l_entry.is_some() {
                        mod_snippet.clone()
                    } else {
                        "<deleted>".to_string()
                    },
                    right_snippet: if r_entry.is_some() {
                        mod_snippet
                    } else {
                        "<deleted>".to_string()
                    },
                    diagnostics: None,
                    created_at_ms: now_millis(),
                });
            }
            _ => {}
        }
    }

    resolve_file_directory_conflicts(
        &effective_base,
        &left_files,
        &right_files,
        &passthrough_subtrees,
        left_agent_id,
        left_intent,
        right_agent_id,
        right_intent,
        &mut merged_files,
        &mut conflicts,
    );

    let merged_tree_id =
        write_tree_from_map_and_subtrees(git, &merged_files, &passthrough_subtrees)?;
    let touched_paths = compute_touched_paths(&base_files, &merged_files);

    Ok(TreeMergeOutcome {
        merged_tree_id,
        merged_files,
        skipped_subtrees,
        conflicts,
        renames_applied,
        migrations_resequenced,
        touched_paths,
    })
}

/// Returns `true` if `bytes` contains NUL bytes or is not valid UTF-8.
fn is_binary_bytes(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

/// Formats a human-readable snippet for a symlink, gitlink, or regular file entry without ever
/// calling `read_blob` on a submodule commit (`EntryMode::Gitlink`).
fn format_special_entry_snippet(git: &GitBackend, entry: Option<&FileEntry>) -> CoreResult<String> {
    let Some(e) = entry else {
        return Ok("<absent>".to_string());
    };

    if e.is_gitlink() {
        return Ok(format!("gitlink {}", e.blob_id));
    }

    if e.is_symlink() {
        let bytes = git.read_blob(&e.blob_id)?;
        return Ok(format!("symlink -> {}", String::from_utf8_lossy(&bytes)));
    }

    format_entry_preview(git, e)
}

/// Formats a short preview snippet for an entry in delete/modify conflicts safely handling
/// binary files, symlinks, and submodule gitlinks.
fn format_entry_preview(git: &GitBackend, entry: &FileEntry) -> CoreResult<String> {
    if entry.is_gitlink() {
        return Ok(format!("gitlink {}", entry.blob_id));
    }

    let bytes = git.read_blob(&entry.blob_id)?;

    if entry.is_symlink() {
        return Ok(format!("symlink -> {}", String::from_utf8_lossy(&bytes)));
    }

    if is_binary_bytes(&bytes) {
        return Ok(format!("<binary: {} bytes>", bytes.len()));
    }

    let text = std::str::from_utf8(&bytes).unwrap_or("");
    Ok(text.lines().take(15).collect::<Vec<_>>().join("\n"))
}

/// Detects leaf paths in `merged_files` that collide with directory prefixes implied by other
/// entries in `merged_files` or `passthrough_subtrees`, emitting [`ConflictKind::FileDirectory`]
/// records and removing colliding leaf paths so the resulting Git tree has no duplicate entries.
#[allow(clippy::too_many_arguments)]
fn resolve_file_directory_conflicts(
    base_files: &BTreeMap<String, FileEntry>,
    left_files: &BTreeMap<String, FileEntry>,
    right_files: &BTreeMap<String, FileEntry>,
    passthrough_subtrees: &BTreeMap<String, ObjectId>,
    left_agent_id: &str,
    left_intent: &str,
    right_agent_id: &str,
    right_intent: &str,
    merged_files: &mut BTreeMap<String, FileEntry>,
    conflicts: &mut Vec<ConflictRecord>,
) {
    let mut dir_prefixes: BTreeSet<String> = BTreeSet::new();

    for path in merged_files.keys() {
        let mut cur = path.as_str();

        while let Some((parent, _)) = cur.rsplit_once('/') {
            dir_prefixes.insert(parent.to_string());
            cur = parent;
        }
    }

    for dir_path in passthrough_subtrees.keys() {
        let trimmed = dir_path.trim_matches('/');

        if !trimmed.is_empty() {
            dir_prefixes.insert(trimmed.to_string());
            let mut cur = trimmed;

            while let Some((parent, _)) = cur.rsplit_once('/') {
                dir_prefixes.insert(parent.to_string());
                cur = parent;
            }
        }
    }

    let colliding_files: Vec<String> = merged_files
        .keys()
        .filter(|p| dir_prefixes.contains(p.as_str()))
        .cloned()
        .collect();

    for file_path in colliding_files {
        let removed_entry = merged_files.remove(&file_path);
        conflicts.retain(|c| c.path != file_path);

        let b_oid = base_files.get(&file_path).map(|e| e.blob_id);
        let l_oid = left_files.get(&file_path).map(|e| e.blob_id);
        let r_oid = right_files.get(&file_path).map(|e| e.blob_id);

        let left_is_dir = has_descendant_prefix(left_files, &file_path);
        let right_is_dir = has_descendant_prefix(right_files, &file_path);
        let base_is_dir = has_descendant_prefix(base_files, &file_path);

        let blob_merge = Merge {
            removes: vec![b_oid],
            adds: vec![
                l_oid.or_else(|| removed_entry.as_ref().map(|e| e.blob_id)),
                r_oid,
            ],
        };
        let cid = ConflictRecord::compute_id(&file_path, None, &blob_merge);
        let mut intents = BTreeMap::new();
        intents.insert(left_agent_id.to_string(), left_intent.to_string());
        intents.insert(right_agent_id.to_string(), right_intent.to_string());

        let describe_side = |is_file: bool, is_dir: bool| -> String {
            if is_dir {
                format!("<directory {file_path}/>")
            } else if is_file {
                format!("<file {file_path}>")
            } else {
                "<absent>".to_string()
            }
        };

        conflicts.push(ConflictRecord {
            id: cid,
            path: file_path.clone(),
            symbol: None,
            kind: ConflictKind::FileDirectory,
            merge: blob_merge,
            agent_intents: intents,
            base_snippet: describe_side(b_oid.is_some(), base_is_dir),
            left_snippet: describe_side(l_oid.is_some(), left_is_dir),
            right_snippet: describe_side(r_oid.is_some(), right_is_dir),
            diagnostics: None,
            created_at_ms: now_millis(),
        });
    }
}

/// Returns `true` if `map` contains any path nested beneath `dir_prefix/`.
fn has_descendant_prefix(map: &BTreeMap<String, FileEntry>, dir_prefix: &str) -> bool {
    let prefix = format!("{dir_prefix}/");
    map.range(prefix.clone()..)
        .next()
        .is_some_and(|(k, _)| k.starts_with(&prefix))
}

/// Recursively walks `(base_tree, left_tree, right_tree)` in lockstep, skipping any directory
/// subtree in `O(1)` where `base_dir_id == left_dir_id && left_dir_id == right_dir_id`.
#[allow(clippy::too_many_arguments)]
fn collect_selective_trees_3way(
    git: &GitBackend,
    base_tree: Option<&ObjectId>,
    left_tree: Option<&ObjectId>,
    right_tree: Option<&ObjectId>,
    prefix: &str,
    base_files: &mut BTreeMap<String, FileEntry>,
    left_files: &mut BTreeMap<String, FileEntry>,
    right_files: &mut BTreeMap<String, FileEntry>,
    passthrough_subtrees: &mut BTreeMap<String, ObjectId>,
) -> CoreResult<usize> {
    if !prefix.is_empty() && base_tree == left_tree && left_tree == right_tree {
        if let Some(unchanged_id) = base_tree {
            passthrough_subtrees.insert(prefix.to_string(), *unchanged_id);
            return Ok(1);
        }
    }

    let b_nodes = read_nodes_map(git, base_tree)?;
    let l_nodes = read_nodes_map(git, left_tree)?;
    let r_nodes = read_nodes_map(git, right_tree)?;

    let mut names = BTreeSet::new();

    for k in b_nodes.keys().chain(l_nodes.keys()).chain(r_nodes.keys()) {
        names.insert(k.clone());
    }

    let mut skipped = 0usize;

    for name in names {
        let rel_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        let bn = b_nodes.get(&name);
        let ln = l_nodes.get(&name);
        let rn = r_nodes.get(&name);

        let is_dir = bn.is_some_and(|n| n.mode == EntryMode::Directory)
            || ln.is_some_and(|n| n.mode == EntryMode::Directory)
            || rn.is_some_and(|n| n.mode == EntryMode::Directory);

        if is_dir {
            let b_sub = bn.filter(|n| n.mode == EntryMode::Directory).map(|n| &n.id);
            let l_sub = ln.filter(|n| n.mode == EntryMode::Directory).map(|n| &n.id);
            let r_sub = rn.filter(|n| n.mode == EntryMode::Directory).map(|n| &n.id);

            record_file_node_if_present(bn, &rel_path, base_files);
            record_file_node_if_present(ln, &rel_path, left_files);
            record_file_node_if_present(rn, &rel_path, right_files);

            skipped += collect_selective_trees_3way(
                git,
                b_sub,
                l_sub,
                r_sub,
                &rel_path,
                base_files,
                left_files,
                right_files,
                passthrough_subtrees,
            )?;
        } else {
            record_file_node_if_present(bn, &rel_path, base_files);
            record_file_node_if_present(ln, &rel_path, left_files);
            record_file_node_if_present(rn, &rel_path, right_files);
        }
    }

    Ok(skipped)
}

/// Reads the direct child [`GitTreeNode`] entries of `tree_id` keyed by entry name.
fn read_nodes_map(
    git: &GitBackend,
    tree_id: Option<&ObjectId>,
) -> CoreResult<BTreeMap<String, GitTreeNode>> {
    let mut map = BTreeMap::new();

    if let Some(tid) = tree_id {
        for node in git.read_tree_nodes(tid)? {
            map.insert(node.name.clone(), node);
        }
    }

    Ok(map)
}

/// Inserts `node_opt` into `out` at `rel_path` if it is a non-directory leaf entry,
/// preserving exact [`EntryMode`] (`100644`, `100755`, `120000`, `160000`).
fn record_file_node_if_present(
    node_opt: Option<&GitTreeNode>,
    rel_path: &str,
    out: &mut BTreeMap<String, FileEntry>,
) {
    if let Some(node) = node_opt {
        if node.mode != EntryMode::Directory {
            out.insert(
                rel_path.to_string(),
                FileEntry::from_mode(node.id, node.mode),
            );
        }
    }
}

/// Writes a hierarchical Git tree combining modified `files` and unchanged `passthrough_subtrees`,
/// guaranteeing exact entry modes and zero duplicate entry names within any tree level.
fn write_tree_from_map_and_subtrees(
    git: &GitBackend,
    files: &BTreeMap<String, FileEntry>,
    passthrough_subtrees: &BTreeMap<String, ObjectId>,
) -> CoreResult<ObjectId> {
    let mut direct_nodes_by_name: BTreeMap<String, GitTreeNode> = BTreeMap::new();
    let mut subdirs_files: BTreeMap<String, BTreeMap<String, FileEntry>> = BTreeMap::new();
    let mut subdirs_trees: BTreeMap<String, BTreeMap<String, ObjectId>> = BTreeMap::new();

    for (rel_path, entry) in files {
        let trimmed = rel_path.trim_matches('/');

        if trimmed.is_empty() {
            continue;
        }

        if let Some((first, rest)) = trimmed.split_once('/') {
            subdirs_files
                .entry(first.to_string())
                .or_default()
                .insert(rest.to_string(), entry.clone());
        } else {
            direct_nodes_by_name.insert(
                trimmed.to_string(),
                GitTreeNode {
                    mode: entry.entry_mode(),
                    name: trimmed.to_string(),
                    id: entry.blob_id,
                },
            );
        }
    }

    for (dir_path, tree_id) in passthrough_subtrees {
        let trimmed = dir_path.trim_matches('/');

        if trimmed.is_empty() {
            continue;
        }

        if let Some((first, rest)) = trimmed.split_once('/') {
            subdirs_trees
                .entry(first.to_string())
                .or_default()
                .insert(rest.to_string(), *tree_id);
        } else {
            direct_nodes_by_name.insert(
                trimmed.to_string(),
                GitTreeNode {
                    mode: EntryMode::Directory,
                    name: trimmed.to_string(),
                    id: *tree_id,
                },
            );
        }
    }

    let mut all_subdir_names = BTreeSet::new();

    for k in subdirs_files.keys().chain(subdirs_trees.keys()) {
        all_subdir_names.insert(k.clone());
    }

    for dir_name in all_subdir_names {
        let f_map = subdirs_files.remove(&dir_name).unwrap_or_default();
        let t_map = subdirs_trees.remove(&dir_name).unwrap_or_default();
        let sub_tree_id = write_tree_from_map_and_subtrees(git, &f_map, &t_map)?;
        direct_nodes_by_name.insert(
            dir_name.clone(),
            GitTreeNode {
                mode: EntryMode::Directory,
                name: dir_name,
                id: sub_tree_id,
            },
        );
    }

    let direct_nodes: Vec<GitTreeNode> = direct_nodes_by_name.into_values().collect();
    git.write_tree_nodes(direct_nodes)
}

/// Identifies files moved or renamed between `base_files` and `side_files`, running an
/// `O(D + A)` exact blob-OID match pass before reading blobs for fuzzy shingle similarity.
fn find_side_renames(
    git: &GitBackend,
    base_files: &BTreeMap<String, FileEntry>,
    side_files: &BTreeMap<String, FileEntry>,
) -> CoreResult<Vec<RenameMatch>> {
    let mut deleted_entries: BTreeMap<String, &FileEntry> = BTreeMap::new();

    for (path, entry) in base_files {
        if !side_files.contains_key(path) && !entry.is_gitlink() {
            deleted_entries.insert(path.clone(), entry);
        }
    }

    if deleted_entries.is_empty() {
        return Ok(Vec::new());
    }

    let mut added_entries: BTreeMap<String, &FileEntry> = BTreeMap::new();

    for (path, entry) in side_files {
        if !base_files.contains_key(path) && !entry.is_gitlink() {
            added_entries.insert(path.clone(), entry);
        }
    }

    if added_entries.is_empty() {
        return Ok(Vec::new());
    }

    let mut del_by_oid: BTreeMap<ObjectId, Vec<String>> = BTreeMap::new();
    let mut add_by_oid: BTreeMap<ObjectId, Vec<String>> = BTreeMap::new();

    for (path, entry) in &deleted_entries {
        del_by_oid
            .entry(entry.blob_id)
            .or_default()
            .push(path.clone());
    }

    for (path, entry) in &added_entries {
        add_by_oid
            .entry(entry.blob_id)
            .or_default()
            .push(path.clone());
    }

    let mut exact_candidates: Vec<RenameMatch> = Vec::new();

    for (oid, del_paths) in &del_by_oid {
        let Some(add_paths) = add_by_oid.get(oid) else {
            continue;
        };

        let sample_bytes = git.read_blob(oid)?;

        if sample_bytes.is_empty() {
            continue;
        }

        if is_binary_bytes(&sample_bytes) {
            if sample_bytes.len() < MIN_EXACT_BINARY_BYTES {
                continue;
            }
        } else {
            let token_count = std::str::from_utf8(&sample_bytes)
                .map(|s| s.split_whitespace().take(3).count())
                .unwrap_or(0);

            if token_count < 3 {
                continue;
            }
        }

        for old_path in del_paths {
            for new_path in add_paths {
                exact_candidates.push(RenameMatch {
                    old_path: old_path.clone(),
                    new_path: new_path.clone(),
                    similarity_bps: 10_000,
                });
            }
        }
    }

    exact_candidates.sort_by(|a, b| {
        let a_same_base = a.old_path.rsplit('/').next() == a.new_path.rsplit('/').next();
        let b_same_base = b.old_path.rsplit('/').next() == b.new_path.rsplit('/').next();
        let a_same_dir = a.old_path.rsplit_once('/').map(|(d, _)| d)
            == a.new_path.rsplit_once('/').map(|(d, _)| d);
        let b_same_dir = b.old_path.rsplit_once('/').map(|(d, _)| d)
            == b.new_path.rsplit_once('/').map(|(d, _)| d);

        b_same_base
            .cmp(&a_same_base)
            .then_with(|| b_same_dir.cmp(&a_same_dir))
            .then_with(|| a.old_path.cmp(&b.old_path))
            .then_with(|| a.new_path.cmp(&b.new_path))
    });

    let mut matched_old: BTreeSet<String> = BTreeSet::new();
    let mut matched_new: BTreeSet<String> = BTreeSet::new();
    let mut results: Vec<RenameMatch> = Vec::new();

    for cand in exact_candidates {
        if matched_old.contains(&cand.old_path) || matched_new.contains(&cand.new_path) {
            continue;
        }

        matched_old.insert(cand.old_path.clone());
        matched_new.insert(cand.new_path.clone());
        results.push(cand);
    }

    let mut rem_deleted = BTreeMap::new();

    for (path, entry) in &deleted_entries {
        if !matched_old.contains(path) && !entry.is_symlink() {
            rem_deleted.insert(path.clone(), git.read_blob(&entry.blob_id)?);
        }
    }

    let mut rem_added = BTreeMap::new();

    for (path, entry) in &added_entries {
        if !matched_new.contains(path) && !entry.is_symlink() {
            rem_added.insert(path.clone(), git.read_blob(&entry.blob_id)?);
        }
    }

    if !rem_deleted.is_empty() && !rem_added.is_empty() {
        results.extend(detect_renames(&rem_deleted, &rem_added));
    }

    Ok(results)
}

/// Extracts a normalized pair stem from a migration suffix so paired `.up`/`.down` files
/// (e.g., `_billing.up.sql` and `_billing.down.sql`) share the same grouping key.
fn migration_pair_stem(suffix: &str) -> String {
    let lower = suffix.to_ascii_lowercase();

    for marker in [".up.", ".down.", "_up.", "_down."] {
        if let Some(pos) = lower.rfind(marker) {
            return suffix[..pos].to_string();
        }
    }

    if let Some(pos) = lower.rfind(".up").filter(|&p| p + 3 == lower.len()) {
        return suffix[..pos].to_string();
    }

    if let Some(pos) = lower.rfind(".down").filter(|&p| p + 5 == lower.len()) {
        return suffix[..pos].to_string();
    }

    suffix.to_string()
}

/// Metadata for a single newly added migration file on `right`.
struct RightMigrationFile {
    /// Original repository-relative path.
    path: String,
    /// Zero-padding width of the original numeric prefix.
    width: usize,
    /// Filename suffix after the numeric prefix (including `_` or `-`).
    suffix: String,
}

/// Detects colliding sequential migration files added in both `left_files` and `right_files`,
/// renumbering colliding `right_files` entries (and all subsequent higher-numbered `right`
/// migrations in the same directory) while keeping paired `.up`/`.down` files on the same sequence number.
fn resequence_migrations_in_place(
    base_files: &BTreeMap<String, FileEntry>,
    left_files: &BTreeMap<String, FileEntry>,
    right_files: &mut BTreeMap<String, FileEntry>,
    resequenced: &mut Vec<MigrationResequence>,
) {
    let mut base_left_max_num: BTreeMap<String, u64> = BTreeMap::new();
    let mut left_added_nums: BTreeSet<(String, u64)> = BTreeSet::new();

    for path in base_files.keys().chain(left_files.keys()) {
        if let Some((dir, file_name)) = path.rsplit_once('/') {
            if is_migration_dir(dir) && is_migration_file(file_name) {
                if let Some((num, _, _)) = parse_numeric_prefix(file_name) {
                    let entry = base_left_max_num.entry(dir.to_string()).or_insert(0);
                    *entry = (*entry).max(num);
                }
            }
        }
    }

    for path in left_files.keys() {
        if !base_files.contains_key(path) {
            if let Some((dir, file_name)) = path.rsplit_once('/') {
                if is_migration_dir(dir) && is_migration_file(file_name) {
                    if let Some((num, _, _)) = parse_numeric_prefix(file_name) {
                        left_added_nums.insert((dir.to_string(), num));
                    }
                }
            }
        }
    }

    // Group right-added migrations per directory by `(num, pair_stem)`.
    let mut right_groups_by_dir: BTreeMap<
        String,
        BTreeMap<(u64, String), Vec<RightMigrationFile>>,
    > = BTreeMap::new();

    for path in right_files.keys() {
        if base_files.contains_key(path) {
            continue;
        }

        if left_files.get(path) == right_files.get(path) {
            continue;
        }

        if let Some((dir, file_name)) = path.rsplit_once('/') {
            if is_migration_dir(dir) && is_migration_file(file_name) {
                if let Some((num, width, suffix)) = parse_numeric_prefix(file_name) {
                    let stem = migration_pair_stem(suffix);
                    right_groups_by_dir
                        .entry(dir.to_string())
                        .or_default()
                        .entry((num, stem))
                        .or_default()
                        .push(RightMigrationFile {
                            path: path.clone(),
                            width,
                            suffix: suffix.to_string(),
                        });
                }
            }
        }
    }

    for (dir, groups) in right_groups_by_dir {
        let min_colliding_num = groups
            .keys()
            .filter(|(num, _)| left_added_nums.contains(&(dir.clone(), *num)))
            .map(|(num, _)| *num)
            .min();

        let Some(first_collision) = min_colliding_num else {
            continue;
        };

        let mut next_num = base_left_max_num.get(&dir).copied().unwrap_or(0);

        for (num, _) in groups.keys() {
            if *num < first_collision {
                next_num = next_num.max(*num);
            }
        }

        let groups_to_shift: Vec<((u64, String), Vec<RightMigrationFile>)> = groups
            .into_iter()
            .filter(|((num, _), _)| *num >= first_collision)
            .collect();

        // Remove all shifted entries first so renumbering 0042 -> 0043 cannot overwrite 0043 before it moves to 0044.
        let mut staged: Vec<(String, String, FileEntry)> = Vec::new();

        for (_, files_in_group) in groups_to_shift {
            next_num = next_num.saturating_add(1);

            for mf in files_in_group {
                let new_file_name = format_resequenced_filename(next_num, mf.width, &mf.suffix);
                let new_path = format!("{dir}/{new_file_name}");

                if let Some(entry) = right_files.remove(&mf.path) {
                    staged.push((mf.path, new_path, entry));
                }
            }
        }

        for (old_path, new_path, entry) in staged {
            right_files.insert(new_path.clone(), entry);
            resequenced.push(MigrationResequence {
                original_path: old_path,
                resequenced_path: new_path,
            });
        }
    }
}

/// Computes the sorted list of paths that were added, modified, or deleted between `base` and `target`.
fn compute_touched_paths(
    base: &BTreeMap<String, FileEntry>,
    target: &BTreeMap<String, FileEntry>,
) -> Vec<String> {
    let mut touched = BTreeSet::new();

    for (p, entry) in target {
        if base.get(p) != Some(entry) {
            touched.insert(p.clone());
        }
    }

    for p in base.keys() {
        if !target.contains_key(p) {
            touched.insert(p.clone());
        }
    }

    touched.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_merge_handles_cross_package_rename_and_migration_resequencing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let base_util = git
            .write_blob(
                b"pub fn validate_id(id: &str) -> bool {\n    !id.is_empty()\n}\n\npub fn format_id(id: &str) -> String {\n    id.to_uppercase()\n}\n",
            )
            .expect("blob");
        let untouched_pkg = git
            .write_blob(b"pub fn untouched() -> u32 { 42 }\n")
            .expect("untouched blob");
        let mut base_map = BTreeMap::new();
        base_map.insert(
            "packages/api/src/util.rs".to_string(),
            FileEntry::regular(base_util, false),
        );
        base_map.insert(
            "packages/untouched-core/src/lib.rs".to_string(),
            FileEntry::regular(untouched_pkg, false),
        );
        let base_tree = git.write_tree_from_map(&base_map).expect("base tree");

        let mut left_map = BTreeMap::new();
        left_map.insert(
            "packages/shared/src/util.rs".to_string(),
            base_map["packages/api/src/util.rs"].clone(),
        );
        left_map.insert(
            "packages/untouched-core/src/lib.rs".to_string(),
            base_map["packages/untouched-core/src/lib.rs"].clone(),
        );
        left_map.insert(
            "db/migrations/0001_users.sql".to_string(),
            FileEntry::regular(
                git.write_blob(b"CREATE TABLE users (id INT);\n")
                    .expect("m1"),
                false,
            ),
        );
        let left_tree = git.write_tree_from_map(&left_map).expect("left tree");

        let right_util = git
            .write_blob(
                b"pub fn validate_id(id: &str) -> bool {\n    !id.is_empty()\n}\n\npub fn format_id(id: &str) -> String {\n    id.to_uppercase()\n}\n\npub fn parse_cents(raw: &str) -> u64 {\n    raw.parse().unwrap_or(0)\n}\n",
            )
            .expect("right util");
        let mut right_map = BTreeMap::new();
        right_map.insert(
            "packages/api/src/util.rs".to_string(),
            FileEntry::regular(right_util, false),
        );
        right_map.insert(
            "packages/untouched-core/src/lib.rs".to_string(),
            base_map["packages/untouched-core/src/lib.rs"].clone(),
        );
        right_map.insert(
            "db/migrations/0001_billing.sql".to_string(),
            FileEntry::regular(
                git.write_blob(b"CREATE TABLE invoices (id INT);\n")
                    .expect("m2"),
                false,
            ),
        );
        let right_tree = git.write_tree_from_map(&right_map).expect("right tree");

        let outcome = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_tree,
            "agent-refactor",
            "Move util.rs to shared package",
            "agent-billing",
            "Add parse_cents and billing migration",
        )
        .expect("merge trees");

        assert!(outcome.conflicts.is_empty());
        assert_eq!(outcome.skipped_subtrees, 1);
        assert_eq!(outcome.renames_applied.len(), 1);
        assert_eq!(outcome.migrations_resequenced.len(), 1);
        assert_eq!(
            outcome.migrations_resequenced[0].resequenced_path,
            "db/migrations/0002_billing.sql"
        );
        assert!(
            !outcome
                .merged_files
                .contains_key("packages/api/src/util.rs")
        );
        let full_merged_tree = git
            .read_tree_to_map(&outcome.merged_tree_id)
            .expect("read full merged tree");
        assert_eq!(
            full_merged_tree["packages/untouched-core/src/lib.rs"].blob_id,
            untouched_pkg
        );
        let merged_util_bytes = git
            .read_blob(&full_merged_tree["packages/shared/src/util.rs"].blob_id)
            .expect("read merged util");
        let merged_util_str = String::from_utf8_lossy(&merged_util_bytes);
        assert!(merged_util_str.contains("pub fn parse_cents"));
    }

    #[test]
    fn binary_conflict_preserves_binary_blob_and_reports_binary_conflict_kind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let base_png = git.write_blob(b"\x89PNG\r\n\x1a\n\x00base").expect("base");
        let left_png = git.write_blob(b"\x89PNG\r\n\x1a\n\x00left").expect("left");
        let right_png = git
            .write_blob(b"\x89PNG\r\n\x1a\n\x00right")
            .expect("right");

        let mut base_map = BTreeMap::new();
        base_map.insert("logo.png".to_string(), FileEntry::regular(base_png, false));
        let mut left_map = BTreeMap::new();
        left_map.insert("logo.png".to_string(), FileEntry::regular(left_png, false));
        let mut right_map = BTreeMap::new();
        right_map.insert("logo.png".to_string(), FileEntry::regular(right_png, false));

        let base_tree = git.write_tree_from_map(&base_map).expect("base tree");
        let left_tree = git.write_tree_from_map(&left_map).expect("left tree");
        let right_tree = git.write_tree_from_map(&right_map).expect("right tree");

        let outcome = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_tree,
            "a",
            "update logo",
            "b",
            "update logo",
        )
        .expect("merge");

        assert_eq!(outcome.conflicts.len(), 1);
        assert_eq!(outcome.conflicts[0].kind, ConflictKind::Binary);
        assert_eq!(outcome.merged_files["logo.png"].blob_id, left_png);
    }

    #[test]
    fn symlinks_and_gitlinks_preserve_entry_modes_and_conflict_on_divergence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let link_target_a = git.write_blob(b"../target_a").expect("link a");
        let link_target_b = git.write_blob(b"../target_b").expect("link b");
        let txt_base = git.write_blob(b"hello\n").expect("txt base");
        let txt_left = git.write_blob(b"hello left\n").expect("txt left");
        let sub_oid_a =
            ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
        let sub_oid_b =
            ObjectId::from_hex("2222222222222222222222222222222222222222").expect("oid");

        let mut base_map = BTreeMap::new();
        base_map.insert("note.txt".to_string(), FileEntry::regular(txt_base, false));
        base_map.insert(
            "current".to_string(),
            FileEntry::from_mode(link_target_a, EntryMode::Symlink),
        );
        base_map.insert(
            "vendor/sub".to_string(),
            FileEntry::from_mode(sub_oid_a, EntryMode::Gitlink),
        );

        let mut left_map = base_map.clone();
        left_map.insert("note.txt".to_string(), FileEntry::regular(txt_left, false));

        let mut right_map = base_map.clone();
        right_map.insert(
            "current".to_string(),
            FileEntry::from_mode(link_target_b, EntryMode::Symlink),
        );
        right_map.insert(
            "vendor/sub".to_string(),
            FileEntry::from_mode(sub_oid_b, EntryMode::Gitlink),
        );

        let base_tree = git.write_tree_from_map(&base_map).expect("base tree");
        let left_tree = git.write_tree_from_map(&left_map).expect("left tree");
        let right_tree = git.write_tree_from_map(&right_map).expect("right tree");

        let outcome = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_tree,
            "a",
            "edit note",
            "b",
            "update link and sub",
        )
        .expect("merge");

        assert!(outcome.conflicts.is_empty());
        let merged_map = git
            .read_tree_to_map(&outcome.merged_tree_id)
            .expect("read merged");
        assert_eq!(merged_map["current"].entry_mode(), EntryMode::Symlink);
        assert_eq!(merged_map["current"].blob_id, link_target_b);
        assert_eq!(merged_map["vendor/sub"].entry_mode(), EntryMode::Gitlink);
        assert_eq!(merged_map["vendor/sub"].blob_id, sub_oid_b);
    }

    #[test]
    fn file_vs_directory_conflict_emits_conflict_and_writes_valid_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let base_blob = git.write_blob(b"root\n").expect("base");
        let left_file = git.write_blob(b"i am a file\n").expect("left file");
        let right_nested = git.write_blob(b"i am inside dir\n").expect("right nested");

        let mut base_map = BTreeMap::new();
        base_map.insert(
            "README.md".to_string(),
            FileEntry::regular(base_blob, false),
        );

        let mut left_map = base_map.clone();
        left_map.insert("foo".to_string(), FileEntry::regular(left_file, false));

        let mut right_map = base_map.clone();
        right_map.insert(
            "foo/bar.txt".to_string(),
            FileEntry::regular(right_nested, false),
        );

        let base_tree = git.write_tree_from_map(&base_map).expect("base");
        let left_tree = git.write_tree_from_map(&left_map).expect("left");
        let right_tree = git.write_tree_from_map(&right_map).expect("right");

        let outcome = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_tree,
            "a",
            "add foo file",
            "b",
            "add foo dir",
        )
        .expect("merge");

        assert_eq!(outcome.conflicts.len(), 1);
        assert_eq!(outcome.conflicts[0].kind, ConflictKind::FileDirectory);
        assert_eq!(outcome.conflicts[0].path, "foo");

        let nodes = git
            .read_tree_nodes(&outcome.merged_tree_id)
            .expect("read root nodes");
        assert_eq!(nodes.iter().filter(|n| n.name == "foo").count(), 1);
    }

    #[test]
    fn paired_up_down_migrations_and_subsequent_migrations_preserve_pairing_and_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let init_blob = git
            .write_blob(b"CREATE TABLE init (id INT);\n")
            .expect("init");
        let mut base_map = BTreeMap::new();
        base_map.insert(
            "db/migrations/0041_init.sql".to_string(),
            FileEntry::regular(init_blob, false),
        );

        let mut left_map = base_map.clone();
        left_map.insert(
            "db/migrations/0042_users.up.sql".to_string(),
            FileEntry::regular(
                git.write_blob(b"CREATE TABLE users (id INT);\n")
                    .expect("lu"),
                false,
            ),
        );
        left_map.insert(
            "db/migrations/0042_users.down.sql".to_string(),
            FileEntry::regular(git.write_blob(b"DROP TABLE users;\n").expect("ld"), false),
        );

        let mut right_map = base_map.clone();
        right_map.insert(
            "db/migrations/0042_billing.up.sql".to_string(),
            FileEntry::regular(
                git.write_blob(b"CREATE TABLE billing (id INT);\n")
                    .expect("ru"),
                false,
            ),
        );
        right_map.insert(
            "db/migrations/0042_billing.down.sql".to_string(),
            FileEntry::regular(git.write_blob(b"DROP TABLE billing;\n").expect("rd"), false),
        );
        right_map.insert(
            "db/migrations/0043_alter_billing.up.sql".to_string(),
            FileEntry::regular(
                git.write_blob(b"ALTER TABLE billing ADD COLUMN cents INT;\n")
                    .expect("r2u"),
                false,
            ),
        );
        right_map.insert(
            "db/migrations/0043_alter_billing.down.sql".to_string(),
            FileEntry::regular(
                git.write_blob(b"ALTER TABLE billing DROP COLUMN cents;\n")
                    .expect("r2d"),
                false,
            ),
        );

        let base_tree = git.write_tree_from_map(&base_map).expect("base");
        let left_tree = git.write_tree_from_map(&left_map).expect("left");
        let right_tree = git.write_tree_from_map(&right_map).expect("right");

        let outcome = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_tree,
            "a",
            "users migration",
            "b",
            "billing migrations",
        )
        .expect("merge");

        assert!(outcome.conflicts.is_empty());
        assert!(
            outcome
                .merged_files
                .contains_key("db/migrations/0043_billing.up.sql")
        );
        assert!(
            outcome
                .merged_files
                .contains_key("db/migrations/0043_billing.down.sql")
        );
        assert!(
            outcome
                .merged_files
                .contains_key("db/migrations/0044_alter_billing.up.sql")
        );
        assert!(
            outcome
                .merged_files
                .contains_key("db/migrations/0044_alter_billing.down.sql")
        );
    }

    #[test]
    fn rename_add_and_rename_rename_collisions_do_not_drop_edits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let orig = git
            .write_blob(b"pub fn calc(x: i32) -> i32 {\n    x + 1\n}\n")
            .expect("orig");
        let left_moved = git
            .write_blob(b"pub fn calc(x: i32) -> i32 {\n    x + 2\n}\n")
            .expect("left moved");
        let right_moved = git
            .write_blob(b"pub fn calc(x: i32) -> i32 {\n    x + 3\n}\n")
            .expect("right moved");

        let mut base_map = BTreeMap::new();
        base_map.insert("src/a.rs".to_string(), FileEntry::regular(orig, false));

        let mut left_map = BTreeMap::new();
        left_map.insert(
            "src/b.rs".to_string(),
            FileEntry::regular(left_moved, false),
        );

        let mut right_map = BTreeMap::new();
        right_map.insert(
            "src/c.rs".to_string(),
            FileEntry::regular(right_moved, false),
        );

        let base_tree = git.write_tree_from_map(&base_map).expect("base");
        let left_tree = git.write_tree_from_map(&left_map).expect("left");
        let right_tree = git.write_tree_from_map(&right_map).expect("right");

        let outcome = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_tree,
            "a",
            "rename a -> b",
            "b",
            "rename a -> c",
        )
        .expect("merge");

        assert_eq!(outcome.conflicts.len(), 1);
        assert_eq!(outcome.conflicts[0].path, "src/a.rs");
        assert!(outcome.merged_files.contains_key("src/b.rs"));
        assert!(outcome.merged_files.contains_key("src/c.rs"));

        // Rename/add collision: left renames a -> b while right edits a AND independently creates b.
        let right_edited_a = git
            .write_blob(b"pub fn calc(x: i32) -> i32 {\n    x + 99\n}\n")
            .expect("right a");
        let right_new_b = git
            .write_blob(b"pub fn brand_new() -> bool { true }\n")
            .expect("right b");
        let mut right_collision_map = BTreeMap::new();
        right_collision_map.insert(
            "src/a.rs".to_string(),
            FileEntry::regular(right_edited_a, false),
        );
        right_collision_map.insert(
            "src/b.rs".to_string(),
            FileEntry::regular(right_new_b, false),
        );
        let right_collision_tree = git
            .write_tree_from_map(&right_collision_map)
            .expect("right collision tree");

        let outcome_ra = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_collision_tree,
            "a",
            "rename a -> b",
            "b",
            "edit a and create b",
        )
        .expect("merge rename/add");

        assert!(
            !outcome_ra.conflicts.is_empty(),
            "rename/add collision must conflict instead of dropping right's edits"
        );
        assert_eq!(
            outcome_ra.merged_files.get("src/a.rs").map(|e| e.blob_id),
            Some(right_edited_a)
        );
    }

    #[test]
    fn rename_vs_delete_records_conflict_while_clean_rename_succeeds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let keep_blob = git
            .write_blob(b"pub fn keep() -> i32 { 1 }\n")
            .expect("keep");
        let keep_edited = git
            .write_blob(b"pub fn keep() -> i32 { 2 }\n")
            .expect("keep edited");
        let orig_blob = git
            .write_blob(b"pub fn alpha(x: i32) -> i32 {\n    x + 1\n}\n")
            .expect("orig");

        let mut base_map = BTreeMap::new();
        base_map.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_blob, false),
        );
        base_map.insert(
            "src/old.rs".to_string(),
            FileEntry::regular(orig_blob, false),
        );

        let mut left_renamed = BTreeMap::new();
        left_renamed.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_blob, false),
        );
        left_renamed.insert(
            "src/new.rs".to_string(),
            FileEntry::regular(orig_blob, false),
        );

        let mut right_deleted = BTreeMap::new();
        right_deleted.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_blob, false),
        );

        let base_tree = git.write_tree_from_map(&base_map).expect("base");
        let left_tree = git.write_tree_from_map(&left_renamed).expect("left");
        let right_tree = git.write_tree_from_map(&right_deleted).expect("right");

        // Left renames src/old.rs -> src/new.rs while right explicitly deletes src/old.rs.
        let outcome_lr = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_tree,
            "agent-rename",
            "rename old -> new",
            "agent-delete",
            "delete old",
        )
        .expect("merge left rename vs right delete");

        assert_eq!(outcome_lr.conflicts.len(), 1);
        assert_eq!(outcome_lr.conflicts[0].path, "src/old.rs");
        assert_eq!(
            outcome_lr.conflicts[0].symbol.as_deref(),
            Some("rename/delete")
        );
        assert!(
            outcome_lr.conflicts[0]
                .diagnostics
                .as_deref()
                .is_some_and(|d| d.contains("Rename/delete conflict"))
        );
        assert!(outcome_lr.merged_files.contains_key("src/new.rs"));

        // Symmetric case: left explicitly deletes src/old.rs while right renames src/old.rs -> src/new.rs.
        let outcome_rl = merge_trees_3way(
            &git,
            &base_tree,
            &right_tree,
            &left_tree,
            "agent-delete",
            "delete old",
            "agent-rename",
            "rename old -> new",
        )
        .expect("merge left delete vs right rename");

        assert_eq!(outcome_rl.conflicts.len(), 1);
        assert_eq!(outcome_rl.conflicts[0].path, "src/old.rs");
        assert_eq!(
            outcome_rl.conflicts[0].symbol.as_deref(),
            Some("rename/delete")
        );
        assert!(
            outcome_rl.conflicts[0]
                .diagnostics
                .as_deref()
                .is_some_and(|d| d.contains("Rename/delete conflict"))
        );
        assert!(outcome_rl.merged_files.contains_key("src/new.rs"));

        // Clean rename: right keeps src/old.rs untouched and only edits src/keep.rs.
        let mut right_untouched_old = BTreeMap::new();
        right_untouched_old.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_edited, false),
        );
        right_untouched_old.insert(
            "src/old.rs".to_string(),
            FileEntry::regular(orig_blob, false),
        );
        let right_clean_tree = git
            .write_tree_from_map(&right_untouched_old)
            .expect("right clean");

        let outcome_clean = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_clean_tree,
            "agent-rename",
            "rename old -> new",
            "agent-keep",
            "edit keep",
        )
        .expect("merge clean rename");

        assert!(outcome_clean.conflicts.is_empty());
        assert_eq!(outcome_clean.renames_applied.len(), 1);
        assert!(!outcome_clean.merged_files.contains_key("src/old.rs"));
        assert!(outcome_clean.merged_files.contains_key("src/new.rs"));
    }

    #[test]
    fn tiny_binary_placeholders_are_not_paired_as_exact_renames() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = GitBackend::init_or_open(tmp.path()).expect("init git");

        let keep_blob = git
            .write_blob(b"pub fn keep() -> i32 { 1 }\n")
            .expect("keep");
        let keep_edited = git
            .write_blob(b"pub fn keep() -> i32 { 2 }\n")
            .expect("keep edited");
        let tiny_bin = git.write_blob(b"\x00\x01\x02\x03").expect("tiny bin");
        let large_bin = git
            .write_blob(b"\x89PNG\r\n\x1a\n0123456789abcdef")
            .expect("large bin");

        let mut base_map = BTreeMap::new();
        base_map.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_blob, false),
        );
        base_map.insert(
            "assets/old.bin".to_string(),
            FileEntry::regular(tiny_bin, false),
        );

        let mut left_map = BTreeMap::new();
        left_map.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_blob, false),
        );
        left_map.insert(
            "assets/new.bin".to_string(),
            FileEntry::regular(tiny_bin, false),
        );

        let mut right_map = BTreeMap::new();
        right_map.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_edited, false),
        );
        right_map.insert(
            "assets/old.bin".to_string(),
            FileEntry::regular(tiny_bin, false),
        );

        let base_tree = git.write_tree_from_map(&base_map).expect("base");
        let left_tree = git.write_tree_from_map(&left_map).expect("left");
        let right_tree = git.write_tree_from_map(&right_map).expect("right");

        let outcome_tiny = merge_trees_3way(
            &git,
            &base_tree,
            &left_tree,
            &right_tree,
            "a",
            "replace tiny placeholder",
            "b",
            "edit keep",
        )
        .expect("merge tiny bin");

        assert!(
            outcome_tiny.renames_applied.is_empty(),
            "tiny <16-byte binary placeholder must not be paired as a rename"
        );

        let mut base_large = BTreeMap::new();
        base_large.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_blob, false),
        );
        base_large.insert(
            "assets/logo_old.png".to_string(),
            FileEntry::regular(large_bin, false),
        );

        let mut left_large = BTreeMap::new();
        left_large.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_blob, false),
        );
        left_large.insert(
            "assets/logo_new.png".to_string(),
            FileEntry::regular(large_bin, false),
        );

        let mut right_large = BTreeMap::new();
        right_large.insert(
            "src/keep.rs".to_string(),
            FileEntry::regular(keep_edited, false),
        );
        right_large.insert(
            "assets/logo_old.png".to_string(),
            FileEntry::regular(large_bin, false),
        );

        let base_large_tree = git.write_tree_from_map(&base_large).expect("base large");
        let left_large_tree = git.write_tree_from_map(&left_large).expect("left large");
        let right_large_tree = git.write_tree_from_map(&right_large).expect("right large");

        let outcome_large = merge_trees_3way(
            &git,
            &base_large_tree,
            &left_large_tree,
            &right_large_tree,
            "a",
            "move logo",
            "b",
            "edit keep",
        )
        .expect("merge large bin");

        assert_eq!(outcome_large.renames_applied.len(), 1);
        assert!(outcome_large.conflicts.is_empty());
    }
}
