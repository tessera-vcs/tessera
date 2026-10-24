//! Cross-directory and cross-package file move/rename detection using 3-gram token shingling.

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

/// Computes Dice similarity (in basis points `0..=10000`) between `old_bytes` and `new_bytes`
/// using normalized non-whitespace line and token 3-gram sets.
pub fn content_similarity_bps(old_bytes: &[u8], new_bytes: &[u8]) -> u32 {
    if old_bytes == new_bytes {
        return 10_000;
    }

    if old_bytes.is_empty() || new_bytes.is_empty() {
        return 0;
    }

    let old_shingles = extract_shingles(old_bytes);
    let new_shingles = extract_shingles(new_bytes);

    if old_shingles.is_empty() || new_shingles.is_empty() {
        return 0;
    }

    let intersection = old_shingles.intersection(&new_shingles).count();
    let total = old_shingles.len() + new_shingles.len();
    u32::try_from((intersection * 20_000) / total).unwrap_or(0)
}

/// Extracts a deterministic set of 64-bit Blake3 token 3-gram shingle hashes from `bytes`.
fn extract_shingles(bytes: &[u8]) -> BTreeSet<u64> {
    let text = String::from_utf8_lossy(bytes);
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut set = BTreeSet::new();

    if tokens.len() < 3 {
        for tok in tokens {
            set.insert(hash_token_slice(&[tok]));
        }

        return set;
    }

    for window in tokens.windows(3) {
        set.insert(hash_token_slice(window));
    }

    set
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

/// Matches deleted paths (`deleted_blobs`) against newly added paths (`added_blobs`)
/// and returns all greedy best-fit [`RenameMatch`]es with similarity `>= RENAME_SIMILARITY_THRESHOLD_BPS`.
pub fn detect_renames(
    deleted_blobs: &BTreeMap<String, Vec<u8>>,
    added_blobs: &BTreeMap<String, Vec<u8>>,
) -> Vec<RenameMatch> {
    let mut candidates: Vec<RenameMatch> = Vec::new();

    for (old_path, old_bytes) in deleted_blobs {
        for (new_path, new_bytes) in added_blobs {
            let sim = content_similarity_bps(old_bytes, new_bytes);

            if sim >= RENAME_SIMILARITY_THRESHOLD_BPS {
                candidates.push(RenameMatch {
                    old_path: old_path.clone(),
                    new_path: new_path.clone(),
                    similarity_bps: sim,
                });
            }
        }
    }

    // Sort highest similarity first, with deterministic path tie-breaking.
    candidates.sort_by(|a, b| {
        b.similarity_bps
            .cmp(&a.similarity_bps)
            .then_with(|| a.old_path.cmp(&b.old_path))
            .then_with(|| a.new_path.cmp(&b.new_path))
    });

    let mut matched_old = BTreeSet::new();
    let mut matched_new = BTreeSet::new();
    let mut results = Vec::new();

    for cand in candidates {
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
}
