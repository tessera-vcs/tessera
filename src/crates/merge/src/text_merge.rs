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

/// Cyclic boundary shift applied to normalize a structural diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CyclicRotation {
    /// No cyclic boundary shift was applied.
    None,
    /// Leading closing delimiters (`}`, `end`, `</tag>`) rotated to the end of the chunk.
    LeadingClosers(Vec<String>),
    /// Trailing `[section]` headers rotated to the start of the chunk.
    TrailingSections(Vec<String>),
    /// Leading deeper-indented continuation lines rotated to the end of the chunk.
    LeadingDeeperIndent(usize),
}

/// Structural block fingerprint capturing declaration keys, cyclic rotation symmetry, and indentation.
#[derive(Debug, Clone)]
struct StructuralBlockSignature {
    /// Canonical declaration/block keys present in the hunk.
    keys: std::collections::BTreeSet<String>,
    /// Cyclic boundary rotation applied when normalizing the hunk.
    rotation: CyclicRotation,
    /// Minimum non-empty indentation width (in bytes) of the normalized hunk.
    min_indent: usize,
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
/// Histogram diff alignment with safe sub-line and structural reconciliation.
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
    merge_text_3way_inner(base, left, right, left_label, right_label, true)
}

/// Performs a strict 3-way line-level diff3 merge without sub-line token or delimited-span
/// heuristics, used by manifest/lockfile drivers for format-preserving fast paths and conflict output.
pub(crate) fn merge_text_3way_strict(
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> TextMergeOutcome {
    merge_text_3way_inner(base, left, right, left_label, right_label, false)
}

/// Core 3-way line-level diff3 engine parameterized by `allow_heuristics`.
fn merge_text_3way_inner(
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
    allow_heuristics: bool,
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
                let boundary_collision = allow_heuristics
                    && (le.base_end == re.base_start || re.base_end == le.base_start)
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
                    let mut region_end = le.base_end.max(re.base_end);
                    let mut l_end_idx = l_idx + 1;
                    let mut r_end_idx = r_idx + 1;

                    // Transitively absorb all subsequent left/right hunks overlapping the window
                    // so `base_pos` never rewinds and no lines are duplicated or lost.
                    loop {
                        let mut expanded = false;

                        while let Some(next_le) = left_edits.get(l_end_idx) {
                            let last_re = &right_edits[r_end_idx - 1];

                            if edit_overlaps_window(
                                next_le,
                                region_start,
                                region_end,
                                last_re,
                                allow_heuristics,
                            ) {
                                region_end = region_end.max(next_le.base_end);
                                l_end_idx += 1;
                                expanded = true;
                            } else {
                                break;
                            }
                        }

                        while let Some(next_re) = right_edits.get(r_end_idx) {
                            let last_le = &left_edits[l_end_idx - 1];

                            if edit_overlaps_window(
                                next_re,
                                region_start,
                                region_end,
                                last_le,
                                allow_heuristics,
                            ) {
                                region_end = region_end.max(next_re.base_end);
                                r_end_idx += 1;
                                expanded = true;
                            } else {
                                break;
                            }
                        }

                        if !expanded {
                            break;
                        }
                    }

                    while base_pos < region_start && base_pos < base_lines.len() {
                        out_lines.push(base_lines[base_pos].to_string());
                        base_pos += 1;
                    }

                    let clamped_end = region_end.min(base_lines.len());
                    let clamped_start = region_start.min(clamped_end);
                    let base_chunk: Vec<&str> = base_lines[clamped_start..clamped_end].to_vec();
                    let left_chunk = materialize_chunk(
                        &base_lines,
                        clamped_start,
                        clamped_end,
                        &left_edits[l_idx..l_end_idx],
                    );
                    let right_chunk = materialize_chunk(
                        &base_lines,
                        clamped_start,
                        clamped_end,
                        &right_edits[r_idx..r_end_idx],
                    );

                    if left_chunk == right_chunk {
                        out_lines.extend(left_chunk);
                    } else if allow_heuristics {
                        if let Some(reconciled) =
                            try_reconcile_overlapping_hunk(&base_chunk, &left_chunk, &right_chunk)
                        {
                            out_lines.extend(reconciled);
                        } else {
                            let (l_conf, r_conf, clean_tail) = split_disjoint_trailing_blocks(
                                &base_chunk,
                                &left_chunk,
                                &right_chunk,
                            );
                            conflict_count += 1;
                            out_lines.push(format!("<<<<<<< {left_label}"));
                            out_lines.extend(l_conf);
                            out_lines.push("=======".to_string());
                            out_lines.extend(r_conf);
                            out_lines.push(format!(">>>>>>> {right_label}"));
                            out_lines.extend(clean_tail);
                        }
                    } else {
                        conflict_count += 1;
                        out_lines.push(format!("<<<<<<< {left_label}"));
                        out_lines.extend(left_chunk);
                        out_lines.push("=======".to_string());
                        out_lines.extend(right_chunk);
                        out_lines.push(format!(">>>>>>> {right_label}"));
                    }

                    base_pos = region_end;
                    l_idx = l_end_idx;
                    r_idx = r_end_idx;
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

/// Returns `true` if `next_edit` overlaps the active conflict window `[region_start, region_end]`
/// or collides at `region_end` with `last_opposite`.
fn edit_overlaps_window(
    next_edit: &LineEdit,
    region_start: usize,
    region_end: usize,
    last_opposite: &LineEdit,
    allow_heuristics: bool,
) -> bool {
    if next_edit.base_start < region_end {
        return true;
    }

    if next_edit.base_start == region_end {
        if region_start == region_end {
            return true;
        }

        if last_opposite.base_end == region_end {
            let next_pure = next_edit.base_start == next_edit.base_end;
            let opp_pure = last_opposite.base_start == last_opposite.base_end;

            if next_pure && opp_pure {
                return true;
            }

            if allow_heuristics
                && (next_pure != opp_pure)
                && chunks_share_structural_key(&next_edit.replacement, &last_opposite.replacement)
            {
                return true;
            }
        }
    }

    false
}

/// Applies all `edits` contained within `[region_start, region_end]` onto `base_lines`,
/// preserving untouched base lines between edits inside the window.
fn materialize_chunk(
    base_lines: &[&str],
    region_start: usize,
    region_end: usize,
    edits: &[LineEdit],
) -> Vec<String> {
    let mut chunk = Vec::new();
    let mut pos = region_start;

    for edit in edits {
        while pos < edit.base_start && pos < base_lines.len() {
            chunk.push(base_lines[pos].to_string());
            pos += 1;
        }

        chunk.extend(edit.replacement.iter().cloned());
        pos = edit.base_end;
    }

    while pos < region_end && pos < base_lines.len() {
        chunk.push(base_lines[pos].to_string());
        pos += 1;
    }

    chunk
}

/// Returns `true` if the `'` character at byte offset `quote_idx` in `line` opens a real
/// single-quoted string/char literal rather than a Rust lifetime (`'a`, `'static`) or an
/// English apostrophe (`don't`, `agent's`).
fn is_single_quote_literal_start(line: &str, quote_idx: usize) -> bool {
    let bytes = line.as_bytes();

    if quote_idx > 0 {
        let prev = bytes[quote_idx - 1];

        if prev.is_ascii_alphanumeric() || matches!(prev, b'_' | b'&' | b'<') {
            return false;
        }
    }

    let mut escaped = false;
    let rest = &line[quote_idx + 1..];

    for (rel_idx, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if ch == '\'' {
            let after_idx = quote_idx + 1 + rel_idx + 1;
            let next_byte = bytes.get(after_idx).copied();

            if !next_byte.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return true;
            }
        }
    }

    false
}

/// Returns `true` if the `<` at `open_idx` opens a generic or tag angle-bracket pair rather than
/// a comparison or shift operator (`a < b`, `<=`, `<<`, `<-`).
fn is_generic_angle_open(s: &str, open_idx: usize) -> bool {
    let bytes = s.as_bytes();
    let next = bytes.get(open_idx + 1).copied();

    if matches!(next, Some(b'=' | b'<' | b'-') | None) {
        return false;
    }

    let prev_ws = open_idx > 0 && bytes[open_idx - 1].is_ascii_whitespace();
    let next_ws = next.is_some_and(|b| b.is_ascii_whitespace());

    if prev_ws && next_ws {
        return false;
    }

    // Verify a matching closing `>` exists before any top-level `;` or unmatched closer.
    let mut nested_angles = 1usize;
    let mut paren_depth = 0i32;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for (rel_idx, ch) in s[open_idx + 1..].char_indices() {
        let abs_idx = open_idx + 1 + rel_idx;

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

        if ch == '"' || ch == '`' {
            in_quote = Some(ch);
            continue;
        }

        if ch == '\'' && is_single_quote_literal_start(s, abs_idx) {
            in_quote = Some('\'');
            continue;
        }

        match ch {
            '(' | '[' | '{' => paren_depth += 1,
            ')' | ']' | '}' => {
                paren_depth -= 1;

                if paren_depth < 0 {
                    return false;
                }
            }
            ';' if paren_depth == 0 => return false,
            '<' if paren_depth == 0 => {
                if !matches!(bytes.get(abs_idx + 1), Some(b'=' | b'<' | b'-')) {
                    nested_angles += 1;
                }
            }
            '>' if paren_depth == 0 => {
                let prev = bytes.get(abs_idx.saturating_sub(1)).copied();

                if !matches!(prev, Some(b'=' | b'-')) {
                    nested_angles -= 1;

                    if nested_angles == 0 {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }

    false
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
            out.extend(
                left_chunk[(left_chunk.len() - suffix_len)..]
                    .iter()
                    .cloned(),
            );
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

    if base_chunk.is_empty() && left_chunk.len() >= 2 && right_chunk.len() >= 2 {
        if let Some(shared_hdr_len) = find_shared_container_header_prefix(left_chunk, right_chunk) {
            let l_inner = &left_chunk[shared_hdr_len..];
            let r_inner = &right_chunk[shared_hdr_len..];

            if let Some(mid) = try_reconcile_overlapping_hunk(&[], l_inner, r_inner) {
                let mut out = Vec::with_capacity(shared_hdr_len + mid.len());
                out.extend(left_chunk[..shared_hdr_len].iter().cloned());
                out.extend(mid);
                return Some(out);
            }
        }
    }

    try_reconcile_overlapping_hunk_inner(base_chunk, left_chunk, right_chunk)
}

/// Checks if `left_chunk` and `right_chunk` start with an identical YAML mapping container
/// header (e.g. `dependencies:`) or singleton INI/TOML `[section]` header whose body lines
/// are `key = value` entries rather than code declarations (such as C# `[Authorize]` methods).
fn find_shared_container_header_prefix(
    left_chunk: &[String],
    right_chunk: &[String],
) -> Option<usize> {
    let mut idx = 0usize;
    let mut last_header_end = None;
    let mut last_header_indent = 0usize;
    let mut is_section = false;

    while idx < left_chunk.len() && idx < right_chunk.len() && left_chunk[idx] == right_chunk[idx] {
        let line = &left_chunk[idx];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            idx += 1;
            continue;
        }

        let indent = line.len() - line.trim_start().len();

        if is_yaml_mapping_container_header(trimmed)
            && (last_header_end.is_none() || indent > last_header_indent)
        {
            last_header_indent = indent;
            last_header_end = Some(idx + 1);
            is_section = false;
            idx += 1;
            continue;
        }

        if last_header_end.is_none() && is_single_section_header(trimmed) {
            last_header_indent = indent;
            last_header_end = Some(idx + 1);
            is_section = true;
            break;
        }

        break;
    }

    let hdr_len = last_header_end?;
    let l_rest = &left_chunk[hdr_len..];
    let r_rest = &right_chunk[hdr_len..];

    if l_rest.iter().all(|l| l.trim().is_empty()) || r_rest.iter().all(|l| l.trim().is_empty()) {
        return None;
    }

    if is_section {
        if is_ini_toml_section_body(l_rest) && is_ini_toml_section_body(r_rest) {
            return Some(hdr_len);
        }

        return None;
    }

    let all_indented_deeper = l_rest.iter().chain(r_rest.iter()).all(|l| {
        let t = l.trim();
        t.is_empty() || (l.len() - l.trim_start().len()) > last_header_indent
    });

    if all_indented_deeper {
        return Some(hdr_len);
    }

    None
}

/// Returns `true` if `lines` looks like an INI/TOML section body of `key = value` entries
/// rather than C#/Rust/Java/code declarations following an attribute like `[Authorize]` or `[Test]`.
fn is_ini_toml_section_body(lines: &[String]) -> bool {
    let mut saw_kv = false;

    for line in lines {
        let t = line.trim();

        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }

        if is_section_header_line(t) {
            return false;
        }

        if t.ends_with('{') || t.ends_with('}') {
            return false;
        }

        if !saw_kv {
            if !is_ini_toml_kv_line(t) {
                return false;
            }

            saw_kv = true;
        } else if looks_like_code_declaration(t) {
            return false;
        }
    }

    saw_kv
}

/// Returns `true` if `trimmed` is a plain INI/TOML `key = value` line.
fn is_ini_toml_kv_line(trimmed: &str) -> bool {
    if looks_like_code_declaration(trimmed) {
        return false;
    }

    let Some(eq_idx) = trimmed.find('=') else {
        return false;
    };

    if trimmed.as_bytes().get(eq_idx + 1) == Some(&b'=')
        || trimmed.as_bytes().get(eq_idx + 1) == Some(&b'>')
    {
        return false;
    }

    let key = trimmed[..eq_idx].trim();

    if key.is_empty() || key.contains('(') || key.contains(')') || key.contains('{') {
        return false;
    }

    key.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'"' | b'\'' | b' '))
}

/// Returns `true` if `trimmed` starts with a common method, class, or function declaration keyword.
fn looks_like_code_declaration(trimmed: &str) -> bool {
    for kw in [
        "public ",
        "private ",
        "protected ",
        "internal ",
        "static ",
        "async ",
        "virtual ",
        "override ",
        "abstract ",
        "sealed ",
        "readonly ",
        "extern ",
        "void ",
        "class ",
        "struct ",
        "interface ",
        "enum ",
        "record ",
        "namespace ",
        "fn ",
        "pub ",
        "func ",
        "function ",
        "def ",
        "let ",
        "var ",
        "const ",
        "return ",
    ] {
        if trimmed.starts_with(kw) {
            return true;
        }
    }

    false
}

/// Returns `true` if `trimmed` is a YAML mapping container header (`key:`) rather than a
/// code declaration or control-flow statement (`def`, `class`, `if`, `for`, etc.).
fn is_yaml_mapping_container_header(trimmed: &str) -> bool {
    let clean = strip_inline_hash_comment(trimmed);

    if !clean.ends_with(':') || clean.starts_with('-') || clean.starts_with('@') {
        return false;
    }

    for kw in [
        "def ", "async ", "class ", "if ", "elif ", "else:", "for ", "while ", "with ", "try:",
        "except", "finally:", "match ", "case ", "fn ", "func ", "proc ",
    ] {
        if clean.starts_with(kw) {
            return false;
        }
    }

    true
}

/// Returns `true` if `trimmed` is a singleton `[section]` header (excluding `[[array]]` stanzas).
fn is_single_section_header(trimmed: &str) -> bool {
    is_section_header_line(trimmed) && !trimmed.starts_with("[[")
}

/// Returns `true` if `trimmed` is a `[section]` or `[[section]]` table/stanza header.
fn is_section_header_line(trimmed: &str) -> bool {
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') || trimmed.len() < 3 {
        return false;
    }

    let inner = trimmed.trim_start_matches('[').trim_end_matches(']').trim();

    if inner.is_empty()
        || !inner.chars().any(|c| c.is_ascii_alphanumeric())
        || inner.contains('(')
        || inner.contains(')')
    {
        return false;
    }

    split_top_level(inner, ',').len() == 1
}

/// Strips a trailing `# ...` comment outside string quotes and trims trailing whitespace.
fn strip_inline_hash_comment(line: &str) -> &str {
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    let bytes = line.as_bytes();

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

        if matches!(ch, '"' | '`') || (ch == '\'' && is_single_quote_literal_start(line, idx)) {
            in_quote = Some(ch);
            continue;
        }

        if ch == '#' && (idx == 0 || bytes[idx - 1].is_ascii_whitespace()) {
            return line[..idx].trim_end();
        }
    }

    line.trim_end()
}

/// Returns `true` if `trimmed` is a closing keyword for an `end`/`fi`/`esac`-delimited block.
fn is_keyword_block_closer(trimmed: &str) -> bool {
    matches!(
        trimmed,
        "end" | "end;" | "end," | "end)" | "fi" | "esac" | "endmodule"
    )
}

/// Returns `true` if `trimmed` is a mid-block branch clause (`else`, `elsif`, `rescue`, etc.).
fn is_mid_block_clause(trimmed: &str) -> bool {
    for kw in [
        "else", "elsif", "elseif", "elif", "rescue", "ensure", "when", "then", "catch", "after",
        "except", "finally",
    ] {
        if trimmed == kw
            || trimmed.starts_with(&format!("{kw} "))
            || trimmed.starts_with(&format!("{kw}:"))
            || trimmed.starts_with(&format!("{kw}("))
        {
            return true;
        }
    }

    false
}

/// Returns `true` if `trimmed` is a valid header line for an indentation-scoped block.
fn is_indentation_block_header(trimmed: &str) -> bool {
    let clean = strip_inline_hash_comment(trimmed);

    if clean.is_empty() || is_keyword_block_closer(clean) {
        return false;
    }

    if clean.ends_with(':')
        || clean.ends_with(": |")
        || clean.ends_with(": |-")
        || clean.ends_with(": |+")
        || clean.ends_with(": >")
        || clean.ends_with(": >-")
        || clean.ends_with(": >+")
        || clean.starts_with("- ")
    {
        return true;
    }

    for kw in [
        "proc ",
        "func ",
        "method ",
        "iterator ",
        "macro ",
        "template ",
    ] {
        if clean.starts_with(kw) && clean.ends_with('=') {
            return true;
        }
    }

    false
}

/// Checks whether `(`, `[`, and `{` delimiters across `lines` are balanced outside string literals.
fn are_inline_delimiters_balanced(lines: &[&String]) -> bool {
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;

    for line in lines {
        let trimmed = strip_inline_hash_comment(line.trim());
        let mut in_quote: Option<char> = None;
        let mut escaped = false;

        for (idx, ch) in trimmed.char_indices() {
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

            if matches!(ch, '"' | '`')
                || (ch == '\'' && is_single_quote_literal_start(trimmed, idx))
            {
                in_quote = Some(ch);
                continue;
            }

            match ch {
                '(' => paren += 1,
                ')' => {
                    paren -= 1;

                    if paren < 0 {
                        return false;
                    }
                }
                '[' => bracket += 1,
                ']' => {
                    bracket -= 1;

                    if bracket < 0 {
                        return false;
                    }
                }
                '{' => brace += 1,
                '}' => {
                    brace -= 1;

                    if brace < 0 {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }

    paren == 0 && bracket == 0 && brace == 0
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
    let (Some(l_sig), Some(r_sig)) = (
        extract_structural_block_signature(left),
        extract_structural_block_signature(right),
    ) else {
        return false;
    };

    !l_sig.keys.is_disjoint(&r_sig.keys)
}

/// Checks whether a slice of lines has balanced braces `{ ... }`, matching HTML/XML closing tags,
/// balanced `end`/`fi`/`esac` closers, or well-formed `[section]` / indentation-scoped block boundaries.
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

        return true;
    }

    let non_comment: Vec<&String> = non_empty
        .iter()
        .copied()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with('#') && !t.starts_with("//") && !t.starts_with("--")
        })
        .collect();

    if non_comment.is_empty() {
        return false;
    }

    let min_indent = non_comment
        .iter()
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    let has_keyword_closer_at_min = non_comment.iter().any(|l| {
        (l.len() - l.trim_start().len()) == min_indent && is_keyword_block_closer(l.trim())
    });

    if has_keyword_closer_at_min {
        let mut depth = 0i32;

        for line in &non_comment {
            let indent = line.len() - line.trim_start().len();

            if indent != min_indent {
                continue;
            }

            let t = line.trim();

            if is_keyword_block_closer(t) {
                depth -= 1;

                if depth < 0 {
                    return false;
                }
            } else if is_mid_block_clause(t) {
                if depth != 1 {
                    return false;
                }
            } else if !t.starts_with('@') {
                if depth != 0 {
                    return false;
                }

                depth = 1;
            }
        }

        return depth == 0;
    }

    let has_section_at_min = non_comment.iter().any(|l| {
        (l.len() - l.trim_start().len()) == min_indent && is_section_header_line(l.trim())
    });

    if has_section_at_min {
        let first_t = non_comment[0].trim();
        let last_t = non_comment[non_comment.len() - 1].trim();
        return is_section_header_line(first_t) && !is_section_header_line(last_t);
    }

    let has_deeper_indent = non_comment
        .iter()
        .any(|l| (l.len() - l.trim_start().len()) > min_indent);

    if has_deeper_indent
        && !non_comment
            .iter()
            .any(|l| l.contains('{') || l.contains('}'))
    {
        let first_indent = non_comment[0].len() - non_comment[0].trim_start().len();

        if first_indent != min_indent || is_mid_block_clause(non_comment[0].trim()) {
            return false;
        }
    }

    true
}

/// Finds a line index in `chunk` where all `shared_keys` reside in `&chunk[..split_at]` and
/// `&chunk[split_at..]` consists solely of newly added balanced declarations disjoint from
/// `other_keys` and `base_keys`.
fn find_disjoint_tail_split(
    chunk: &[String],
    base_keys: &std::collections::BTreeSet<String>,
    other_keys: &std::collections::BTreeSet<String>,
    shared_keys: &std::collections::BTreeSet<String>,
) -> Option<usize> {
    if chunk.len() < 2 || chunk.len() > 256 {
        return None;
    }

    let min_indent = chunk
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()?;

    for split_at in 1..chunk.len() {
        let line = &chunk[split_at];

        if line.trim().is_empty() || (line.len() - line.trim_start().len()) > min_indent {
            continue;
        }

        let head = &chunk[..split_at];
        let tail = &chunk[split_at..];

        if !is_structurally_balanced(head) || !is_structurally_balanced(tail) {
            continue;
        }

        let (Some(head_sig), Some(tail_sig)) = (
            extract_structural_block_signature(head),
            extract_structural_block_signature(tail),
        ) else {
            continue;
        };

        if head_sig.rotation != CyclicRotation::None || tail_sig.rotation != CyclicRotation::None {
            continue;
        }

        if !head_sig.keys.is_empty()
            && !tail_sig.keys.is_empty()
            && shared_keys.is_subset(&head_sig.keys)
            && tail_sig.keys.is_disjoint(other_keys)
            && tail_sig.keys.is_disjoint(base_keys)
            && tail_sig.keys.is_disjoint(&head_sig.keys)
        {
            return Some(split_at);
        }
    }

    None
}

/// Peels off genuinely newly added, non-conflicting trailing structural declarations from
/// `left_chunk` and `right_chunk` when only the leading declaration collides, never peeling
/// blocks that existed in `base_chunk` (so deletions on the other side are never resurrected).
fn split_disjoint_trailing_blocks(
    base_chunk: &[&str],
    left_chunk: &[String],
    right_chunk: &[String],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let base_non_empty: Vec<String> = base_chunk
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| (*l).to_string())
        .collect();

    let base_keys = if base_non_empty.is_empty() {
        std::collections::BTreeSet::new()
    } else {
        let Some(b_sig) = extract_structural_block_signature(&base_non_empty) else {
            return (left_chunk.to_vec(), right_chunk.to_vec(), Vec::new());
        };
        b_sig.keys
    };

    let (Some(l_sig), Some(r_sig)) = (
        extract_structural_block_signature(left_chunk),
        extract_structural_block_signature(right_chunk),
    ) else {
        return (left_chunk.to_vec(), right_chunk.to_vec(), Vec::new());
    };

    let shared: std::collections::BTreeSet<String> =
        l_sig.keys.intersection(&r_sig.keys).cloned().collect();

    if shared.is_empty() {
        return (left_chunk.to_vec(), right_chunk.to_vec(), Vec::new());
    }

    let mut l_conf = left_chunk.to_vec();
    let mut r_conf = right_chunk.to_vec();
    let mut clean_tail = Vec::new();

    if l_sig.keys.len() > shared.len() {
        if let Some(split_idx) =
            find_disjoint_tail_split(left_chunk, &base_keys, &r_sig.keys, &shared)
        {
            l_conf = left_chunk[..split_idx].to_vec();
            clean_tail.extend_from_slice(&left_chunk[split_idx..]);
        }
    }

    if r_sig.keys.len() > shared.len() {
        if let Some(split_idx) =
            find_disjoint_tail_split(right_chunk, &base_keys, &l_sig.keys, &shared)
        {
            r_conf = right_chunk[..split_idx].to_vec();
            clean_tail.extend_from_slice(&right_chunk[split_idx..]);
        }
    }

    (l_conf, r_conf, clean_tail)
}

/// Returns `true` if `left` and `right` represent distinct structural declarations sharing the
/// exact same cyclic boundary shift and minimum indentation level.
fn are_compatible_structural_blocks(left: &[String], right: &[String]) -> bool {
    let Some(left_sig) = extract_structural_block_signature(left) else {
        return false;
    };
    let Some(right_sig) = extract_structural_block_signature(right) else {
        return false;
    };

    if left_sig.rotation != right_sig.rotation || left_sig.min_indent != right_sig.min_indent {
        return false;
    }

    !left_sig.keys.is_empty()
        && !right_sig.keys.is_empty()
        && left_sig.keys.is_disjoint(&right_sig.keys)
}

/// Extracts canonical structural block identifiers, cyclic rotation metadata, and indentation
/// from a multi-line diff hunk.
fn extract_structural_block_signature(chunk: &[String]) -> Option<StructuralBlockSignature> {
    let raw_non_empty: Vec<&String> = chunk.iter().filter(|l| !l.trim().is_empty()).collect();

    if raw_non_empty.is_empty() {
        return None;
    }

    // When inserting a block that ends with the same closing delimiter(s) (`}`, `end`, or `</tag>`)
    // as the preceding block in `base`, `imara-diff` often anchors the trailing delimiter(s) to `base`
    // so the diff hunk is cyclically shifted (`[closing_delims..., block_header, ...]`).
    // Recording the exact rotation ensures we only concatenate two chunks when BOTH share the
    // exact same boundary shift and indentation level.
    let is_closing_delim = |s: &str| -> bool {
        let t = s.trim();
        matches!(t, "}" | "};" | "}," | ")" | ");" | "]" | "];")
            || is_keyword_block_closer(t)
            || (t.starts_with("</") && t.ends_with('>'))
    };

    let mut split_idx = 0usize;

    while split_idx < raw_non_empty.len() && is_closing_delim(raw_non_empty[split_idx]) {
        split_idx += 1;
    }

    let (non_empty, base_rotation): (Vec<&String>, CyclicRotation) =
        if split_idx > 0 && split_idx < raw_non_empty.len() {
            let closers: Vec<String> = raw_non_empty[..split_idx]
                .iter()
                .map(|s| (*s).clone())
                .collect();
            let mut rotated = raw_non_empty[split_idx..].to_vec();
            rotated.extend_from_slice(&raw_non_empty[..split_idx]);
            (rotated, CyclicRotation::LeadingClosers(closers))
        } else {
            (raw_non_empty, CyclicRotation::None)
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
            return Some(StructuralBlockSignature {
                keys,
                rotation: base_rotation,
                min_indent,
            });
        }
    }

    let mut brace_depth = 0i32;
    let mut keys = std::collections::BTreeSet::new();
    let mut saw_braced_block = false;
    let mut braced_min_indent: Option<usize> = None;

    for (idx, line) in non_empty.iter().enumerate() {
        let t = line.trim();
        let indent = line.len() - line.trim_start().len();

        if brace_depth == 0 && !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('#') {
            if t == "{" {
                if idx > 0 {
                    let prev = non_empty[idx - 1].trim();

                    if !prev.is_empty() && !prev.starts_with("//") {
                        let prev_indent =
                            non_empty[idx - 1].len() - non_empty[idx - 1].trim_start().len();
                        braced_min_indent =
                            Some(braced_min_indent.map_or(prev_indent, |m| m.min(prev_indent)));
                        keys.insert(format!("block:{prev}"));
                        saw_braced_block = true;
                    }
                }
            } else if t.ends_with('{') {
                let header = t.trim_end_matches('{').trim();

                if !header.is_empty() {
                    braced_min_indent = Some(braced_min_indent.map_or(indent, |m| m.min(indent)));
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
        return Some(StructuralBlockSignature {
            keys,
            rotation: base_rotation,
            min_indent: braced_min_indent.unwrap_or(0),
        });
    }

    let non_comment: Vec<&String> = non_empty
        .iter()
        .copied()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with('#') && !t.starts_with("//") && !t.starts_with("--")
        })
        .collect();

    if non_comment.len() < 2 || !are_inline_delimiters_balanced(&non_comment) {
        return None;
    }

    let min_indent = non_comment
        .iter()
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    // Keyword-closed blocks (`def` / `do` / `function` ... `end`, `if` ... `fi`, `case` ... `esac`).
    let has_closer_at_min = non_comment.iter().any(|l| {
        (l.len() - l.trim_start().len()) == min_indent && is_keyword_block_closer(l.trim())
    });

    if has_closer_at_min {
        let mut kw_keys = std::collections::BTreeSet::new();
        let mut open_depth = 0i32;
        let mut saw_closed = false;
        let mut valid_kw = true;

        for line in &non_comment {
            let indent = line.len() - line.trim_start().len();

            if indent != min_indent {
                continue;
            }

            let t = strip_inline_hash_comment(line.trim());

            if is_keyword_block_closer(t) {
                if open_depth != 1 {
                    valid_kw = false;
                    break;
                }

                open_depth = 0;
                saw_closed = true;
            } else if is_mid_block_clause(t) {
                if open_depth != 1 {
                    valid_kw = false;
                    break;
                }
            } else if !t.starts_with('@') {
                if open_depth != 0 {
                    valid_kw = false;
                    break;
                }

                kw_keys.insert(format!("endblock:{t}"));
                open_depth = 1;
            }
        }

        if valid_kw && saw_closed && open_depth == 0 && !kw_keys.is_empty() {
            return Some(StructuralBlockSignature {
                keys: kw_keys,
                rotation: base_rotation,
                min_indent,
            });
        }
    }

    // Section / stanza blocks (`[section]` in INI/TOML files or `[[package]]` in lockfiles).
    let (section_ordered, section_rotation): (Vec<&String>, CyclicRotation) = if base_rotation
        == CyclicRotation::None
        && !is_section_header_line(non_comment[0].trim())
        && is_section_header_line(non_comment[non_comment.len() - 1].trim())
    {
        let mut trail_start = non_comment.len();

        while trail_start > 0 && is_section_header_line(non_comment[trail_start - 1].trim()) {
            trail_start -= 1;
        }

        let headers: Vec<String> = non_comment[trail_start..]
            .iter()
            .map(|s| (*s).clone())
            .collect();
        let mut rot = non_comment[trail_start..].to_vec();
        rot.extend_from_slice(&non_comment[..trail_start]);
        (rot, CyclicRotation::TrailingSections(headers))
    } else {
        (non_comment.clone(), base_rotation.clone())
    };

    if is_section_header_line(section_ordered[0].trim()) {
        let mut sec_keys = std::collections::BTreeSet::new();
        let mut valid_sec = true;
        let mut cur_body: Vec<String> = Vec::new();

        for (idx, line) in section_ordered.iter().enumerate() {
            let indent = line.len() - line.trim_start().len();
            let t = strip_inline_hash_comment(line.trim());

            if indent == min_indent && is_section_header_line(t) {
                if idx > 0 && !is_ini_toml_section_body(&cur_body) {
                    valid_sec = false;
                    break;
                }

                cur_body.clear();

                if t.starts_with("[[") && t.ends_with("]]") {
                    let next_hint = section_ordered
                        .get(idx + 1)
                        .map(|n| strip_inline_hash_comment(n.trim()))
                        .unwrap_or("");
                    sec_keys.insert(format!("section:{t}::{next_hint}"));
                } else {
                    sec_keys.insert(format!("section:{t}"));
                }
            } else {
                cur_body.push((*line).clone());
            }
        }

        if valid_sec && is_ini_toml_section_body(&cur_body) && !sec_keys.is_empty() {
            return Some(StructuralBlockSignature {
                keys: sec_keys,
                rotation: section_rotation,
                min_indent,
            });
        }
    }

    // Indentation-scoped blocks (YAML, `pnpm-lock.yaml`, `yarn.lock`, `pubspec.lock`, `Podfile.lock`,
    // Python `def`/`class`/`if`, Nim, Makefile targets, etc.).
    let has_deeper = non_comment
        .iter()
        .any(|l| (l.len() - l.trim_start().len()) > min_indent);

    if has_deeper {
        let first_min_idx = non_comment
            .iter()
            .position(|l| (l.len() - l.trim_start().len()) == min_indent)
            .unwrap_or(0);
        let last_indent = non_comment[non_comment.len() - 1].len()
            - non_comment[non_comment.len() - 1].trim_start().len();

        let (indent_ordered, indent_rotation): (Vec<&String>, CyclicRotation) = if base_rotation
            == CyclicRotation::None
            && first_min_idx > 0
            && last_indent > min_indent
        {
            let mut rot = non_comment[first_min_idx..].to_vec();
            rot.extend_from_slice(&non_comment[..first_min_idx]);
            (rot, CyclicRotation::LeadingDeeperIndent(min_indent))
        } else {
            (non_comment, base_rotation)
        };

        let mut indent_keys = std::collections::BTreeSet::new();
        let mut valid_indent = true;

        for (idx, line) in indent_ordered.iter().enumerate() {
            let indent = line.len() - line.trim_start().len();

            if indent != min_indent {
                continue;
            }

            let t = strip_inline_hash_comment(line.trim());
            let next_indent = indent_ordered
                .get(idx + 1)
                .map(|n| n.len() - n.trim_start().len());

            if (t.starts_with('@') || t.ends_with(',')) && next_indent == Some(min_indent) {
                continue;
            }

            if !is_indentation_block_header(t) {
                valid_indent = false;
                break;
            }

            if !next_indent.is_some_and(|ni| ni > min_indent) {
                valid_indent = false;
                break;
            }

            if is_mid_block_clause(t) {
                if indent_keys.is_empty() {
                    valid_indent = false;
                    break;
                }

                continue;
            }

            indent_keys.insert(format!("indent:{t}"));
        }

        if valid_indent && !indent_keys.is_empty() {
            return Some(StructuralBlockSignature {
                keys: indent_keys,
                rotation: indent_rotation,
                min_indent,
            });
        }
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
    let mut base_indent: Option<usize> = None;

    for line in left.iter().chain(right.iter()) {
        if !line.trim().is_empty() {
            let ind = line.len() - line.trim_start().len();

            if let Some(expected) = base_indent {
                if ind != expected {
                    return None;
                }
            } else {
                base_indent = Some(ind);
            }
        }
    }

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
            if out
                .last()
                .is_none_or(|(_, last_l)| !last_l.trim().is_empty())
            {
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

    if (t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ")) && !t.ends_with(':') {
        return Some(format!("list:{t}"));
    }

    if !t.ends_with('{') && !t.ends_with('(') && !t.ends_with(':') {
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

        if let Some((k, v)) = split_top_level_kv(without_comma) {
            if !v.is_empty() {
                return Some(format!("kv:{}", normalize_key(k)));
            }
        }

        return Some(format!("elem:{without_comma}"));
    }

    if !t.ends_with('{') && !t.ends_with('(') && !t.ends_with(':') {
        if let Some((k, v)) = split_top_level_kv(t) {
            let norm = normalize_key(k);

            if !norm.is_empty() && !v.is_empty() {
                return Some(format!("kv:{norm}"));
            }
        }
    }

    None
}

/// Attempts to reconcile 3 versions of a single line (`base`, `left`, `right`) via pipe-union
/// merging, delimited-skeleton merging, span normalization, or safe sub-line token diff3.
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

/// Reconciles 3-way edits to an explicit TypeScript pipe-delimited type union line
/// (`type X = "a" | "b";` or `export type X = "a" | "b";`).
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

    if has_unkeyed_replacement_conflict(&b_items, &l_items, &r_items) {
        return None;
    }

    let merged = merge_keyed_items(&b_items, &l_items, &r_items)?;
    let semi = if l_semi || r_semi || b_semi { ";" } else { "" };
    Some(format!(
        "{} = {}{}",
        l_lhs.trim_end(),
        merged.join(" | "),
        semi
    ))
}

/// Returns `true` if `s` contains `//` or `/*` comment markers outside quoted string literals.
fn contains_line_or_block_comment(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
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

        if matches!(ch, '"' | '`') || (ch == '\'' && is_single_quote_literal_start(s, idx)) {
            in_quote = Some(ch);
            continue;
        }

        if ch == '/' && matches!(bytes.get(idx + 1), Some(b'/' | b'*')) {
            return true;
        }
    }

    false
}

/// Splits an explicit TypeScript type-union assignment line (`type T = A | B` or
/// `export type T = A | B`) into `(lhs, rhs)` around the assignment `'='`.
fn split_type_union_line(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();

    if !(trimmed.starts_with("type ") || trimmed.starts_with("export type ")) {
        return None;
    }

    if contains_line_or_block_comment(line) {
        return None;
    }

    let bytes = line.as_bytes();
    let mut eq_pos = None;

    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'='
            && bytes.get(idx + 1) != Some(&b'=')
            && bytes.get(idx + 1) != Some(&b'>')
            && (idx == 0 || !matches!(bytes[idx - 1], b'=' | b'!' | b'<' | b'>'))
        {
            eq_pos = Some(idx);
            break;
        }
    }

    let eq_idx = eq_pos?;
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

/// Returns `true` if both `left` and `right` removed the same base item while each adding
/// distinct replacement items (indicating a value replacement conflict rather than additions).
fn has_unkeyed_replacement_conflict(
    base: &[(String, String)],
    left: &[(String, String)],
    right: &[(String, String)],
) -> bool {
    let base_keys: std::collections::BTreeSet<&str> =
        base.iter().map(|(k, _)| k.as_str()).collect();
    let left_keys: std::collections::BTreeSet<&str> =
        left.iter().map(|(k, _)| k.as_str()).collect();
    let right_keys: std::collections::BTreeSet<&str> =
        right.iter().map(|(k, _)| k.as_str()).collect();

    let both_removed_same = base_keys
        .iter()
        .any(|k| !left_keys.contains(k) && !right_keys.contains(k));
    let left_added = left_keys.iter().any(|k| !base_keys.contains(k));
    let right_added = right_keys.iter().any(|k| !base_keys.contains(k));

    both_removed_same && left_added && right_added
}

/// Tuple `(opening_delimiter_char, start_byte_offset, end_byte_offset_exclusive)` for a top-level delimited span.
type DelimSpan = (char, usize, usize);

/// Returns the final ASCII identifier word of `prefix.trim_end()`.
fn trailing_identifier_word(prefix: &str) -> &str {
    let trimmed = prefix.trim_end();
    let start = trimmed
        .char_indices()
        .rev()
        .find(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '_'))
        .map(|(idx, c)| idx + c.len_utf8())
        .unwrap_or(0);
    &trimmed[start..]
}

/// Returns `true` if a `(...)` span preceded by `before` is a control-flow condition or
/// expression operator (`if (...)`, `while (...)`, `for (...)`, etc.) that must never be
/// comma-merged.
fn is_control_flow_paren(before: &str) -> bool {
    let word = trailing_identifier_word(before);
    matches!(
        word,
        "if" | "elif"
            | "while"
            | "for"
            | "switch"
            | "match"
            | "catch"
            | "until"
            | "unless"
            | "return"
            | "yield"
            | "await"
            | "throw"
            | "typeof"
            | "sizeof"
            | "assert"
            | "synchronized"
            | "lock"
            | "using"
            | "with"
            | "when"
    )
}

/// Returns `true` if a `[...]` span preceded by `before` is an array/map subscript expression
/// (`arr[1]`, `get()[i]`, `m["k"]`) rather than an array literal or attribute.
fn is_array_subscript(before: &str) -> bool {
    let trimmed = before.trim_end();

    if trimmed.is_empty() {
        return false;
    }

    let Some(last_ch) = trimmed.chars().last() else {
        return false;
    };

    if matches!(
        last_ch,
        '=' | ':'
            | ','
            | '('
            | '['
            | '{'
            | '>'
            | '#'
            | '@'
            | '?'
            | '|'
            | '&'
            | '+'
            | '-'
            | '*'
            | '/'
            | ';'
            | '!'
            | '~'
            | '^'
            | '%'
    ) {
        return false;
    }

    let word = trailing_identifier_word(trimmed);

    if matches!(
        word,
        "return"
            | "yield"
            | "await"
            | "in"
            | "of"
            | "case"
            | "else"
            | "and"
            | "or"
            | "not"
            | "is"
            | "let"
            | "const"
            | "var"
            | "type"
            | "export"
            | "import"
    ) {
        return false;
    }

    last_ch.is_ascii_alphanumeric() || matches!(last_ch, '_' | ')' | ']' | '}' | '"' | '\'')
}

/// Returns `true` if a `{...}` span preceded by `before` is a code block body
/// (`fn tax() { a * 15 }`, `if cond { ... }`, `() => { ... }`) rather than an import,
/// destructuring, object/map, or set literal.
fn is_code_block_brace(before: &str, inners: &[&str]) -> bool {
    for inner in inners {
        if split_top_level(inner, ';').len() > 1 {
            return true;
        }
    }

    let trimmed = before.trim();

    if trimmed == "import"
        || trimmed.starts_with("import ")
        || trimmed == "export"
        || trimmed == "export type"
        || trimmed.starts_with("export {")
        || trimmed.starts_with("export type {")
        || trimmed == "use"
        || trimmed.starts_with("use ")
        || trimmed.starts_with("pub use ")
        || trimmed.starts_with("pub(crate) use ")
    {
        return false;
    }

    if trimmed.ends_with(')') || trimmed.ends_with("=>") {
        return true;
    }

    let last_word = trailing_identifier_word(trimmed);

    if matches!(
        last_word,
        "else" | "try" | "finally" | "loop" | "do" | "unsafe" | "async" | "move"
    ) {
        return true;
    }

    for kw in [
        "fn ",
        "pub fn ",
        "pub(crate) fn ",
        "async fn ",
        "pub async fn ",
        "func ",
        "function ",
        "def ",
        "if ",
        "while ",
        "for ",
        "match ",
        "switch ",
        "class ",
        "struct ",
        "enum ",
        "impl ",
        "trait ",
        "mod ",
    ] {
        if trimmed.starts_with(kw) {
            return true;
        }
    }

    false
}

/// Returns `true` if the surrounding syntax indicates an explicit import, export, destructuring,
/// or attribute/derive list where even a 1-item base list can safely accept additive insertions.
fn is_explicit_list_syntax(open_ch: char, before: &str, after: &str) -> bool {
    let b = before.trim();
    let a = after.trim();

    if b == "import"
        || b.starts_with("import ")
        || b == "export"
        || b.starts_with("export ")
        || b == "use"
        || b.starts_with("use ")
        || b.starts_with("pub use ")
        || b.starts_with("pub(crate) use ")
        || b == "from"
        || b.starts_with("from ")
    {
        return true;
    }

    if b.starts_with("#[") || b.starts_with("#![") || b.starts_with('@') {
        return true;
    }

    if matches!(open_ch, '{' | '[')
        && (b == "const" || b == "let" || b == "var" || b.starts_with("export const"))
        && a.starts_with('=')
    {
        return true;
    }

    false
}

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
        let before = &trimmed[..start];
        let after = &trimmed[end..];
        let inner = &trimmed[start + 1..end - 1];

        if open_ch == '{' && is_code_block_brace(before, &[inner]) {
            return None;
        }

        if open_ch == '[' && is_array_subscript(before) {
            return None;
        }

        if open_ch == '(' && is_control_flow_paren(before) {
            return None;
        }

        parse_comma_items(open_ch, inner)?;

        let close_ch = match open_ch {
            '(' => ')',
            '[' => ']',
            '{' => '}',
            _ => return None,
        };
        let outer_before = normalize_skeleton_fragment(before);
        let outer_after = normalize_skeleton_fragment(after);

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
    quote_norm.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Merges two lines (`left` and `right`, with optional `base`) that share the same outer
/// [`delimited_skeleton_key`] by performing a 3-way keyed union over their comma-separated items.
pub fn try_merge_same_skeleton_line(base: Option<&str>, left: &str, right: &str) -> Option<String> {
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

        let b_inner_opt = base.and_then(|b| {
            let b_spans = find_top_level_delim_spans(b);
            let &(b_open, bs, be) = b_spans.last()?;
            (b_open == open_ch).then_some(&b[bs + 1..be - 1])
        });
        let l_before = &left[..l_start];
        let l_after = &left[l_end..];
        let l_inner = &left[l_start + 1..l_end - 1];
        let r_inner = &right[r_start + 1..r_end - 1];

        let merged_inner =
            merge_delimited_items(open_ch, l_before, l_after, b_inner_opt, l_inner, r_inner)?;
        let mut out = left.to_string();
        out.replace_range(l_start + 1..l_end - 1, &merged_inner);
        return Some(out);
    }

    let l_imp = left.find(" import ")?;
    let r_imp = right.find(" import ")?;
    let b_inner_opt = base.and_then(|b| b.find(" import ").map(|bi| &b[bi + 8..]));
    let l_before = &left[..l_imp + 8];
    let l_inner = &left[l_imp + 8..];
    let r_inner = &right[r_imp + 8..];
    let merged_inner = merge_delimited_items('(', l_before, "", b_inner_opt, l_inner, r_inner)?;
    Some(format!("{l_before}{merged_inner}"))
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

    if b_spans.is_empty() || b_spans.len() != l_spans.len() || b_spans.len() != r_spans.len() {
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

        let b_before = &base[..b_start];
        let b_after = &base[b_end..];
        let merged_inner =
            merge_delimited_items(open_ch, b_before, b_after, Some(b_inner), l_inner, r_inner)?;
        b_out.replace_range(b_start + 1..b_end - 1, &merged_inner);
        l_out.replace_range(l_start + 1..l_end - 1, &merged_inner);
        r_out.replace_range(r_start + 1..r_end - 1, &merged_inner);
    }

    Some((b_out, l_out, r_out))
}

/// Scans `line` for balanced top-level delimiter spans `(...)`, `[...]`, and `{...}`,
/// ignoring delimiters inside quoted string literals while handling Rust lifetimes safely.
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

        if matches!(ch, '"' | '`') || (ch == '\'' && is_single_quote_literal_start(line, idx)) {
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

/// Parsed comma-separated item inside a delimited span.
struct ParsedDelimItem {
    /// Lookup key (KV property name when `is_kv == true`, otherwise the item itself).
    key: String,
    /// Original trimmed item text.
    raw: String,
    /// `true` when this item is a structured `key: value` / `key = value` / `key => value` binding.
    is_kv: bool,
}

/// Safely merges comma-separated items inside a delimited span (`open_ch` in `{'(', '[', '{'}`),
/// rejecting control-flow conditions, array subscripts, code blocks, single-item replacements,
/// positional slot replacements, and duplicate nested Rust `use` subtrees.
fn merge_delimited_items(
    open_ch: char,
    before_span: &str,
    after_span: &str,
    base_inner_opt: Option<&str>,
    left_inner: &str,
    right_inner: &str,
) -> Option<String> {
    if open_ch == '(' && is_control_flow_paren(before_span) {
        return None;
    }

    if open_ch == '[' && is_array_subscript(before_span) {
        return None;
    }

    let base_inner = base_inner_opt.unwrap_or("");

    if open_ch == '{' && is_code_block_brace(before_span, &[base_inner, left_inner, right_inner]) {
        return None;
    }

    let b_parsed = parse_comma_items(open_ch, base_inner)?;
    let l_parsed = parse_comma_items(open_ch, left_inner)?;
    let r_parsed = parse_comma_items(open_ch, right_inner)?;

    let all_kv = !l_parsed.is_empty()
        && !r_parsed.is_empty()
        && b_parsed.iter().all(|it| it.is_kv)
        && l_parsed.iter().all(|it| it.is_kv)
        && r_parsed.iter().all(|it| it.is_kv);
    let explicit_list = is_explicit_list_syntax(open_ch, before_span, after_span);

    if base_inner_opt.is_some() {
        // When base has fewer than 2 items, only allow merging for explicit imports,
        // destructuring, attributes, or key-value maps (and never for single-item replacements).
        if b_parsed.len() < 2 && !explicit_list && !all_kv {
            return None;
        }
    } else {
        // Add/add without base: require explicit list, KV map, or shared anchor item with >= 2 items.
        let l_keys: std::collections::BTreeSet<&str> =
            l_parsed.iter().map(|it| it.key.as_str()).collect();
        let shares_item = r_parsed.iter().any(|it| l_keys.contains(it.key.as_str()));

        if !explicit_list && !all_kv && !shares_item {
            return None;
        }
    }

    // Guard against nested Rust `use` subtree collisions (`collections::{A}` vs `collections::{B}`).
    if open_ch == '{'
        && (before_span.trim_start().starts_with("use ")
            || before_span.trim_start().starts_with("pub use ")
            || before_span.trim_start().starts_with("pub(crate) use "))
    {
        for l_it in &l_parsed {
            if let Some((l_prefix, _)) = l_it.raw.split_once("::{") {
                for r_it in &r_parsed {
                    if let Some((r_prefix, _)) = r_it.raw.split_once("::{") {
                        if l_prefix == r_prefix && l_it.raw != r_it.raw {
                            return None;
                        }
                    }
                }
            }
        }
    }

    let b_items: Vec<(String, String)> = b_parsed
        .iter()
        .map(|it| (it.key.clone(), it.raw.clone()))
        .collect();
    let l_items: Vec<(String, String)> = l_parsed
        .iter()
        .map(|it| (it.key.clone(), it.raw.clone()))
        .collect();
    let r_items: Vec<(String, String)> = r_parsed
        .iter()
        .map(|it| (it.key.clone(), it.raw.clone()))
        .collect();

    if base_inner_opt.is_some() && !all_kv {
        // Reject when both sides replaced the same unkeyed base item with different values.
        let b_unkeyed: Vec<(String, String)> = b_parsed
            .iter()
            .filter(|it| !it.is_kv)
            .map(|it| (it.key.clone(), it.raw.clone()))
            .collect();
        let l_unkeyed: Vec<(String, String)> = l_parsed
            .iter()
            .filter(|it| !it.is_kv)
            .map(|it| (it.key.clone(), it.raw.clone()))
            .collect();
        let r_unkeyed: Vec<(String, String)> = r_parsed
            .iter()
            .filter(|it| !it.is_kv)
            .map(|it| (it.key.clone(), it.raw.clone()))
            .collect();

        if has_unkeyed_replacement_conflict(&b_unkeyed, &l_unkeyed, &r_unkeyed) {
            return None;
        }

        // Reject positional slot collisions where both sides changed the same index differently.
        let min_pos = b_parsed.len().min(l_parsed.len()).min(r_parsed.len());

        for idx in 0..min_pos {
            if !b_parsed[idx].is_kv
                && l_parsed[idx].raw != b_parsed[idx].raw
                && r_parsed[idx].raw != b_parsed[idx].raw
                && l_parsed[idx].raw != r_parsed[idx].raw
            {
                return None;
            }
        }

        // For positional `(...)` calls/params, refuse if both sides mutated/removed unkeyed base
        // items (prevents swapping argument order like `charge(200, v)`).
        if open_ch == '(' && !explicit_list {
            let l_removed_unkeyed = b_unkeyed
                .iter()
                .any(|(k, _)| !l_unkeyed.iter().any(|(lk, _)| lk == k));
            let r_removed_unkeyed = b_unkeyed
                .iter()
                .any(|(k, _)| !r_unkeyed.iter().any(|(rk, _)| rk == k));

            if l_removed_unkeyed && r_removed_unkeyed {
                return None;
            }
        }
    }

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

/// Splits `inner` on top-level commas and returns [`ParsedDelimItem`]s if all keys are unique.
fn parse_comma_items(open_ch: char, inner: &str) -> Option<Vec<ParsedDelimItem>> {
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

        let (key, is_kv) = classify_delimited_item_key(open_ch, item_str);

        if !seen_keys.insert(key.clone()) {
            return None;
        }

        items.push(ParsedDelimItem {
            key,
            raw: item_str.to_string(),
            is_kv,
        });
    }

    Some(items)
}

/// Extracts `(lookup_key, is_kv)` for a comma-separated item inside `open_ch`.
fn classify_delimited_item_key(open_ch: char, item: &str) -> (String, bool) {
    if matches!(open_ch, '{' | '(') {
        if let Some((k, _)) = split_top_level_kv(item) {
            let norm = normalize_key(k);

            if !norm.is_empty() {
                return (norm, true);
            }
        }
    }

    if open_ch == '[' && item.contains("=>") {
        if let Some((k, _)) = split_top_level_kv(item) {
            let norm = normalize_key(k);

            if !norm.is_empty() {
                return (norm, true);
            }
        }
    }

    (item.to_string(), false)
}

/// Splits `item` around a top-level key-value separator (`:`, whitespace-padded `::`, `=`, or `=>`),
/// rejecting compound assignment operators (`+=`, `-=`, etc.), ternary `? ... : ...` colons,
/// and properly handling Rust lifetimes (`'a`, `'static`) and `<`/`>` comparisons.
fn split_top_level_kv(item: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut angle_depth = 0usize;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    let mut ternary_q_idx: Option<usize> = None;
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

        if matches!(ch, '"' | '`') || (ch == '\'' && is_single_quote_literal_start(item, idx)) {
            in_quote = Some(ch);
            continue;
        }

        if matches!(ch, '(' | '[' | '{') {
            depth += 1;
            continue;
        }

        if matches!(ch, ')' | ']' | '}') {
            depth = depth.saturating_sub(1);
            continue;
        }

        if ch == '<' && is_generic_angle_open(item, idx) {
            angle_depth += 1;
            continue;
        }

        if ch == '>' && angle_depth > 0 {
            let prev = idx.checked_sub(1).and_then(|p| bytes.get(p)).copied();

            if !matches!(prev, Some(b'=' | b'-')) {
                angle_depth -= 1;
                continue;
            }
        }

        if depth == 0 && angle_depth == 0 {
            if ch == '?' {
                ternary_q_idx = Some(idx);
                continue;
            }

            if ch == ':' {
                if bytes.get(idx + 1) == Some(&b'=') {
                    continue;
                }

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

                // Skip ternary `cond ? a : b` colons while keeping TypeScript optional properties `foo?: T`.
                if let Some(q_idx) = ternary_q_idx {
                    if !item[q_idx + 1..idx].trim().is_empty() {
                        ternary_q_idx = None;
                        continue;
                    }
                }

                return Some((item[..idx].trim(), item[idx + 1..].trim()));
            }

            if ch == '=' {
                if bytes.get(idx + 1) == Some(&b'=')
                    || (idx > 0
                        && matches!(
                            bytes[idx - 1],
                            b'=' | b'!'
                                | b'<'
                                | b'>'
                                | b'+'
                                | b'-'
                                | b'*'
                                | b'/'
                                | b'%'
                                | b'|'
                                | b'&'
                                | b'^'
                                | b'~'
                                | b':'
                        ))
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

/// Splits `input` by `sep` only at nesting depth `0` outside string literals, properly handling
/// Rust lifetimes and `<`/`>` comparisons.
fn split_top_level(input: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut angle_depth = 0usize;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    let mut last_idx = 0usize;
    let bytes = input.as_bytes();

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

        if matches!(ch, '"' | '`') || (ch == '\'' && is_single_quote_literal_start(input, idx)) {
            in_quote = Some(ch);
            continue;
        }

        if matches!(ch, '(' | '[' | '{') {
            depth += 1;
            continue;
        }

        if matches!(ch, ')' | ']' | '}') {
            depth = depth.saturating_sub(1);
            continue;
        }

        if ch == '<' && is_generic_angle_open(input, idx) {
            angle_depth += 1;
            continue;
        }

        if ch == '>' && angle_depth > 0 {
            let prev = idx.checked_sub(1).and_then(|p| bytes.get(p)).copied();

            if !matches!(prev, Some(b'=' | b'-')) {
                angle_depth -= 1;
                continue;
            }
        }

        if depth == 0 && angle_depth == 0 && ch == sep {
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
    let base_map: std::collections::BTreeMap<&str, &str> =
        base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let left_map: std::collections::BTreeMap<&str, &str> =
        left.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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

/// Returns `true` if `tok` is a real alphabetic/identifier anchor word (not punctuation,
/// whitespace, quoted string literal, URL, or numeric/version literal).
fn is_word_anchor(tok: &str) -> bool {
    if tok.is_empty()
        || tok.starts_with('"')
        || tok.starts_with('\'')
        || tok.starts_with('`')
        || tok.contains("://")
    {
        return false;
    }

    let mut has_alpha = false;

    for ch in tok.chars() {
        if ch.is_alphabetic() {
            has_alpha = true;
        } else if !ch.is_alphanumeric() && ch != '_' {
            return false;
        }
    }

    has_alpha
}

/// Returns `true` if `edit` in `b_toks` is directly adjacent to an arithmetic operator token
/// (`+`, `-`, `*`, `/`, `%`).
fn is_edit_adjacent_to_arithmetic(b_toks: &[&str], edit: &LineEdit) -> bool {
    let is_arith_op = |t: &str| matches!(t, "+" | "-" | "*" | "/" | "%");

    let mut idx = edit.base_start;

    while idx > 0 {
        idx -= 1;
        let tok = b_toks[idx];

        if tok.trim().is_empty() {
            continue;
        }

        if is_arith_op(tok) {
            return true;
        }

        break;
    }

    let mut fwd = edit.base_end;

    while fwd < b_toks.len() {
        let tok = b_toks[fwd];

        if tok.trim().is_empty() {
            fwd += 1;
            continue;
        }

        if is_arith_op(tok) {
            return true;
        }

        break;
    }

    for tok in &b_toks[edit.base_start..edit.base_end.min(b_toks.len())] {
        if is_arith_op(tok) {
            return true;
        }
    }

    false
}

/// Performs safe sub-line token diff3 on a single line, keeping string literals, URLs, and
/// numeric/version literals atomic and requiring at least one unchanged alphabetic identifier
/// anchor separating concurrent edits.
fn try_token_merge_line(base: &str, left: &str, right: &str) -> Option<String> {
    let b_toks = tokenize_line_atomic(base);
    let l_toks = tokenize_line_atomic(left);
    let r_toks = tokenize_line_atomic(right);

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

    // Require every non-identical pair of left and right token edits to be separated by at least
    // one real unchanged alphabetic/identifier anchor token (never punctuation-only separators),
    // and refuse to token-merge concurrent arithmetic edits.
    for le in &l_edits {
        for re in &r_edits {
            if le.base_start == re.base_start
                && le.base_end == re.base_end
                && le.replacement == re.replacement
            {
                continue;
            }

            if is_edit_adjacent_to_arithmetic(&b_toks, le)
                && is_edit_adjacent_to_arithmetic(&b_toks, re)
            {
                return None;
            }

            let (first_end, second_start) = if le.base_end <= re.base_start {
                (le.base_end, re.base_start)
            } else if re.base_end <= le.base_start {
                (re.base_end, le.base_start)
            } else {
                return None;
            };

            let has_word_anchor = b_toks[first_end..second_start]
                .iter()
                .any(|t| is_word_anchor(t));

            if !has_word_anchor {
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

/// Splits `line` into tokens while keeping quoted string literals, URLs, and numeric/semver
/// literals as single indivisible tokens so partial edits inside them always conflict cleanly.
fn tokenize_line_atomic(line: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        let ch = line[i..].chars().next().unwrap_or('\0');

        // Quoted string literal as a single atomic token.
        if ch == '"' || ch == '`' || (ch == '\'' && is_single_quote_literal_start(line, i)) {
            let start = i;
            i += ch.len_utf8();
            let mut escaped = false;

            while i < len {
                let c = line[i..].chars().next().unwrap_or('\0');
                i += c.len_utf8();

                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == ch {
                    break;
                }
            }

            tokens.push(&line[start..i]);
            continue;
        }

        // Whitespace run.
        if ch.is_whitespace() {
            let start = i;

            while i < len {
                let c = line[i..].chars().next().unwrap_or('\0');

                if !c.is_whitespace() {
                    break;
                }

                i += c.len_utf8();
            }

            tokens.push(&line[start..i]);
            continue;
        }

        // URL (`scheme://...`) as a single atomic token.
        if ch.is_ascii_alphabetic() {
            let mut scan = i;

            while scan < len
                && (bytes[scan].is_ascii_alphanumeric()
                    || matches!(bytes[scan], b'+' | b'-' | b'.'))
            {
                scan += 1;
            }

            if scan + 2 < len && &bytes[scan..scan + 3] == b"://" {
                let start = i;
                i = scan + 3;

                while i < len
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(
                        bytes[i],
                        b'"' | b'\'' | b'`' | b'<' | b'>' | b')' | b']' | b'}' | b',' | b';'
                    )
                {
                    i += 1;
                }

                tokens.push(&line[start..i]);
                continue;
            }
        }

        // Numeric, decimal, IP, or semver literal (`1.15`, `0.0.0.0`, `1.0.200`, `v1.2.3-rc.1`).
        let is_version_v =
            (ch == 'v' || ch == 'V') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit);

        if ch.is_ascii_digit() || is_version_v {
            let start = i;
            i += 1;

            while i < len {
                let b = bytes[i];
                let continues = b.is_ascii_alphanumeric()
                    || b == b'_'
                    || (b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_alphanumeric))
                    || ((b == b'-' || b == b'+')
                        && bytes.get(i + 1).is_some_and(u8::is_ascii_alphanumeric)
                        && (line[start..i].contains('.')
                            || bytes[i - 1] == b'e'
                            || bytes[i - 1] == b'E'));

                if continues {
                    i += 1;
                } else {
                    break;
                }
            }

            tokens.push(&line[start..i]);
            continue;
        }

        // Regular identifier/word token.
        if ch.is_alphanumeric() || ch == '_' {
            let start = i;

            while i < len {
                let c = line[i..].chars().next().unwrap_or('\0');

                if !(c.is_alphanumeric() || c == '_') {
                    break;
                }

                i += c.len_utf8();
            }

            tokens.push(&line[start..i]);
            continue;
        }

        // Single punctuation token.
        let end = i + ch.len_utf8();
        tokens.push(&line[i..end]);
        i = end;
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

    raw_ranges.sort_unstable();

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
    fn multi_hunk_overlap_window_never_rewinds_or_duplicates_lines() {
        let base = "l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\n";
        let left = "l0_L\nl1_L\nl2_L\nl3_L\nl4_L\nl5_L\nl6\nl7\n";
        let right = "l0\nl1_R\nl2\nl3\nl4_R\nl5\nl6\nl7\n";

        let res = merge_text_3way(base, left, right, "left", "right");
        assert!(res.has_conflicts);
        assert_eq!(res.conflict_count, 1);
        assert_eq!(res.content.matches("l6").count(), 1);
        assert_eq!(res.content.matches("l7").count(), 1);
        assert!(res.content.contains("l1_R"));
        assert!(res.content.contains("l4_R"));
        assert!(res.content.ends_with("l6\nl7\n"));
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
    fn conflicting_expressions_closures_versions_and_subscripts_always_conflict() {
        // Conflicting call argument scalar replacement must NOT comma-concatenate into `charge(u, 150, 200)`.
        let r_call = merge_text_3way(
            "charge(u, 100);\n",
            "charge(u, 150);\n",
            "charge(u, 200);\n",
            "a",
            "b",
        );
        assert!(
            r_call.has_conflicts,
            "expected conflict, got:\n{}",
            r_call.content
        );

        // Conflicting `if (...)` condition must NOT become `if (retries > 5, retries > 10)`.
        let r_if = merge_text_3way(
            "if (retries > 3) {\n",
            "if (retries > 5) {\n",
            "if (retries > 10) {\n",
            "a",
            "b",
        );
        assert!(
            r_if.has_conflicts,
            "expected conflict, got:\n{}",
            r_if.content
        );

        // Conflicting array subscript must NOT become `arr[1, 2]`.
        let r_sub = merge_text_3way(
            "let x = arr[0];\n",
            "let x = arr[1];\n",
            "let x = arr[2];\n",
            "a",
            "b",
        );
        assert!(
            r_sub.has_conflicts,
            "expected conflict, got:\n{}",
            r_sub.content
        );

        // Conflicting inline function body must NOT become `fn tax() { a * 15, a * 20 }`.
        let r_fn = merge_text_3way(
            "fn tax() { a * 10 }\n",
            "fn tax() { a * 15 }\n",
            "fn tax() { a * 20 }\n",
            "a",
            "b",
        );
        assert!(
            r_fn.has_conflicts,
            "expected conflict, got:\n{}",
            r_fn.content
        );

        // Rust closure with `|` must NOT be treated as a TypeScript pipe union!
        let r_closure = merge_text_3way(
            "let f = |x| x * 2;\n",
            "let f = |x| x * 3;\n",
            "let f = |x| x * 4;\n",
            "a",
            "b",
        );
        assert!(
            r_closure.has_conflicts,
            "expected closure conflict, got:\n{}",
            r_closure.content
        );

        // Semver strings & decimals & URLs must NOT token-merge on `.` or `/`.
        let r_ver = merge_text_3way(
            "serde = \"1.0.100\"\n",
            "serde = \"1.0.200\"\n",
            "serde = \"1.1.100\"\n",
            "a",
            "b",
        );
        assert!(
            r_ver.has_conflicts,
            "expected semver conflict, got:\n{}",
            r_ver.content
        );

        let r_num = merge_text_3way(
            "let rate = 1.05;\n",
            "let rate = 1.15;\n",
            "let rate = 0.25;\n",
            "a",
            "b",
        );
        assert!(
            r_num.has_conflicts,
            "expected decimal conflict, got:\n{}",
            r_num.content
        );

        let r_url = merge_text_3way(
            "const URL = \"https://api.v1.example.com/users\";\n",
            "const URL = \"https://api.v2.example.com/users\";\n",
            "const URL = \"https://api.v1.example.com/accounts\";\n",
            "a",
            "b",
        );
        assert!(
            r_url.has_conflicts,
            "expected URL conflict, got:\n{}",
            r_url.content
        );
    }

    #[test]
    fn csharp_authorize_attribute_is_never_stripped_as_shared_section_header() {
        let base = "public class ApiController {\n}\n";
        let left = "public class ApiController {\n    [Authorize]\n    public void GetUsers() {\n    }\n}\n";
        let right = "public class ApiController {\n    [Authorize]\n    public void GetInvoices() {\n    }\n}\n";

        let res = merge_text_3way(base, left, right, "a", "b");
        if !res.has_conflicts {
            assert_eq!(
                res.content.matches("[Authorize]").count(),
                2,
                "both C# endpoints must keep [Authorize], got:\n{}",
                res.content
            );
        }
    }

    #[test]
    fn colliding_css_rules_always_conflict() {
        let base = ".header {\n  color: black;\n}\n";
        let left = ".header {\n  color: black;\n}\n\n.btn {\n  color: blue;\n}\n\n.card {\n  padding: 4px;\n}\n";
        let right = ".header {\n  color: black;\n}\n\n.btn {\n  color: red;\n}\n\n.badge {\n  margin: 2px;\n}\n";

        let res = merge_text_3way(base, left, right, "a", "b");
        assert!(
            res.has_conflicts,
            "colliding .btn CSS rules must conflict, got:\n{}",
            res.content
        );
    }

    #[test]
    fn asymmetric_cyclic_rotation_never_teleports_code_between_blocks() {
        let base = "fn alpha() {\n    start();\n}\n";
        let left = "fn alpha() {\n    start();\n}\n\nfn beta() {\n    run_beta();\n}\n";
        let right = "fn alpha() {\n    start();\n    if ready {\n        finish();\n    }\n}\n";

        let res = merge_text_3way(base, left, right, "a", "b");
        if !res.has_conflicts {
            let beta_idx = res.content.find("fn beta()").expect("beta");
            let ready_idx = res.content.find("if ready").expect("ready");
            assert!(
                ready_idx < beta_idx,
                "if ready must stay inside alpha, not teleport into beta:\n{}",
                res.content
            );
        }
    }

    #[test]
    fn split_kv_rejects_compound_assignments_and_ternaries_and_handles_lifetimes() {
        assert!(split_top_level_kv("count += 1").is_none());
        assert!(split_top_level_kv("flags |= MASK").is_none());
        assert!(split_top_level_kv("cond ? a : b").is_none());
        assert_eq!(
            split_top_level_kv("name?: string"),
            Some(("name?", "string"))
        );
        assert_eq!(split_top_level_kv("key: &'a str"), Some(("key", "&'a str")));
        assert_eq!(
            split_top_level("a < b, c > d", ','),
            vec!["a < b", " c > d"]
        );
        assert_eq!(
            split_top_level("m: HashMap<String, i32>, n: usize", ','),
            vec!["m: HashMap<String, i32>", " n: usize"]
        );
    }

    #[test]
    fn split_disjoint_trailing_blocks_never_resurrects_deleted_base_block() {
        let base = "fn a() {\n    old_a();\n}\nfn b() {\n    old_b();\n}\n";
        let left = "fn a() {\n    left_a();\n}\nfn b() {\n    old_b();\n}\n";
        let right = "fn a() {\n    right_a();\n}\n";

        let res = merge_text_3way(base, left, right, "a", "b");
        assert!(res.has_conflicts);
        let marker_end = res.content.find(">>>>>>> b").expect("end marker");
        assert!(
            !res.content[marker_end..].contains("fn b()"),
            "deleted fn b() must not be emitted outside conflict markers:\n{}",
            res.content
        );
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

    #[test]
    fn indentation_scoped_blocks_merge_yaml_lockfiles_and_python_cleanly() {
        let base_pnpm = r#"lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      react:
        specifier: ^18.2.0
        version: 18.2.0

packages:
  /react@18.2.0:
    resolution: {integrity: sha512-react}
    dev: false
"#;
        let left_pnpm = r#"lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      react:
        specifier: ^18.2.0
        version: 18.2.0
      axios:
        specifier: ^1.6.0
        version: 1.6.8

packages:
  /react@18.2.0:
    resolution: {integrity: sha512-react}
    dev: false
  /axios@1.6.8:
    resolution: {integrity: sha512-axios}
    dev: false
"#;
        let right_pnpm = r#"lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      react:
        specifier: ^18.2.0
        version: 18.2.0
      zod:
        specifier: ^3.22.0
        version: 3.22.4

packages:
  /react@18.2.0:
    resolution: {integrity: sha512-react}
    dev: false
  /zod@3.22.4:
    resolution: {integrity: sha512-zod}
    dev: false
"#;
        let res_pnpm = merge_text_3way(base_pnpm, left_pnpm, right_pnpm, "agent-a", "agent-b");
        assert!(
            !res_pnpm.has_conflicts,
            "pnpm-lock.yaml merge should be conflict-free, got:\n{}",
            res_pnpm.content
        );
        assert!(
            res_pnpm
                .content
                .contains("axios:\n        specifier: ^1.6.0")
                && res_pnpm
                    .content
                    .contains("zod:\n        specifier: ^3.22.0")
                && res_pnpm.content.contains("/axios@1.6.8:")
                && res_pnpm.content.contains("/zod@3.22.4:"),
            "all pnpm importer and package blocks must be preserved:\n{}",
            res_pnpm.content
        );
        assert_eq!(
            res_pnpm.content.matches("dev: false").count(),
            3,
            "shared child lines inside distinct YAML blocks must not be deduplicated across blocks:\n{}",
            res_pnpm.content
        );

        let right_conflict = r#"lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      react:
        specifier: ^18.2.0
        version: 18.2.0
      axios:
        specifier: ^1.7.0
        version: 1.7.2

packages:
  /react@18.2.0:
    resolution: {integrity: sha512-react}
    dev: false
"#;
        let res_conflict =
            merge_text_3way(base_pnpm, left_pnpm, right_conflict, "agent-a", "agent-b");
        assert!(
            res_conflict.has_conflicts,
            "conflicting versions for the same YAML block key must conflict:\n{}",
            res_conflict.content
        );
    }

    #[test]
    fn shared_yaml_container_headers_and_yarn_lock_blocks_merge_cleanly() {
        let base_yaml = "name: service\nversion: 1.0.0\n";
        let left_yaml =
            "name: service\nversion: 1.0.0\ndependencies:\n  axios:\n    version: 1.6.8\n";
        let right_yaml =
            "name: service\nversion: 1.0.0\ndependencies:\n  zod:\n    version: 3.22.4\n";
        let res_yaml = merge_text_3way(base_yaml, left_yaml, right_yaml, "a", "b");
        assert!(
            !res_yaml.has_conflicts,
            "shared new YAML container header should merge cleanly:\n{}",
            res_yaml.content
        );
        assert_eq!(res_yaml.content.matches("dependencies:").count(), 1);
        assert!(res_yaml.content.contains("axios:") && res_yaml.content.contains("zod:"));

        let base_yarn = r#"# yarn lockfile v1

"lodash@^4.17.21":
  version "4.17.21"
  resolved "https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz"
"#;
        let left_yarn = r#"# yarn lockfile v1

"lodash@^4.17.21":
  version "4.17.21"
  resolved "https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz"

"axios@^1.6.0":
  version "1.6.8"
  resolved "https://registry.yarnpkg.com/axios/-/axios-1.6.8.tgz"
"#;
        let right_yarn = r#"# yarn lockfile v1

"lodash@^4.17.21":
  version "4.17.21"
  resolved "https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz"

"zod@^3.22.0":
  version "3.22.4"
  resolved "https://registry.yarnpkg.com/zod/-/zod-3.22.4.tgz"
"#;
        let res_yarn = merge_text_3way(base_yarn, left_yarn, right_yarn, "a", "b");
        assert!(
            !res_yarn.has_conflicts,
            "yarn.lock should merge cleanly:\n{}",
            res_yarn.content
        );
        assert!(
            res_yarn.content.contains("\"axios@^1.6.0\":")
                && res_yarn.content.contains("\"zod@^3.22.0\":")
        );
    }

    #[test]
    fn keyword_closed_and_section_stanza_blocks_merge_cleanly() {
        let base_rb = "class OrderService\n  def initialize(repo)\n    @repo = repo\n  end\nend\n";
        let left_rb = "class OrderService\n  def initialize(repo)\n    @repo = repo\n  end\n\n  def charge(order)\n    if order.valid?\n      @repo.save(order)\n    else\n      false\n    end\n  end\nend\n";
        let right_rb = "class OrderService\n  def initialize(repo)\n    @repo = repo\n  end\n\n  def refund(order)\n    @repo.delete(order)\n  end\nend\n";
        let res_rb = merge_text_3way(base_rb, left_rb, right_rb, "a", "b");
        assert!(
            !res_rb.has_conflicts,
            "Ruby end-delimited methods should merge cleanly, got:\n{}",
            res_rb.content
        );
        assert!(
            res_rb.content.contains("def charge(order)")
                && res_rb.content.contains("def refund(order)")
        );

        let base_poetry = r#"[[package]]
name = "certifi"
version = "2024.2.2"
"#;
        let left_poetry = r#"[[package]]
name = "certifi"
version = "2024.2.2"

[[package]]
name = "fastapi"
version = "0.110.0"
"#;
        let right_poetry = r#"[[package]]
name = "certifi"
version = "2024.2.2"

[[package]]
name = "uvicorn"
version = "0.29.0"
"#;
        let res_poetry = merge_text_3way(base_poetry, left_poetry, right_poetry, "a", "b");
        assert!(
            !res_poetry.has_conflicts,
            "poetry.lock [[package]] stanzas should merge cleanly, got:\n{}",
            res_poetry.content
        );
        assert_eq!(res_poetry.content.matches("[[package]]").count(), 3);
        assert!(
            res_poetry.content.contains("name = \"fastapi\"")
                && res_poetry.content.contains("name = \"uvicorn\"")
        );
    }

    #[test]
    fn multibyte_utf8_before_delimiters_never_panics() {
        assert_eq!(trailing_identifier_word("caf\u{00e9}"), "");
        assert_eq!(trailing_identifier_word("caf\u{00e9}_ok"), "_ok");
        assert_eq!(trailing_identifier_word("// r\u{00e9}sum\u{00e9} "), "");

        let base_call = "caf\u{00e9}(a, b)\n";
        let left_call = "caf\u{00e9}(a, b, c)\n";
        let right_call = "caf\u{00e9}(a, b, d)\n";
        let res_call = merge_text_3way(base_call, left_call, right_call, "a", "b");
        assert!(
            !res_call.has_conflicts,
            "expected clean merge without panic, got:\n{}",
            res_call.content
        );
        assert_eq!(res_call.content, "caf\u{00e9}(a, b, c, d)\n");

        let base_comment = "// r\u{00e9}sum\u{00e9} (v1)\n";
        let left_comment = "// r\u{00e9}sum\u{00e9} (v2)\n";
        let right_comment = "// r\u{00e9}sum\u{00e9} (v3)\n";
        let res_comment = merge_text_3way(base_comment, left_comment, right_comment, "a", "b");
        assert!(res_comment.has_conflicts);

        let base_bracket = "caf\u{00e9}[0]\n";
        let left_bracket = "caf\u{00e9}[1]\n";
        let right_bracket = "caf\u{00e9}[2]\n";
        let res_bracket = merge_text_3way(base_bracket, left_bracket, right_bracket, "a", "b");
        assert!(res_bracket.has_conflicts);

        let base_brace = "caf\u{00e9} { x: 1, y: 2 }\n";
        let left_brace = "caf\u{00e9} { x: 1, y: 2, z: 3 }\n";
        let right_brace = "caf\u{00e9} { x: 1, y: 2, w: 4 }\n";
        let res_brace = merge_text_3way(base_brace, left_brace, right_brace, "a", "b");
        assert!(
            !res_brace.has_conflicts,
            "expected clean brace merge without panic, got:\n{}",
            res_brace.content
        );
    }
}
