//! Full 3-way repository tree merging with `O(1)` unchanged subtree skipping,
//! cross-directory rename routing, and sequential migration collision re-sequencing.

use std::collections::{BTreeMap, BTreeSet};

use core::{
    now_millis, ConflictKind, ConflictRecord, CoreResult, EntryMode, FileEntry, GitBackend,
    GitTreeNode, Merge, ObjectId,
};
use serde::{Deserialize, Serialize};

use crate::ast_merge::merge_file_3way;
use crate::drivers::{
    format_resequenced_filename, is_migration_dir, parse_numeric_prefix, MigrationResequence,
};
use crate::rename::{detect_renames, RenameMatch};

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
/// - Sequential migration collision re-sequencing;
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

    // Detect cross-package file moves on `left` and route `right`'s modifications.
    let left_renames = find_side_renames(git, &base_files, &left_files)?;
    for rm in left_renames {
        if let Some(base_old) = base_files.get(&rm.old_path) {
            if let Some(right_old) = right_files.remove(&rm.old_path) {
                // Route right's state of `old_path` to `new_path` if right didn't independently create `new_path`.
                right_files.entry(rm.new_path.clone()).or_insert(right_old);
                // Anchor base at `new_path` so the path-level pass runs a 3-way AST merge at `new_path`!
                let _ = base_old;
                renames_applied.push(rm);
            }
        }
    }

    // Detect cross-package file moves on `right` and route `left`'s modifications.
    let right_renames = find_side_renames(git, &base_files, &right_files)?;
    for rm in right_renames {
        if base_files.contains_key(&rm.old_path) {
            if let Some(left_old) = left_files.remove(&rm.old_path) {
                left_files.entry(rm.new_path.clone()).or_insert(left_old);
                renames_applied.push(rm);
            }
        }
    }

    // Build an effective base map that includes moved base entries at their new destination paths.
    let mut effective_base = base_files.clone();
    for rm in &renames_applied {
        if let Some(orig_entry) = effective_base.remove(&rm.old_path) {
            effective_base.insert(rm.new_path.clone(), orig_entry);
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
    let mut conflicts = Vec::new();

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

        // Unresolved algebraic entry: either both modified/added `path` with different blobs,
        // or one deleted while the other modified.
        match (&b_entry, &l_entry, &r_entry) {
            (_, Some(le), Some(re)) => {
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
                    merged_files.insert(
                        path,
                        FileEntry {
                            blob_id: *resolved_blob_id,
                            executable: merged_exec,
                        },
                    );
                    continue;
                }

                let base_bytes = match &b_entry {
                    Some(be) => git.read_blob(&be.blob_id)?,
                    None => Vec::new(),
                };
                let left_bytes = git.read_blob(&le.blob_id)?;
                let right_bytes = git.read_blob(&re.blob_id)?;

                let base_str = String::from_utf8_lossy(&base_bytes);
                let left_str = String::from_utf8_lossy(&left_bytes);
                let right_str = String::from_utf8_lossy(&right_bytes);

                let ast_out = merge_file_3way(
                    &path,
                    &base_str,
                    &left_str,
                    &right_str,
                    &left_label,
                    &right_label,
                );
                let merged_blob_id = git.write_blob(ast_out.content.as_bytes())?;
                merged_files.insert(
                    path.clone(),
                    FileEntry {
                        blob_id: merged_blob_id,
                        executable: merged_exec,
                    },
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
                // Delete/modify conflict: preserve the modified file in the tree and record a DeleteModify conflict.
                merged_files.insert(path.clone(), le.clone());
                let base_bytes = git.read_blob(&be.blob_id)?;
                let mod_bytes = git.read_blob(&le.blob_id)?;
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

                conflicts.push(ConflictRecord {
                    id: cid,
                    path,
                    symbol: None,
                    kind: ConflictKind::DeleteModify,
                    merge: blob_merge,
                    agent_intents: intents,
                    base_snippet: String::from_utf8_lossy(&base_bytes)
                        .lines()
                        .take(15)
                        .collect::<Vec<_>>()
                        .join("\n"),
                    left_snippet: if l_entry.is_some() {
                        String::from_utf8_lossy(&mod_bytes)
                            .lines()
                            .take(15)
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        "<deleted>".to_string()
                    },
                    right_snippet: if r_entry.is_some() {
                        String::from_utf8_lossy(&mod_bytes)
                            .lines()
                            .take(15)
                            .collect::<Vec<_>>()
                            .join("\n")
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
            let b_sub = bn
                .filter(|n| n.mode == EntryMode::Directory)
                .map(|n| &n.id);
            let l_sub = ln
                .filter(|n| n.mode == EntryMode::Directory)
                .map(|n| &n.id);
            let r_sub = rn
                .filter(|n| n.mode == EntryMode::Directory)
                .map(|n| &n.id);

            // Handle file-vs-directory replacements by also recording any non-directory entries.
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

/// Inserts `node_opt` into `out` at `rel_path` if it is a file rather than a directory.
fn record_file_node_if_present(
    node_opt: Option<&GitTreeNode>,
    rel_path: &str,
    out: &mut BTreeMap<String, FileEntry>,
) {
    if let Some(node) = node_opt {
        if node.mode != EntryMode::Directory {
            out.insert(
                rel_path.to_string(),
                FileEntry {
                    blob_id: node.id,
                    executable: node.mode == EntryMode::ExecutableFile,
                },
            );
        }
    }
}

/// Writes a hierarchical Git tree combining modified `files` and unchanged `passthrough_subtrees`.
fn write_tree_from_map_and_subtrees(
    git: &GitBackend,
    files: &BTreeMap<String, FileEntry>,
    passthrough_subtrees: &BTreeMap<String, ObjectId>,
) -> CoreResult<ObjectId> {
    let mut direct_nodes: Vec<GitTreeNode> = Vec::new();
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
            let mode = if entry.executable {
                EntryMode::ExecutableFile
            } else {
                EntryMode::NormalFile
            };
            direct_nodes.push(GitTreeNode {
                mode,
                name: trimmed.to_string(),
                id: entry.blob_id,
            });
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
            direct_nodes.push(GitTreeNode {
                mode: EntryMode::Directory,
                name: trimmed.to_string(),
                id: *tree_id,
            });
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
        direct_nodes.push(GitTreeNode {
            mode: EntryMode::Directory,
            name: dir_name,
            id: sub_tree_id,
        });
    }

    git.write_tree_nodes(direct_nodes)
}

/// Identifies files moved or renamed between `base_files` and `side_files`.
fn find_side_renames(
    git: &GitBackend,
    base_files: &BTreeMap<String, FileEntry>,
    side_files: &BTreeMap<String, FileEntry>,
) -> CoreResult<Vec<RenameMatch>> {
    let mut deleted = BTreeMap::new();
    for (path, entry) in base_files {
        if !side_files.contains_key(path) {
            deleted.insert(path.clone(), git.read_blob(&entry.blob_id)?);
        }
    }

    if deleted.is_empty() {
        return Ok(Vec::new());
    }

    let mut added = BTreeMap::new();
    for (path, entry) in side_files {
        if !base_files.contains_key(path) {
            added.insert(path.clone(), git.read_blob(&entry.blob_id)?);
        }
    }

    if added.is_empty() {
        return Ok(Vec::new());
    }

    Ok(detect_renames(&deleted, &added))
}

/// Detects colliding sequential migration files added in both `left_files` and `right_files`
/// and renumbers `right_files` entries in place.
fn resequence_migrations_in_place(
    base_files: &BTreeMap<String, FileEntry>,
    left_files: &BTreeMap<String, FileEntry>,
    right_files: &mut BTreeMap<String, FileEntry>,
    resequenced: &mut Vec<MigrationResequence>,
) {
    // Track highest numeric prefix per migration directory across base + left + right.
    let mut dir_max_num: BTreeMap<String, u64> = BTreeMap::new();
    let mut left_added_nums: BTreeSet<(String, u64)> = BTreeSet::new();

    for (path, _) in base_files
        .iter()
        .chain(left_files.iter())
        .chain(right_files.iter())
    {
        if let Some((dir, file_name)) = path.rsplit_once('/') {
            if is_migration_dir(dir) {
                if let Some((num, _, _)) = parse_numeric_prefix(file_name) {
                    let entry = dir_max_num.entry(dir.to_string()).or_insert(0);
                    *entry = (*entry).max(num);
                }
            }
        }
    }

    for path in left_files.keys() {
        if !base_files.contains_key(path) {
            if let Some((dir, file_name)) = path.rsplit_once('/') {
                if is_migration_dir(dir) {
                    if let Some((num, _, _)) = parse_numeric_prefix(file_name) {
                        left_added_nums.insert((dir.to_string(), num));
                    }
                }
            }
        }
    }

    let right_paths: Vec<String> = right_files.keys().cloned().collect();
    for path in right_paths {
        if base_files.contains_key(&path) {
            continue;
        }

        // Skip if identical blob was added on both sides.
        if left_files.get(&path) == right_files.get(&path) {
            continue;
        }

        if let Some((dir, file_name)) = path.rsplit_once('/') {
            if is_migration_dir(dir) {
                if let Some((num, width, suffix)) = parse_numeric_prefix(file_name) {
                    if left_added_nums.contains(&(dir.to_string(), num)) {
                        let next_num = dir_max_num
                            .get(dir)
                            .copied()
                            .unwrap_or(num)
                            .saturating_add(1);
                        dir_max_num.insert(dir.to_string(), next_num);
                        let new_file_name = format_resequenced_filename(next_num, width, suffix);
                        let new_path = format!("{dir}/{new_file_name}");
                        if let Some(entry) = right_files.remove(&path) {
                            right_files.insert(new_path.clone(), entry);
                            resequenced.push(MigrationResequence {
                                original_path: path,
                                resequenced_path: new_path,
                            });
                        }
                    }
                }
            }
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
            FileEntry {
                blob_id: base_util,
                executable: false,
            },
        );
        base_map.insert(
            "packages/untouched-core/src/lib.rs".to_string(),
            FileEntry {
                blob_id: untouched_pkg,
                executable: false,
            },
        );
        let base_tree = git.write_tree_from_map(&base_map).expect("base tree");

        // Agent A moves `packages/api/src/util.rs` -> `packages/shared/src/util.rs` and adds migration 0001_users.sql
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
            FileEntry {
                blob_id: git.write_blob(b"CREATE TABLE users (id INT);\n").expect("m1"),
                executable: false,
            },
        );
        let left_tree = git.write_tree_from_map(&left_map).expect("left tree");

        // Agent B edits `packages/api/src/util.rs` (adding a new function) and also creates `db/migrations/0001_billing.sql`!
        let right_util = git
            .write_blob(
                b"pub fn validate_id(id: &str) -> bool {\n    !id.is_empty()\n}\n\npub fn format_id(id: &str) -> String {\n    id.to_uppercase()\n}\n\npub fn parse_cents(raw: &str) -> u64 {\n    raw.parse().unwrap_or(0)\n}\n",
            )
            .expect("right util");
        let mut right_map = BTreeMap::new();
        right_map.insert(
            "packages/api/src/util.rs".to_string(),
            FileEntry {
                blob_id: right_util,
                executable: false,
            },
        );
        right_map.insert(
            "packages/untouched-core/src/lib.rs".to_string(),
            base_map["packages/untouched-core/src/lib.rs"].clone(),
        );
        right_map.insert(
            "db/migrations/0001_billing.sql".to_string(),
            FileEntry {
                blob_id: git.write_blob(b"CREATE TABLE invoices (id INT);\n").expect("m2"),
                executable: false,
            },
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
        assert!(!outcome.merged_files.contains_key("packages/api/src/util.rs"));
        let full_merged_tree = git.read_tree_to_map(&outcome.merged_tree_id).expect("read full merged tree");
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
}
