//! Histogram line-level diff3 merge, sub-line token reconciliation, and structural block/skeleton unioning.

use imara_diff::{Algorithm, BasicLineDiffPrinter, Diff, InternedInput, UnifiedDiffConfig};
use serde::{Deserialize, Serialize};

/// Outcome of a 3-way textual merge over lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMergeOutcome {
    /// Merged text content (including conflict markers if `has_conflicts` is `true`).
    pub content: String,
    /// `true` if any overlapping line hunks could not be reconciled automatically.
    pub has_conflicts: bool,
    /// Number of unresolved conflict blocks.
    pub conflict_count: usize,
}

/// Contiguous line replacement hunk relative to a base document's 0-based line indices.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LineEdit {
    /// Start line index in `base` (inclusive, 0-based).
    base_start: usize,
    /// End line index in `base` (exclusive, 0-based).
    base_end: usize,
    /// Replacement lines from the target document.
    replacement: Vec<String>,
}

/// Computes the list of line replacement hunks transforming `base_text` into `target_text`
/// using `imara-diff`'s Histogram algorithm.
fn compute_line_edits(base_text: &str, target_text: &str) -> Vec<LineEdit> {
    let input = InternedInput::new(base_text, target_text);
    let target_lines: Vec<&str> = target_text.lines().collect();
    let diff = Diff::compute(Algorithm::Histogram, &input);

    diff.hunks()
        .map(|hunk| {
            let base_start = hunk.before.start as usize;
            let base_end = hunk.before.end as usize;
            let after_start = hunk.after.start as usize;
            let after_end = hunk.after.end as usize;

            let replacement = target_lines[after_start..after_end]
                .iter()
                .map(|s| (*s).to_string())
                .collect();

            LineEdit {
                base_start,
                base_end,
                replacement,
            }
        })
        .collect()
}

/// Performs a 3-way line-level merge (diff3) between `base`, `left`, and `right` using
/// Histogram diff alignment.
///
/// Non-overlapping line hunks and identical edits on both sides merge cleanly. Overlapping
/// incompatible line hunks emit intent-annotated conflict markers.
pub fn merge_text_3way(
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> TextMergeOutcome {
    if left == right || right == base {
        return TextMergeOutcome {
            content: left.to_string(),
            has_conflicts: false,
            conflict_count: 0,
        };
    }

    if left == base {
        return TextMergeOutcome {
            content: right.to_string(),
            has_conflicts: false,
            conflict_count: 0,
        };
    }

    let base_lines: Vec<&str> = base.lines().collect();
    let left_edits = compute_line_edits(base, left);
    let right_edits = compute_line_edits(base, right);

    let mut out_lines: Vec<String> = Vec::new();
    let mut conflict_count = 0usize;
    let mut base_pos = 0usize;
    let mut l_idx = 0usize;
    let mut r_idx = 0usize;

    while l_idx < left_edits.len() || r_idx < right_edits.len() {
        let next_l = left_edits.get(l_idx);
        let next_r = right_edits.get(r_idx);

        match (next_l, next_r) {
            (Some(le), None) => {
                while base_pos < le.base_start && base_pos < base_lines.len() {
                    out_lines.push(base_lines[base_pos].to_string());
                    base_pos += 1;
                }

                out_lines.extend(le.replacement.iter().cloned());
                base_pos = le.base_end;
                l_idx += 1;
            }
            (None, Some(re)) => {
                while base_pos < re.base_start && base_pos < base_lines.len() {
                    out_lines.push(base_lines[base_pos].to_string());
                    base_pos += 1;
                }

                out_lines.extend(re.replacement.iter().cloned());
                base_pos = re.base_end;
                r_idx += 1;
            }
            (Some(le), Some(re)) => {
                let l_pure_insert = le.base_start == le.base_end;
                let r_pure_insert = re.base_start == re.base_end;
                let boundary_collision = (le.base_end == re.base_start
                    || re.base_end == le.base_start)
                    && (l_pure_insert != r_pure_insert)
                    && chunks_share_structural_key(&le.replacement, &re.replacement);

                if le.base_end < re.base_start
                    || (le.base_end == re.base_start
                        && !(l_pure_insert && r_pure_insert)
                        && !boundary_collision)
                {
                    while base_pos < le.base_start && base_pos < base_lines.len() {
                        out_lines.push(base_lines[base_pos].to_string());
                        base_pos += 1;
                    }

                    out_lines.extend(le.replacement.iter().cloned());
                    base_pos = le.base_end;
                    l_idx += 1;
                } else if re.base_end < le.base_start
                    || (re.base_end == le.base_start
                        && !(l_pure_insert && r_pure_insert)
                        && !boundary_collision)
                {
                    while base_pos < re.base_start && base_pos < base_lines.len() {
                        out_lines.push(base_lines[base_pos].to_string());
                        base_pos += 1;
                    }

                    out_lines.extend(re.replacement.iter().cloned());
                    base_pos = re.base_end;
                    r_idx += 1;
                } else if le.base_start == re.base_start
                    && le.base_end == re.base_end
                    && le.replacement == re.replacement
                {
                    while base_pos < le.base_start && base_pos < base_lines.len() {
                        out_lines.push(base_lines[base_pos].to_string());
                        base_pos += 1;
                    }

                    out_lines.extend(le.replacement.iter().cloned());
                    base_pos = le.base_end;
                    l_idx += 1;
                    r_idx += 1;
                } else {
                    let region_start = le.base_start.min(re.base_start);
                    let region_end = le.base_end.max(re.base_end);

                    while base_pos < region_start && base_pos < base_lines.len() {
                        out_lines.push(base_lines[base_pos].to_string());
                        base_pos += 1;
                    }

                    let base_chunk: Vec<&str> = base_lines[region_start..region_end].to_vec();

                    let mut left_chunk: Vec<String> = Vec::new();
                    for line in &base_lines[region_start..le.base_start] {
                        left_chunk.push((*line).to_string());
                    }
                    left_chunk.extend(le.replacement.iter().cloned());
                    for line in &base_lines[le.base_end..region_end] {
                        left_chunk.push((*line).to_string());
                    }

                    let mut right_chunk: Vec<String> = Vec::new();
                    for line in &base_lines[region_start..re.base_start] {
                        right_chunk.push((*line).to_string());
                    }
                    right_chunk.extend(re.replacement.iter().cloned());
                    for line in &base_lines[re.base_end..region_end] {
                        right_chunk.push((*line).to_string());
                    }

                    if let Some(reconciled) =
                        try_reconcile_overlapping_hunk(&base_chunk, &left_chunk, &right_chunk)
                    {
                        out_lines.extend(reconciled);
                    } else {
                        let (l_conf, r_conf, clean_tail) =
                            split_disjoint_trailing_blocks(&left_chunk, &right_chunk);
                        conflict_count += 1;
                        out_lines.push(format!("<<<<<<< {left_label}"));
                        out_lines.extend(l_conf);
                        out_lines.push("=======".to_string());
                        out_lines.extend(r_conf);
                        out_lines.push(format!(">>>>>>> {right_label}"));
                        out_lines.extend(clean_tail);
                    }

                    base_pos = region_end;
                    l_idx += 1;
                    r_idx += 1;
                }
            }
            (None, None) => break,
        }
    }

    while base_pos < base_lines.len() {
        out_lines.push(base_lines[base_pos].to_string());
        base_pos += 1;
    }

    let mut content = out_lines.join("\n");
    if (left.ends_with('\n') || right.ends_with('\n')) && !content.is_empty() {
        content.push('\n');
    }

    TextMergeOutcome {
        content,
        has_conflicts: conflict_count > 0,
        conflict_count,
    }
}

/// Attempts to reconcile an overlapping line hunk by stripping shared unchanged prefixes/suffixes
/// before delegating to [`try_reconcile_overlapping_hunk_inner`].
fn try_reconcile_overlapping_hunk(
    base_chunk: &[&str],
    left_chunk: &[String],
    right_chunk: &[String],
) -> Option<Vec<String>> {
    let mut prefix_len = 0usize;

    while prefix_len < base_chunk.len()
        && prefix_len < left_chunk.len()
        && prefix_len < right_chunk.len()
        && base_chunk[prefix_len] == left_chunk[prefix_len]
        && base_chunk[prefix_len] == right_chunk[prefix_len]
    {
        prefix_len += 1;
    }

    let mut suffix_len = 0usize;

    while suffix_len < (base_chunk.len() - prefix_len)
        && suffix_len < (left_chunk.len() - prefix_len)
        && suffix_len < (right_chunk.len() - prefix_len)
        && base_chunk[base_chunk.len() - 1 - suffix_len]
            == left_chunk[left_chunk.len() - 1 - suffix_len]
        && base_chunk[base_chunk.len() - 1 - suffix_len]
            == right_chunk[right_chunk.len() - 1 - suffix_len]
    {
        suffix_len += 1;
    }

    if prefix_len > 0 || suffix_len > 0 {
        let b_inner = &base_chunk[prefix_len..(base_chunk.len() - suffix_len)];
        let l_inner = &left_chunk[prefix_len..(left_chunk.len() - suffix_len)];
        let r_inner = &right_chunk[prefix_len..(right_chunk.len() - suffix_len)];

        if let Some(mid) = try_reconcile_overlapping_hunk_inner(b_inner, l_inner, r_inner) {
            let mut out = Vec::with_capacity(prefix_len + mid.len() + suffix_len);
            out.extend(left_chunk[..prefix_len].iter().cloned());
            out.extend(mid);
            out.extend(left_chunk[(left_chunk.len() - suffix_len)..].iter().cloned());
            return Some(out);
        }
    }

    if base_chunk.is_empty() && left_chunk.len() >= 3 && right_chunk.len() >= 3 {
        let mut l_pre = 0usize;

        while l_pre < left_chunk.len()
            && l_pre < right_chunk.len()
            && left_chunk[l_pre] == right_chunk[l_pre]
            && (left_chunk[l_pre].trim().is_empty() || left_chunk[l_pre].trim().starts_with('<'))
        {
            l_pre += 1;
        }

        let mut l_suf = 0usize;

        while l_suf < (left_chunk.len() - l_pre)
            && l_suf < (right_chunk.len() - l_pre)
            && left_chunk[left_chunk.len() - 1 - l_suf]
                == right_chunk[right_chunk.len() - 1 - l_suf]
            && (left_chunk[left_chunk.len() - 1 - l_suf].trim().is_empty()
                || left_chunk[left_chunk.len() - 1 - l_suf]
                    .trim()
                    .starts_with("</"))
        {
            l_suf += 1;
        }

        if l_pre > 0 && l_suf > 0 {
            let l_inner = &left_chunk[l_pre..(left_chunk.len() - l_suf)];
            let r_inner = &right_chunk[l_pre..(right_chunk.len() - l_suf)];

            if let Some(mid) = try_reconcile_overlapping_hunk_inner(&[], l_inner, r_inner) {
                let mut out = Vec::with_capacity(l_pre + mid.len() + l_suf);
                out.extend(left_chunk[..l_pre].iter().cloned());
                out.extend(mid);
                out.extend(left_chunk[(left_chunk.len() - l_suf)..].iter().cloned());
                return Some(out);
            }
        }
    }

    try_reconcile_overlapping_hunk_inner(base_chunk, left_chunk, right_chunk)
}

/// Reconciles the core interior of an overlapping hunk via multi-line insertion unioning
/// or 1-to-1 line-by-line token/skeleton reconciliation.
fn try_reconcile_overlapping_hunk_inner(
    base_chunk: &[&str],
    left_chunk: &[String],
    right_chunk: &[String],
) -> Option<Vec<String>> {
    if let Some(lines) = try_reconcile_multiline_insertion(base_chunk, left_chunk, right_chunk) {
        return Some(lines);
    }

    if !base_chunk.is_empty()
        && base_chunk.len() == left_chunk.len()
        && base_chunk.len() == right_chunk.len()
    {
        let mut merged_lines = Vec::with_capacity(base_chunk.len());

        for i in 0..base_chunk.len() {
            let b = base_chunk[i];
            let l = left_chunk[i].as_str();
            let r = right_chunk[i].as_str();

            if l == r || r == b {
                merged_lines.push(l.to_string());
            } else if l == b {
                merged_lines.push(r.to_string());
            } else {
                let reconciled = try_reconcile_single_line(b, l, r)?;
                merged_lines.push(reconciled);
            }
        }

        return Some(merged_lines);
    }

    None
}

/// Reconciles concurrent multi-line insertions at the same boundary (including comma-list tails
/// and adjacent structural or additive blocks).
fn try_reconcile_multiline_insertion(
    base_chunk: &[&str],
    left_chunk: &[String],
    right_chunk: &[String],
) -> Option<Vec<String>> {
    // Both agents appended after a single line by adding a trailing comma to that line.
    if base_chunk.len() == 1 && left_chunk.len() >= 2 && right_chunk.len() >= 2 {
        let b0 = base_chunk[0].trim_end();
        let b0_comma = format!("{b0},");

        if !b0.ends_with(',')
            && left_chunk[0].trim_end() == b0_comma
            && right_chunk[0].trim_end() == b0_comma
        {
            let l_added = &left_chunk[1..];
            let r_added = &right_chunk[1..];

            if are_compatible_comma_collection_tails(l_added, r_added) {
                let mut out = Vec::new();
                out.push(left_chunk[0].clone());
                let mut l_vec: Vec<String> = l_added.to_vec();

                if let Some(last) = l_vec.last_mut() {
                    let trimmed = last.trim_end();
                    if !trimmed.is_empty() && !trimmed.ends_with(',') {
                        *last = format!("{trimmed},");
                    }
                }

                out.extend(l_vec);
                out.extend(r_added.iter().cloned());
                return Some(out);
            }
        }
    }

    // Pure insertion at the exact same boundary (`base_chunk.is_empty()`).
    if base_chunk.is_empty() && !left_chunk.is_empty() && !right_chunk.is_empty() {
        if left_chunk.len() == 1 && right_chunk.len() == 1 {
            if let Some(merged_line) =
                try_merge_same_skeleton_line(None, &left_chunk[0], &right_chunk[0])
            {
                return Some(vec![merged_line]);
            }
        }

        if are_compatible_structural_blocks(left_chunk, right_chunk) {
            let mut out = left_chunk.to_vec();
            let skip_first_r = left_chunk.len() >= 2
                && right_chunk.len() >= 2
                && left_chunk.last() == right_chunk.first()
                && right_chunk
                    .first()
                    .is_some_and(|l| l.trim().starts_with('<') && l.trim().ends_with('>'));

            for (idx, r_line) in right_chunk.iter().enumerate() {
                if idx == 0 && skip_first_r {
                    continue;
                }

                out.push(r_line.clone());
            }

            return Some(out);
        }

        if let Some(merged_additive) = try_merge_additive_chunks(left_chunk, right_chunk) {
            return Some(merged_additive);
        }
    }

    None
}

/// Returns `true` if `left` and `right` contain at least one overlapping structural declaration key.
fn chunks_share_structural_key(left: &[String], right: &[String]) -> bool {
    let (Some(l_keys), Some(r_keys)) = (
        extract_structural_block_keys(left),
        extract_structural_block_keys(right),
    ) else {
        return false;
    };

    !l_keys.is_disjoint(&r_keys)
}

/// Checks whether a slice of lines has balanced braces `{ ... }` and matching HTML/XML closing tags.
fn is_structurally_balanced(lines: &[String]) -> bool {
    let mut brace_depth = 0i32;
    let non_empty: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();

    if non_empty.is_empty() {
        return false;
    }

    for line in &non_empty {
        for ch in line.chars() {
            match ch {
                '{' => brace_depth += 1,
                '}' => {
                    brace_depth -= 1;
                    if brace_depth < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }

    if brace_depth != 0 {
        return false;
    }

    if non_empty[0].trim().starts_with('<') {
        let min_indent = non_empty
            .iter()
            .filter(|l| l.trim().starts_with('<') && !l.trim().starts_with("</"))
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        let joined = non_empty
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        for line in &non_empty {
            let indent = line.len() - line.trim_start().len();
            let t = line.trim();

            if indent == min_indent
                && t.starts_with('<')
                && !t.starts_with("</")
                && !t.starts_with("<!--")
                && !t.ends_with("/>")
            {
                let tag_name: String = t[1..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == ':')
                    .collect();

                if !tag_name.is_empty()
                    && !matches!(
                        tag_name.as_str(),
                        "input" | "img" | "br" | "hr" | "meta" | "link" | "source"
                    )
                {
                    let close_tag = format!("</{tag_name}>");
                    if !joined.contains(&close_tag) {
                        return false;
                    }
                }
            }
        }
    }

    true
}

/// Finds a line index in `chunk` where all `shared_keys` reside in `&chunk[..split_at]` and
/// `&chunk[split_at..]` consists solely of balanced declarations disjoint from `other_keys`.
fn find_disjoint_tail_split(
    chunk: &[String],
    other_keys: &std::collections::BTreeSet<String>,
    shared_keys: &std::collections::BTreeSet<String>,
) -> Option<usize> {
    if chunk.len() < 2 {
        return None;
    }

    for split_at in 1..chunk.len() {
        let head = &chunk[..split_at];
        let tail = &chunk[split_at..];

        if !is_structurally_balanced(head) || !is_structurally_balanced(tail) {
            continue;
        }

        let (Some(head_keys), Some(tail_keys)) = (
            extract_structural_block_keys(head),
            extract_structural_block_keys(tail),
        ) else {
            continue;
        };

        if !head_keys.is_empty()
            && !tail_keys.is_empty()
            && shared_keys.is_subset(&head_keys)
            && tail_keys.is_disjoint(other_keys)
            && tail_keys.is_disjoint(&head_keys)
        {
            return Some(split_at);
        }
    }

    None
}

/// Peels off non-conflicting trailing structural declarations from `left_chunk` and `right_chunk`
/// when only the leading declaration collides.
fn split_disjoint_trailing_blocks(
    left_chunk: &[String],
    right_chunk: &[String],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (Some(l_keys), Some(r_keys)) = (
        extract_structural_block_keys(left_chunk),
        extract_structural_block_keys(right_chunk),
    ) else {
        return (left_chunk.to_vec(), right_chunk.to_vec(), Vec::new());
    };

    let shared: std::collections::BTreeSet<String> =
        l_keys.intersection(&r_keys).cloned().collect();

    if shared.is_empty() {
        return (left_chunk.to_vec(), right_chunk.to_vec(), Vec::new());
    }

    let mut l_conf = left_chunk.to_vec();
    let mut r_conf = right_chunk.to_vec();
    let mut clean_tail = Vec::new();

    if l_keys.len() > shared.len() {
        if let Some(split_idx) = find_disjoint_tail_split(left_chunk, &r_keys, &shared) {
            l_conf = left_chunk[..split_idx].to_vec();
            clean_tail.extend_from_slice(&left_chunk[split_idx..]);
        }
    }

    if r_keys.len() > shared.len() {
        if let Some(split_idx) = find_disjoint_tail_split(right_chunk, &l_keys, &shared) {
            r_conf = right_chunk[..split_idx].to_vec();
            clean_tail.extend_from_slice(&right_chunk[split_idx..]);
        }
    }

    (l_conf, r_conf, clean_tail)
}

/// Returns `true` if `left` and `right` represent distinct structural declarations (e.g. HTML elements,
/// CSS rules, or braced procedures) that can be concatenated without conflict.
fn are_compatible_structural_blocks(left: &[String], right: &[String]) -> bool {
    let Some(mut left_keys) = extract_structural_block_keys(left) else {
        return false;
    };
    let Some(mut right_keys) = extract_structural_block_keys(right) else {
        return false;
    };

    if left_keys.len() > 1
        && right_keys.len() > 1
        && left_keys
            .iter()
            .chain(right_keys.iter())
            .all(|k| k.starts_with("block:.") || k.starts_with("block:@"))
    {
        let shared: Vec<String> = left_keys.intersection(&right_keys).cloned().collect();
        if shared.len() < left_keys.len() && shared.len() < right_keys.len() {
            for k in shared {
                left_keys.remove(&k);
                right_keys.remove(&k);
            }
        }
    }

    !left_keys.is_empty() && !right_keys.is_empty() && left_keys.is_disjoint(&right_keys)
}

/// Extracts a set of canonical structural block identifiers (`html:<tag>` or `block:<header>`)
/// from a multi-line diff hunk.
fn extract_structural_block_keys(chunk: &[String]) -> Option<std::collections::BTreeSet<String>> {
    let raw_non_empty: Vec<&String> = chunk.iter().filter(|l| !l.trim().is_empty()).collect();

    if raw_non_empty.is_empty() {
        return None;
    }

    // When inserting a block that ends with the same closing delimiter(s) (`}`, `end`, or `</tag>`)
    // as the preceding block in `base`, `imara-diff` often anchors the trailing delimiter(s) to `base`
    // so the diff hunk is cyclically shifted (`[closing_delims..., block_header, ...]`).
    // Rotating all leading closing delimiters to the end restores the canonical block structure
    // while preserving exact concatenation semantics.
    let is_closing_delim = |s: &str| -> bool {
        let t = s.trim();
        matches!(t, "}" | "};" | "}," | ")" | ");" | "]" | "];" | "end")
            || (t.starts_with("</") && t.ends_with('>'))
    };

    let mut split_idx = 0usize;
    while split_idx < raw_non_empty.len() && is_closing_delim(raw_non_empty[split_idx]) {
        split_idx += 1;
    }

    let non_empty: Vec<&String> = if split_idx > 0 && split_idx < raw_non_empty.len() {
        let mut rotated = raw_non_empty[split_idx..].to_vec();
        rotated.extend_from_slice(&raw_non_empty[..split_idx]);
        rotated
    } else {
        raw_non_empty
    };

    let first_trimmed = non_empty[0].trim();

    if first_trimmed.starts_with('<') && !first_trimmed.starts_with("</") {
        let min_indent = non_empty
            .iter()
            .filter(|l| l.trim().starts_with('<') && !l.trim().starts_with("</"))
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);

        let mut keys = std::collections::BTreeSet::new();

        for (idx, line) in non_empty.iter().enumerate() {
            let indent = line.len() - line.trim_start().len();
            let t = line.trim();

            if indent == min_indent
                && t.starts_with('<')
                && !t.starts_with("</")
                && !t.starts_with("<!--")
            {
                let open_tag = t.find('>').map(|i| &t[..=i]).unwrap_or(t);
                let next_hint = non_empty.get(idx + 1).map(|n| n.trim()).unwrap_or("");

                if open_tag.contains(' ') || open_tag.ends_with("/>") {
                    keys.insert(format!("html:{open_tag}"));
                } else {
                    keys.insert(format!("html:{open_tag}::{next_hint}"));
                }
            }
        }

        if !keys.is_empty() {
            return Some(keys);
        }
    }

    let mut brace_depth = 0i32;
    let mut keys = std::collections::BTreeSet::new();
    let mut saw_braced_block = false;

    for (idx, line) in non_empty.iter().enumerate() {
        let t = line.trim();

        if brace_depth == 0 && !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('#') {
            if t == "{" {
                if idx > 0 {
                    let prev = non_empty[idx - 1].trim();
                    if !prev.is_empty() && !prev.starts_with("//") {
                        keys.insert(format!("block:{prev}"));
                        saw_braced_block = true;
                    }
                }
            } else if t.ends_with('{') {
                let header = t.trim_end_matches('{').trim();
                if !header.is_empty() {
                    keys.insert(format!("block:{header}"));
                    saw_braced_block = true;
                }
            }
        }

        for ch in t.chars() {
            match ch {
                '{' => brace_depth += 1,
                '}' => brace_depth = (brace_depth - 1).max(0),
                _ => {}
            }
        }
    }

    if saw_braced_block && brace_depth == 0 && !keys.is_empty() {
        return Some(keys);
    }

    None
}

/// Checks whether two multi-line comma-separated collection tails contain disjoint element keys.
fn are_compatible_comma_collection_tails(left: &[String], right: &[String]) -> bool {
    let extract_key = |line: &str| -> String {
        let without_comma = line.trim().trim_end_matches(',').trim();
        if let Some((k, _)) = split_top_level_kv(without_comma) {
            format!("kv:{}", normalize_key(k))
        } else {
            format!("elem:{without_comma}")
        }
    };

    let left_keys: std::collections::BTreeSet<String> = left
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| extract_key(l))
        .collect();
    let right_keys: std::collections::BTreeSet<String> = right
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| extract_key(l))
        .collect();

    !left_keys.is_empty() && !right_keys.is_empty() && left_keys.is_disjoint(&right_keys)
}

/// Merges two chunks composed of additive line declarations (imports, constants, key-value pairs,
/// or markdown bullets), deduplicating identical lines and unioning compatible skeleton lines.
fn try_merge_additive_chunks(left: &[String], right: &[String]) -> Option<Vec<String>> {
    let mut left_keyed: Vec<(Option<String>, String)> = Vec::new();
    let mut has_additive = false;

    for line in left {
        if line.trim().is_empty() {
            left_keyed.push((None, line.clone()));
        } else {
            let key = extract_additive_line_key(line)?;
            has_additive = true;
            left_keyed.push((Some(key), line.clone()));
        }
    }

    let mut right_keyed: Vec<(Option<String>, String)> = Vec::new();

    for line in right {
        if line.trim().is_empty() {
            right_keyed.push((None, line.clone()));
        } else {
            let key = extract_additive_line_key(line)?;
            has_additive = true;
            right_keyed.push((Some(key), line.clone()));
        }
    }

    if !has_additive {
        return None;
    }

    let mut out: Vec<(Option<String>, String)> = left_keyed;

    for (r_key_opt, r_line) in right_keyed {
        let Some(r_key) = r_key_opt else {
            if out.last().is_none_or(|(_, last_l)| !last_l.trim().is_empty()) {
                out.push((None, r_line));
            }
            continue;
        };

        if let Some((_, existing_line)) = out
            .iter_mut()
            .find(|(k, l)| k.as_deref() == Some(r_key.as_str()) || l.trim() == r_line.trim())
        {
            if existing_line.trim() == r_line.trim() {
                continue;
            }

            if let Some(merged_line) = try_merge_same_skeleton_line(None, existing_line, &r_line) {
                *existing_line = merged_line;
                continue;
            }

            // Conflicting binding for the same key (e.g. `MAX_ITEMS :: 10` vs `MAX_ITEMS :: 20`)
            return None;
        }

        out.push((Some(r_key), r_line));
    }

    Some(out.into_iter().map(|(_, line)| line).collect())
}

/// Classifies a single line as an additive declaration (list bullet, delimited skeleton, import,
/// comma-list entry, or top-level key-value binding) and returns its canonical key.
fn extract_additive_line_key(line: &str) -> Option<String> {
    let t = line.trim();

    if t.is_empty() {
        return None;
    }

    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return Some(format!("list:{t}"));
    }

    if !t.ends_with('{') && !t.ends_with('(') {
        if let Some(skel) = delimited_skeleton_key(t) {
            return Some(format!("skel:{skel}"));
        }
    }

    for kw in [
        "import ",
        "using import ",
        "from ",
        "use ",
        "pub use ",
        "#include ",
        "#define ",
        "#pragma ",
        "require(",
        "alias ",
        "include ",
    ] {
        if t.starts_with(kw) {
            return Some(format!("imp:{t}"));
        }
    }

    if t.ends_with(',') {
        let without_comma = t.trim_end_matches(',').trim();
        if let Some((k, _)) = split_top_level_kv(without_comma) {
            return Some(format!("kv:{}", normalize_key(k)));
        }
        return Some(format!("elem:{without_comma}"));
    }

    if !t.ends_with('{') && !t.ends_with('(') {
        if let Some((k, _)) = split_top_level_kv(t) {
            let norm = normalize_key(k);
            if !norm.is_empty() {
                return Some(format!("kv:{norm}"));
            }
        }
    }

    None
}

/// Attempts to reconcile 3 versions of a single line (`base`, `left`, `right`) via pipe-union
/// merging, delimited-skeleton merging, span normalization, or sub-line token diff3.
fn try_reconcile_single_line(base: &str, left: &str, right: &str) -> Option<String> {
    if let Some(merged) = try_merge_pipe_union_line(base, left, right) {
        return Some(merged);
    }

    if let Some(merged) = try_merge_same_skeleton_line(Some(base), left, right) {
        return Some(merged);
    }

    let (b_norm, l_norm, r_norm) = normalize_delimited_spans(base, left, right)?;

    if l_norm == r_norm || r_norm == b_norm {
        return Some(l_norm);
    }

    if l_norm == b_norm {
        return Some(r_norm);
    }

    try_token_merge_line(&b_norm, &l_norm, &r_norm)
}

/// Reconciles 3-way edits to a pipe-delimited type union line (`type X = "a" | "b";`).
fn try_merge_pipe_union_line(base: &str, left: &str, right: &str) -> Option<String> {
    let (b_lhs, b_rhs) = split_type_union_line(base)?;
    let (l_lhs, l_rhs) = split_type_union_line(left)?;
    let (r_lhs, r_rhs) = split_type_union_line(right)?;

    if b_lhs.trim() != l_lhs.trim() || b_lhs.trim() != r_lhs.trim() {
        return None;
    }

    let (b_items, b_semi) = parse_pipe_items(b_rhs)?;
    let (l_items, l_semi) = parse_pipe_items(l_rhs)?;
    let (r_items, r_semi) = parse_pipe_items(r_rhs)?;

    let merged = merge_keyed_items(&b_items, &l_items, &r_items)?;
    let semi = if l_semi || r_semi || b_semi { ";" } else { "" };
    Some(format!("{} = {}{}", l_lhs.trim_end(), merged.join(" | "), semi))
}

/// Splits a type-union assignment line (`type T = A | B`) into `(lhs, rhs)` around `'='`.
fn split_type_union_line(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();

    if !(trimmed.starts_with("type ")
        || trimmed.starts_with("export type ")
        || trimmed.contains('|'))
    {
        return None;
    }

    let eq_idx = line.find('=')?;
    let rhs = &line[eq_idx + 1..];

    if !rhs.contains('|') || rhs.contains("||") {
        return None;
    }

    Some((&line[..eq_idx], rhs))
}

/// Parses a pipe-separated union right-hand side into `(keyed_items, has_trailing_semicolon)`.
fn parse_pipe_items(rhs: &str) -> Option<(Vec<(String, String)>, bool)> {
    let trimmed = rhs.trim();
    let has_semi = trimmed.ends_with(';');
    let core = trimmed.trim_end_matches(';').trim();
    let parts = split_top_level(core, '|');

    if parts.len() < 2 {
        return None;
    }

    let items = parts
        .into_iter()
        .map(|p| {
            let s = p.trim().to_string();
            (s.clone(), s)
        })
        .collect();

    Some((items, has_semi))
}

/// Tuple `(opening_delimiter_char, start_byte_offset, end_byte_offset_exclusive)` for a top-level delimited span.
type DelimSpan = (char, usize, usize);

/// Computes a language-agnostic canonical skeleton key for any line that contains a delimited
/// comma-list (`{...}`, `(...)`, `[...]`) or a prefix comma-list (`from <mod> import a, b`).
///
/// Examples across languages:
/// - `import { ref, computed } from 'vue'` -> `Some("import {} from \"vue\"")`
/// - `use std::{fs, io};` -> `Some("use std::{}")`
/// - `#[derive(Debug, Clone)]` -> `Some("#[derive()]")`
/// - `Flags :: bit_set[Flag]{ .Read, .Write }` -> `Some("Flags :: bit_set[Flag]{}")`
/// - `@(require_results, cold)` -> `Some("@()")`
/// - `from typing import List, Optional` -> `Some("from typing import")`
pub fn delimited_skeleton_key(line: &str) -> Option<String> {
    let trimmed = line.trim().trim_end_matches(';').trim();
    let spans = find_top_level_delim_spans(trimmed);

    if let Some(&(open_ch, start, end)) = spans.last() {
        let inner = &trimmed[start + 1..end - 1];

        if open_ch == '{' && split_top_level(inner, ';').len() > 1 {
            return None;
        }

        parse_comma_items(open_ch, inner)?;

        let close_ch = match open_ch {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            _ => return None,
        };
        let outer_before = normalize_skeleton_fragment(&trimmed[..start]);
        let outer_after = normalize_skeleton_fragment(&trimmed[end..]);

        if open_ch == '(' {
            let is_attr_or_decl = trimmed.starts_with('#')
                || trimmed.starts_with('@')
                || outer_before.starts_with("import")
                || outer_before.starts_with("from ")
                || outer_before.starts_with("use ")
                || outer_before.contains("fn ")
                || outer_before.contains("func ")
                || outer_before.contains("def ")
                || outer_before.contains("proc");

            if !is_attr_or_decl {
                return None;
            }
        }

        let has_ident = outer_before
            .chars()
            .chain(outer_after.chars())
            .any(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '@' | '#'));

        if !has_ident {
            return None;
        }

        return Some(format!("{outer_before}{open_ch}{close_ch}{outer_after}"));
    }

    if trimmed.starts_with("from ") {
        if let Some(imp_idx) = trimmed.find(" import ") {
            let prefix = normalize_skeleton_fragment(&trimmed[..imp_idx + 8]);
            let rest = trimmed[imp_idx + 8..].trim();

            if !rest.is_empty() && !rest.contains(';') {
                parse_comma_items('(', rest)?;
                return Some(prefix);
            }
        }
    }

    None
}

/// Normalizes quotes and whitespace in a skeleton prefix/suffix fragment.
fn normalize_skeleton_fragment(s: &str) -> String {
    let quote_norm: String = s.chars().map(|c| if c == '\'' { '"' } else { c }).collect();
    quote_norm
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Merges two lines (`left` and `right`, with optional `base`) that share the same outer
/// [`delimited_skeleton_key`] by performing a 3-way keyed union over their comma-separated items.
pub fn try_merge_same_skeleton_line(
    base: Option<&str>,
    left: &str,
    right: &str,
) -> Option<String> {
    let l_skel = delimited_skeleton_key(left)?;
    let r_skel = delimited_skeleton_key(right)?;

    if l_skel != r_skel {
        return None;
    }

    let l_spans = find_top_level_delim_spans(left);
    let r_spans = find_top_level_delim_spans(right);

    if !l_spans.is_empty() && l_spans.len() == r_spans.len() {
        let &(open_ch, l_start, l_end) = l_spans.last()?;
        let &(r_open, r_start, r_end) = r_spans.last()?;

        if open_ch != r_open {
            return None;
        }

        for i in 0..l_spans.len().saturating_sub(1) {
            let (_, ls, le) = l_spans[i];
            let (_, rs, re) = r_spans[i];

            if left[ls..le].trim() != right[rs..re].trim() {
                return None;
            }
        }

        let b_inner = base
            .and_then(|b| {
                let b_spans = find_top_level_delim_spans(b);
                let &(b_open, bs, be) = b_spans.last()?;
                (b_open == open_ch).then_some(&b[bs + 1..be - 1])
            })
            .unwrap_or("");
        let l_inner = &left[l_start + 1..l_end - 1];
        let r_inner = &right[r_start + 1..r_end - 1];

        let merged_inner = merge_delimited_items(open_ch, b_inner, l_inner, r_inner)?;
        let mut out = left.to_string();
        out.replace_range(l_start + 1..l_end - 1, &merged_inner);
        return Some(out);
    }

    let l_imp = left.find(" import ")?;
    let r_imp = right.find(" import ")?;
    let b_inner = base
        .and_then(|b| b.find(" import ").map(|bi| &b[bi + 8..]))
        .unwrap_or("");
    let l_inner = &left[l_imp + 8..];
    let r_inner = &right[r_imp + 8..];
    let merged_inner = merge_delimited_items('(', b_inner, l_inner, r_inner)?;
    Some(format!("{}{merged_inner}", &left[..l_imp + 8]))
}

/// Reconciles matching top-level delimited comma-list spans (`(...)`, `[...]`, `{...}`) across
/// `base`, `left`, and `right` prior to line-level token diffing.
fn normalize_delimited_spans(
    base: &str,
    left: &str,
    right: &str,
) -> Option<(String, String, String)> {
    let b_spans = find_top_level_delim_spans(base);
    let l_spans = find_top_level_delim_spans(left);
    let r_spans = find_top_level_delim_spans(right);

    if b_spans.is_empty()
        || b_spans.len() != l_spans.len()
        || b_spans.len() != r_spans.len()
    {
        return Some((base.to_string(), left.to_string(), right.to_string()));
    }

    for i in 0..b_spans.len() {
        if b_spans[i].0 != l_spans[i].0 || b_spans[i].0 != r_spans[i].0 {
            return Some((base.to_string(), left.to_string(), right.to_string()));
        }
    }

    let mut b_out = base.to_string();
    let mut l_out = left.to_string();
    let mut r_out = right.to_string();

    // Process from right to left so byte offsets remain valid.
    for i in (0..b_spans.len()).rev() {
        let (open_ch, b_start, b_end) = b_spans[i];
        let (_, l_start, l_end) = l_spans[i];
        let (_, r_start, r_end) = r_spans[i];

        let b_inner = &base[b_start + 1..b_end - 1];
        let l_inner = &left[l_start + 1..l_end - 1];
        let r_inner = &right[r_start + 1..r_end - 1];

        if l_inner == r_inner || (l_inner == b_inner && r_inner == b_inner) {
            continue;
        }

        if l_inner == b_inner {
            b_out.replace_range(b_start + 1..b_end - 1, r_inner);
            l_out.replace_range(l_start + 1..l_end - 1, r_inner);
            continue;
        }

        if r_inner == b_inner {
            b_out.replace_range(b_start + 1..b_end - 1, l_inner);
            r_out.replace_range(r_start + 1..r_end - 1, l_inner);
            continue;
        }

        let merged_inner = merge_delimited_items(open_ch, b_inner, l_inner, r_inner)?;
        b_out.replace_range(b_start + 1..b_end - 1, &merged_inner);
        l_out.replace_range(l_start + 1..l_end - 1, &merged_inner);
        r_out.replace_range(r_start + 1..r_end - 1, &merged_inner);
    }

    Some((b_out, l_out, r_out))
}

/// Scans `line` for balanced top-level delimiter spans `(...)`, `[...]`, and `{...}`,
/// ignoring delimiters inside quoted string literals.
fn find_top_level_delim_spans(line: &str) -> Vec<DelimSpan> {
    let mut spans = Vec::new();
    let mut stack: Vec<(char, usize)> = Vec::new();
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if let Some(q) = in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                in_quote = None;
            }
            continue;
        }

        if matches!(ch, '"' | '\'' | '`') {
            in_quote = Some(ch);
            continue;
        }

        if matches!(ch, '(' | '[' | '{') {
            stack.push((ch, idx));
        } else if matches!(ch, ')' | ']' | '}') {
            let expected = match ch {
                ')' => '(',
                ']' => '[',
                '}' => '{',
                _ => unreachable!(),
            };

            if let Some((open_ch, start_idx)) = stack.pop() {
                if open_ch == expected && stack.is_empty() {
                    spans.push((open_ch, start_idx, idx + 1));
                }
            }
        }
    }

    spans
}

/// Merges comma-separated items inside a delimited span (`open_ch` in `{'(', '[', '{'}`).
fn merge_delimited_items(
    open_ch: char,
    base_inner: &str,
    left_inner: &str,
    right_inner: &str,
) -> Option<String> {
    // Reject statement blocks `{ return x; }` that contain top-level semicolons.
    if open_ch == '{'
        && (split_top_level(base_inner, ';').len() > 1
            || split_top_level(left_inner, ';').len() > 1
            || split_top_level(right_inner, ';').len() > 1)
    {
        return None;
    }

    let b_items = parse_comma_items(open_ch, base_inner)?;
    let l_items = parse_comma_items(open_ch, left_inner)?;
    let r_items = parse_comma_items(open_ch, right_inner)?;

    let merged = merge_keyed_items(&b_items, &l_items, &r_items)?;
    let pad_spaces = open_ch == '{'
        && ((left_inner.starts_with(' ') && left_inner.ends_with(' '))
            || (right_inner.starts_with(' ') && right_inner.ends_with(' ')));

    if pad_spaces && !merged.is_empty() {
        Some(format!(" {} ", merged.join(", ")))
    } else {
        Some(merged.join(", "))
    }
}

/// Splits `inner` on top-level commas and returns `(canonical_key, raw_item)` pairs if all keys are unique.
fn parse_comma_items(open_ch: char, inner: &str) -> Option<Vec<(String, String)>> {
    let trimmed = inner.trim();

    if trimmed.is_empty() {
        return Some(Vec::new());
    }

    let parts = split_top_level(trimmed, ',');
    let mut items = Vec::new();
    let mut seen_keys = std::collections::BTreeSet::new();

    for part in parts {
        let item_str = part.trim();
        if item_str.is_empty() {
            continue;
        }

        let key = classify_delimited_item_key(open_ch, item_str);
        if !seen_keys.insert(key.clone()) {
            // Duplicate keys in the same list cannot be keyed unambiguously.
            return None;
        }

        items.push((key, item_str.to_string()));
    }

    Some(items)
}

/// Extracts the lookup key for a comma-separated item inside `open_ch` (extracting the property
/// name for key-value entries inside `{...}` or `(...)`).
fn classify_delimited_item_key(open_ch: char, item: &str) -> String {
    if matches!(open_ch, '{' | '(') {
        if let Some((k, _)) = split_top_level_kv(item) {
            let norm = normalize_key(k);
            if !norm.is_empty() {
                return norm;
            }
        }
    }

    item.to_string()
}

/// Splits `item` around a top-level key-value separator (`:`, whitespace-padded `::`, `=`, or `=>`).
fn split_top_level_kv(item: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    let bytes = item.as_bytes();

    for (idx, ch) in item.char_indices() {
        if let Some(q) = in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                in_quote = None;
            }
            continue;
        }

        if matches!(ch, '"' | '\'' | '`') {
            in_quote = Some(ch);
            continue;
        }

        if matches!(ch, '(' | '[' | '{' | '<') {
            depth += 1;
            continue;
        }

        if matches!(ch, ')' | ']' | '}' | '>') {
            depth = depth.saturating_sub(1);
            continue;
        }

        if depth == 0 {
            if ch == ':' {
                if bytes.get(idx + 1) == Some(&b':') {
                    // Whitespace-delimited `::` (e.g. Odin/Jai `CONST :: 10`) is a declaration binding;
                    // contiguous `::` (e.g. `std::io::Error`) is a namespace path separator.
                    let has_space_around = (idx > 0 && bytes[idx - 1].is_ascii_whitespace())
                        || bytes.get(idx + 2).is_some_and(u8::is_ascii_whitespace);

                    if has_space_around {
                        return Some((item[..idx].trim(), item[idx + 2..].trim()));
                    }

                    continue;
                }

                if idx > 0 && bytes.get(idx - 1) == Some(&b':') {
                    continue;
                }

                return Some((item[..idx].trim(), item[idx + 1..].trim()));
            }

            if ch == '=' {
                if bytes.get(idx + 1) == Some(&b'=')
                    || (idx > 0 && matches!(bytes[idx - 1], b'=' | b'!' | b'<' | b'>'))
                {
                    continue;
                }

                if bytes.get(idx + 1) == Some(&b'>') {
                    return Some((item[..idx].trim(), item[idx + 2..].trim()));
                }

                return Some((item[..idx].trim(), item[idx + 1..].trim()));
            }
        }
    }

    None
}

/// Strips outer whitespace and surrounding single/double quotes from a key identifier.
fn normalize_key(raw: &str) -> String {
    raw.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

/// Splits `input` by `sep` only at nesting depth `0` outside string literals.
fn split_top_level(input: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    let mut last_idx = 0usize;

    for (idx, ch) in input.char_indices() {
        if let Some(q) = in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                in_quote = None;
            }
            continue;
        }

        if matches!(ch, '"' | '\'' | '`') {
            in_quote = Some(ch);
            continue;
        }

        if matches!(ch, '(' | '[' | '{' | '<') {
            depth += 1;
            continue;
        }

        if matches!(ch, ')' | ']' | '}' | '>') {
            depth = depth.saturating_sub(1);
            continue;
        }

        if depth == 0 && ch == sep {
            parts.push(&input[last_idx..idx]);
            last_idx = idx + ch.len_utf8();
        }
    }

    parts.push(&input[last_idx..]);
    parts
}

/// Performs a 3-way merge over ordered `(key, value)` lists, preserving insertion order and
/// returning `None` if both sides modified the same key to incompatible values.
fn merge_keyed_items(
    base: &[(String, String)],
    left: &[(String, String)],
    right: &[(String, String)],
) -> Option<Vec<String>> {
    let base_map: std::collections::BTreeMap<&str, &str> = base
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let left_map: std::collections::BTreeMap<&str, &str> = left
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let right_map: std::collections::BTreeMap<&str, &str> = right
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut ordered_keys: Vec<&str> = Vec::new();
    for (k, _) in left.iter().chain(right.iter()) {
        if !ordered_keys.contains(&k.as_str()) {
            ordered_keys.push(k.as_str());
        }
    }

    let mut merged = Vec::new();

    for key in ordered_keys {
        match (
            base_map.get(key).copied(),
            left_map.get(key).copied(),
            right_map.get(key).copied(),
        ) {
            (Some(b), Some(l), Some(r)) => {
                if l == r || r == b {
                    merged.push(l.to_string());
                } else if l == b {
                    merged.push(r.to_string());
                } else {
                    return None;
                }
            }
            (None, Some(l), Some(r)) => {
                if l == r {
                    merged.push(l.to_string());
                } else {
                    return None;
                }
            }
            (None, Some(l), None) => merged.push(l.to_string()),
            (None, None, Some(r)) => merged.push(r.to_string()),
            (Some(b), Some(l), None) => {
                if l != b {
                    return None;
                }
            }
            (Some(b), None, Some(r)) => {
                if r != b {
                    return None;
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    Some(merged)
}

/// Performs sub-line word/punctuation token diff3 on a single line, requiring at least one
/// unchanged non-whitespace anchor token between concurrent left and right token edits.
fn try_token_merge_line(base: &str, left: &str, right: &str) -> Option<String> {
    let b_toks = tokenize_line(base);
    let l_toks = tokenize_line(left);
    let r_toks = tokenize_line(right);

    let b_joined = b_toks.join("\n");
    let l_joined = l_toks.join("\n");
    let r_joined = r_toks.join("\n");

    let l_edits = compute_line_edits(&b_joined, &l_joined);
    let r_edits = compute_line_edits(&b_joined, &r_joined);

    if l_edits.is_empty() {
        return Some(right.to_string());
    }

    if r_edits.is_empty() {
        return Some(left.to_string());
    }

    // Require every left token edit and right token edit to be separated by at least one
    // unchanged non-whitespace token in `base` so adjacent/same-value edits still flag a conflict.
    for le in &l_edits {
        for re in &r_edits {
            if le.base_start == re.base_start
                && le.base_end == re.base_end
                && le.replacement == re.replacement
            {
                continue;
            }

            let (first_end, second_start) = if le.base_end <= re.base_start {
                (le.base_end, re.base_start)
            } else if re.base_end <= le.base_start {
                (re.base_end, le.base_start)
            } else {
                return None;
            };

            let has_non_ws_anchor = b_toks[first_end..second_start]
                .iter()
                .any(|t| !t.trim().is_empty());

            if !has_non_ws_anchor {
                return None;
            }
        }
    }

    let mut out_toks: Vec<String> = Vec::new();
    let mut base_pos = 0usize;
    let mut l_idx = 0usize;
    let mut r_idx = 0usize;

    while l_idx < l_edits.len() || r_idx < r_edits.len() {
        match (l_edits.get(l_idx), r_edits.get(r_idx)) {
            (Some(le), None) => {
                while base_pos < le.base_start {
                    out_toks.push(b_toks[base_pos].to_string());
                    base_pos += 1;
                }

                out_toks.extend(le.replacement.iter().cloned());
                base_pos = le.base_end;
                l_idx += 1;
            }
            (None, Some(re)) => {
                while base_pos < re.base_start {
                    out_toks.push(b_toks[base_pos].to_string());
                    base_pos += 1;
                }

                out_toks.extend(re.replacement.iter().cloned());
                base_pos = re.base_end;
                r_idx += 1;
            }
            (Some(le), Some(re)) => {
                if le.base_start == re.base_start
                    && le.base_end == re.base_end
                    && le.replacement == re.replacement
                {
                    while base_pos < le.base_start {
                        out_toks.push(b_toks[base_pos].to_string());
                        base_pos += 1;
                    }

                    out_toks.extend(le.replacement.iter().cloned());
                    base_pos = le.base_end;
                    l_idx += 1;
                    r_idx += 1;
                } else if le.base_end <= re.base_start {
                    while base_pos < le.base_start {
                        out_toks.push(b_toks[base_pos].to_string());
                        base_pos += 1;
                    }

                    out_toks.extend(le.replacement.iter().cloned());
                    base_pos = le.base_end;
                    l_idx += 1;
                } else {
                    while base_pos < re.base_start {
                        out_toks.push(b_toks[base_pos].to_string());
                        base_pos += 1;
                    }

                    out_toks.extend(re.replacement.iter().cloned());
                    base_pos = re.base_end;
                    r_idx += 1;
                }
            }
            (None, None) => break,
        }
    }

    while base_pos < b_toks.len() {
        out_toks.push(b_toks[base_pos].to_string());
        base_pos += 1;
    }

    Some(out_toks.concat())
}

/// Splits `line` into contiguous identifier/number words, whitespace runs, and single-char punctuation tokens.
fn tokenize_line(line: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0usize;
    let mut current_kind: Option<u8> = None;

    for (idx, ch) in line.char_indices() {
        let kind = if ch.is_alphanumeric() || ch == '_' {
            Some(0)
        } else if ch.is_whitespace() {
            Some(1)
        } else {
            None
        };

        match (current_kind, kind) {
            (Some(k1), Some(k2)) if k1 == k2 => {}
            (Some(_), Some(k2)) => {
                tokens.push(&line[start..idx]);
                start = idx;
                current_kind = Some(k2);
            }
            (Some(_), None) => {
                tokens.push(&line[start..idx]);
                tokens.push(&line[idx..idx + ch.len_utf8()]);
                start = idx + ch.len_utf8();
                current_kind = None;
            }
            (None, Some(k2)) => {
                start = idx;
                current_kind = Some(k2);
            }
            (None, None) => {
                tokens.push(&line[idx..idx + ch.len_utf8()]);
                start = idx + ch.len_utf8();
            }
        }
    }

    if current_kind.is_some() && start < line.len() {
        tokens.push(&line[start..]);
    }

    tokens
}

/// Renders a unified diff between `before` and `after` using `imara-diff`.
pub fn unified_diff(before: &str, after: &str) -> String {
    let input = InternedInput::new(before, after);
    let diff = Diff::compute(Algorithm::Histogram, &input);

    diff.unified_diff(
        &BasicLineDiffPrinter(&input.interner),
        UnifiedDiffConfig::default(),
        &input,
    )
    .to_string()
}

/// Maps the line edits between `base_text` and `agent_text` onto 1-based inclusive line ranges
/// `(start_line, end_line, in_working_tree)` in `working_text`.
///
/// When the agent's replacement lines are present in `working_text`, returns their exact 1-based
/// span with `in_working_tree = true`. When the agent is on another branch whose lines are not yet
/// materialized in `working_text`, returns the corresponding anchor span in `working_text` with
/// `in_working_tree = false`.
pub fn attribute_agent_hunks(
    base_text: &str,
    agent_text: &str,
    working_text: &str,
) -> Vec<(usize, usize, bool)> {
    let agent_edits = compute_line_edits(base_text, agent_text);
    if agent_edits.is_empty() {
        return Vec::new();
    }

    let working_lines: Vec<&str> = working_text.lines().collect();
    let max_line = working_lines.len().max(1);
    let working_edits = compute_line_edits(base_text, working_text);

    let base_to_working_line = |b_line: usize| -> usize {
        let mut delta: isize = 0;

        for w_edit in &working_edits {
            if b_line < w_edit.base_start {
                break;
            }

            if b_line < w_edit.base_end {
                return (w_edit.base_start as isize + delta).max(0) as usize;
            }

            let added = w_edit.replacement.len() as isize;
            let removed = (w_edit.base_end - w_edit.base_start) as isize;
            delta += added - removed;
        }

        (b_line as isize + delta).max(0) as usize
    };

    let mut raw_ranges: Vec<(usize, usize, bool)> = Vec::new();

    for a_edit in &agent_edits {
        let expected_w_start = base_to_working_line(a_edit.base_start);

        if !a_edit.replacement.is_empty() && a_edit.replacement.len() <= working_lines.len() {
            let rep_len = a_edit.replacement.len();
            let mut best_match: Option<usize> = None;

            for idx in 0..=(working_lines.len() - rep_len) {
                let matches = working_lines[idx..idx + rep_len]
                    .iter()
                    .zip(a_edit.replacement.iter())
                    .all(|(w, r)| *w == r.as_str());

                if matches {
                    let is_closer = best_match.is_none_or(|prev| {
                        idx.abs_diff(expected_w_start) < prev.abs_diff(expected_w_start)
                    });

                    if is_closer {
                        best_match = Some(idx);
                    }
                }
            }

            if let Some(matched_idx) = best_match {
                let start_1 = (matched_idx + 1).clamp(1, max_line);
                let end_1 = (matched_idx + rep_len).clamp(start_1, max_line);
                raw_ranges.push((start_1, end_1, true));
                continue;
            }
        }

        let w_start = base_to_working_line(a_edit.base_start);
        let w_end = if a_edit.base_end > a_edit.base_start {
            base_to_working_line(a_edit.base_end.saturating_sub(1))
        } else {
            w_start
        };

        let start_1 = (w_start + 1).clamp(1, max_line);
        let end_1 = (w_end + 1).clamp(start_1, max_line);
        raw_ranges.push((start_1, end_1, false));
    }

    let mut merged: Vec<(usize, usize, bool)> = Vec::new();

    for (s, e, in_wt) in raw_ranges {
        if let Some(last) = merged.last_mut() {
            if last.2 == in_wt && s <= last.1 + 1 {
                last.1 = last.1.max(e);
                continue;
            }
        }

        merged.push((s, e, in_wt));
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_agent_hunks_maps_same_branch_and_cross_branch_edits() {
        let base = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        let agent_a = "line 1\nline 2 modified A1\nline 2 modified A2\nline 3\nline 4\nline 5\n";
        let agent_b = "line 1\nline 2\nline 3\nline 4\nline 5 modified B\n";
        let merged_working =
            "line 1\nline 2 modified A1\nline 2 modified A2\nline 3\nline 4\nline 5 modified B\n";

        let ranges_a = attribute_agent_hunks(base, agent_a, merged_working);
        assert_eq!(ranges_a, vec![(2, 3, true)]);

        let ranges_b = attribute_agent_hunks(base, agent_b, merged_working);
        assert_eq!(ranges_b, vec![(6, 6, true)]);

        let ranges_b_on_base = attribute_agent_hunks(base, agent_b, base);
        assert_eq!(ranges_b_on_base, vec![(5, 5, false)]);
    }

    #[test]
    fn non_overlapping_line_edits_merge_cleanly() {
        let base = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        let left = "line 1 modified by A\nline 2\nline 3\nline 4\nline 5\n";
        let right = "line 1\nline 2\nline 3\nline 4\nline 5 modified by B\n";

        let res = merge_text_3way(base, left, right, "agent-a", "agent-b");
        assert!(!res.has_conflicts);
        assert_eq!(
            res.content,
            "line 1 modified by A\nline 2\nline 3\nline 4\nline 5 modified by B\n"
        );
    }

    #[test]
    fn colliding_line_edits_emit_diff3_markers() {
        let base = "const LIMIT = 10;\n";
        let left = "const LIMIT = 25;\n";
        let right = "const LIMIT = 50;\n";

        let res = merge_text_3way(base, left, right, "agent-a", "agent-b");
        assert!(res.has_conflicts);
        assert_eq!(res.conflict_count, 1);
        assert!(res.content.contains("<<<<<<< agent-a"));
        assert!(res.content.contains(">>>>>>> agent-b"));
    }

    #[test]
    fn same_line_arrays_maps_imports_unions_and_args_merge_cleanly() {
        let b_arr = "const ROLES = [\"admin\", \"user\"];\n";
        let l_arr = "const ROLES = [\"admin\", \"user\", \"editor\"];\n";
        let r_arr = "const ROLES = [\"admin\", \"user\", \"auditor\"];\n";
        let res_arr = merge_text_3way(b_arr, l_arr, r_arr, "a", "b");
        assert!(!res_arr.has_conflicts);
        assert_eq!(
            res_arr.content,
            "const ROLES = [\"admin\", \"user\", \"editor\", \"auditor\"];\n"
        );

        let b_map = "const cfg = { host: \"localhost\", port: 8080 };\n";
        let l_map = "const cfg = { host: \"localhost\", port: 9000, tls: true };\n";
        let r_map = "const cfg = { host: \"localhost\", port: 8080, timeout: 5000 };\n";
        let res_map = merge_text_3way(b_map, l_map, r_map, "a", "b");
        assert!(!res_map.has_conflicts);
        assert_eq!(
            res_map.content,
            "const cfg = { host: \"localhost\", port: 9000, tls: true, timeout: 5000 };\n"
        );

        let b_imp = "import { readFile, writeFile } from \"fs\";\n";
        let l_imp = "import { readFile, writeFile, stat } from \"fs\";\n";
        let r_imp = "import { readFile, writeFile, mkdir } from \"fs\";\n";
        let res_imp = merge_text_3way(b_imp, l_imp, r_imp, "a", "b");
        assert!(!res_imp.has_conflicts);
        assert_eq!(
            res_imp.content,
            "import { readFile, writeFile, stat, mkdir } from \"fs\";\n"
        );

        let b_uni = "export type Status = \"pending\" | \"active\";\n";
        let l_uni = "export type Status = \"pending\" | \"active\" | \"paused\";\n";
        let r_uni = "export type Status = \"pending\" | \"active\" | \"archived\";\n";
        let res_uni = merge_text_3way(b_uni, l_uni, r_uni, "a", "b");
        assert!(!res_uni.has_conflicts);
        assert_eq!(
            res_uni.content,
            "export type Status = \"pending\" | \"active\" | \"paused\" | \"archived\";\n"
        );
    }

    #[test]
    fn normal_text_sentences_and_multiline_lists_merge_cleanly() {
        let b_txt = "The server listens on localhost by default and writes logs to stdout.\n";
        let l_txt = "The server listens on 0.0.0.0 by default and writes logs to stdout.\n";
        let r_txt = "The server listens on localhost by default and writes logs to stderr.\n";
        let res_txt = merge_text_3way(b_txt, l_txt, r_txt, "a", "b");
        assert!(!res_txt.has_conflicts);
        assert_eq!(
            res_txt.content,
            "The server listens on 0.0.0.0 by default and writes logs to stderr.\n"
        );

        let b_multi = "const ITEMS = [\n    \"alpha\",\n    \"beta\"\n];\n";
        let l_multi = "const ITEMS = [\n    \"alpha\",\n    \"beta\",\n    \"gamma\"\n];\n";
        let r_multi = "const ITEMS = [\n    \"alpha\",\n    \"beta\",\n    \"delta\"\n];\n";
        let res_multi = merge_text_3way(b_multi, l_multi, r_multi, "a", "b");
        assert!(!res_multi.has_conflicts, "got:\n{}", res_multi.content);
        assert_eq!(
            res_multi.content,
            "const ITEMS = [\n    \"alpha\",\n    \"beta\",\n    \"gamma\",\n    \"delta\"\n];\n"
        );
    }

    #[test]
    fn odin_and_universal_delimited_skeleton_lines_merge_cleanly() {
        let base_odin = r#"package main

import "core:fmt"

main :: proc() {
    if true {
        fmt.println("boot")
    }
}
"#;
        let left_odin = r#"package main

import "core:fmt"
import "core:time"
import "core:math"
DEFAULT_POMODORO :: 1500
Flags :: bit_set[Flag]{ .Read, .Write }

main :: proc() {
    if true {
        fmt.println("boot")
    }
}

start_timer :: proc(seconds: int) {
    if seconds > 0 {
        fmt.println("timer:", seconds)
    }
}
"#;
        let right_odin = r#"package main

import "core:fmt"
import "core:time"
import "core:strings"
MAX_HABITS :: 10
Flags :: bit_set[Flag]{ .Read, .Execute }

main :: proc() {
    if true {
        fmt.println("boot")
    }
}

track_habit :: proc(name: string) {
    if len(name) > 0 {
        fmt.println("habit:", name)
    }
}
"#;
        let res_odin = merge_text_3way(base_odin, left_odin, right_odin, "timer", "habits");
        assert!(
            !res_odin.has_conflicts,
            "Odin merge should be conflict-free, got:\n{}",
            res_odin.content
        );
        assert_eq!(
            res_odin.content.matches("import \"core:time\"").count(),
            1,
            "shared Odin import must be deduplicated:\n{}",
            res_odin.content
        );
        assert!(
            res_odin.content.contains("import \"core:math\"")
                && res_odin.content.contains("import \"core:strings\"")
        );
        assert!(
            res_odin
                .content
                .contains("Flags :: bit_set[Flag]{ .Read, .Write, .Execute }"),
            "Odin bit_set must merge and deduplicate flags:\n{}",
            res_odin.content
        );
        assert!(
            res_odin.content.contains("start_timer :: proc")
                && res_odin.content.contains("track_habit :: proc"),
            "both Odin procedures must be preserved:\n{}",
            res_odin.content
        );
    }
}
