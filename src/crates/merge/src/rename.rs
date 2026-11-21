//! Cross-directory and cross-package file move/rename detection using exact-hash matching
//! and precomputed 3-gram token shingling.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Detected file move or rename across directories or packages in a codebase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameMatch {
    /// Original path deleted on one side of the merge.
    pub old_path: String,
    /// New destination path added on the same side of the merge.
    pub new_path: String,
    /// Similarity score in basis points (`0..=10000`, where `7000` = `70.0%`).
    pub similarity_bps: u32,
}

/// Minimum similarity threshold in basis points (`70.0%`) required to classify a
/// `(deleted_path, added_path)` pair as a file rename/move.
pub const RENAME_SIMILARITY_THRESHOLD_BPS: u32 = 7000;

/// Default maximum number of unmatched deleted or added files considered for quadratic
/// fuzzy shingle similarity matching before capping to avoid hangs on huge directory moves.
pub const DEFAULT_MAX_RENAME_CANDIDATES: usize = 2_000;

/// Minimum byte length required for exact-byte binary rename matching so trivial placeholders
/// are not falsely paired across unrelated paths.
const MIN_EXACT_BINARY_BYTES: usize = 16;

/// Precomputed content profile for a deleted or added file candidate.
struct FileProfile<'a> {
    /// Repository-relative path.
    path: &'a str,
    /// Raw file bytes.
    bytes: &'a [u8],
    /// Blake3 digest for `O(1)` exact-content pairing.
    hash: [u8; 32],
    /// Sorted deduplicated 3-gram token shingle hashes (`None` for binary, empty, or tiny `<3`-token files).
    shingles: Option<Vec<u64>>,
    /// Whether this file is eligible for exact-byte rename matching.
    exact_eligible: bool,
}

/// Computes Dice similarity (in basis points `0..=10000`) between `old_bytes` and `new_bytes`
/// using normalized non-whitespace token 3-gram sets.
///
/// Returns `0` for empty inputs, binary inputs, inputs with fewer than 3 tokens, or pairs
/// whose byte sizes fail the 50% length ratio bound.
pub fn content_similarity_bps(old_bytes: &[u8], new_bytes: &[u8]) -> u32 {
    if old_bytes.is_empty() || new_bytes.is_empty() {
        return 0;
    }

    if old_bytes == new_bytes {
        return 10_000;
    }

    if !passes_size_ratio(old_bytes.len(), new_bytes.len()) {
        return 0;
    }

    let Some(old_shingles) = extract_shingles(old_bytes) else {
        return 0;
    };

    let Some(new_shingles) = extract_shingles(new_bytes) else {
        return 0;
    };

    shingle_dice_bps(&old_shingles, &new_shingles)
}

/// Checks whether two byte lengths are within the size-ratio window (`min * 10 >= max * 5`)
/// required for a 70% token-shingle similarity match to be plausible.
fn passes_size_ratio(len_a: usize, len_b: usize) -> bool {
    let min_len = len_a.min(len_b) as u128;
    let max_len = len_a.max(len_b) as u128;
    max_len > 0 && min_len * 10 >= max_len * 5
}

/// Returns `true` if `bytes` contains NUL bytes or invalid UTF-8 sequences.
fn is_binary_bytes(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

/// Extracts a sorted deduplicated list of 64-bit Blake3 token 3-gram shingle hashes from `bytes`.
///
/// Returns `None` for binary files, empty files, or tiny files with fewer than 3 whitespace tokens
/// to guard against boilerplate false matches.
fn extract_shingles(bytes: &[u8]) -> Option<Vec<u64>> {
    if bytes.is_empty() || is_binary_bytes(bytes) {
        return None;
    }

    let text = std::str::from_utf8(bytes).ok()?;
    let tokens: Vec<&str> = text.split_whitespace().collect();

    if tokens.len() < 3 {
        return None;
    }

    let mut hashes = Vec::with_capacity(tokens.len().saturating_sub(2));

    for window in tokens.windows(3) {
        hashes.push(hash_token_slice(window));
    }

    hashes.sort_unstable();
    hashes.dedup();
    Some(hashes)
}

/// Computes Dice similarity in basis points (`0..=10000`) between two sorted deduplicated
/// shingle hash vectors via a linear two-pointer intersection scan.
fn shingle_dice_bps(a: &[u64], b: &[u64]) -> u32 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }

    let mut i = 0usize;
    let mut j = 0usize;
    let mut intersection = 0usize;

    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                j += 1;
            }
        }
    }

    let total = a.len() + b.len();
    u32::try_from((intersection * 20_000) / total).unwrap_or(0)
}

/// Computes a 64-bit little-endian Blake3 digest over a slice of whitespace-separated tokens.
fn hash_token_slice(tokens: &[&str]) -> u64 {
    let mut hasher = blake3::Hasher::new();

    for t in tokens {
        hasher.update(t.as_bytes());
        hasher.update(b" ");
    }

    let out = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&out.as_bytes()[..8]);
    u64::from_le_bytes(bytes)
}

/// Returns the final filename segment of a repository-relative path.
fn path_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Returns the parent directory prefix of a repository-relative path (empty string at repo root).
fn path_parent(path: &str) -> &str {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("")
}

/// Orders rename candidates by highest similarity first, then preferring matching basename,
/// then matching parent directory, then deterministic lexicographic `(old_path, new_path)`.
fn compare_rename_candidates(a: &RenameMatch, b: &RenameMatch) -> std::cmp::Ordering {
    let a_same_base = path_basename(&a.old_path) == path_basename(&a.new_path);
    let b_same_base = path_basename(&b.old_path) == path_basename(&b.new_path);
    let a_same_dir = path_parent(&a.old_path) == path_parent(&a.new_path);
    let b_same_dir = path_parent(&b.old_path) == path_parent(&b.new_path);

    b.similarity_bps
        .cmp(&a.similarity_bps)
        .then_with(|| b_same_base.cmp(&a_same_base))
        .then_with(|| b_same_dir.cmp(&a_same_dir))
        .then_with(|| a.old_path.cmp(&b.old_path))
        .then_with(|| a.new_path.cmp(&b.new_path))
}

/// Builds a [`FileProfile`] for a single path and its raw bytes, computing its Blake3 hash
/// and optional token shingle set once.
fn build_profile<'a>(path: &'a str, bytes: &'a [u8]) -> FileProfile<'a> {
    let hash = *blake3::hash(bytes).as_bytes();
    let binary = is_binary_bytes(bytes);
    let shingles = if binary {
        None
    } else {
        extract_shingles(bytes)
    };
    let exact_eligible = if bytes.is_empty() {
        false
    } else if binary {
        bytes.len() >= MIN_EXACT_BINARY_BYTES
    } else {
        shingles.is_some()
    };

    FileProfile {
        path,
        bytes,
        hash,
        shingles,
        exact_eligible,
    }
}

/// Matches deleted paths (`deleted_blobs`) against newly added paths (`added_blobs`)
/// and returns all greedy best-fit [`RenameMatch`]es with similarity `>= RENAME_SIMILARITY_THRESHOLD_BPS`.
///
/// Uses [`DEFAULT_MAX_RENAME_CANDIDATES`] as the fuzzy shingle matching cap while always
/// performing linear-time exact-byte rename matching.
pub fn detect_renames(
    deleted_blobs: &BTreeMap<String, Vec<u8>>,
    added_blobs: &BTreeMap<String, Vec<u8>>,
) -> Vec<RenameMatch> {
    detect_renames_with_limit(deleted_blobs, added_blobs, DEFAULT_MAX_RENAME_CANDIDATES)
}

/// Matches deleted paths (`deleted_blobs`) against newly added paths (`added_blobs`) with an
/// explicit `max_fuzzy_candidates` cap on the fuzzy shingling phase.
pub fn detect_renames_with_limit(
    deleted_blobs: &BTreeMap<String, Vec<u8>>,
    added_blobs: &BTreeMap<String, Vec<u8>>,
    max_fuzzy_candidates: usize,
) -> Vec<RenameMatch> {
    if deleted_blobs.is_empty() || added_blobs.is_empty() {
        return Vec::new();
    }

    let deleted_profiles: Vec<FileProfile<'_>> = deleted_blobs
        .iter()
        .map(|(p, b)| build_profile(p.as_str(), b.as_slice()))
        .collect();
    let added_profiles: Vec<FileProfile<'_>> = added_blobs
        .iter()
        .map(|(p, b)| build_profile(p.as_str(), b.as_slice()))
        .collect();

    let mut matched_old: BTreeSet<String> = BTreeSet::new();
    let mut matched_new: BTreeSet<String> = BTreeSet::new();
    let mut results: Vec<RenameMatch> = Vec::new();

    let mut deleted_by_hash: BTreeMap<[u8; 32], Vec<&FileProfile<'_>>> = BTreeMap::new();
    let mut added_by_hash: BTreeMap<[u8; 32], Vec<&FileProfile<'_>>> = BTreeMap::new();

    for prof in &deleted_profiles {
        if prof.exact_eligible {
            deleted_by_hash.entry(prof.hash).or_default().push(prof);
        }
    }

    for prof in &added_profiles {
        if prof.exact_eligible {
            added_by_hash.entry(prof.hash).or_default().push(prof);
        }
    }

    let mut exact_candidates: Vec<RenameMatch> = Vec::new();

    for (hash, del_list) in &deleted_by_hash {
        let Some(add_list) = added_by_hash.get(hash) else {
            continue;
        };

        for d in del_list {
            for a in add_list {
                if d.bytes == a.bytes {
                    exact_candidates.push(RenameMatch {
                        old_path: d.path.to_string(),
                        new_path: a.path.to_string(),
                        similarity_bps: 10_000,
                    });
                }
            }
        }
    }

    exact_candidates.sort_by(compare_rename_candidates);

    for cand in exact_candidates {
        if matched_old.contains(&cand.old_path) || matched_new.contains(&cand.new_path) {
            continue;
        }

        matched_old.insert(cand.old_path.clone());
        matched_new.insert(cand.new_path.clone());
        results.push(cand);
    }

    let rem_deleted: Vec<&FileProfile<'_>> = deleted_profiles
        .iter()
        .filter(|p| p.shingles.is_some() && !matched_old.contains(p.path))
        .collect();
    let rem_added: Vec<&FileProfile<'_>> = added_profiles
        .iter()
        .filter(|p| p.shingles.is_some() && !matched_new.contains(p.path))
        .collect();

    if rem_deleted.is_empty()
        || rem_added.is_empty()
        || rem_deleted.len() > max_fuzzy_candidates
        || rem_added.len() > max_fuzzy_candidates
    {
        return results;
    }

    let mut fuzzy_candidates: Vec<RenameMatch> = Vec::new();

    for old_prof in &rem_deleted {
        let Some(old_shingles) = old_prof.shingles.as_deref() else {
            continue;
        };

        for new_prof in &rem_added {
            if !passes_size_ratio(old_prof.bytes.len(), new_prof.bytes.len()) {
                continue;
            }

            let Some(new_shingles) = new_prof.shingles.as_deref() else {
                continue;
            };

            let sim = shingle_dice_bps(old_shingles, new_shingles);

            if sim >= RENAME_SIMILARITY_THRESHOLD_BPS {
                fuzzy_candidates.push(RenameMatch {
                    old_path: old_prof.path.to_string(),
                    new_path: new_prof.path.to_string(),
                    similarity_bps: sim,
                });
            }
        }
    }

    fuzzy_candidates.sort_by(compare_rename_candidates);

    for cand in fuzzy_candidates {
        if matched_old.contains(&cand.old_path) || matched_new.contains(&cand.new_path) {
            continue;
        }

        matched_old.insert(cand.old_path.clone());
        matched_new.insert(cand.new_path.clone());
        results.push(cand);
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cross_package_file_move_with_edits() {
        let orig = b"pub fn verify_token(token: &str) -> bool {\n    !token.is_empty() && token.starts_with(\"jwt_\")\n}\n\npub fn hash_secret(s: &str) -> usize {\n    s.len() * 31\n}\n";
        let moved = b"pub fn verify_token(token: &str) -> bool {\n    !token.is_empty() && token.starts_with(\"jwt_\")\n}\n\npub fn hash_secret(s: &str) -> usize {\n    s.len() * 31 + 7\n}\n";

        let mut deleted = BTreeMap::new();
        deleted.insert("packages/api/src/auth_helper.rs".to_string(), orig.to_vec());

        let mut added = BTreeMap::new();
        added.insert(
            "packages/shared/src/auth_helper.rs".to_string(),
            moved.to_vec(),
        );

        let renames = detect_renames(&deleted, &added);
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].old_path, "packages/api/src/auth_helper.rs");
        assert_eq!(renames[0].new_path, "packages/shared/src/auth_helper.rs");
        assert!(renames[0].similarity_bps >= RENAME_SIMILARITY_THRESHOLD_BPS);
    }

    #[test]
    fn tie_breaking_prefers_matching_basename_and_parent_directory() {
        let content =
            b"pub fn shared_helper(a: u64, b: u64) -> u64 {\n    a.saturating_add(b)\n}\n";

        let mut deleted = BTreeMap::new();
        deleted.insert("src/billing/util.rs".to_string(), content.to_vec());
        deleted.insert("src/auth/util.rs".to_string(), content.to_vec());

        let mut added = BTreeMap::new();
        added.insert("src/auth/renamed_helper.rs".to_string(), content.to_vec());
        added.insert("packages/common/util.rs".to_string(), content.to_vec());

        let renames = detect_renames(&deleted, &added);
        assert_eq!(renames.len(), 2);

        let map: BTreeMap<&str, &str> = renames
            .iter()
            .map(|r| (r.old_path.as_str(), r.new_path.as_str()))
            .collect();
        assert_eq!(map["src/auth/util.rs"], "packages/common/util.rs");
        assert_eq!(map["src/billing/util.rs"], "src/auth/renamed_helper.rs");

        let mut del_dir = BTreeMap::new();
        del_dir.insert("src/z_dir/old.rs".to_string(), content.to_vec());

        let mut add_dir = BTreeMap::new();
        add_dir.insert("src/a_dir/new_a.rs".to_string(), content.to_vec());
        add_dir.insert("src/z_dir/new_z.rs".to_string(), content.to_vec());

        let dir_renames = detect_renames(&del_dir, &add_dir);
        assert_eq!(dir_renames.len(), 1);
        assert_eq!(dir_renames[0].new_path, "src/z_dir/new_z.rs");
    }

    #[test]
    fn guards_against_empty_tiny_boilerplate_and_binary_fuzzy_false_matches() {
        let mut deleted = BTreeMap::new();
        deleted.insert("a/__init__.py".to_string(), Vec::new());
        deleted.insert("a/stub.ts".to_string(), b"export {};\n".to_vec());
        deleted.insert(
            "a/img.bin".to_string(),
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17],
        );

        let mut added = BTreeMap::new();
        added.insert("b/__init__.py".to_string(), Vec::new());
        added.insert("b/stub.ts".to_string(), b"export {};\n".to_vec());
        added.insert(
            "b/img.bin".to_string(),
            vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 99],
        );

        let renames = detect_renames(&deleted, &added);
        assert!(renames.is_empty(), "unexpected renames: {renames:?}");
    }

    #[test]
    fn fuzzy_candidate_cap_preserves_exact_matches_and_skips_quadratic_explosion() {
        let mut deleted = BTreeMap::new();
        let mut added = BTreeMap::new();

        let exact_body = b"pub fn exact_move(x: i32) -> i32 { x + 1 }\n";
        deleted.insert("old/exact.rs".to_string(), exact_body.to_vec());
        added.insert("new/exact.rs".to_string(), exact_body.to_vec());

        for i in 0..5 {
            deleted.insert(
                format!("old/file_{i}.rs"),
                format!("pub fn func_{i}(x: i32) -> i32 {{ x + {i} + 1 }}\n").into_bytes(),
            );
            added.insert(
                format!("new/file_{i}.rs"),
                format!("pub fn func_{i}(x: i32) -> i32 {{ x + {i} + 2 }}\n").into_bytes(),
            );
        }

        let renames = detect_renames_with_limit(&deleted, &added, 2);
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].old_path, "old/exact.rs");
        assert_eq!(renames[0].new_path, "new/exact.rs");
        assert_eq!(renames[0].similarity_bps, 10_000);
    }
}
