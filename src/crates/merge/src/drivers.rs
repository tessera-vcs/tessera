//! Specialized 3-way structural merge drivers for TOML, JSON, `Cargo.lock`, and sequential migrations.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::text_merge::merge_text_3way_strict;

/// Outcome of a specialized manifest or lockfile merge driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverMergeOutcome {
    /// Merged file content.
    pub content: String,
    /// `true` if conflicting leaf values could not be resolved by the driver.
    pub has_conflicts: bool,
    /// List of conflicting key paths (e.g., `dependencies.serde`).
    pub conflicting_keys: Vec<String>,
}

/// Attempts to merge `base`, `left`, and `right` using a specialized manifest/lockfile driver based on `path`.
///
/// Returns `Some(DriverMergeOutcome)` if `path` is handled by a specialized driver
/// (`Cargo.lock`, `*.toml`, `*.json`), or `None` if it should proceed to the Tree-sitter AST merger.
pub fn try_merge_with_driver(
    path: &str,
    base: &str,
    left: &str,
    right: &str,
) -> Option<DriverMergeOutcome> {
    try_merge_with_driver_labeled(path, base, left, right, "ours", "theirs")
}

/// Attempts to merge `base`, `left`, and `right` using a specialized manifest/lockfile driver
/// with explicit conflict marker labels.
pub(crate) fn try_merge_with_driver_labeled(
    path: &str,
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> Option<DriverMergeOutcome> {
    let file_name = path.rsplit('/').next().unwrap_or(path);

    if file_name == "Cargo.lock" {
        return merge_cargo_lock_3way_labeled(base, left, right, left_label, right_label);
    }

    if std::path::Path::new(file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
    {
        return merge_toml_3way_labeled(base, left, right, left_label, right_label);
    }

    if std::path::Path::new(file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        return merge_json_3way_labeled(base, left, right, left_label, right_label);
    }

    None
}

/// Performs a format-preserving structural 3-way merge on TOML files (`Cargo.toml`, `.git/tessera/config.toml`, etc.).
pub fn merge_toml_3way(base: &str, left: &str, right: &str) -> Option<DriverMergeOutcome> {
    merge_toml_3way_labeled(base, left, right, "ours", "theirs")
}

/// Performs a format-preserving structural 3-way merge on TOML files with explicit conflict labels.
fn merge_toml_3way_labeled(
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> Option<DriverMergeOutcome> {
    if left == right || right == base {
        return Some(DriverMergeOutcome {
            content: left.to_string(),
            has_conflicts: false,
            conflicting_keys: Vec::new(),
        });
    }

    if left == base {
        return Some(DriverMergeOutcome {
            content: right.to_string(),
            has_conflicts: false,
            conflicting_keys: Vec::new(),
        });
    }

    let base_val: toml::Value = if base.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(base).ok()?
    };
    let left_val: toml::Value = toml::from_str(left).ok()?;
    let right_val: toml::Value = toml::from_str(right).ok()?;

    let mut conflicts = Vec::new();
    let merged_val = merge_toml_value("", Some(&base_val), &left_val, &right_val, &mut conflicts);

    if !conflicts.is_empty() {
        return Some(build_strict_conflict_outcome(
            base,
            left,
            right,
            left_label,
            right_label,
            conflicts,
        ));
    }

    let strict = merge_text_3way_strict(base, left, right, left_label, right_label);

    if !strict.has_conflicts {
        if let Ok(parsed) = toml::from_str::<toml::Value>(&strict.content) {
            if parsed == merged_val {
                return Some(DriverMergeOutcome {
                    content: strict.content,
                    has_conflicts: false,
                    conflicting_keys: Vec::new(),
                });
            }
        }
    }

    if let Some(surgical) = try_surgical_toml_merge(base, left, right, &merged_val) {
        return Some(DriverMergeOutcome {
            content: surgical,
            has_conflicts: false,
            conflicting_keys: Vec::new(),
        });
    }

    let serialized = serialize_toml_preserving_header_and_order(left, right, &merged_val)?;

    Some(DriverMergeOutcome {
        content: serialized,
        has_conflicts: false,
        conflicting_keys: Vec::new(),
    })
}

/// Builds a conflicted [`DriverMergeOutcome`] using strict line diff3 without running
/// sub-line token heuristics so conflicting scalar values never synthesize bogus values.
fn build_strict_conflict_outcome(
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
    conflicting_keys: Vec<String>,
) -> DriverMergeOutcome {
    let strict = merge_text_3way_strict(base, left, right, left_label, right_label);

    let content = if strict.has_conflicts {
        strict.content
    } else {
        let mut out = String::new();
        out.push_str(&format!("<<<<<<< {left_label}\n"));
        out.push_str(left);

        if !left.ends_with('\n') && !left.is_empty() {
            out.push('\n');
        }

        out.push_str("=======\n");
        out.push_str(right);

        if !right.ends_with('\n') && !right.is_empty() {
            out.push('\n');
        }

        out.push_str(&format!(">>>>>>> {right_label}\n"));
        out
    };

    DriverMergeOutcome {
        content,
        has_conflicts: true,
        conflicting_keys,
    }
}

/// Recursively merges three [`toml::Value`] trees (`base`, `left`, `right`), recording any
/// conflicting dotted key paths into `conflicts`.
fn merge_toml_value(
    key_path: &str,
    base: Option<&toml::Value>,
    left: &toml::Value,
    right: &toml::Value,
    conflicts: &mut Vec<String>,
) -> toml::Value {
    if left == right || base == Some(right) {
        return left.clone();
    }

    if base == Some(left) {
        return right.clone();
    }

    match (left, right) {
        (toml::Value::Table(l_map), toml::Value::Table(r_map)) => {
            let b_map_opt = match base {
                Some(toml::Value::Table(m)) => Some(m),
                Some(_) => {
                    conflicts.push(key_path.to_string());
                    return left.clone();
                }
                None => None,
            };

            let mut all_keys = BTreeSet::new();

            if let Some(b_map) = b_map_opt {
                for k in b_map.keys() {
                    all_keys.insert(k.clone());
                }
            }

            for k in l_map.keys().chain(r_map.keys()) {
                all_keys.insert(k.clone());
            }

            let mut out = toml::map::Map::new();

            for k in all_keys {
                let child_path = if key_path.is_empty() {
                    k.clone()
                } else {
                    format!("{key_path}.{k}")
                };
                let b_entry = b_map_opt.and_then(|m| m.get(&k));

                match (b_entry, l_map.get(&k), r_map.get(&k)) {
                    (Some(b), Some(l), Some(r)) => {
                        out.insert(k, merge_toml_value(&child_path, Some(b), l, r, conflicts));
                    }
                    (None, Some(l), Some(r)) => {
                        out.insert(k, merge_toml_value(&child_path, None, l, r, conflicts));
                    }
                    (None, Some(l), None) => {
                        out.insert(k, l.clone());
                    }
                    (None, None, Some(r)) => {
                        out.insert(k, r.clone());
                    }
                    (Some(b), Some(l), None) => {
                        if l != b {
                            conflicts.push(child_path);
                            out.insert(k, l.clone());
                        }
                    }
                    (Some(b), None, Some(r)) => {
                        if r != b {
                            conflicts.push(child_path);
                            out.insert(k, r.clone());
                        }
                    }
                    (Some(_), None, None) | (None, None, None) => {}
                }
            }

            toml::Value::Table(out)
        }
        (toml::Value::Array(l_arr), toml::Value::Array(r_arr)) => {
            let b_arr = match base {
                Some(toml::Value::Array(a)) => a.as_slice(),
                Some(_) => {
                    conflicts.push(key_path.to_string());
                    return left.clone();
                }
                None => &[],
            };

            let is_complex =
                |v: &toml::Value| matches!(v, toml::Value::Table(_) | toml::Value::Array(_));

            match merge_scalar_arrays_3way(b_arr, l_arr, r_arr, is_complex) {
                Some(merged) => toml::Value::Array(merged),
                None => {
                    conflicts.push(key_path.to_string());
                    left.clone()
                }
            }
        }
        _ => {
            conflicts.push(key_path.to_string());
            left.clone()
        }
    }
}

/// Merges three arrays without deduplicating legitimate repeated elements (such as `rustflags`)
/// and flags a conflict on modified arrays of tables/objects or incompatible in-place replacements.
fn merge_scalar_arrays_3way<T, F>(
    base: &[T],
    left: &[T],
    right: &[T],
    is_complex: F,
) -> Option<Vec<T>>
where
    T: Clone + PartialEq,
    F: Fn(&T) -> bool,
{
    if left == right || right == base {
        return Some(left.to_vec());
    }

    if left == base {
        return Some(right.to_vec());
    }

    if base.iter().any(&is_complex) || left.iter().any(&is_complex) || right.iter().any(&is_complex)
    {
        return None;
    }

    let left_extends_base = left.len() >= base.len() && &left[..base.len()] == base;
    let right_extends_base = right.len() >= base.len() && &right[..base.len()] == base;

    if left_extends_base && right_extends_base {
        let l_added = &left[base.len()..];
        let r_added = &right[base.len()..];

        let has_internal_dups = |slice: &[T]| -> bool {
            for i in 0..slice.len() {
                if slice[i + 1..].contains(&slice[i]) {
                    return true;
                }
            }

            false
        };

        let is_set_like =
            !has_internal_dups(base) && !has_internal_dups(left) && !has_internal_dups(right);

        let mut merged = left.to_vec();

        if is_set_like {
            for item in r_added {
                if !l_added.contains(item) && !base.contains(item) {
                    merged.push(item.clone());
                }
            }
        } else {
            merged.extend(r_added.iter().cloned());
        }

        return Some(merged);
    }

    let has_dups = |slice: &[T]| -> bool {
        for i in 0..slice.len() {
            if slice[i + 1..].contains(&slice[i]) {
                return true;
            }
        }

        false
    };

    if has_dups(base) || has_dups(left) || has_dups(right) {
        return None;
    }

    let left_is_pure_subseq = is_ordered_subsequence(left, base);
    let right_is_pure_subseq = is_ordered_subsequence(right, base);

    if left_is_pure_subseq && right_extends_base {
        let mut merged = left.to_vec();
        merged.extend(right[base.len()..].iter().cloned());
        return Some(merged);
    }

    if right_is_pure_subseq && left_extends_base {
        let mut merged = right.to_vec();
        merged.extend(left[base.len()..].iter().cloned());
        return Some(merged);
    }

    if left_is_pure_subseq && right_is_pure_subseq {
        let merged: Vec<T> = base
            .iter()
            .filter(|item| left.contains(item) && right.contains(item))
            .cloned()
            .collect();
        return Some(merged);
    }

    None
}

/// Returns `true` if `needle` is an ordered subsequence of `haystack`.
fn is_ordered_subsequence<T: PartialEq>(needle: &[T], haystack: &[T]) -> bool {
    let mut h_idx = 0usize;

    for item in needle {
        let mut found = false;

        while h_idx < haystack.len() {
            if &haystack[h_idx] == item {
                h_idx += 1;
                found = true;
                break;
            }

            h_idx += 1;
        }

        if !found {
            return false;
        }
    }

    true
}

/// Extracts leading `#` comment lines (plus their separating blank line) from a TOML document.
fn extract_leading_toml_comments(source: &str) -> String {
    let mut header_lines = Vec::new();
    let mut saw_comment = false;

    for line in source.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with('#') {
            saw_comment = true;
            header_lines.push(line);
        } else if trimmed.is_empty() && saw_comment {
            header_lines.push(line);
        } else {
            break;
        }
    }

    if !saw_comment {
        return String::new();
    }

    let mut out = header_lines.join("\n");

    if !out.ends_with('\n') {
        out.push('\n');
    }

    out
}

/// Raw section parsed from a TOML document for comment- and order-preserving surgical merges.
#[derive(Debug, Clone)]
struct RawTomlSection {
    /// Normalized section header key (`""` for root table, or `"[dependencies]"` etc.).
    header: String,
    /// Raw header line (empty for root).
    header_line: Option<String>,
    /// Leading comment lines attached directly to this section header.
    leading_comments: Vec<String>,
    /// Body lines belonging to this section.
    body_lines: Vec<String>,
}

/// Splits a TOML document into raw top-level sections when no duplicate `[[array]]` table headers
/// appear (which are handled by structural merge).
fn split_raw_toml_sections(source: &str) -> Option<Vec<RawTomlSection>> {
    let mut sections: Vec<RawTomlSection> = Vec::new();
    let mut cur_header = String::new();
    let mut cur_header_line: Option<String> = None;
    let mut cur_leading_comments: Vec<String> = Vec::new();
    let mut cur_body: Vec<String> = Vec::new();
    let mut seen_headers: BTreeSet<String> = BTreeSet::new();

    let mut in_triple_double = false;
    let mut in_triple_single = false;

    for line in source.lines() {
        let trimmed = line.trim();
        let was_in_multiline = in_triple_double || in_triple_single;
        update_toml_multiline_state(line, &mut in_triple_double, &mut in_triple_single);

        if !was_in_multiline && !in_triple_double && !in_triple_single && trimmed.starts_with('[') {
            if trimmed.starts_with("[[") {
                return None;
            }

            if let Some(close_idx) = trimmed.find(']') {
                let after = trimmed[close_idx + 1..].trim();

                if after.is_empty() || after.starts_with('#') {
                    let hdr_key = trimmed[..=close_idx].to_string();

                    if !seen_headers.insert(hdr_key.clone()) {
                        return None;
                    }

                    let mut trailing_comments = Vec::new();

                    while let Some(last) = cur_body.last() {
                        if last.trim().starts_with('#') {
                            if let Some(popped) = cur_body.pop() {
                                trailing_comments.push(popped);
                            }
                        } else {
                            break;
                        }
                    }

                    trailing_comments.reverse();

                    sections.push(RawTomlSection {
                        header: cur_header,
                        header_line: cur_header_line,
                        leading_comments: cur_leading_comments,
                        body_lines: cur_body,
                    });

                    cur_header = hdr_key;
                    cur_header_line = Some(line.to_string());
                    cur_leading_comments = trailing_comments;
                    cur_body = Vec::new();
                    continue;
                }
            }
        }

        cur_body.push(line.to_string());
    }

    sections.push(RawTomlSection {
        header: cur_header,
        header_line: cur_header_line,
        leading_comments: cur_leading_comments,
        body_lines: cur_body,
    });

    Some(sections)
}

/// Updates TOML triple-quoted string state across a single line.
fn update_toml_multiline_state(
    line: &str,
    in_triple_double: &mut bool,
    in_triple_single: &mut bool,
) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut in_single_line_double = false;
    let mut in_single_line_single = false;

    while i < bytes.len() {
        if *in_triple_double {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }

            if i + 2 < bytes.len() && &bytes[i..i + 3] == b"\"\"\"" {
                *in_triple_double = false;
                i += 3;
                continue;
            }

            i += 1;
            continue;
        }

        if *in_triple_single {
            if i + 2 < bytes.len() && &bytes[i..i + 3] == b"'''" {
                *in_triple_single = false;
                i += 3;
                continue;
            }

            i += 1;
            continue;
        }

        if in_single_line_double {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }

            if bytes[i] == b'"' {
                in_single_line_double = false;
            }

            i += 1;
            continue;
        }

        if in_single_line_single {
            if bytes[i] == b'\'' {
                in_single_line_single = false;
            }

            i += 1;
            continue;
        }

        if bytes[i] == b'#' {
            break;
        }

        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"\"\"\"" {
            *in_triple_double = true;
            i += 3;
            continue;
        }

        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"'''" {
            *in_triple_single = true;
            i += 3;
            continue;
        }

        if bytes[i] == b'"' {
            in_single_line_double = true;
        } else if bytes[i] == b'\'' {
            in_single_line_single = true;
        }

        i += 1;
    }
}

/// Keyed assignment blocks and trailing comments/blanks parsed from a single TOML section body.
type ParsedTomlSectionEntries = (Vec<(String, Vec<String>)>, Vec<String>);

/// Parses key-value assignment blocks within a single TOML section body, keeping leading comments
/// attached to their target assignment.
fn parse_toml_section_entries(lines: &[String]) -> Option<ParsedTomlSectionEntries> {
    let mut entries: Vec<(String, Vec<String>)> = Vec::new();
    let mut pending_prefix: Vec<String> = Vec::new();
    let mut seen_keys: BTreeSet<String> = BTreeSet::new();
    let mut idx = 0usize;

    while idx < lines.len() {
        let trimmed = lines[idx].trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            pending_prefix.push(lines[idx].clone());
            idx += 1;
            continue;
        }

        let key = extract_toml_assignment_key(trimmed)?;

        if !seen_keys.insert(key.clone()) {
            return None;
        }

        let mut block = std::mem::take(&mut pending_prefix);
        let mut depth = 0i32;
        let mut in_td = false;
        let mut in_ts = false;

        loop {
            let cur_line = &lines[idx];
            block.push(cur_line.clone());
            update_toml_assignment_balance(cur_line, &mut depth, &mut in_td, &mut in_ts);
            idx += 1;

            if (depth <= 0 && !in_td && !in_ts) || idx >= lines.len() {
                break;
            }
        }

        if depth != 0 || in_td || in_ts {
            return None;
        }

        entries.push((key, block));
    }

    Some((entries, pending_prefix))
}

/// Extracts the normalized key name from a TOML `key = value` line.
fn extract_toml_assignment_key(trimmed: &str) -> Option<String> {
    let mut in_quote: Option<u8> = None;
    let mut escaped = false;
    let bytes = trimmed.as_bytes();

    for (idx, &b) in bytes.iter().enumerate() {
        if let Some(q) = in_quote {
            if escaped {
                escaped = false;
            } else if q == b'"' && b == b'\\' {
                escaped = true;
            } else if b == q {
                in_quote = None;
            }

            continue;
        }

        if b == b'"' || b == b'\'' {
            in_quote = Some(b);
            continue;
        }

        if b == b'#' {
            return None;
        }

        if b == b'=' {
            let key_raw = trimmed[..idx].trim();

            if key_raw.is_empty() {
                return None;
            }

            return Some(key_raw.to_string());
        }
    }

    None
}

/// Updates bracket/brace nesting depth and triple-quoted string state across a TOML assignment line.
fn update_toml_assignment_balance(
    line: &str,
    depth: &mut i32,
    in_triple_double: &mut bool,
    in_triple_single: &mut bool,
) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut in_double = false;
    let mut in_single = false;

    while i < bytes.len() {
        if *in_triple_double {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }

            if i + 2 < bytes.len() && &bytes[i..i + 3] == b"\"\"\"" {
                *in_triple_double = false;
                i += 3;
                continue;
            }

            i += 1;
            continue;
        }

        if *in_triple_single {
            if i + 2 < bytes.len() && &bytes[i..i + 3] == b"'''" {
                *in_triple_single = false;
                i += 3;
                continue;
            }

            i += 1;
            continue;
        }

        if in_double {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }

            if bytes[i] == b'"' {
                in_double = false;
            }

            i += 1;
            continue;
        }

        if in_single {
            if bytes[i] == b'\'' {
                in_single = false;
            }

            i += 1;
            continue;
        }

        if bytes[i] == b'#' {
            break;
        }

        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"\"\"\"" {
            *in_triple_double = true;
            i += 3;
            continue;
        }

        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"'''" {
            *in_triple_single = true;
            i += 3;
            continue;
        }

        match bytes[i] {
            b'"' => in_double = true,
            b'\'' => in_single = true,
            b'[' | b'{' => *depth += 1,
            b']' | b'}' => *depth -= 1,
            _ => {}
        }

        i += 1;
    }
}

/// Attempts a comment-, inline-table-, and key-order-preserving surgical merge on TOML documents
/// and verifies that the stitched text parses identically to `expected_val`.
fn try_surgical_toml_merge(
    base: &str,
    left: &str,
    right: &str,
    expected_val: &toml::Value,
) -> Option<String> {
    let base_secs = split_raw_toml_sections(base)?;
    let left_secs = split_raw_toml_sections(left)?;
    let right_secs = split_raw_toml_sections(right)?;

    let base_map: BTreeMap<&str, &RawTomlSection> =
        base_secs.iter().map(|s| (s.header.as_str(), s)).collect();
    let left_map: BTreeMap<&str, &RawTomlSection> =
        left_secs.iter().map(|s| (s.header.as_str(), s)).collect();
    let right_map: BTreeMap<&str, &RawTomlSection> =
        right_secs.iter().map(|s| (s.header.as_str(), s)).collect();

    let mut ordered_headers: Vec<&str> = Vec::new();

    for sec in left_secs.iter().chain(right_secs.iter()) {
        if !ordered_headers.contains(&sec.header.as_str()) {
            ordered_headers.push(sec.header.as_str());
        }
    }

    let mut out_lines: Vec<String> = Vec::new();

    for hdr in ordered_headers {
        let b_sec = base_map.get(hdr).copied();
        let l_sec = left_map.get(hdr).copied();
        let r_sec = right_map.get(hdr).copied();

        let rendered_section: Option<Vec<String>> = match (b_sec, l_sec, r_sec) {
            (Some(b), Some(l), Some(r)) => {
                if l.body_lines == r.body_lines || r.body_lines == b.body_lines {
                    Some(render_raw_toml_section(l))
                } else if l.body_lines == b.body_lines {
                    Some(render_raw_toml_section(r))
                } else {
                    let merged_body =
                        merge_toml_section_body(&b.body_lines, &l.body_lines, &r.body_lines)?;
                    Some(render_toml_section_with_body(l, merged_body))
                }
            }
            (None, Some(l), Some(r)) => {
                if l.body_lines == r.body_lines {
                    Some(render_raw_toml_section(l))
                } else {
                    let merged_body = merge_toml_section_body(&[], &l.body_lines, &r.body_lines)?;
                    Some(render_toml_section_with_body(l, merged_body))
                }
            }
            (None, Some(l), None) => Some(render_raw_toml_section(l)),
            (None, None, Some(r)) => Some(render_raw_toml_section(r)),
            (Some(b), Some(l), None) => {
                if l.body_lines == b.body_lines {
                    None
                } else {
                    return None;
                }
            }
            (Some(b), None, Some(r)) => {
                if r.body_lines == b.body_lines {
                    None
                } else {
                    return None;
                }
            }
            (Some(_), None, None) | (None, None, None) => None,
        };

        if let Some(sec_lines) = rendered_section {
            if !out_lines.is_empty()
                && !sec_lines.is_empty()
                && out_lines.last().is_some_and(|l| !l.trim().is_empty())
                && sec_lines.first().is_some_and(|l| !l.trim().is_empty())
            {
                out_lines.push(String::new());
            }

            out_lines.extend(sec_lines);
        }
    }

    let mut stitched = out_lines.join("\n");

    if (left.ends_with('\n') || right.ends_with('\n')) && !stitched.is_empty() {
        stitched.push('\n');
    }

    let parsed = toml::from_str::<toml::Value>(&stitched).ok()?;

    if &parsed == expected_val {
        Some(stitched)
    } else {
        None
    }
}

/// Renders a [`RawTomlSection`] back to lines preserving comments and formatting.
fn render_raw_toml_section(sec: &RawTomlSection) -> Vec<String> {
    render_toml_section_with_body(sec, sec.body_lines.clone())
}

/// Renders a [`RawTomlSection`] with a replacement body line sequence.
fn render_toml_section_with_body(sec: &RawTomlSection, body: Vec<String>) -> Vec<String> {
    let mut lines = Vec::new();
    lines.extend(sec.leading_comments.iter().cloned());

    if let Some(hdr) = &sec.header_line {
        lines.push(hdr.clone());
    }

    lines.extend(body);
    lines
}

/// Merges key-value entries inside a single TOML section body while preserving `left`'s key order,
/// inline tables, and comments.
fn merge_toml_section_body(
    base_lines: &[String],
    left_lines: &[String],
    right_lines: &[String],
) -> Option<Vec<String>> {
    let (b_entries, _) = parse_toml_section_entries(base_lines)?;
    let (l_entries, l_tail) = parse_toml_section_entries(left_lines)?;
    let (r_entries, r_tail) = parse_toml_section_entries(right_lines)?;

    let b_map: BTreeMap<&str, &[String]> = b_entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_slice()))
        .collect();
    let l_map: BTreeMap<&str, &[String]> = l_entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_slice()))
        .collect();
    let r_map: BTreeMap<&str, &[String]> = r_entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_slice()))
        .collect();

    let mut ordered_keys: Vec<&str> = Vec::new();

    for (k, _) in l_entries.iter().chain(r_entries.iter()) {
        if !ordered_keys.contains(&k.as_str()) {
            ordered_keys.push(k.as_str());
        }
    }

    let mut out_lines: Vec<String> = Vec::new();

    for key in ordered_keys {
        match (
            b_map.get(key).copied(),
            l_map.get(key).copied(),
            r_map.get(key).copied(),
        ) {
            (Some(b), Some(l), Some(r)) => {
                if l == r || r == b {
                    out_lines.extend(l.iter().cloned());
                } else if l == b {
                    out_lines.extend(r.iter().cloned());
                } else {
                    return None;
                }
            }
            (None, Some(l), Some(r)) => {
                if l == r {
                    out_lines.extend(l.iter().cloned());
                } else {
                    return None;
                }
            }
            (None, Some(l), None) => out_lines.extend(l.iter().cloned()),
            (None, None, Some(r)) => {
                let trimmed_r: Vec<String> = r
                    .iter()
                    .skip_while(|line| line.trim().is_empty())
                    .cloned()
                    .collect();
                out_lines.extend(trimmed_r);
            }
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

    if !l_tail.is_empty() {
        out_lines.extend(l_tail);
    } else if !r_tail.is_empty() {
        out_lines.extend(r_tail);
    }

    Some(out_lines)
}

/// Serializes a merged [`toml::Value`] preserving any leading `#` comment header from `left`/`right`
/// and preserving table key order from `left` (with newly added keys from `right` appended in order).
fn serialize_toml_preserving_header_and_order(
    left: &str,
    right: &str,
    merged_val: &toml::Value,
) -> Option<String> {
    let header_comments = {
        let l_hdr = extract_leading_toml_comments(left);

        if !l_hdr.is_empty() {
            l_hdr
        } else {
            extract_leading_toml_comments(right)
        }
    };

    if let toml::Value::Table(root_tbl) = merged_val {
        let mut rendered = header_comments.clone();
        let l_order = extract_toml_key_order(left);
        let r_order = extract_toml_key_order(right);

        if write_ordered_toml_table(root_tbl, "", &l_order, &r_order, &mut rendered) {
            if let Ok(parsed) = toml::from_str::<toml::Value>(&rendered) {
                if &parsed == merged_val {
                    return Some(rendered);
                }
            }
        }
    }

    let fallback_body = toml::to_string_pretty(merged_val).ok()?;
    Some(format!("{header_comments}{fallback_body}"))
}

/// Scans raw TOML source to record observed table/key ordering per dotted table path.
fn extract_toml_key_order(source: &str) -> BTreeMap<String, Vec<String>> {
    let mut order: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current_table = String::new();
    let mut in_td = false;
    let mut in_ts = false;

    for line in source.lines() {
        let trimmed = line.trim();
        let was_in_ml = in_td || in_ts;
        update_toml_multiline_state(line, &mut in_td, &mut in_ts);

        if was_in_ml || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            if let Some(close_idx) = trimmed.find(']') {
                let tbl_name = trimmed[1..close_idx].trim().to_string();
                let entry = order.entry(String::new()).or_default();

                if let Some(top) = tbl_name.split('.').next() {
                    let top_clean = top.trim().trim_matches('"').trim_matches('\'').to_string();

                    if !entry.contains(&top_clean) {
                        entry.push(top_clean);
                    }
                }

                current_table = tbl_name;
                continue;
            }
        }

        if let Some(raw_key) = extract_toml_assignment_key(trimmed) {
            let clean_key = raw_key
                .split('.')
                .next()
                .unwrap_or(&raw_key)
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            let entry = order.entry(current_table.clone()).or_default();

            if !entry.contains(&clean_key) {
                entry.push(clean_key);
            }
        }
    }

    order
}

/// Writes a [`toml::map::Map`] in `left`-then-`right` key order (with scalars before subtables).
fn write_ordered_toml_table(
    tbl: &toml::map::Map<String, toml::Value>,
    table_path: &str,
    left_order: &BTreeMap<String, Vec<String>>,
    right_order: &BTreeMap<String, Vec<String>>,
    out: &mut String,
) -> bool {
    let mut ordered_keys: Vec<String> = Vec::new();

    if let Some(l_keys) = left_order.get(table_path) {
        for k in l_keys {
            if tbl.contains_key(k) && !ordered_keys.contains(k) {
                ordered_keys.push(k.clone());
            }
        }
    }

    if let Some(r_keys) = right_order.get(table_path) {
        for k in r_keys {
            if tbl.contains_key(k) && !ordered_keys.contains(k) {
                ordered_keys.push(k.clone());
            }
        }
    }

    for k in tbl.keys() {
        if !ordered_keys.contains(k) {
            ordered_keys.push(k.clone());
        }
    }

    let mut scalar_keys = Vec::new();
    let mut subtable_keys = Vec::new();

    for k in ordered_keys {
        if let Some(val) = tbl.get(&k) {
            let is_array_of_tables = matches!(
                val,
                toml::Value::Array(arr) if !arr.is_empty() && arr.iter().all(|v| matches!(v, toml::Value::Table(_)))
            );

            if matches!(val, toml::Value::Table(_)) || is_array_of_tables {
                subtable_keys.push(k);
            } else {
                scalar_keys.push(k);
            }
        }
    }

    for k in scalar_keys {
        let Some(val) = tbl.get(&k) else {
            continue;
        };

        let mut single = toml::map::Map::new();
        single.insert(k, val.clone());

        let Ok(fragment) = toml::to_string_pretty(&toml::Value::Table(single)) else {
            return false;
        };

        out.push_str(&fragment);
    }

    for k in subtable_keys {
        let Some(val) = tbl.get(&k) else {
            continue;
        };

        if !out.is_empty() && !out.ends_with("\n\n") {
            if !out.ends_with('\n') {
                out.push('\n');
            }

            out.push('\n');
        }

        let formatted_key = format_toml_key_segment(&k);
        let child_path = if table_path.is_empty() {
            formatted_key.clone()
        } else {
            format!("{table_path}.{formatted_key}")
        };

        match val {
            toml::Value::Table(sub) => {
                out.push_str(&format!("[{child_path}]\n"));

                if !write_ordered_toml_table(sub, &child_path, left_order, right_order, out) {
                    return false;
                }
            }
            toml::Value::Array(arr) => {
                for item in arr {
                    let toml::Value::Table(sub) = item else {
                        return false;
                    };

                    if !out.is_empty() && !out.ends_with("\n\n") {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }

                        out.push('\n');
                    }

                    out.push_str(&format!("[[{child_path}]]\n"));

                    if !write_ordered_toml_table(sub, &child_path, left_order, right_order, out) {
                        return false;
                    }
                }
            }
            _ => return false,
        }
    }

    true
}

/// Formats a single TOML key segment, quoting it if it contains non-bare-key characters.
fn format_toml_key_segment(key: &str) -> String {
    if !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        key.to_string()
    } else {
        serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
    }
}

/// Order-preserving JSON AST used to preserve original key order, number formatting, and
/// reject duplicate object keys during 3-way structural merges.
#[derive(Debug, Clone)]
enum OrderedJson {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// JSON number preserving its original raw source lexeme alongside parsed semantic value.
    Number {
        /// Original raw numeric text.
        raw: String,
        /// Parsed numeric representation for semantic equality.
        value: serde_json::Number,
    },
    /// JSON string.
    String(String),
    /// JSON array.
    Array(Vec<OrderedJson>),
    /// JSON object preserving exact key insertion order.
    Object(Vec<(String, OrderedJson)>),
}

impl PartialEq for OrderedJson {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Number { value: a, .. }, Self::Number { value: b, .. }) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => a == b,
            (Self::Object(a), Self::Object(b)) => {
                if a.len() != b.len() {
                    return false;
                }

                let a_map: BTreeMap<&str, &OrderedJson> =
                    a.iter().map(|(k, v)| (k.as_str(), v)).collect();
                let b_map: BTreeMap<&str, &OrderedJson> =
                    b.iter().map(|(k, v)| (k.as_str(), v)).collect();
                a_map == b_map
            }
            _ => false,
        }
    }
}

impl Eq for OrderedJson {}

/// Lightweight recursive-descent parser that parses valid JSON into [`OrderedJson`] while
/// rejecting duplicate keys within the same JSON object.
struct OrderedJsonParser<'a> {
    /// Source JSON text.
    src: &'a str,
    /// Raw UTF-8 bytes of `src`.
    bytes: &'a [u8],
    /// Current byte cursor position.
    pos: usize,
}

impl<'a> OrderedJsonParser<'a> {
    /// Parses `src` into an [`OrderedJson`] tree, returning `None` on syntax error or duplicate object keys.
    fn parse(src: &'a str) -> Option<OrderedJson> {
        let mut p = Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        };
        let val = p.parse_value()?;
        p.skip_ws();

        if p.pos != p.bytes.len() {
            return None;
        }

        Some(val)
    }

    /// Advances past JSON whitespace characters.
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len()
            && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n')
        {
            self.pos += 1;
        }
    }

    /// Parses a single JSON value.
    fn parse_value(&mut self) -> Option<OrderedJson> {
        self.skip_ws();
        let &b = self.bytes.get(self.pos)?;

        match b {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(OrderedJson::String),
            b't' => {
                if self.src[self.pos..].starts_with("true") {
                    self.pos += 4;
                    Some(OrderedJson::Bool(true))
                } else {
                    None
                }
            }
            b'f' => {
                if self.src[self.pos..].starts_with("false") {
                    self.pos += 5;
                    Some(OrderedJson::Bool(false))
                } else {
                    None
                }
            }
            b'n' => {
                if self.src[self.pos..].starts_with("null") {
                    self.pos += 4;
                    Some(OrderedJson::Null)
                } else {
                    None
                }
            }
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    /// Parses a JSON object while rejecting duplicate keys within the same object.
    fn parse_object(&mut self) -> Option<OrderedJson> {
        self.pos += 1;
        self.skip_ws();

        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();

        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Some(OrderedJson::Object(entries));
        }

        loop {
            self.skip_ws();
            let key = self.parse_string()?;

            if !seen.insert(key.clone()) {
                return None;
            }

            self.skip_ws();

            if self.bytes.get(self.pos) != Some(&b':') {
                return None;
            }

            self.pos += 1;
            let val = self.parse_value()?;
            entries.push((key, val));
            self.skip_ws();

            match self.bytes.get(self.pos) {
                Some(&b',') => {
                    self.pos += 1;
                }
                Some(&b'}') => {
                    self.pos += 1;
                    return Some(OrderedJson::Object(entries));
                }
                _ => return None,
            }
        }
    }

    /// Parses a JSON array.
    fn parse_array(&mut self) -> Option<OrderedJson> {
        self.pos += 1;
        self.skip_ws();

        let mut items = Vec::new();

        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Some(OrderedJson::Array(items));
        }

        loop {
            let val = self.parse_value()?;
            items.push(val);
            self.skip_ws();

            match self.bytes.get(self.pos) {
                Some(&b',') => {
                    self.pos += 1;
                }
                Some(&b']') => {
                    self.pos += 1;
                    return Some(OrderedJson::Array(items));
                }
                _ => return None,
            }
        }
    }

    /// Parses a JSON string token using `serde_json` for RFC 8259 unescaping.
    fn parse_string(&mut self) -> Option<String> {
        if self.bytes.get(self.pos) != Some(&b'"') {
            return None;
        }

        let start = self.pos;
        self.pos += 1;
        let mut escaped = false;

        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];

            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                let end = self.pos;
                self.pos += 1;
                return serde_json::from_str::<String>(&self.src[start..=end]).ok();
            }

            self.pos += 1;
        }

        None
    }

    /// Parses a JSON numeric literal preserving its raw source spelling.
    fn parse_number(&mut self) -> Option<OrderedJson> {
        let start = self.pos;

        while self.pos < self.bytes.len()
            && matches!(
                self.bytes[self.pos],
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'
            )
        {
            self.pos += 1;
        }

        let raw = &self.src[start..self.pos];
        let serde_json::Value::Number(value) =
            serde_json::from_str::<serde_json::Value>(raw).ok()?
        else {
            return None;
        };

        Some(OrderedJson::Number {
            raw: raw.to_string(),
            value,
        })
    }
}

/// Detects the indentation unit (e.g., `"  "`, `"    "`, or `"\t"`) used in a JSON file.
fn detect_json_indent(source: &str) -> String {
    for line in source.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let lead_len = line.len() - line.trim_start().len();

        if lead_len > 0 {
            let lead = &line[..lead_len];

            if lead.starts_with('\t') {
                return "\t".to_string();
            }

            return lead.to_string();
        }
    }

    "  ".to_string()
}

/// Serializes an [`OrderedJson`] value using `indent_unit` per nesting level.
fn format_ordered_json(val: &OrderedJson, indent_unit: &str, depth: usize, out: &mut String) {
    match val {
        OrderedJson::Null => out.push_str("null"),
        OrderedJson::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        OrderedJson::Number { raw, .. } => out.push_str(raw),
        OrderedJson::String(s) => {
            out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()));
        }
        OrderedJson::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }

            out.push_str("[\n");
            let child_indent = indent_unit.repeat(depth + 1);

            for (idx, item) in items.iter().enumerate() {
                out.push_str(&child_indent);
                format_ordered_json(item, indent_unit, depth + 1, out);

                if idx + 1 < items.len() {
                    out.push(',');
                }

                out.push('\n');
            }

            out.push_str(&indent_unit.repeat(depth));
            out.push(']');
        }
        OrderedJson::Object(entries) => {
            if entries.is_empty() {
                out.push_str("{}");
                return;
            }

            out.push_str("{\n");
            let child_indent = indent_unit.repeat(depth + 1);

            for (idx, (k, v)) in entries.iter().enumerate() {
                out.push_str(&child_indent);
                out.push_str(&serde_json::to_string(k).unwrap_or_else(|_| "\"\"".to_string()));
                out.push_str(": ");
                format_ordered_json(v, indent_unit, depth + 1, out);

                if idx + 1 < entries.len() {
                    out.push(',');
                }

                out.push('\n');
            }

            out.push_str(&indent_unit.repeat(depth));
            out.push('}');
        }
    }
}

/// Performs a format-preserving structural 3-way merge on JSON files (`package.json`, `tsconfig.json`, etc.).
pub fn merge_json_3way(base: &str, left: &str, right: &str) -> Option<DriverMergeOutcome> {
    merge_json_3way_labeled(base, left, right, "ours", "theirs")
}

/// Performs a format-preserving structural 3-way merge on JSON files with explicit conflict labels.
fn merge_json_3way_labeled(
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> Option<DriverMergeOutcome> {
    if left == right || right == base {
        return Some(DriverMergeOutcome {
            content: left.to_string(),
            has_conflicts: false,
            conflicting_keys: Vec::new(),
        });
    }

    if left == base {
        return Some(DriverMergeOutcome {
            content: right.to_string(),
            has_conflicts: false,
            conflicting_keys: Vec::new(),
        });
    }

    let base_val = if base.trim().is_empty() {
        OrderedJson::Object(Vec::new())
    } else {
        OrderedJsonParser::parse(base)?
    };
    let left_val = OrderedJsonParser::parse(left)?;
    let right_val = OrderedJsonParser::parse(right)?;

    let mut conflicts = Vec::new();
    let merged_val = merge_ordered_json("", Some(&base_val), &left_val, &right_val, &mut conflicts);

    if !conflicts.is_empty() {
        return Some(build_strict_conflict_outcome(
            base,
            left,
            right,
            left_label,
            right_label,
            conflicts,
        ));
    }

    let strict = merge_text_3way_strict(base, left, right, left_label, right_label);

    if !strict.has_conflicts {
        if let Some(parsed) = OrderedJsonParser::parse(&strict.content) {
            if parsed == merged_val {
                return Some(DriverMergeOutcome {
                    content: strict.content,
                    has_conflicts: false,
                    conflicting_keys: Vec::new(),
                });
            }
        }
    }

    let indent = detect_json_indent(left);
    let mut serialized = String::new();
    format_ordered_json(&merged_val, &indent, 0, &mut serialized);

    if left.ends_with('\n') || right.ends_with('\n') || base.ends_with('\n') {
        serialized.push('\n');
    }

    Some(DriverMergeOutcome {
        content: serialized,
        has_conflicts: false,
        conflicting_keys: Vec::new(),
    })
}

/// Recursively merges three [`OrderedJson`] trees preserving key order from `left`
/// (with newly added keys from `right` appended in order) and recording any conflicts.
fn merge_ordered_json(
    key_path: &str,
    base: Option<&OrderedJson>,
    left: &OrderedJson,
    right: &OrderedJson,
    conflicts: &mut Vec<String>,
) -> OrderedJson {
    if left == right || base == Some(right) {
        return left.clone();
    }

    if base == Some(left) {
        return right.clone();
    }

    match (left, right) {
        (OrderedJson::Object(l_entries), OrderedJson::Object(r_entries)) => {
            let b_entries_opt = match base {
                Some(OrderedJson::Object(entries)) => Some(entries),
                Some(_) => {
                    conflicts.push(key_path.to_string());
                    return left.clone();
                }
                None => None,
            };

            let b_map: BTreeMap<&str, &OrderedJson> = b_entries_opt
                .map(|entries| entries.iter().map(|(k, v)| (k.as_str(), v)).collect())
                .unwrap_or_default();
            let l_map: BTreeMap<&str, &OrderedJson> =
                l_entries.iter().map(|(k, v)| (k.as_str(), v)).collect();
            let r_map: BTreeMap<&str, &OrderedJson> =
                r_entries.iter().map(|(k, v)| (k.as_str(), v)).collect();

            let mut ordered_keys: Vec<&str> = Vec::new();

            for (k, _) in l_entries.iter().chain(r_entries.iter()) {
                if !ordered_keys.contains(&k.as_str()) {
                    ordered_keys.push(k.as_str());
                }
            }

            let mut out = Vec::new();

            for k in ordered_keys {
                let child_path = if key_path.is_empty() {
                    k.to_string()
                } else {
                    format!("{key_path}.{k}")
                };

                match (
                    b_map.get(k).copied(),
                    l_map.get(k).copied(),
                    r_map.get(k).copied(),
                ) {
                    (Some(b), Some(l), Some(r)) => {
                        out.push((
                            k.to_string(),
                            merge_ordered_json(&child_path, Some(b), l, r, conflicts),
                        ));
                    }
                    (None, Some(l), Some(r)) => {
                        out.push((
                            k.to_string(),
                            merge_ordered_json(&child_path, None, l, r, conflicts),
                        ));
                    }
                    (None, Some(l), None) => {
                        out.push((k.to_string(), l.clone()));
                    }
                    (None, None, Some(r)) => {
                        out.push((k.to_string(), r.clone()));
                    }
                    (Some(b), Some(l), None) => {
                        if l != b {
                            conflicts.push(child_path);
                            out.push((k.to_string(), l.clone()));
                        }
                    }
                    (Some(b), None, Some(r)) => {
                        if r != b {
                            conflicts.push(child_path);
                            out.push((k.to_string(), r.clone()));
                        }
                    }
                    (Some(_), None, None) | (None, None, None) => {}
                }
            }

            OrderedJson::Object(out)
        }
        (OrderedJson::Array(l_arr), OrderedJson::Array(r_arr)) => {
            let b_arr = match base {
                Some(OrderedJson::Array(a)) => a.as_slice(),
                Some(_) => {
                    conflicts.push(key_path.to_string());
                    return left.clone();
                }
                None => &[],
            };

            let is_complex =
                |v: &OrderedJson| matches!(v, OrderedJson::Object(_) | OrderedJson::Array(_));

            match merge_scalar_arrays_3way(b_arr, l_arr, r_arr, is_complex) {
                Some(merged) => OrderedJson::Array(merged),
                None => {
                    conflicts.push(key_path.to_string());
                    left.clone()
                }
            }
        }
        _ => {
            conflicts.push(key_path.to_string());
            left.clone()
        }
    }
}

/// Performs a structural 3-way merge on `Cargo.lock` files by indexing `[[package]]` stanzas
/// by `(name, version, source)`, 3-way merging top-level metadata, and preserving `# @generated` headers.
pub fn merge_cargo_lock_3way(base: &str, left: &str, right: &str) -> Option<DriverMergeOutcome> {
    merge_cargo_lock_3way_labeled(base, left, right, "ours", "theirs")
}

/// Performs a structural 3-way merge on `Cargo.lock` files with explicit conflict labels.
fn merge_cargo_lock_3way_labeled(
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> Option<DriverMergeOutcome> {
    if left == right || right == base {
        return Some(DriverMergeOutcome {
            content: left.to_string(),
            has_conflicts: false,
            conflicting_keys: Vec::new(),
        });
    }

    if left == base {
        return Some(DriverMergeOutcome {
            content: right.to_string(),
            has_conflicts: false,
            conflicting_keys: Vec::new(),
        });
    }

    let base_val: toml::Value = if base.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(base).ok()?
    };
    let left_val: toml::Value = toml::from_str(left).ok()?;
    let right_val: toml::Value = toml::from_str(right).ok()?;

    let base_pkgs = extract_lock_packages(&base_val);
    let left_pkgs = extract_lock_packages(&left_val);
    let right_pkgs = extract_lock_packages(&right_val);

    let mut all_keys = BTreeSet::new();

    for k in base_pkgs
        .keys()
        .chain(left_pkgs.keys())
        .chain(right_pkgs.keys())
    {
        all_keys.insert(k.clone());
    }

    let mut conflicts = Vec::new();
    let mut merged_pkgs = Vec::new();

    for key in all_keys {
        let pkg_path = format!("package.{}.{}", key.0, key.1);

        match (
            base_pkgs.get(&key),
            left_pkgs.get(&key),
            right_pkgs.get(&key),
        ) {
            (Some(b), Some(l), Some(r)) => {
                merged_pkgs.push(merge_toml_value(&pkg_path, Some(b), l, r, &mut conflicts));
            }
            (None, Some(l), Some(r)) => {
                merged_pkgs.push(merge_toml_value(&pkg_path, None, l, r, &mut conflicts));
            }
            (None, Some(l), None) => merged_pkgs.push(l.clone()),
            (None, None, Some(r)) => merged_pkgs.push(r.clone()),
            (Some(b), Some(l), None) => {
                if l != b {
                    merged_pkgs.push(l.clone());
                }
            }
            (Some(b), None, Some(r)) => {
                if r != b {
                    merged_pkgs.push(r.clone());
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    let base_meta = strip_package_table(&base_val);
    let left_meta = strip_package_table(&left_val);
    let right_meta = strip_package_table(&right_val);
    let merged_meta = merge_toml_value(
        "",
        Some(&base_meta),
        &left_meta,
        &right_meta,
        &mut conflicts,
    );

    if !conflicts.is_empty() {
        return Some(build_strict_conflict_outcome(
            base,
            left,
            right,
            left_label,
            right_label,
            conflicts,
        ));
    }

    let mut out_table = match merged_meta {
        toml::Value::Table(t) => t,
        _ => toml::map::Map::new(),
    };
    out_table.insert("package".to_string(), toml::Value::Array(merged_pkgs));
    let expected_val = toml::Value::Table(out_table);

    let strict = merge_text_3way_strict(base, left, right, left_label, right_label);

    if !strict.has_conflicts {
        if let Ok(parsed) = toml::from_str::<toml::Value>(&strict.content) {
            let strict_pkgs = extract_lock_packages(&parsed);
            let strict_pkg_count = parsed
                .get("package")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
            let expected_pkgs = extract_lock_packages(&expected_val);
            let strict_meta = strip_package_table(&parsed);
            let expected_meta = strip_package_table(&expected_val);

            if strict_pkgs.len() == strict_pkg_count
                && strict_pkgs == expected_pkgs
                && strict_meta == expected_meta
            {
                return Some(DriverMergeOutcome {
                    content: strict.content,
                    has_conflicts: false,
                    conflicting_keys: Vec::new(),
                });
            }
        }
    }

    let header_comments = {
        let l_hdr = extract_leading_toml_comments(left);

        if !l_hdr.is_empty() {
            l_hdr
        } else {
            extract_leading_toml_comments(base)
        }
    };
    let serialized_body = toml::to_string_pretty(&expected_val).ok()?;

    Some(DriverMergeOutcome {
        content: format!("{header_comments}{serialized_body}"),
        has_conflicts: false,
        conflicting_keys: Vec::new(),
    })
}

/// Returns a clone of a `Cargo.lock` root table with the `"package"` array removed so top-level
/// metadata (`version`, `[metadata]`, etc.) can be 3-way merged independently.
fn strip_package_table(val: &toml::Value) -> toml::Value {
    let mut tbl = match val {
        toml::Value::Table(t) => t.clone(),
        _ => toml::map::Map::new(),
    };
    tbl.remove("package");
    toml::Value::Table(tbl)
}

/// Extracts `[[package]]` entries from a parsed `Cargo.lock` TOML document keyed by
/// `(name, version, source)`.
fn extract_lock_packages(val: &toml::Value) -> BTreeMap<(String, String, String), toml::Value> {
    let mut map = BTreeMap::new();

    if let Some(pkgs) = val.get("package").and_then(|v| v.as_array()) {
        for pkg in pkgs {
            let name = pkg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let version = pkg
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let source = pkg
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if !name.is_empty() {
                map.insert((name, version, source), pkg.clone());
            }
        }
    }

    map
}

/// Record of a sequential migration file automatically renumbered to resolve a prefix collision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationResequence {
    /// Original path created by the second agent (e.g., `migrations/0042_add_billing.sql`).
    pub original_path: String,
    /// Re-sequenced path in the merged tree (e.g., `migrations/0043_add_billing.sql`).
    pub resequenced_path: String,
}

/// Checks whether `dir_path` is a sequential database migration directory (`migrations`, `migrate`,
/// `db/migrations`), excluding documentation directories such as `docs/migrations`.
pub fn is_migration_dir(dir_path: &str) -> bool {
    let segments: Vec<&str> = dir_path.split('/').filter(|seg| !seg.is_empty()).collect();

    if segments.is_empty() {
        return false;
    }

    for seg in &segments {
        if matches!(
            seg.to_ascii_lowercase().as_str(),
            "docs" | "doc" | "documentation" | "notes" | "examples"
        ) {
            return false;
        }
    }

    let len = segments.len();
    let last_is_mig = matches!(segments[len - 1], "migrations" | "migrate");
    let parent_is_mig = len >= 2 && matches!(segments[len - 2], "migrations" | "migrate");
    last_is_mig || parent_is_mig
}

/// Returns `true` if `file_name` has a recognized database migration extension (excluding
/// documentation/config formats like `.md`, `.txt`, `.json`, `.yaml`, or `.toml`).
pub fn is_migration_file(file_name: &str) -> bool {
    let Some(ext) = std::path::Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
    else {
        return false;
    };

    matches!(
        ext.to_ascii_lowercase().as_str(),
        "sql"
            | "ts"
            | "js"
            | "mjs"
            | "cjs"
            | "py"
            | "rb"
            | "go"
            | "rs"
            | "cs"
            | "java"
            | "kt"
            | "php"
            | "ex"
            | "exs"
            | "cql"
            | "cypher"
            | "sh"
    )
}

/// Splits a filename like `"0042_create_users.sql"` into `Some((42, 4, "_create_users.sql"))`.
pub fn parse_numeric_prefix(file_name: &str) -> Option<(u64, usize, &str)> {
    let digit_len = file_name.bytes().take_while(u8::is_ascii_digit).count();

    if digit_len == 0 {
        return None;
    }

    let rest = &file_name[digit_len..];

    if !rest.starts_with('_') && !rest.starts_with('-') {
        return None;
    }

    let num = file_name[..digit_len].parse::<u64>().ok()?;
    Some((num, digit_len, rest))
}

/// Formats a re-sequenced migration filename preserving zero-padding width (e.g., `43`, `width=4`, `"_billing.sql"` -> `"0043_billing.sql"`).
pub fn format_resequenced_filename(next_num: u64, width: usize, suffix: &str) -> String {
    format!("{next_num:0width$}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_toml_concurrent_dependencies_merge_cleanly() {
        let base = r#"# Crate manifest
[package]
name = "demo"
version = "0.1.0"

[dependencies]
# Core serialization
serde = { version = "1.0", features = ["derive"] }
"#;
        let left = r#"# Crate manifest
[package]
name = "demo"
version = "0.1.0"

[dependencies]
# Core serialization
serde = { version = "1.0", features = ["derive"] }
tokio = "1.40"
"#;
        let right = r#"# Crate manifest
[package]
name = "demo"
version = "0.1.0"

[dependencies]
# Core serialization
serde = { version = "1.0", features = ["derive"] }
axum = "0.7"
"#;

        let res = merge_toml_3way(base, left, right).expect("toml merge");
        assert!(!res.has_conflicts);
        assert!(res.content.contains("# Crate manifest"));
        assert!(res.content.contains("# Core serialization"));
        assert!(
            res.content
                .contains("serde = { version = \"1.0\", features = [\"derive\"] }")
        );
        assert!(res.content.contains("tokio = \"1.40\""));
        assert!(res.content.contains("axum = \"0.7\""));
    }

    #[test]
    fn toml_semver_conflict_flags_conflict_instead_of_synthesizing_version() {
        let base = "[dependencies]\nserde = \"1.0.100\"\n";
        let left = "[dependencies]\nserde = \"1.0.200\"\n";
        let right = "[dependencies]\nserde = \"1.1.100\"\n";

        let res = merge_toml_3way(base, left, right).expect("toml conflict outcome");
        assert!(res.has_conflicts);
        assert_eq!(res.conflicting_keys, vec!["dependencies.serde".to_string()]);
        assert!(res.content.contains("<<<<<<<"));
        assert!(!res.content.contains("1.1.200"));
    }

    #[test]
    fn toml_and_json_preserve_duplicate_array_elements_and_detect_add_add_type_conflicts() {
        let base_toml = "[build]\nrustflags = [\"-C\", \"opt\"]\n";
        let left_toml = "[build]\nrustflags = [\"-C\", \"opt\", \"-C\", \"lto\"]\n";
        let right_toml = "[build]\nrustflags = [\"-C\", \"opt\", \"-C\", \"strip=symbols\"]\n";

        let res_toml = merge_toml_3way(base_toml, left_toml, right_toml).expect("toml array merge");
        assert!(!res_toml.has_conflicts, "got:\n{}", res_toml.content);
        let parsed: toml::Value = toml::from_str(&res_toml.content).expect("valid toml");
        let flags = parsed["build"]["rustflags"]
            .as_array()
            .expect("flags array");
        assert_eq!(flags.len(), 6);
        assert_eq!(flags.iter().filter(|v| v.as_str() == Some("-C")).count(), 3);

        let base_empty_bug = "[package]\nname = \"x\"\n";
        let left_empty_bug = "[package]\nname = \"x\"\n\n[extra]\n";
        let right_empty_bug = "extra = \"scalar\"\n\n[package]\nname = \"x\"\n";
        let res_empty_bug =
            merge_toml_3way(base_empty_bug, left_empty_bug, right_empty_bug).expect("conflict");
        assert!(res_empty_bug.has_conflicts);

        let base_json = "{\n    \"name\": \"web\"\n}\n";
        let left_json = "{\n    \"name\": \"web\",\n    \"extra\": {}\n}\n";
        let right_json = "{\n    \"name\": \"web\",\n    \"extra\": \"scalar\"\n}\n";
        let res_json = merge_json_3way(base_json, left_json, right_json).expect("json conflict");
        assert!(res_json.has_conflicts);

        let base_bin = "[[bin]]\nname = \"app\"\npath = \"src/main.rs\"\n";
        let left_bin = "[[bin]]\nname = \"app\"\npath = \"src/cli.rs\"\n";
        let right_bin = "[[bin]]\nname = \"app\"\npath = \"src/server.rs\"\n";
        let res_bin = merge_toml_3way(base_bin, left_bin, right_bin).expect("bin conflict");
        assert!(res_bin.has_conflicts);
    }

    #[test]
    fn package_json_concurrent_deps_preserve_indent_and_key_order() {
        let base = "{\n    \"name\": \"web\",\n    \"version\": \"1.0.0\",\n    \"dependencies\": {\n        \"react\": \"^19.0.0\"\n    }\n}\n";
        let left = "{\n    \"name\": \"web\",\n    \"version\": \"1.0.0\",\n    \"dependencies\": {\n        \"react\": \"^19.0.0\",\n        \"zod\": \"^3.24.0\"\n    }\n}\n";
        let right = "{\n    \"name\": \"web\",\n    \"version\": \"1.0.0\",\n    \"dependencies\": {\n        \"react\": \"^19.0.0\",\n        \"lucide-react\": \"^0.460.0\"\n    }\n}\n";

        let res = merge_json_3way(base, left, right).expect("json merge");
        assert!(!res.has_conflicts);
        assert!(res.content.contains("    \"name\": \"web\""));
        assert!(res.content.contains("        \"zod\": \"^3.24.0\""));
        assert!(
            res.content
                .contains("        \"lucide-react\": \"^0.460.0\"")
        );
        let name_pos = res.content.find("\"name\"").expect("name");
        let ver_pos = res.content.find("\"version\"").expect("version");
        let deps_pos = res.content.find("\"dependencies\"").expect("dependencies");
        assert!(name_pos < ver_pos && ver_pos < deps_pos);
    }

    #[test]
    fn cargo_lock_preserves_generated_header_source_key_and_top_level_metadata() {
        let base = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 3

[[package]]
name = "serde"
version = "1.0.217"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        let left = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 3

[[package]]
name = "serde"
version = "1.0.217"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "blake3"
version = "1.5.5"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        let right = r#"# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "serde"
version = "1.0.217"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "blake3"
version = "1.5.5"
source = "git+https://example.com/blake3"
"#;

        let res = merge_cargo_lock_3way(base, left, right).expect("cargo.lock merge");
        assert!(!res.has_conflicts, "got:\n{}", res.content);
        assert!(
            res.content
                .starts_with("# This file is automatically @generated by Cargo.")
        );
        assert!(res.content.contains("version = 4"));
        assert_eq!(res.content.matches("name = \"blake3\"").count(), 2);
    }

    #[test]
    fn migration_dir_rejects_docs_and_markdown_files() {
        assert!(is_migration_dir("db/migrations"));
        assert!(is_migration_dir("migrations"));
        assert!(is_migration_dir("db/migrations/postgres"));
        assert!(!is_migration_dir("docs/migrations"));
        assert!(!is_migration_dir("examples/migrations"));
        assert!(is_migration_file("0001_users.sql"));
        assert!(!is_migration_file("0001_overview.md"));
    }
}
