//! Tree-sitter structural 3-way AST/CST merging, symbol extraction, and single-file component merging.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser};

use crate::drivers::try_merge_with_driver;
use crate::text_merge::merge_text_3way;

/// Supported programming languages for Tree-sitter structural AST analysis and 3-way merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AstLanguage {
    /// Rust (`.rs`).
    Rust,
    /// TypeScript (`.ts`).
    TypeScript,
    /// TSX (`.tsx`).
    Tsx,
    /// JavaScript / JSX (`.js`, `.jsx`, `.mjs`).
    JavaScript,
    /// Python (`.py`).
    Python,
    /// Go (`.go`).
    Go,
    /// JSON (`.json`).
    Json,
}

impl AstLanguage {
    /// Detects the [`AstLanguage`] from a file path extension.
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = Path::new(path).extension()?.to_str()?;
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "py" => Some(Self::Python),
            "go" => Some(Self::Go),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    /// Returns the Tree-sitter [`Language`] grammar corresponding to `self`.
    fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
        }
    }
}

/// Metadata for a single top-level or nested AST symbol extracted via Tree-sitter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AstSymbol {
    /// Normalized display label (e.g. `fn verify_jwt`, `struct User`, `impl AuthService`).
    pub label: String,
    /// Raw Tree-sitter node kind (e.g. `function_item`).
    pub kind: String,
    /// Identifier name of the declaration.
    pub name: String,
    /// 1-indexed start line in the file.
    pub start_line: usize,
    /// 1-indexed end line in the file.
    pub end_line: usize,
    /// Blake3 digest of the normalized declaration body.
    pub body_hash: String,
}

/// Outcome of a 3-way structural AST or driver file merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AstMergeOutcome {
    /// Merged source file content.
    pub content: String,
    /// `true` if one or more AST nodes or hunks conflicted.
    pub has_conflicts: bool,
    /// Names of the specific AST symbols that conflicted (empty if clean or non-AST).
    pub conflicted_symbols: Vec<String>,
    /// Base snippet of the first conflict (for structured agent inspection).
    pub base_conflict_snippet: String,
    /// Left snippet of the first conflict.
    pub left_conflict_snippet: String,
    /// Right snippet of the first conflict.
    pub right_conflict_snippet: String,
}

/// Classified top-level or container-member AST item.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TopItem {
    /// Top-level import or `use` declaration.
    Import(String),
    /// Named declaration (`fn`, `struct`, `enum`, `trait`, `impl`, `class`, `const`, etc.).
    Decl {
        /// Canonical key (`<kind>::<name>`) used for declaration alignment.
        key: String,
        /// Human-readable symbol label (`<kind> <name>`).
        label: String,
        /// Full source slice including leading doc comments and attributes.
        text: String,
    },
    /// Unclassified statement, inner attribute, or comment block.
    Other(String),
}

/// Extracts all named AST symbols (`fn`, `struct`, `enum`, `impl`, `trait`, `class`, `interface`, `def`, etc.)
/// from `source` for `path`.
pub fn extract_ast_symbols(path: &str, source: &str) -> Vec<AstSymbol> {
    if is_sfc_path(path) {
        if let Some((_, _, script_body, _, _)) = split_sfc_script_block(source) {
            return extract_ast_symbols("sfc.ts", &script_body);
        }
    }

    let Some(lang) = AstLanguage::from_path(path) else {
        return Vec::new();
    };

    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }

    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let mut symbols = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if let Some((kind_prefix, name)) = classify_declaration(child, source.as_bytes()) {
            let slice = &source[child.start_byte()..child.end_byte()];
            let normalized: String = slice.split_whitespace().collect::<Vec<_>>().join(" ");
            let body_hash = blake3::hash(normalized.as_bytes()).to_hex().to_string();
            symbols.push(AstSymbol {
                label: format!("{kind_prefix} {name}"),
                kind: child.kind().to_string(),
                name,
                start_line: child.start_position().row + 1,
                end_line: child.end_position().row + 1,
                body_hash,
            });
        }
    }
    symbols
}

/// Computes the list of [`AstSymbol`]s in `current` that were added or modified compared to `base`.
///
/// Used by `tes radar` to detect overlapping symbol edits across agents and by Auto-Pilot
/// to synthesize semantic intent descriptions automatically when no prompt hook was fired.
pub fn diff_modified_symbols(path: &str, base: &str, current: &str) -> Vec<AstSymbol> {
    let base_syms: BTreeMap<String, String> = extract_ast_symbols(path, base)
        .into_iter()
        .map(|s| (s.label.clone(), s.body_hash))
        .collect();

    let curr_syms = extract_ast_symbols(path, current);
    curr_syms
        .into_iter()
        .filter(|sym| base_syms.get(&sym.label) != Some(&sym.body_hash))
        .collect()
}

/// Merges `base`, `left`, and `right` for `path` using:
/// - Specialized manifest and lockfile drivers (`Cargo.toml`, `Cargo.lock`, `package.json`) if applicable;
/// - Vue/Svelte Single-File Component (`<script>` TypeScript AST + `<template>`/`<style>` structural block) merge;
/// - Tree-sitter structural 3-way CST alignment for supported source code languages;
/// - Histogram diff3 line merge as the fallback.
pub fn merge_file_3way(
    path: &str,
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> AstMergeOutcome {
    if left == right || right == base {
        return AstMergeOutcome {
            content: left.to_string(),
            has_conflicts: false,
            conflicted_symbols: Vec::new(),
            base_conflict_snippet: String::new(),
            left_conflict_snippet: String::new(),
            right_conflict_snippet: String::new(),
        };
    }

    if left == base {
        return AstMergeOutcome {
            content: right.to_string(),
            has_conflicts: false,
            conflicted_symbols: Vec::new(),
            base_conflict_snippet: String::new(),
            left_conflict_snippet: String::new(),
            right_conflict_snippet: String::new(),
        };
    }

    if let Some(driver_res) = try_merge_with_driver(path, base, left, right) {
        if !driver_res.has_conflicts {
            return AstMergeOutcome {
                content: driver_res.content,
                has_conflicts: false,
                conflicted_symbols: Vec::new(),
                base_conflict_snippet: String::new(),
                left_conflict_snippet: String::new(),
                right_conflict_snippet: String::new(),
            };
        }
    }

    if is_sfc_path(path) {
        if let Some(sfc_res) = try_sfc_merge(base, left, right, left_label, right_label) {
            return sfc_res;
        }
    }

    if let Some(lang) = AstLanguage::from_path(path) {
        if lang != AstLanguage::Json {
            if let Some(ast_res) =
                try_structural_ast_merge(lang, base, left, right, left_label, right_label)
            {
                return ast_res;
            }
        }
    }

    let text_res = merge_text_3way(base, left, right, left_label, right_label);
    AstMergeOutcome {
        content: text_res.content,
        has_conflicts: text_res.has_conflicts,
        conflicted_symbols: Vec::new(),
        base_conflict_snippet: base.lines().take(20).collect::<Vec<_>>().join("\n"),
        left_conflict_snippet: left.lines().take(20).collect::<Vec<_>>().join("\n"),
        right_conflict_snippet: right.lines().take(20).collect::<Vec<_>>().join("\n"),
    }
}

/// Returns `true` if `path` has a `.vue` or `.svelte` Single-File Component extension.
fn is_sfc_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "vue" | "svelte"))
}

/// Splits a Vue or Svelte Single-File Component into `(before_script, open_tag, script_body, close_tag, after_script)`.
fn split_sfc_script_block(source: &str) -> Option<(String, String, String, String, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let open_idx = lines
        .iter()
        .position(|l| l.trim_start().starts_with("<script"))?;
    let close_rel = lines[(open_idx + 1)..]
        .iter()
        .position(|l| l.trim() == "</script>")?;
    let close_idx = open_idx + 1 + close_rel;

    let before = lines[..open_idx].join("\n");
    let open_tag = lines[open_idx].to_string();
    let body = lines[(open_idx + 1)..close_idx].join("\n");
    let close_tag = lines[close_idx].to_string();
    let mut after = lines[(close_idx + 1)..].join("\n");
    if source.ends_with('\n') && !after.is_empty() {
        after.push('\n');
    }
    Some((before, open_tag, body, close_tag, after))
}

/// Merges a Vue or Svelte Single-File Component by running TypeScript structural AST merge on
/// the `<script>` block and structural block/line diff3 on the `<template>` and `<style>` sections.
fn try_sfc_merge(
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> Option<AstMergeOutcome> {
    let (b_before, b_open, b_body, _, b_after) = split_sfc_script_block(base)?;
    let (l_before, l_open, l_body, l_close, l_after) = split_sfc_script_block(left)?;
    let (r_before, r_open, r_body, _, r_after) = split_sfc_script_block(right)?;

    if l_open.trim() != r_open.trim()
        && b_open.trim() != l_open.trim()
        && b_open.trim() != r_open.trim()
    {
        return None;
    }

    let open_tag = if l_open.trim() != b_open.trim() {
        l_open
    } else {
        r_open
    };

    let script_outcome = try_structural_ast_merge(
        AstLanguage::TypeScript,
        &b_body,
        &l_body,
        &r_body,
        left_label,
        right_label,
    )
    .unwrap_or_else(|| {
        let t = merge_text_3way(&b_body, &l_body, &r_body, left_label, right_label);
        AstMergeOutcome {
            content: t.content,
            has_conflicts: t.has_conflicts,
            conflicted_symbols: Vec::new(),
            base_conflict_snippet: b_body.clone(),
            left_conflict_snippet: l_body.clone(),
            right_conflict_snippet: r_body.clone(),
        }
    });

    let before_res = merge_text_3way(&b_before, &l_before, &r_before, left_label, right_label);
    let after_res = merge_text_3way(&b_after, &l_after, &r_after, left_label, right_label);

    let has_conflicts =
        script_outcome.has_conflicts || before_res.has_conflicts || after_res.has_conflicts;

    let mut out = String::new();
    if !before_res.content.trim().is_empty() {
        out.push_str(before_res.content.trim_end());
        out.push('\n');
    }
    out.push_str(&open_tag);
    out.push('\n');

    if !script_outcome.content.trim().is_empty() {
        out.push_str(script_outcome.content.trim_end());
        out.push('\n');
    }
    out.push_str(&l_close);
    out.push('\n');

    if !after_res.content.is_empty() {
        if !after_res.content.starts_with('\n') {
            out.push('\n');
        }
        out.push_str(after_res.content.trim_start_matches('\n'));
    }

    if (left.ends_with('\n') || right.ends_with('\n')) && !out.ends_with('\n') {
        out.push('\n');
    }

    let mut conflicted_symbols = script_outcome.conflicted_symbols;
    if after_res.has_conflicts && conflicted_symbols.is_empty() {
        conflicted_symbols.push("<template>/<style>".to_string());
    }

    Some(AstMergeOutcome {
        content: out,
        has_conflicts,
        conflicted_symbols,
        base_conflict_snippet: if !script_outcome.base_conflict_snippet.is_empty() {
            script_outcome.base_conflict_snippet
        } else {
            b_after.lines().take(20).collect::<Vec<_>>().join("\n")
        },
        left_conflict_snippet: if !script_outcome.left_conflict_snippet.is_empty() {
            script_outcome.left_conflict_snippet
        } else {
            l_after.lines().take(20).collect::<Vec<_>>().join("\n")
        },
        right_conflict_snippet: if !script_outcome.right_conflict_snippet.is_empty() {
            script_outcome.right_conflict_snippet
        } else {
            r_after.lines().take(20).collect::<Vec<_>>().join("\n")
        },
    })
}

/// Attempts a full 3-way structural AST merge by decomposing `base`, `left`, and `right` into
/// top-level imports and named declarations.
fn try_structural_ast_merge(
    lang: AstLanguage,
    base: &str,
    left: &str,
    right: &str,
    left_label: &str,
    right_label: &str,
) -> Option<AstMergeOutcome> {
    let base_items = parse_top_items(lang, base)?;
    let left_items = parse_top_items(lang, left)?;
    let right_items = parse_top_items(lang, right)?;

    // Only use structural declaration decomposition if the file contains recognizable imports or
    // named declarations AND no top-level imperative statements (`TopItem::Other`), since unnamed
    // top-level statements (like `document.querySelector('#app').innerHTML = ...`) have no stable
    // declaration identifier and must be merged via line/sub-line diff3.
    let has_structure = left_items
        .iter()
        .chain(right_items.iter())
        .any(|it| matches!(it, TopItem::Import(_) | TopItem::Decl { .. }));

    let collect_other = |items: &[TopItem]| -> Vec<String> {
        items
            .iter()
            .filter_map(|it| match it {
                TopItem::Other(s) if !is_inner_crate_header("", s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    };

    let base_other = collect_other(&base_items);
    let left_other = collect_other(&left_items);
    let right_other = collect_other(&right_items);
    let has_modified_imperative_statements =
        base_other != left_other || base_other != right_other;
    if !has_structure || has_modified_imperative_statements {
        return None;
    }

    let merged_imports =
        merge_imports_3way(&base_items, &left_items, &right_items, left_label, right_label);

    let base_decls = extract_decl_map(&base_items);
    let left_decls = extract_decl_map(&left_items);
    let right_decls = extract_decl_map(&right_items);

    // Maintain canonical declaration ordering: all keys in `left` order followed by newly added keys in `right`.
    let mut ordered_keys: Vec<String> = Vec::new();
    for item in &left_items {
        if let TopItem::Decl { key, .. } | TopItem::Other(key) = item {
            if !ordered_keys.contains(key) {
                ordered_keys.push(key.clone());
            }
        }
    }

    for item in &right_items {
        if let TopItem::Decl { key, .. } | TopItem::Other(key) = item {
            if !ordered_keys.contains(key) {
                ordered_keys.push(key.clone());
            }
        }
    }

    let mut out_blocks: Vec<String> = Vec::new();
    let mut header_keys: BTreeSet<String> = BTreeSet::new();
    let mut header_lines: Vec<String> = Vec::new();
    for key in &ordered_keys {
        if key.starts_with("//!") || key.starts_with("/*!") || key.starts_with("#![") {
            header_keys.insert(key.clone());
            header_lines.push(key.clone());
        } else {
            break;
        }
    }

    if !header_lines.is_empty() {
        out_blocks.push(header_lines.join("\n"));
    }

    if !merged_imports.is_empty() {
        out_blocks.push(merged_imports.join("\n"));
    }
    ordered_keys.retain(|k| !header_keys.contains(k));

    let mut conflicted_symbols = Vec::new();
    let mut base_conflict_snippet = String::new();
    let mut left_conflict_snippet = String::new();
    let mut right_conflict_snippet = String::new();

    for key in ordered_keys {
        let b_entry = base_decls.get(&key);
        let l_entry = left_decls.get(&key);
        let r_entry = right_decls.get(&key);

        match (b_entry, l_entry, r_entry) {
            (Some((_, b_txt)), Some((l_lbl, l_txt)), Some((_, r_txt))) => {
                if l_txt == r_txt || r_txt == b_txt {
                    out_blocks.push(l_txt.clone());
                } else if l_txt == b_txt {
                    out_blocks.push(r_txt.clone());
                } else if let Some(container_out) = try_merge_container_declaration(
                    lang,
                    l_lbl,
                    b_txt,
                    l_txt,
                    r_txt,
                    left_label,
                    right_label,
                ) {
                    if container_out.has_conflicts {
                        conflicted_symbols.extend(container_out.conflicted_symbols);
                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = container_out.base_conflict_snippet;
                            left_conflict_snippet = container_out.left_conflict_snippet;
                            right_conflict_snippet = container_out.right_conflict_snippet;
                        }
                    }
                    out_blocks.push(container_out.content.trim_end().to_string());
                } else if let Some(fn_out) = try_merge_function_declaration(
                    lang,
                    l_lbl,
                    b_txt,
                    l_txt,
                    r_txt,
                    left_label,
                    right_label,
                ) {
                    if fn_out.has_conflicts {
                        conflicted_symbols.extend(fn_out.conflicted_symbols);
                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = fn_out.base_conflict_snippet;
                            left_conflict_snippet = fn_out.left_conflict_snippet;
                            right_conflict_snippet = fn_out.right_conflict_snippet;
                        }
                    }
                    out_blocks.push(fn_out.content.trim_end().to_string());
                } else {
                    // Both agents modified the exact same leaf declaration (e.g., `fn login`).
                    // Run 3-way line merge scoped strictly to this declaration's text.
                    let inner = merge_text_3way(b_txt, l_txt, r_txt, left_label, right_label);
                    if inner.has_conflicts {
                        conflicted_symbols.push(l_lbl.clone());
                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = b_txt.clone();
                            left_conflict_snippet = l_txt.clone();
                            right_conflict_snippet = r_txt.clone();
                        }
                    }
                    out_blocks.push(inner.content.trim_end().to_string());
                }
            }
            (None, Some((l_lbl, l_txt)), Some((_, r_txt))) => {
                if l_txt == r_txt {
                    out_blocks.push(l_txt.clone());
                } else if let Some(container_out) = try_merge_container_declaration(
                    lang,
                    l_lbl,
                    "",
                    l_txt,
                    r_txt,
                    left_label,
                    right_label,
                ) {
                    if container_out.has_conflicts {
                        conflicted_symbols.extend(container_out.conflicted_symbols);
                    }
                    out_blocks.push(container_out.content.trim_end().to_string());
                } else {
                    // Both agents independently added a declaration with the exact same name!
                    let inner = merge_text_3way("", l_txt, r_txt, left_label, right_label);
                    if inner.has_conflicts {
                        conflicted_symbols.push(l_lbl.clone());
                        if base_conflict_snippet.is_empty() {
                            left_conflict_snippet = l_txt.clone();
                            right_conflict_snippet = r_txt.clone();
                        }
                    }
                    out_blocks.push(inner.content.trim_end().to_string());
                }
            }
            (None, Some((_, l_txt)), None) => {
                out_blocks.push(l_txt.clone());
            }
            (None, None, Some((_, r_txt))) => {
                out_blocks.push(r_txt.clone());
            }
            (Some((_, b_txt)), Some((l_lbl, l_txt)), None) => {
                // Deleted on right; keep only if modified on left (delete/modify conflict)
                if l_txt != b_txt {
                    let inner = merge_text_3way(b_txt, l_txt, "", left_label, right_label);
                    conflicted_symbols.push(l_lbl.clone());
                    if base_conflict_snippet.is_empty() {
                        base_conflict_snippet = b_txt.clone();
                        left_conflict_snippet = l_txt.clone();
                    }
                    out_blocks.push(inner.content.trim_end().to_string());
                }
            }
            (Some((_, b_txt)), None, Some((r_lbl, r_txt))) => {
                // Deleted on left; keep only if modified on right (delete/modify conflict)
                if r_txt != b_txt {
                    let inner = merge_text_3way(b_txt, "", r_txt, left_label, right_label);
                    conflicted_symbols.push(r_lbl.clone());
                    if base_conflict_snippet.is_empty() {
                        base_conflict_snippet = b_txt.clone();
                        right_conflict_snippet = r_txt.clone();
                    }
                    out_blocks.push(inner.content.trim_end().to_string());
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    let mut content = out_blocks.join("\n\n");
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }

    Some(AstMergeOutcome {
        content,
        has_conflicts: !conflicted_symbols.is_empty(),
        conflicted_symbols,
        base_conflict_snippet,
        left_conflict_snippet,
        right_conflict_snippet,
    })
}

/// Resolves a file conflict using the `ast-union` strategy:
/// merges all imports, declarations, and container members, and when the same leaf statement/line
/// was modified differently on `left` and `right`, includes both modifications sequentially
/// without conflict markers.
pub fn merge_ast_union(path: &str, base: &str, left: &str, right: &str) -> String {
    let outcome = merge_file_3way(path, base, left, right, "ours", "theirs");
    if !outcome.has_conflicts {
        return outcome.content;
    }

    // Strip diff3 conflict markers while keeping both `ours` and `theirs` sides and dropping `||||||| base` sections.
    let mut resolved_lines = Vec::new();
    let mut in_base_section = false;

    for line in outcome.content.lines() {
        if line.starts_with("<<<<<<< ") {
            in_base_section = false;
            continue;
        }

        if line.starts_with("||||||| ") {
            in_base_section = true;
            continue;
        }

        if line == "=======" {
            in_base_section = false;
            continue;
        }

        if line.starts_with(">>>>>>> ") {
            in_base_section = false;
            continue;
        }

        if !in_base_section {
            resolved_lines.push(line);
        }
    }

    let mut out = resolved_lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Merges a container declaration (`impl`, `trait`, `struct`, `enum`, `class`, `interface`, `mod`)
/// by aligning its inner member declarations recursively.
fn try_merge_container_declaration(
    lang: AstLanguage,
    container_label: &str,
    base_src: &str,
    left_src: &str,
    right_src: &str,
    left_label: &str,
    right_label: &str,
) -> Option<AstMergeOutcome> {
    let (l_header, left_members, l_footer) = parse_container_members(lang, left_src)?;
    let (r_header, right_members, _) = parse_container_members(lang, right_src)?;
    if l_header.trim() != r_header.trim() {
        return None;
    }

    let base_members = if base_src.trim().is_empty() {
        Vec::new()
    } else {
        parse_container_members(lang, base_src)?.1
    };

    let base_map = extract_decl_map(&base_members);
    let left_map = extract_decl_map(&left_members);
    let right_map = extract_decl_map(&right_members);

    let mut ordered_keys = Vec::new();
    for item in left_members.iter().chain(right_members.iter()) {
        if let TopItem::Decl { key, .. } | TopItem::Other(key) = item {
            if !ordered_keys.contains(key) {
                ordered_keys.push(key.clone());
            }
        }
    }

    let mut member_blocks = Vec::new();
    let mut conflicted_symbols = Vec::new();
    let mut base_conflict_snippet = String::new();
    let mut left_conflict_snippet = String::new();
    let mut right_conflict_snippet = String::new();

    for key in ordered_keys {
        match (base_map.get(&key), left_map.get(&key), right_map.get(&key)) {
            (Some((_, b_m)), Some((m_lbl, l_m)), Some((_, r_m))) => {
                if l_m == r_m || r_m == b_m {
                    member_blocks.push(indent_member(l_m));
                } else if l_m == b_m {
                    member_blocks.push(indent_member(r_m));
                } else if let Some(fn_out) = try_merge_function_declaration(
                    lang,
                    &format!("{container_label}::{m_lbl}"),
                    b_m,
                    l_m,
                    r_m,
                    left_label,
                    right_label,
                ) {
                    if fn_out.has_conflicts {
                        conflicted_symbols.extend(fn_out.conflicted_symbols);
                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = fn_out.base_conflict_snippet;
                            left_conflict_snippet = fn_out.left_conflict_snippet;
                            right_conflict_snippet = fn_out.right_conflict_snippet;
                        }
                    }
                    member_blocks.push(indent_member(fn_out.content.trim_end()));
                } else {
                    let inner = merge_text_3way(b_m, l_m, r_m, left_label, right_label);
                    if inner.has_conflicts {
                        conflicted_symbols.push(format!("{container_label}::{m_lbl}"));
                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = b_m.clone();
                            left_conflict_snippet = l_m.clone();
                            right_conflict_snippet = r_m.clone();
                        }
                    }
                    member_blocks.push(indent_member(inner.content.trim_end()));
                }
            }
            (None, Some((m_lbl, l_m)), Some((_, r_m))) => {
                if l_m == r_m {
                    member_blocks.push(indent_member(l_m));
                } else {
                    let inner = merge_text_3way("", l_m, r_m, left_label, right_label);
                    if inner.has_conflicts {
                        conflicted_symbols.push(format!("{container_label}::{m_lbl}"));
                    }
                    member_blocks.push(indent_member(inner.content.trim_end()));
                }
            }
            (None, Some((_, l_m)), None) => member_blocks.push(indent_member(l_m)),
            (None, None, Some((_, r_m))) => member_blocks.push(indent_member(r_m)),
            (Some((_, b_m)), Some((m_lbl, l_m)), None) if l_m != b_m => {
                conflicted_symbols.push(format!("{container_label}::{m_lbl}"));
                member_blocks.push(indent_member(l_m));
            }
            (Some((_, b_m)), None, Some((m_lbl, r_m))) if r_m != b_m => {
                conflicted_symbols.push(format!("{container_label}::{m_lbl}"));
                member_blocks.push(indent_member(r_m));
            }
            _ => {}
        }
    }

    let content = if l_footer.is_empty() {
        format!("{}\n{}", l_header.trim_end(), member_blocks.join("\n\n"))
    } else {
        format!(
            "{}\n{}\n{}",
            l_header.trim_end(),
            member_blocks.join("\n\n"),
            l_footer
        )
    };

    Some(AstMergeOutcome {
        content,
        has_conflicts: !conflicted_symbols.is_empty(),
        conflicted_symbols,
        base_conflict_snippet,
        left_conflict_snippet,
        right_conflict_snippet,
    })
}

/// Merges a function or method declaration when both `left_src` and `right_src` modified the
/// formal parameter list or body concurrently.
fn try_merge_function_declaration(
    lang: AstLanguage,
    fn_label: &str,
    base_src: &str,
    left_src: &str,
    right_src: &str,
    left_label: &str,
    right_label: &str,
) -> Option<AstMergeOutcome> {
    let (b_range, b_params) = extract_function_params_span(lang, base_src)?;
    let (l_range, l_params) = extract_function_params_span(lang, left_src)?;
    let (r_range, r_params) = extract_function_params_span(lang, right_src)?;

    // Only needed when at least one side modified the parameter list.
    if b_params == l_params && b_params == r_params {
        return None;
    }

    let merged_params = merge_param_lists(&b_params, &l_params, &r_params)?;
    let merged_params_str = format!("({})", merged_params.join(", "));

    let base_norm = format!(
        "{}{}{}",
        &base_src[..b_range.0],
        merged_params_str,
        &base_src[b_range.1..]
    );
    let left_norm = format!(
        "{}{}{}",
        &left_src[..l_range.0],
        merged_params_str,
        &left_src[l_range.1..]
    );
    let right_norm = format!(
        "{}{}{}",
        &right_src[..r_range.0],
        merged_params_str,
        &right_src[r_range.1..]
    );

    let inner = merge_text_3way(&base_norm, &left_norm, &right_norm, left_label, right_label);
    let mut conflicted_symbols = Vec::new();
    let mut base_conflict_snippet = String::new();
    let mut left_conflict_snippet = String::new();
    let mut right_conflict_snippet = String::new();
    if inner.has_conflicts {
        conflicted_symbols.push(fn_label.to_string());
        base_conflict_snippet = base_src.to_string();
        left_conflict_snippet = left_src.to_string();
        right_conflict_snippet = right_src.to_string();
    }

    Some(AstMergeOutcome {
        content: inner.content,
        has_conflicts: inner.has_conflicts,
        conflicted_symbols,
        base_conflict_snippet,
        left_conflict_snippet,
        right_conflict_snippet,
    })
}

/// Byte span `(start_byte, end_byte)` of a function parameter list paired with `(key, raw_param_text)` entries.
type ParamsSpan = ((usize, usize), Vec<(String, String)>);

/// Parses `source` and extracts the byte span and individual `(param_name, param_text)` pairs of
/// the first function or method parameter list.
fn extract_function_params_span(
    lang: AstLanguage,
    source: &str,
) -> Option<ParamsSpan> {
    let mut parser = Parser::new();
    parser.set_language(&lang.ts_language()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    let params_node = find_params_node(root)?;
    let raw_params = &source[params_node.start_byte()..params_node.end_byte()];
    if !raw_params.starts_with('(') || !raw_params.ends_with(')') {
        return None;
    }

    let bytes = source.as_bytes();
    let mut cursor = params_node.walk();
    let mut params = Vec::new();
    for child in params_node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "(" | ")" | "," | ";" | "line_comment" | "block_comment" | "comment"
        ) {
            continue;
        }
        let text = source[child.start_byte()..child.end_byte()].trim().to_string();
        if text.is_empty() {
            continue;
        }
        let key = extract_param_key(child, bytes).unwrap_or_else(|| text.clone());
        params.push((key, text));
    }

    Some(((params_node.start_byte(), params_node.end_byte()), params))
}

/// Recursively locates the formal parameters AST node of a function or method declaration.
fn find_params_node(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(
        node.kind(),
        "function_item"
            | "function_signature_item"
            | "function_declaration"
            | "function_definition"
            | "method_declaration"
            | "method_definition"
            | "method_signature"
            | "arrow_function"
    ) {
        if let Some(p) = node.child_by_field_name("parameters") {
            return Some(p);
        }

        let mut c = node.walk();
        for child in node.children(&mut c) {
            if matches!(
                child.kind(),
                "formal_parameters" | "parameters" | "parameter_list"
            ) {
                return Some(child);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "statement_block" | "block" | "declaration_list" | "class_body"
        ) {
            continue;
        }

        if let Some(found) = find_params_node(child) {
            return Some(found);
        }
    }
    None
}

/// Extracts the canonical parameter identifier key from a single parameter AST node.
fn extract_param_key(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    if node.kind() == "self_parameter" {
        return Some("self".to_string());
    }

    for field in ["pattern", "name"] {
        if let Some(f) = node.child_by_field_name(field) {
            if let Some(id) = find_first_param_identifier(f, bytes) {
                return Some(id);
            }
        }
    }
    find_first_param_identifier(node, bytes)
}

/// Finds the first parameter name identifier inside `node`, ignoring type annotations.
fn find_first_param_identifier(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    if matches!(
        node.kind(),
        "identifier" | "shorthand_property_identifier_pattern" | "self"
    ) {
        if let Ok(s) = node.utf8_text(bytes) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "type_annotation" | "type_identifier") {
            continue;
        }

        if let Some(found) = find_first_param_identifier(child, bytes) {
            return Some(found);
        }
    }
    None
}

/// Performs a 3-way structural merge of formal parameter lists keyed by parameter name.
fn merge_param_lists(
    base: &[(String, String)],
    left: &[(String, String)],
    right: &[(String, String)],
) -> Option<Vec<String>> {
    let base_map: BTreeMap<&str, &str> = base
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let left_map: BTreeMap<&str, &str> = left
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let right_map: BTreeMap<&str, &str> = right
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
                    // Both agents modified the exact same parameter incompatibly.
                    return None;
                }
            }
            (None, Some(l), Some(r)) => {
                if l == r {
                    merged.push(l.to_string());
                } else {
                    // Both agents added a parameter with the same name but different types/defaults.
                    return None;
                }
            }
            (None, Some(l), None) => merged.push(l.to_string()),
            (None, None, Some(r)) => merged.push(r.to_string()),
            (Some(b), Some(l), None) => {
                // Deleted on right; only conflict if left also modified it.
                if l != b {
                    return None;
                }
            }
            (Some(b), None, Some(r)) => {
                // Deleted on left; only conflict if right also modified it.
                if r != b {
                    return None;
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    Some(merged)
}

/// Ensures each non-empty line of a container member block is indented by at least one level.
fn indent_member(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else if line.starts_with("    ") || line.starts_with("  ") || line.starts_with('\t') {
                line.to_string()
            } else {
                format!("    {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses a container declaration (`impl`, `trait`, `class`, `interface`, `struct`, `enum`, `mod`)
/// into `(header_prefix, member_items, closing_footer)`.
fn parse_container_members(
    lang: AstLanguage,
    source: &str,
) -> Option<(String, Vec<TopItem>, String)> {
    let mut parser = Parser::new();
    parser.set_language(&lang.ts_language()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    let mut root_cursor = root.walk();
    let container_node = root
        .children(&mut root_cursor)
        .find(|n| !is_leading_annotation(n.kind()))?;

    let actual_container = if container_node.kind() == "export_statement" {
        let mut exp_cursor = container_node.walk();
        let inner = container_node
            .children(&mut exp_cursor)
            .find(|c| is_container_kind(c.kind()));
        inner?
    } else if is_container_kind(container_node.kind()) {
        container_node
    } else {
        return None;
    };

    let mut c_cursor = actual_container.walk();
    let body_node = actual_container.children(&mut c_cursor).find(|n| {
        matches!(
            n.kind(),
            "declaration_list"
                | "field_declaration_list"
                | "enum_variant_list"
                | "class_body"
                | "interface_body"
                | "object_type"
                | "block"
        )
    })?;

    let bytes = source.as_bytes();
    let body_text = &source[body_node.start_byte()..body_node.end_byte()];
    let (header, footer) = if body_text.starts_with('{') && body_text.ends_with('}') {
        (
            source[..=body_node.start_byte()].to_string(),
            "}".to_string(),
        )
    } else {
        (source[..body_node.start_byte()].to_string(), String::new())
    };

    let mut b_cursor = body_node.walk();
    let children: Vec<Node<'_>> = body_node
        .children(&mut b_cursor)
        .filter(|n| !matches!(n.kind(), "{" | "}" | ";"))
        .collect();

    let items = collect_items_from_nodes(&children, source, bytes);
    if items.is_empty() {
        return None;
    }
    Some((header, items, footer))
}

/// Returns `true` if `kind` is an AST container node that can be recursively merged by member.
fn is_container_kind(kind: &str) -> bool {
    matches!(
        kind,
        "impl_item"
            | "trait_item"
            | "mod_item"
            | "struct_item"
            | "enum_item"
            | "class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "class_definition"
    )
}

/// Builds a map from declaration key to `(symbol_label, source_text)` for all non-import items.
fn extract_decl_map(items: &[TopItem]) -> BTreeMap<String, (String, String)> {
    let mut map = BTreeMap::new();
    for item in items {
        match item {
            TopItem::Decl { key, label, text } => {
                map.insert(key.clone(), (label.clone(), text.clone()));
            }
            TopItem::Other(text) => {
                map.insert(text.clone(), ("<statement>".to_string(), text.clone()));
            }
            TopItem::Import(_) => {}
        }
    }
    map
}

/// Parses `source` with Tree-sitter and collects its top-level [`TopItem`]s.
fn parse_top_items(lang: AstLanguage, source: &str) -> Option<Vec<TopItem>> {
    let mut parser = Parser::new();
    parser.set_language(&lang.ts_language()).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    let bytes = source.as_bytes();
    let mut cursor = root.walk();
    let children: Vec<Node<'_>> = root.children(&mut cursor).collect();
    Some(collect_items_from_nodes(&children, source, bytes))
}

/// Groups a slice of sibling Tree-sitter nodes (attaching leading comments/attributes to their
/// target declaration) into [`TopItem`]s.
fn collect_items_from_nodes(
    children: &[Node<'_>],
    source: &str,
    bytes: &[u8],
) -> Vec<TopItem> {
    let mut items = Vec::new();
    let mut idx = 0usize;
    while idx < children.len() {
        let node = children[idx];
        let kind = node.kind();
        let node_slice = source[node.start_byte()..node.end_byte()].trim();

        if is_inner_crate_header(kind, node_slice) {
            if !node_slice.is_empty() {
                items.push(TopItem::Other(node_slice.to_string()));
            }
            idx += 1;
            continue;
        }

        if is_leading_annotation(kind) && idx + 1 < children.len() {
            let start_byte = node.start_byte();
            while idx + 1 < children.len() {
                let cur = children[idx];
                let cur_slice = source[cur.start_byte()..cur.end_byte()].trim();
                if is_leading_annotation(cur.kind())
                    && !is_inner_crate_header(cur.kind(), cur_slice)
                {
                    idx += 1;
                } else {
                    break;
                }
            }
            let decl_node = children[idx];
            let full_text = source[start_byte..decl_node.end_byte()].trim().to_string();
            if is_import_node(decl_node.kind()) {
                items.push(TopItem::Import(full_text));
            } else if let Some((prefix, name)) = classify_declaration(decl_node, bytes) {
                let key = format!("{prefix}::{name}");
                let label = format!("{prefix} {name}");
                items.push(TopItem::Decl {
                    key,
                    label,
                    text: full_text,
                });
            } else if !full_text.is_empty() {
                items.push(TopItem::Other(full_text));
            }
            idx += 1;
            continue;
        }

        let slice = node_slice.to_string();
        if slice.is_empty() {
            idx += 1;
            continue;
        }

        if is_import_node(kind) {
            items.push(TopItem::Import(slice));
        } else if let Some((prefix, name)) = classify_declaration(node, bytes) {
            let key = format!("{prefix}::{name}");
            let label = format!("{prefix} {name}");
            items.push(TopItem::Decl {
                key,
                label,
                text: slice,
            });
        } else {
            items.push(TopItem::Other(slice));
        }
        idx += 1;
    }
    items
}

/// Returns `true` if `kind` or `text` represents a file/module-level inner attribute or doc comment (`//!`, `#![...]`).
fn is_inner_crate_header(kind: &str, text: &str) -> bool {
    kind == "inner_attribute_item"
        || text.starts_with("//!")
        || text.starts_with("/*!")
        || text.starts_with("#![")
}

/// Returns `true` if `kind` is a leading doc comment, attribute, or decorator that attaches to the next declaration.
fn is_leading_annotation(kind: &str) -> bool {
    matches!(
        kind,
        "attribute_item" | "line_comment" | "block_comment" | "comment" | "decorator"
    )
}

/// Returns `true` if `kind` is an import or `use` statement node.
fn is_import_node(kind: &str) -> bool {
    matches!(
        kind,
        "use_declaration"
            | "import_statement"
            | "import_from_statement"
            | "import_declaration"
    )
}

/// Classifies a declaration AST node into `(kind_prefix, symbol_name)`.
fn classify_declaration(node: Node<'_>, source: &[u8]) -> Option<(&'static str, String)> {
    let kind = node.kind();
    match kind {
        "function_item"
        | "function_signature_item"
        | "function_declaration"
        | "function_definition"
        | "method_declaration"
        | "method_definition"
        | "method_signature" => {
            let name = find_identifier(node, source)?;
            Some(("fn", name))
        }
        "field_declaration"
        | "public_field_definition"
        | "field_definition"
        | "property_signature" => {
            let name = find_identifier(node, source)?;
            Some(("field", name))
        }
        "struct_item" => {
            let name = find_identifier(node, source)?;
            Some(("struct", name))
        }
        "enum_item" | "enum_declaration" => {
            let name = find_identifier(node, source)?;
            Some(("enum", name))
        }
        "trait_item" | "interface_declaration" => {
            let name = find_identifier(node, source)?;
            Some(("trait", name))
        }
        "impl_item" => {
            let name = extract_impl_name(node, source)?;
            Some(("impl", name))
        }
        "class_declaration" | "class_definition" => {
            let name = find_identifier(node, source)?;
            Some(("class", name))
        }
        "type_item" | "type_alias_declaration" | "type_declaration" => {
            let name = find_identifier(node, source)?;
            Some(("type", name))
        }
        "const_item" | "static_item" | "lexical_declaration" => {
            let name = find_identifier(node, source)?;
            Some(("const", name))
        }
        "mod_item" => {
            let name = find_identifier(node, source)?;
            Some(("mod", name))
        }
        "export_statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some((prefix, name)) = classify_declaration(child, source) {
                    return Some((prefix, name));
                }
            }
            None
        }
        _ => None,
    }
}

/// Extracts the primary identifier name from a declaration node.
fn find_identifier(node: Node<'_>, source: &[u8]) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        if let Ok(s) = name_node.utf8_text(source) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier" | "type_identifier" | "property_identifier" | "field_identifier"
        ) {
            if let Ok(s) = child.utf8_text(source) {
                return Some(s.trim().to_string());
            }
        }

        if matches!(child.kind(), "variable_declarator" | "type_spec") {
            if let Some(nested) = find_identifier(child, source) {
                return Some(nested);
            }
        }
    }
    None
}

/// Extracts a normalized `<Trait>_for_<Type>` or `<Type>` identifier for a Rust `impl` block.
fn extract_impl_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "type_identifier" | "generic_type" | "scoped_type_identifier"
        ) {
            if let Ok(s) = child.utf8_text(source) {
                parts.push(s.trim().to_string());
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("_for_"))
    }
}

/// Commutatively 3-way merges top-level import declarations, combining specifier lists imported
/// from the same module path.
fn merge_imports_3way(
    base_items: &[TopItem],
    left_items: &[TopItem],
    right_items: &[TopItem],
    left_label: &str,
    right_label: &str,
) -> Vec<String> {
    let base_map = extract_import_map(base_items);
    let left_map = extract_import_map(left_items);
    let right_map = extract_import_map(right_items);

    let mut ordered_keys: Vec<String> = Vec::new();
    for item in left_items.iter().chain(right_items.iter()) {
        if let TopItem::Import(s) = item {
            let k = extract_import_module_key(s.trim());
            if !ordered_keys.contains(&k) {
                ordered_keys.push(k);
            }
        }
    }

    let mut merged = Vec::new();
    for key in ordered_keys {
        match (
            base_map.get(&key),
            left_map.get(&key),
            right_map.get(&key),
        ) {
            (Some(b), Some(l), Some(r)) => {
                if l == r || r == b {
                    merged.push(l.clone());
                } else if l == b {
                    merged.push(r.clone());
                } else if let Some(combined) =
                    crate::text_merge::try_merge_same_skeleton_line(Some(b), l, r)
                {
                    merged.push(combined);
                } else {
                    let res = merge_text_3way(b, l, r, left_label, right_label);
                    merged.push(res.content.trim().to_string());
                }
            }
            (None, Some(l), Some(r)) => {
                if l == r {
                    merged.push(l.clone());
                } else if let Some(combined) =
                    crate::text_merge::try_merge_same_skeleton_line(None, l, r)
                {
                    merged.push(combined);
                } else {
                    merged.push(l.clone());
                    merged.push(r.clone());
                }
            }
            (None, Some(l), None) => merged.push(l.clone()),
            (None, None, Some(r)) => merged.push(r.clone()),
            (Some(b), Some(l), None) => {
                if l != b {
                    merged.push(l.clone());
                }
            }
            (Some(b), None, Some(r)) => {
                if r != b {
                    merged.push(r.clone());
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }
    merged
}

/// Builds a map of import statements keyed by their module skeleton so duplicate imports from
/// the same module are coalesced.
fn extract_import_map(items: &[TopItem]) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for item in items {
        if let TopItem::Import(s) = item {
            let trimmed = s.trim().to_string();
            let key = extract_import_module_key(&trimmed);
            if let Some(existing) = map.get(&key).cloned() {
                if let Some(combined) =
                    crate::text_merge::try_merge_same_skeleton_line(None, &existing, &trimmed)
                {
                    map.insert(key, combined);
                    continue;
                }
            }
            map.insert(key, trimmed);
        }
    }
    map
}

/// Computes the canonical skeleton key for an import statement (`import { ... } from "mod"` -> `import {  } from "mod"`).
fn extract_import_module_key(imp: &str) -> String {
    crate::text_merge::delimited_skeleton_key(imp).unwrap_or_else(|| imp.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_concurrent_imports_and_eof_functions_auto_merge_cleanly() {
        let base = r#"use std::collections::BTreeMap;

pub fn existing_helper() -> i32 {
    42
}
"#;
        let left = r#"use std::collections::BTreeMap;
use std::sync::Arc;

pub fn existing_helper() -> i32 {
    42
}

pub fn authenticate(token: &str) -> bool {
    !token.is_empty()
}
"#;
        let right = r#"use std::collections::BTreeMap;
use std::path::PathBuf;

pub fn existing_helper() -> i32 {
    42
}

pub fn charge_card(amount_cents: u64) -> u64 {
    amount_cents
}
"#;

        let res = merge_file_3way("src/lib.rs", base, left, right, "agent-auth", "agent-billing");
        assert!(!res.has_conflicts, "expected clean AST merge, got:\n{}", res.content);
        assert!(res.content.contains("use std::sync::Arc;"));
        assert!(res.content.contains("use std::path::PathBuf;"));
        assert!(res.content.contains("pub fn authenticate"));
        assert!(res.content.contains("pub fn charge_card"));
    }

    #[test]
    fn rust_concurrent_methods_added_inside_same_impl_block_merge_cleanly() {
        let base = r#"pub struct AuthService;

impl AuthService {
    pub fn new() -> Self {
        Self
    }
}
"#;
        let left = r#"pub struct AuthService;

impl AuthService {
    pub fn new() -> Self {
        Self
    }

    pub fn verify_token(&self, token: &str) -> bool {
        !token.is_empty()
    }
}
"#;
        let right = r#"pub struct AuthService;

impl AuthService {
    pub fn new() -> Self {
        Self
    }

    pub fn revoke_token(&self, token_id: &str) -> bool {
        !token_id.is_empty()
    }
}
"#;

        let res = merge_file_3way("src/auth.rs", base, left, right, "agent-1", "agent-2");
        assert!(
            !res.has_conflicts,
            "expected clean recursive impl merge, got:\n{}",
            res.content
        );
        assert!(res.content.contains("pub fn verify_token"));
        assert!(res.content.contains("pub fn revoke_token"));
    }

    #[test]
    fn typescript_concurrent_sibling_functions_and_imports_merge_cleanly() {
        let base = "import { db } from './db';\n\nexport function healthCheck() {\n  return true;\n}\n";
        let left = "import { db } from './db';\nimport { verifyJwt } from './jwt';\n\nexport function healthCheck() {\n  return true;\n}\n\nexport function loginUser(token: string) {\n  return verifyJwt(token);\n}\n";
        let right = "import { db } from './db';\nimport { stripe } from './stripe';\n\nexport function healthCheck() {\n  return true;\n}\n\nexport function createInvoice(total: number) {\n  return total;\n}\n";

        let res = merge_file_3way("packages/api/routes.ts", base, left, right, "claude", "antigravity");
        assert!(!res.has_conflicts);
        assert!(res.content.contains("import { verifyJwt } from './jwt';"));
        assert!(res.content.contains("import { stripe } from './stripe';"));
        assert!(res.content.contains("export function loginUser"));
        assert!(res.content.contains("export function createInvoice"));
    }

    #[test]
    fn overlapping_edits_to_same_function_produce_scoped_symbol_conflict() {
        let base = "pub fn compute_tax(amount: u64) -> u64 {\n    amount * 10 / 100\n}\n\npub fn untouched() -> bool {\n    true\n}\n";
        let left = "pub fn compute_tax(amount: u64) -> u64 {\n    amount * 15 / 100\n}\n\npub fn untouched() -> bool {\n    true\n}\n";
        let right = "pub fn compute_tax(amount: u64) -> u64 {\n    amount * 20 / 100\n}\n\npub fn untouched() -> bool {\n    true\n}\n";

        let res = merge_file_3way("src/tax.rs", base, left, right, "agent-1", "agent-2");
        assert!(res.has_conflicts);
        assert_eq!(res.conflicted_symbols, vec!["fn compute_tax".to_string()]);
        assert!(res.content.contains("pub fn untouched() -> bool"));

        let unioned = merge_ast_union("src/tax.rs", base, left, right);
        assert!(!unioned.contains("<<<<<<<"));
        assert!(unioned.contains("amount * 15 / 100"));
        assert!(unioned.contains("amount * 20 / 100"));
    }

    #[test]
    fn python_and_go_concurrent_declarations_and_imports_merge_cleanly() {
        let py_base = "import os\n\ndef ping():\n    return 'pong'\n";
        let py_left = "import os\nimport json\n\ndef ping():\n    return 'pong'\n\ndef parse_payload(raw):\n    return json.loads(raw)\n";
        let py_right = "import os\nimport hashlib\n\ndef ping():\n    return 'pong'\n\ndef hash_token(token):\n    return hashlib.sha256(token.encode()).hexdigest()\n";

        let py_res = merge_file_3way("app/handlers.py", py_base, py_left, py_right, "agent-a", "agent-b");
        assert!(!py_res.has_conflicts, "expected clean Python AST merge, got:\n{}", py_res.content);
        assert!(py_res.content.contains("import json"));
        assert!(py_res.content.contains("import hashlib"));
        assert!(py_res.content.contains("def parse_payload"));
        assert!(py_res.content.contains("def hash_token"));

        let go_base = "package main\n\nfunc Health() string {\n\treturn \"ok\"\n}\n";
        let go_left = "package main\n\nfunc Health() string {\n\treturn \"ok\"\n}\n\nfunc VerifyToken(tok string) bool {\n\treturn len(tok) > 0\n}\n";
        let go_right = "package main\n\nfunc Health() string {\n\treturn \"ok\"\n}\n\nfunc ChargeInvoice(cents int) int {\n\treturn cents\n}\n";

        let go_res = merge_file_3way("cmd/server/main.go", go_base, go_left, go_right, "agent-a", "agent-b");
        assert!(!go_res.has_conflicts, "expected clean Go AST merge, got:\n{}", go_res.content);
        assert!(go_res.content.contains("func VerifyToken"));
        assert!(go_res.content.contains("func ChargeInvoice"));
    }

    #[test]
    fn concurrent_function_parameter_and_body_edits_merge_cleanly() {
        let base = r#"export function formatCurrency(amount: number, currency: string): string {
    try {
        return new Intl.NumberFormat("en-US", {
            style: "currency",
            currency,
        }).format(amount);
    } catch {
        return `${currency} ${amount.toFixed(2)}`;
    }
}
"#;
        let left = r#"export function formatCurrency(amount: number, currency: string, locale: string = "en-US"): string {
    try {
        return new Intl.NumberFormat(locale, {
            style: "currency",
            currency,
        }).format(amount);
    } catch {
        return `${currency} ${amount.toFixed(2)}`;
    }
}
"#;
        let right = r#"export function formatCurrency(amount: number, currency: string, decimals: number = 2): string {
    try {
        return new Intl.NumberFormat("en-US", {
            style: "currency",
            currency,
        }).format(amount);
    } catch {
        return `${currency} ${amount.toFixed(decimals)}`;
    }
}
"#;

        let res = merge_file_3way("src/app.ts", base, left, right, "agent-locale", "agent-decimals");
        assert!(
            !res.has_conflicts,
            "expected clean AST parameter + body merge, got:\n{}",
            res.content
        );
        assert!(res.content.contains(
            "export function formatCurrency(amount: number, currency: string, locale: string = \"en-US\", decimals: number = 2): string"
        ));
        assert!(res.content.contains("new Intl.NumberFormat(locale, {"));
        assert!(res.content.contains("return `${currency} ${amount.toFixed(decimals)}`;"));
    }

    #[test]
    fn same_module_imports_arrays_maps_and_any_language_or_text_merge_cleanly() {
        let ts_base = r#"import { readFile } from "./fs";

export const ROLES = ["admin", "user"];
export const CONFIG = { host: "localhost", port: 8080 };
"#;
        let ts_left = r#"import { readFile, writeFile } from "./fs";

export const ROLES = ["admin", "user", "editor"];
export const CONFIG = { host: "localhost", port: 9000, tls: true };
"#;
        let ts_right = r#"import { readFile, stat } from "./fs";

export const ROLES = ["admin", "user", "auditor"];
export const CONFIG = { host: "localhost", port: 8080, timeout: 5000 };
"#;
        let ts_res = merge_file_3way("src/config.ts", ts_base, ts_left, ts_right, "a", "b");
        assert!(!ts_res.has_conflicts, "got:\n{}", ts_res.content);
        assert!(ts_res.content.contains("import { readFile, writeFile, stat } from \"./fs\";"));
        assert!(ts_res.content.contains("export const ROLES = [\"admin\", \"user\", \"editor\", \"auditor\"];"));
        assert!(ts_res.content.contains("export const CONFIG = { host: \"localhost\", port: 9000, tls: true, timeout: 5000 };"));

        let php_base = "<?php\nfunction sendMail($to, $subject) {\n    $headers = ['From' => 'noreply@example.com'];\n    return mail($to, $subject);\n}\n";
        let php_left = "<?php\nfunction sendMail($to, $subject, $cc = null) {\n    $headers = ['From' => 'noreply@example.com', 'Cc' => $cc];\n    return mail($to, $subject);\n}\n";
        let php_right = "<?php\nfunction sendMail($to, $subject, $bcc = null) {\n    $headers = ['From' => 'noreply@example.com', 'Bcc' => $bcc];\n    return mail($to, $subject);\n}\n";
        let php_res = merge_file_3way("app/Mail.php", php_base, php_left, php_right, "a", "b");
        assert!(!php_res.has_conflicts, "got:\n{}", php_res.content);
        assert!(php_res.content.contains("function sendMail($to, $subject, $cc = null, $bcc = null)"));
        assert!(php_res.content.contains("'From' => 'noreply@example.com', 'Cc' => $cc, 'Bcc' => $bcc"));

        let md_base = "# Architecture\n\nTessera merges code across agents in a single working tree.\n\n## Features\n- Fast snapshots\n- Lock-free oplog\n";
        let md_left = "# Architecture\n\nTessera merges code and prose across agents in a single working tree.\n\n## Features\n- Fast snapshots\n- Lock-free oplog\n- Sub-line token merging\n";
        let md_right = "# Architecture\n\nTessera merges code across agents in a single shared working tree.\n\n## Features\n- Fast snapshots\n- Lock-free oplog\n- Delimited collection merging\n";
        let md_res = merge_file_3way("README.md", md_base, md_left, md_right, "a", "b");
        assert!(!md_res.has_conflicts, "got:\n{}", md_res.content);
        assert!(md_res.content.contains("Tessera merges code and prose across agents in a single shared working tree."));
        assert!(md_res.content.contains("- Sub-line token merging"));
        assert!(md_res.content.contains("- Delimited collection merging"));
    }

    #[test]
    fn vue_sfc_concurrent_script_template_and_style_edits_merge_cleanly() {
        let base = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\n</script>\n\n<template>\n  <HelloWorld />\n</template>\n";
        let left = "<script setup lang=\"ts\">\nimport { ref } from 'vue'\nimport HelloWorld from './components/HelloWorld.vue'\n\nconst count = ref(0)\nfunction increment() {\n  count.value += 1\n}\n</script>\n\n<template>\n  <HelloWorld />\n  <section id=\"blue-counter\" class=\"panel-blue\">\n    <h2>Blue Counter: {{ count }}</h2>\n    <button @click=\"increment\">+1</button>\n  </section>\n</template>\n\n<style scoped>\n.panel-blue {\n  background: #1d4ed8;\n  color: #ffffff;\n}\n</style>\n";
        let right = "<script setup lang=\"ts\">\nimport { ref, computed } from 'vue'\nimport HelloWorld from './components/HelloWorld.vue'\n\nconst newTodo = ref('')\nconst todos = ref<string[]>([])\nconst todoCount = computed(() => todos.value.length)\nfunction addTodo() {\n  if (newTodo.value.trim()) {\n    todos.value.push(newTodo.value.trim())\n    newTodo.value = ''\n  }\n}\n</script>\n\n<template>\n  <HelloWorld />\n  <section id=\"orange-todo\" class=\"panel-orange\">\n    <h2>Orange Todos ({{ todoCount }})</h2>\n    <button @click=\"addTodo\">Add</button>\n  </section>\n</template>\n\n<style scoped>\n.panel-orange {\n  background: #ea580c;\n  color: #ffffff;\n}\n</style>\n";

        let res = merge_file_3way(
            "src/App.vue",
            base,
            left,
            right,
            "feat/blue-counter",
            "feat/orange-todo",
        );
        assert!(
            !res.has_conflicts,
            "expected clean Vue SFC 3-way AST + template + style merge, got:\n{}",
            res.content
        );
        assert!(res.content.contains("import { ref, computed } from 'vue'"));
        assert_eq!(res.content.matches("from 'vue'").count(), 1);
        assert!(res.content.contains("const count = ref(0)"));
        assert!(res.content.contains("function increment()"));
        assert!(res.content.contains("const newTodo = ref('')"));
        assert!(res.content.contains("function addTodo()"));
        assert!(res.content.contains("<section id=\"blue-counter\""));
        assert!(res.content.contains("<section id=\"orange-todo\""));
        assert!(res.content.contains(".panel-blue {"));
        assert!(res.content.contains(".panel-orange {"));
        assert_eq!(res.content.matches("<style scoped>").count(), 1);
    }
}
