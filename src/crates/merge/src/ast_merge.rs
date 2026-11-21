//! Tree-sitter structural 3-way AST/CST merging, recursive symbol extraction, and single-file component merging.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tree_sitter::{Language, Node, Parser};

use crate::drivers::try_merge_with_driver_labeled;
use crate::text_merge::merge_text_3way;

/// Supported programming languages for Tree-sitter structural AST analysis and 3-way merging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AstLanguage {
    /// Rust (`.rs`).
    Rust,
    /// TypeScript (`.ts`, `.mts`, `.cts`).
    TypeScript,
    /// TSX (`.tsx`).
    Tsx,
    /// JavaScript / JSX (`.js`, `.jsx`, `.mjs`, `.cjs`).
    JavaScript,
    /// Python (`.py`, `.pyi`).
    Python,
    /// Go (`.go`).
    Go,
}

impl AstLanguage {
    /// Detects the [`AstLanguage`] from a file path extension.
    pub fn from_path(path: &str) -> Option<Self> {
        let ext = Path::new(path).extension()?.to_str()?;

        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
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
        }
    }

    /// Returns the default single-level indentation string for `self`.
    fn default_indent(self) -> &'static str {
        match self {
            Self::Go => "\t",
            Self::TypeScript | Self::Tsx | Self::JavaScript => "  ",
            Self::Rust | Self::Python => "    ",
        }
    }
}

/// Metadata for a single top-level or nested AST symbol extracted via Tree-sitter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AstSymbol {
    /// Normalized display label (e.g. `fn verify_jwt`, `impl AuthService::verify_token`, `class Svc::handle`).
    pub label: String,
    /// Raw Tree-sitter node kind (e.g. `function_item`).
    pub kind: String,
    /// Identifier name of the declaration.
    pub name: String,
    /// 1-indexed start line in the file.
    pub start_line: usize,
    /// 1-indexed end line in the file.
    pub end_line: usize,
    /// Blake3 digest of the normalized declaration body (or container header when nested symbols exist).
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
    /// Leading preamble item that must remain above hoisted imports (Go `package`, Rust `//!`/`#![...]`,
    /// JS/TS `"use client"`/`"use strict"`, Python module docstring).
    Preamble(String),
    /// Top-level or module-level import or `use` declaration.
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
    /// Unclassified statement or standalone comment block.
    Other(String),
}

/// Parsed container metadata and inner member items for recursive structural merging.
struct ParsedContainer {
    /// Opening header text including `{` (or `:` for Python).
    header: String,
    /// Classified child items inside the container body.
    items: Vec<TopItem>,
    /// Closing delimiter (`}` or empty string for Python).
    footer: String,
    /// Detected single-level indentation string used by members inside this container.
    indent: String,
    /// `true` for comma-separated Rust `struct` field lists and `enum` variant lists.
    is_comma_separated: bool,
}

/// Outcome of 3-way import merging, including conflict propagation metadata.
struct MergedImports {
    /// Merged import statement lines.
    lines: Vec<String>,
    /// `true` if one or more import statements conflicted.
    has_conflicts: bool,
    /// Base snippet of the first import conflict.
    base_snippet: String,
    /// Left snippet of the first import conflict.
    left_snippet: String,
    /// Right snippet of the first import conflict.
    right_snippet: String,
}

/// Extracts all named top-level and nested container-member AST symbols (`fn`, `struct`, `enum`,
/// `impl Foo::bar`, `class Svc::handle`, `trait`, `interface`, `mod`, etc.) from `source` for `path`.
pub fn extract_ast_symbols(path: &str, source: &str) -> Vec<AstSymbol> {
    if is_sfc_path(path) {
        if let Some((_, open_tag, script_body, _, _)) = split_sfc_script_block(source) {
            let synth_path = match detect_sfc_script_language(&open_tag) {
                AstLanguage::Tsx => "sfc.tsx",
                AstLanguage::JavaScript => "sfc.jsx",
                _ => "sfc.ts",
            };
            return extract_ast_symbols(synth_path, &script_body);
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
    collect_ast_symbols_in_node(tree.root_node(), source, None, &mut symbols);
    symbols
}

/// Recursively collects AST symbols from `container`, descending into `impl`, `class`,
/// `interface`, `trait`, and `mod` bodies to emit method-level scoped labels.
fn collect_ast_symbols_in_node(
    container: Node<'_>,
    source: &str,
    scope_prefix: Option<&str>,
    symbols: &mut Vec<AstSymbol>,
) {
    let bytes = source.as_bytes();
    let mut cursor = container.walk();

    for child in container.children(&mut cursor) {
        let Some((kind_prefix, name)) = classify_declaration(child, bytes) else {
            continue;
        };

        let label = match scope_prefix {
            Some(parent) => format!("{parent}::{name}"),
            None => format!("{kind_prefix} {name}"),
        };

        let inner_node = unwrap_declaration_wrapper(child);

        if is_symbol_scope_container(inner_node.kind()) {
            if let Some(body_node) = find_container_body_node(inner_node) {
                let mut child_syms = Vec::new();
                collect_ast_symbols_in_node(body_node, source, Some(&label), &mut child_syms);

                if !child_syms.is_empty() {
                    // Hash only the container header when nested symbols exist so modifying two
                    // disjoint methods in the same `impl`/`class` does not collide on the whole container.
                    let hdr_slice = &source[child.start_byte()..body_node.start_byte()];
                    let normalized_hdr: String =
                        hdr_slice.split_whitespace().collect::<Vec<_>>().join(" ");
                    let body_hash = blake3::hash(normalized_hdr.as_bytes()).to_hex().to_string();
                    symbols.push(AstSymbol {
                        label,
                        kind: inner_node.kind().to_string(),
                        name,
                        start_line: child.start_position().row + 1,
                        end_line: child.end_position().row + 1,
                        body_hash,
                    });
                    symbols.extend(child_syms);
                    continue;
                }
            }
        }

        let slice = &source[child.start_byte()..child.end_byte()];
        let normalized: String = slice.split_whitespace().collect::<Vec<_>>().join(" ");
        let body_hash = blake3::hash(normalized.as_bytes()).to_hex().to_string();
        symbols.push(AstSymbol {
            label,
            kind: inner_node.kind().to_string(),
            name,
            start_line: child.start_position().row + 1,
            end_line: child.end_position().row + 1,
            body_hash,
        });
    }
}

/// Unwraps `export_statement` or `decorated_definition` wrappers to reach the underlying declaration node.
fn unwrap_declaration_wrapper(node: Node<'_>) -> Node<'_> {
    if matches!(node.kind(), "export_statement" | "decorated_definition") {
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            if !is_leading_annotation(child.kind())
                && child.kind() != "export"
                && child.kind() != "default"
            {
                return unwrap_declaration_wrapper(child);
            }
        }
    }

    node
}

/// Returns `true` if `kind` is a scoped container whose members should be extracted recursively
/// for method-level collision detection.
fn is_symbol_scope_container(kind: &str) -> bool {
    matches!(
        kind,
        "impl_item"
            | "trait_item"
            | "mod_item"
            | "class_declaration"
            | "interface_declaration"
            | "class_definition"
    )
}

/// Locates the direct body AST node of a container declaration.
fn find_container_body_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|n| {
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
    })
}

/// Computes the list of [`AstSymbol`]s in `current` that were added, modified, or deleted
/// compared to `base`.
///
/// Used by `tes radar` to detect overlapping symbol edits across agents and by Auto-Pilot
/// to synthesize semantic intent descriptions automatically when no prompt hook was fired.
pub fn diff_modified_symbols(path: &str, base: &str, current: &str) -> Vec<AstSymbol> {
    let base_list = extract_ast_symbols(path, base);
    let base_syms: BTreeMap<String, String> = base_list
        .iter()
        .map(|s| (s.label.clone(), s.body_hash.clone()))
        .collect();

    let curr_syms = extract_ast_symbols(path, current);
    let curr_labels: BTreeSet<String> = curr_syms.iter().map(|s| s.label.clone()).collect();

    let mut diff: Vec<AstSymbol> = curr_syms
        .into_iter()
        .filter(|sym| base_syms.get(&sym.label) != Some(&sym.body_hash))
        .collect();

    for base_sym in base_list {
        if !curr_labels.contains(&base_sym.label) {
            diff.push(base_sym);
        }
    }

    diff
}

/// Merges `base`, `left`, and `right` for `path` using:
/// - Specialized manifest and lockfile drivers (`Cargo.toml`, `Cargo.lock`, `package.json`) if applicable;
/// - Vue/Svelte Single-File Component (`<script>` TypeScript/TSX AST + `<template>`/`<style>` block) merge;
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

    if let Some(driver_res) =
        try_merge_with_driver_labeled(path, base, left, right, left_label, right_label)
    {
        return AstMergeOutcome {
            content: driver_res.content,
            has_conflicts: driver_res.has_conflicts,
            conflicted_symbols: driver_res.conflicting_keys,
            base_conflict_snippet: if driver_res.has_conflicts {
                base.lines().take(20).collect::<Vec<_>>().join("\n")
            } else {
                String::new()
            },
            left_conflict_snippet: if driver_res.has_conflicts {
                left.lines().take(20).collect::<Vec<_>>().join("\n")
            } else {
                String::new()
            },
            right_conflict_snippet: if driver_res.has_conflicts {
                right.lines().take(20).collect::<Vec<_>>().join("\n")
            } else {
                String::new()
            },
        };
    }

    if is_sfc_path(path) {
        if let Some(sfc_res) = try_sfc_merge(base, left, right, left_label, right_label) {
            return sfc_res;
        }
    }

    if let Some(lang) = AstLanguage::from_path(path) {
        if let Some(ast_res) =
            try_structural_ast_merge(lang, base, left, right, left_label, right_label)
        {
            return ast_res;
        }
    }

    let text_res = merge_text_3way(base, left, right, left_label, right_label);
    AstMergeOutcome {
        content: text_res.content,
        has_conflicts: text_res.has_conflicts,
        conflicted_symbols: Vec::new(),
        base_conflict_snippet: if text_res.has_conflicts {
            base.lines().take(20).collect::<Vec<_>>().join("\n")
        } else {
            String::new()
        },
        left_conflict_snippet: if text_res.has_conflicts {
            left.lines().take(20).collect::<Vec<_>>().join("\n")
        } else {
            String::new()
        },
        right_conflict_snippet: if text_res.has_conflicts {
            right.lines().take(20).collect::<Vec<_>>().join("\n")
        } else {
            String::new()
        },
    }
}

/// Returns `true` if `path` has a `.vue` or `.svelte` Single-File Component extension.
fn is_sfc_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "vue" | "svelte"))
}

/// Detects the script [`AstLanguage`] from an SFC `<script ...>` opening tag (`lang="tsx"`,
/// `lang="jsx"`, or TypeScript by default).
fn detect_sfc_script_language(open_tag: &str) -> AstLanguage {
    let lower = open_tag.to_ascii_lowercase();

    if lower.contains("lang=\"tsx\"") || lower.contains("lang='tsx'") {
        AstLanguage::Tsx
    } else if lower.contains("lang=\"jsx\"") || lower.contains("lang='jsx'") {
        AstLanguage::JavaScript
    } else {
        AstLanguage::TypeScript
    }
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

/// Merges a Vue or Svelte Single-File Component by running structural AST merge on
/// the `<script>` block and structural block/line diff3 on the surrounding `<template>` and `<style>` sections.
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
    let script_lang = detect_sfc_script_language(&open_tag);

    let script_outcome = try_structural_ast_merge(
        script_lang,
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
            conflicted_symbols: if t.has_conflicts {
                vec!["<script>".to_string()]
            } else {
                Vec::new()
            },
            base_conflict_snippet: if t.has_conflicts {
                b_body.clone()
            } else {
                String::new()
            },
            left_conflict_snippet: if t.has_conflicts {
                l_body.clone()
            } else {
                String::new()
            },
            right_conflict_snippet: if t.has_conflicts {
                r_body.clone()
            } else {
                String::new()
            },
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

    if before_res.has_conflicts {
        conflicted_symbols.push("<template>/<before-script>".to_string());
    }

    if after_res.has_conflicts {
        conflicted_symbols.push("<template>/<style>".to_string());
    }

    let (base_snippet, left_snippet, right_snippet) =
        if script_outcome.has_conflicts && !script_outcome.base_conflict_snippet.is_empty() {
            (
                script_outcome.base_conflict_snippet,
                script_outcome.left_conflict_snippet,
                script_outcome.right_conflict_snippet,
            )
        } else if before_res.has_conflicts {
            (
                b_before.lines().take(20).collect::<Vec<_>>().join("\n"),
                l_before.lines().take(20).collect::<Vec<_>>().join("\n"),
                r_before.lines().take(20).collect::<Vec<_>>().join("\n"),
            )
        } else if after_res.has_conflicts {
            (
                b_after.lines().take(20).collect::<Vec<_>>().join("\n"),
                l_after.lines().take(20).collect::<Vec<_>>().join("\n"),
                r_after.lines().take(20).collect::<Vec<_>>().join("\n"),
            )
        } else {
            (String::new(), String::new(), String::new())
        };

    Some(AstMergeOutcome {
        content: out,
        has_conflicts,
        conflicted_symbols,
        base_conflict_snippet: base_snippet,
        left_conflict_snippet: left_snippet,
        right_conflict_snippet: right_snippet,
    })
}

/// Attempts a full 3-way structural AST merge by decomposing `base`, `left`, and `right` into
/// preambles, imports, and named declarations.
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
    // named declarations AND no modified top-level imperative statements (`TopItem::Other`).
    let has_structure = left_items
        .iter()
        .chain(right_items.iter())
        .any(|it| matches!(it, TopItem::Import(_) | TopItem::Decl { .. }));

    let collect_other = |items: &[TopItem]| -> Vec<String> {
        items
            .iter()
            .filter_map(|it| match it {
                TopItem::Other(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    };

    let base_other = collect_other(&base_items);
    let left_other = collect_other(&left_items);
    let right_other = collect_other(&right_items);
    let has_modified_imperative_statements = base_other != left_other || base_other != right_other;

    if !has_structure || has_modified_imperative_statements {
        return None;
    }

    let mut out_blocks: Vec<String> = Vec::new();
    let mut conflicted_symbols = Vec::new();
    let mut base_conflict_snippet = String::new();
    let mut left_conflict_snippet = String::new();
    let mut right_conflict_snippet = String::new();

    // 3-way merge file preamble items (`package`, `//!`, `#![...]`, `"use client"`, Python docstrings)
    // against `base` so deleted headers are never resurrected and preambles stay above imports.
    let base_preamble = extract_preamble_text(&base_items);
    let left_preamble = extract_preamble_text(&left_items);
    let right_preamble = extract_preamble_text(&right_items);

    if !base_preamble.is_empty() || !left_preamble.is_empty() || !right_preamble.is_empty() {
        let pre_res = merge_text_3way(
            &base_preamble,
            &left_preamble,
            &right_preamble,
            left_label,
            right_label,
        );

        if pre_res.has_conflicts {
            conflicted_symbols.push("<preamble>".to_string());

            if base_conflict_snippet.is_empty() {
                base_conflict_snippet = base_preamble;
                left_conflict_snippet = left_preamble;
                right_conflict_snippet = right_preamble;
            }
        }

        let trimmed_pre = pre_res.content.trim_end();

        if !trimmed_pre.is_empty() {
            out_blocks.push(trimmed_pre.to_string());
        }
    }

    let merged_imports = merge_imports_3way(
        lang,
        &base_items,
        &left_items,
        &right_items,
        left_label,
        right_label,
    );

    if merged_imports.has_conflicts {
        conflicted_symbols.push("<imports>".to_string());

        if base_conflict_snippet.is_empty() {
            base_conflict_snippet = merged_imports.base_snippet;
            left_conflict_snippet = merged_imports.left_snippet;
            right_conflict_snippet = merged_imports.right_snippet;
        }
    }

    if !merged_imports.lines.is_empty() {
        out_blocks.push(merged_imports.lines.join("\n"));
    }

    let base_decls = extract_decl_map(&base_items);
    let left_decls = extract_decl_map(&left_items);
    let right_decls = extract_decl_map(&right_items);

    // Maintain canonical declaration ordering: all keys in `left` order followed by newly added keys in `right`.
    let mut ordered_keys: Vec<String> = Vec::new();

    for item in left_items.iter().chain(right_items.iter()) {
        if let TopItem::Decl { key, .. } | TopItem::Other(key) = item {
            if !ordered_keys.contains(key) {
                ordered_keys.push(key.clone());
            }
        }
    }

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

                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = container_out.base_conflict_snippet;
                            left_conflict_snippet = container_out.left_conflict_snippet;
                            right_conflict_snippet = container_out.right_conflict_snippet;
                        }
                    }

                    out_blocks.push(container_out.content.trim_end().to_string());
                } else {
                    // Both agents independently added a declaration with the exact same key and different bodies/signatures.
                    conflicted_symbols.push(l_lbl.clone());

                    if base_conflict_snippet.is_empty() {
                        left_conflict_snippet = l_txt.clone();
                        right_conflict_snippet = r_txt.clone();
                    }

                    out_blocks.push(format_explicit_conflict(
                        l_txt,
                        r_txt,
                        left_label,
                        right_label,
                    ));
                }
            }
            (None, Some((_, l_txt)), None) => {
                out_blocks.push(l_txt.clone());
            }
            (None, None, Some((_, r_txt))) => {
                out_blocks.push(r_txt.clone());
            }
            (Some((_, b_txt)), Some((l_lbl, l_txt)), None) => {
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

/// Joins all [`TopItem::Preamble`] items in `items` into a single newline-separated block.
fn extract_preamble_text(items: &[TopItem]) -> String {
    let lines: Vec<&str> = items
        .iter()
        .filter_map(|it| match it {
            TopItem::Preamble(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();

    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

/// Formats an explicit diff3 conflict block for two incompatible competing definitions.
fn format_explicit_conflict(
    left_txt: &str,
    right_txt: &str,
    left_label: &str,
    right_label: &str,
) -> String {
    format!(
        "<<<<<<< {left_label}\n{}\n=======\n{}\n>>>>>>> {right_label}",
        left_txt.trim_end(),
        right_txt.trim_end()
    )
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

    // Strip diff3 conflict markers (even if indented inside a container) while keeping both sides.
    let mut resolved_lines = Vec::new();
    let mut in_base_section = false;

    for line in outcome.content.lines() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("<<<<<<< ") {
            in_base_section = false;
            continue;
        }

        if trimmed.starts_with("||||||| ") {
            in_base_section = true;
            continue;
        }

        if trimmed == "=======" {
            in_base_section = false;
            continue;
        }

        if trimmed.starts_with(">>>>>>> ") {
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

/// Ensures a Rust `struct` field or `enum` variant text ends with a trailing comma on its last
/// non-comment code line (without placing a comma after a trailing `//` inline comment).
fn ensure_trailing_comma(member: &str) -> String {
    let lines: Vec<&str> = member.lines().collect();

    if lines.is_empty() {
        return String::new();
    }

    let Some(last_code_idx) = lines.iter().rposition(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with("//")
    }) else {
        return member.to_string();
    };

    let mut owned: Vec<String> = lines.into_iter().map(str::to_string).collect();
    let line = &owned[last_code_idx];
    let comment_pos = find_inline_slash_comment(line);
    let (code_part, comment_part) = match comment_pos {
        Some(pos) => (line[..pos].trim_end(), &line[pos..]),
        None => (line.trim_end(), ""),
    };

    if !code_part.is_empty() && !code_part.ends_with(',') {
        owned[last_code_idx] = if comment_part.is_empty() {
            format!("{code_part},")
        } else {
            format!("{code_part}, {comment_part}")
        };
    }

    owned.join("\n")
}

/// Finds the byte offset of an unquoted `//` inline comment in `line`, if present.
fn find_inline_slash_comment(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_quote: Option<u8> = None;
    let mut escaped = false;

    for (idx, &b) in bytes.iter().enumerate() {
        if let Some(q) = in_quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                in_quote = None;
            }

            continue;
        }

        if b == b'"' {
            in_quote = Some(b'"');
            continue;
        }

        if b == b'/' && bytes.get(idx + 1) == Some(&b'/') {
            return Some(idx);
        }
    }

    None
}

/// Merges a container declaration (`impl`, `trait`, `struct`, `enum`, `class`, `interface`, `mod`)
/// by aligning its inner preambles, imports, and member declarations recursively.
fn try_merge_container_declaration(
    lang: AstLanguage,
    container_label: &str,
    base_src: &str,
    left_src: &str,
    right_src: &str,
    left_label: &str,
    right_label: &str,
) -> Option<AstMergeOutcome> {
    let left_parsed = parse_container_members(lang, left_src)?;
    let right_parsed = parse_container_members(lang, right_src)?;

    if left_parsed.header.trim() != right_parsed.header.trim() {
        return None;
    }

    let base_members = if base_src.trim().is_empty() {
        Vec::new()
    } else {
        parse_container_members(lang, base_src)?.items
    };

    let indent = &left_parsed.indent;
    let is_comma = left_parsed.is_comma_separated;

    let mut member_blocks = Vec::new();
    let mut conflicted_symbols = Vec::new();
    let mut base_conflict_snippet = String::new();
    let mut left_conflict_snippet = String::new();
    let mut right_conflict_snippet = String::new();

    // Preserve and 3-way merge inner preambles (e.g., `#![allow(...)]` or `//!` inside `mod`).
    let b_pre = extract_preamble_text(&base_members);
    let l_pre = extract_preamble_text(&left_parsed.items);
    let r_pre = extract_preamble_text(&right_parsed.items);

    if !b_pre.is_empty() || !l_pre.is_empty() || !r_pre.is_empty() {
        let pre_res = merge_text_3way(&b_pre, &l_pre, &r_pre, left_label, right_label);

        if pre_res.has_conflicts {
            conflicted_symbols.push(format!("{container_label}::<preamble>"));

            if base_conflict_snippet.is_empty() {
                base_conflict_snippet = b_pre;
                left_conflict_snippet = l_pre;
                right_conflict_snippet = r_pre;
            }
        }

        let trimmed = pre_res.content.trim_end();

        if !trimmed.is_empty() {
            member_blocks.push(indent_member(trimmed, indent));
        }
    }

    // Preserve and 3-way merge inner `use` / import statements (e.g., inside Rust `mod` blocks).
    let merged_imports = merge_imports_3way(
        lang,
        &base_members,
        &left_parsed.items,
        &right_parsed.items,
        left_label,
        right_label,
    );

    if merged_imports.has_conflicts {
        conflicted_symbols.push(format!("{container_label}::<imports>"));

        if base_conflict_snippet.is_empty() {
            base_conflict_snippet = merged_imports.base_snippet;
            left_conflict_snippet = merged_imports.left_snippet;
            right_conflict_snippet = merged_imports.right_snippet;
        }
    }

    if !merged_imports.lines.is_empty() {
        let import_block = merged_imports.lines.join("\n");
        member_blocks.push(indent_member(&import_block, indent));
    }

    let base_map = extract_decl_map(&base_members);
    let left_map = extract_decl_map(&left_parsed.items);
    let right_map = extract_decl_map(&right_parsed.items);

    let mut ordered_keys = Vec::new();

    for item in left_parsed.items.iter().chain(right_parsed.items.iter()) {
        if let TopItem::Decl { key, .. } | TopItem::Other(key) = item {
            if !ordered_keys.contains(key) {
                ordered_keys.push(key.clone());
            }
        }
    }

    let format_member = |raw: &str| -> String {
        if is_comma {
            ensure_trailing_comma(raw)
        } else {
            raw.to_string()
        }
    };

    for key in ordered_keys {
        match (base_map.get(&key), left_map.get(&key), right_map.get(&key)) {
            (Some((_, b_m)), Some((m_lbl, l_m)), Some((_, r_m))) => {
                if l_m == r_m || r_m == b_m {
                    member_blocks.push(indent_member(&format_member(l_m), indent));
                } else if l_m == b_m {
                    member_blocks.push(indent_member(&format_member(r_m), indent));
                } else if !is_comma
                    && let Some(nested_out) = try_merge_container_declaration(
                        lang,
                        &format!("{container_label}::{m_lbl}"),
                        b_m,
                        l_m,
                        r_m,
                        left_label,
                        right_label,
                    )
                {
                    if nested_out.has_conflicts {
                        conflicted_symbols.extend(nested_out.conflicted_symbols);

                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = nested_out.base_conflict_snippet;
                            left_conflict_snippet = nested_out.left_conflict_snippet;
                            right_conflict_snippet = nested_out.right_conflict_snippet;
                        }
                    }

                    member_blocks.push(indent_member(nested_out.content.trim_end(), indent));
                } else if !is_comma
                    && let Some(fn_out) = try_merge_function_declaration(
                        lang,
                        &format!("{container_label}::{m_lbl}"),
                        b_m,
                        l_m,
                        r_m,
                        left_label,
                        right_label,
                    )
                {
                    if fn_out.has_conflicts {
                        conflicted_symbols.extend(fn_out.conflicted_symbols);

                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = fn_out.base_conflict_snippet;
                            left_conflict_snippet = fn_out.left_conflict_snippet;
                            right_conflict_snippet = fn_out.right_conflict_snippet;
                        }
                    }

                    member_blocks.push(indent_member(fn_out.content.trim_end(), indent));
                } else {
                    let b_fmt = format_member(b_m);
                    let l_fmt = format_member(l_m);
                    let r_fmt = format_member(r_m);
                    let inner = merge_text_3way(&b_fmt, &l_fmt, &r_fmt, left_label, right_label);

                    if inner.has_conflicts {
                        conflicted_symbols.push(format!("{container_label}::{m_lbl}"));

                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = b_m.clone();
                            left_conflict_snippet = l_m.clone();
                            right_conflict_snippet = r_m.clone();
                        }
                    }

                    member_blocks.push(indent_member(inner.content.trim_end(), indent));
                }
            }
            (None, Some((m_lbl, l_m)), Some((_, r_m))) => {
                if l_m == r_m {
                    member_blocks.push(indent_member(&format_member(l_m), indent));
                } else if !is_comma
                    && let Some(nested_out) = try_merge_container_declaration(
                        lang,
                        &format!("{container_label}::{m_lbl}"),
                        "",
                        l_m,
                        r_m,
                        left_label,
                        right_label,
                    )
                {
                    if nested_out.has_conflicts {
                        conflicted_symbols.extend(nested_out.conflicted_symbols);

                        if base_conflict_snippet.is_empty() {
                            base_conflict_snippet = nested_out.base_conflict_snippet;
                            left_conflict_snippet = nested_out.left_conflict_snippet;
                            right_conflict_snippet = nested_out.right_conflict_snippet;
                        }
                    }

                    member_blocks.push(indent_member(nested_out.content.trim_end(), indent));
                } else {
                    conflicted_symbols.push(format!("{container_label}::{m_lbl}"));

                    if base_conflict_snippet.is_empty() {
                        left_conflict_snippet = l_m.clone();
                        right_conflict_snippet = r_m.clone();
                    }

                    let conflict = format_explicit_conflict(
                        &format_member(l_m),
                        &format_member(r_m),
                        left_label,
                        right_label,
                    );
                    member_blocks.push(indent_member(&conflict, indent));
                }
            }
            (None, Some((_, l_m)), None) => {
                member_blocks.push(indent_member(&format_member(l_m), indent));
            }
            (None, None, Some((_, r_m))) => {
                member_blocks.push(indent_member(&format_member(r_m), indent));
            }
            (Some((_, b_m)), Some((m_lbl, l_m)), None) if l_m != b_m => {
                let b_fmt = format_member(b_m);
                let l_fmt = format_member(l_m);
                let inner = merge_text_3way(&b_fmt, &l_fmt, "", left_label, right_label);
                conflicted_symbols.push(format!("{container_label}::{m_lbl}"));

                if base_conflict_snippet.is_empty() {
                    base_conflict_snippet = b_m.clone();
                    left_conflict_snippet = l_m.clone();
                }

                member_blocks.push(indent_member(inner.content.trim_end(), indent));
            }
            (Some((_, b_m)), None, Some((m_lbl, r_m))) if r_m != b_m => {
                let b_fmt = format_member(b_m);
                let r_fmt = format_member(r_m);
                let inner = merge_text_3way(&b_fmt, "", &r_fmt, left_label, right_label);
                conflicted_symbols.push(format!("{container_label}::{m_lbl}"));

                if base_conflict_snippet.is_empty() {
                    base_conflict_snippet = b_m.clone();
                    right_conflict_snippet = r_m.clone();
                }

                member_blocks.push(indent_member(inner.content.trim_end(), indent));
            }
            _ => {}
        }
    }

    let separator = if is_comma { "\n" } else { "\n\n" };
    let joined_members = member_blocks.join(separator);

    let content = if left_parsed.footer.is_empty() {
        format!("{}\n{}", left_parsed.header.trim_end(), joined_members)
    } else if joined_members.is_empty() {
        format!("{}\n{}", left_parsed.header.trim_end(), left_parsed.footer)
    } else {
        format!(
            "{}\n{}\n{}",
            left_parsed.header.trim_end(),
            joined_members,
            left_parsed.footer
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

    let merged_params = merge_param_lists(lang, &b_params, &l_params, &r_params)?;
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

/// Parses `source` and extracts the byte span and individual `(param_key, param_text)` pairs of
/// the primary function or method parameter list (ignoring decorators and attributes).
fn extract_function_params_span(lang: AstLanguage, source: &str) -> Option<ParamsSpan> {
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
    let mut seen_keys = BTreeSet::new();

    for child in params_node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "(" | ")" | "," | ";" | "line_comment" | "block_comment" | "comment"
        ) {
            continue;
        }

        let text = source[child.start_byte()..child.end_byte()]
            .trim()
            .to_string();

        if text.is_empty() {
            continue;
        }

        // Bail out on Python bare positional-only `/` or keyword-only `*` markers to avoid reordering hazards.
        if lang == AstLanguage::Python && matches!(text.as_str(), "*" | "/") {
            return None;
        }

        let key = extract_param_key(child, bytes).unwrap_or_else(|| text.clone());

        // Reject duplicate parameter keys (such as duplicate destructured `{ id }` or `_` wildcards)
        // so parameters are never silently overwritten or dropped.
        if key == "_" || !seen_keys.insert(key.clone()) {
            return None;
        }

        params.push((key, text));
    }

    Some(((params_node.start_byte(), params_node.end_byte()), params))
}

/// Recursively locates the formal parameters AST node of a function or method declaration,
/// strictly skipping decorators, attributes, call arguments, and function bodies so decorator
/// arrow functions are never hijacked.
fn find_params_node(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(
        node.kind(),
        "decorator"
            | "attribute_item"
            | "attribute"
            | "arguments"
            | "call_expression"
            | "call"
            | "statement_block"
            | "block"
            | "declaration_list"
            | "class_body"
    ) {
        return None;
    }

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
        if let Some(found) = find_params_node(child) {
            return Some(found);
        }
    }

    None
}

/// Extracts the canonical parameter identifier key from a single parameter AST node.
///
/// Uses the full normalized pattern text for destructuring patterns so distinct destructured
/// parameters with overlapping first identifiers never collide.
fn extract_param_key(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    if node.kind() == "self_parameter" {
        return Some("self".to_string());
    }

    for field in ["pattern", "name"] {
        if let Some(f) = node.child_by_field_name(field) {
            if matches!(
                f.kind(),
                "object_pattern" | "array_pattern" | "tuple_pattern" | "struct_pattern"
            ) {
                if let Ok(s) = f.utf8_text(bytes) {
                    let norm: String = s.split_whitespace().collect::<Vec<_>>().join(" ");

                    if !norm.is_empty() {
                        return Some(norm);
                    }
                }
            }

            if let Some(id) = find_first_param_identifier(f, bytes) {
                return Some(id);
            }
        }
    }

    if matches!(
        node.kind(),
        "object_pattern" | "array_pattern" | "tuple_pattern" | "struct_pattern"
    ) {
        if let Ok(s) = node.utf8_text(bytes) {
            let norm: String = s.split_whitespace().collect::<Vec<_>>().join(" ");

            if !norm.is_empty() {
                return Some(norm);
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

/// Classifies a formal parameter into its language-valid positional ordering tier:
/// `0` = receiver (`self`/`&self`/`&mut self`/`cls`/`this`), `1` = required positional,
/// `2` = optional/defaulted, `3` = variadic/rest (`*args`, `...rest`, `...T`), `4` = `**kwargs`.
fn param_order_tier(lang: AstLanguage, key: &str, raw: &str, idx: usize) -> u8 {
    let trimmed = raw.trim();

    if key == "self"
        || trimmed == "self"
        || trimmed == "&self"
        || trimmed == "&mut self"
        || trimmed.starts_with("mut self")
        || trimmed.starts_with("&' ")
        || (lang == AstLanguage::Python && idx == 0 && (key == "self" || key == "cls"))
        || (matches!(lang, AstLanguage::TypeScript | AstLanguage::Tsx)
            && idx == 0
            && (key == "this" || trimmed.starts_with("this:")))
    {
        return 0;
    }

    if lang == AstLanguage::Python && trimmed.starts_with("**") {
        return 4;
    }

    if (lang == AstLanguage::Python && trimmed.starts_with('*'))
        || (matches!(
            lang,
            AstLanguage::TypeScript | AstLanguage::Tsx | AstLanguage::JavaScript
        ) && trimmed.starts_with("..."))
        || (lang == AstLanguage::Go && trimmed.contains("..."))
    {
        return 3;
    }

    if matches!(
        lang,
        AstLanguage::Python | AstLanguage::TypeScript | AstLanguage::Tsx | AstLanguage::JavaScript
    ) && (trimmed.contains('=') || trimmed.contains("?:"))
    {
        return 2;
    }

    1
}

/// Performs a 3-way structural merge of formal parameter lists keyed by parameter name while
/// respecting receiver-first (`self`), required-before-default, and variadic/rest-last invariants.
fn merge_param_lists(
    lang: AstLanguage,
    base: &[(String, String)],
    left: &[(String, String)],
    right: &[(String, String)],
) -> Option<Vec<String>> {
    let base_map: BTreeMap<&str, &str> =
        base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let left_map: BTreeMap<&str, &str> =
        left.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let right_map: BTreeMap<&str, &str> = right
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut left_resolved: Vec<(String, String, u8)> = Vec::new();

    for (idx, (k, _)) in left.iter().enumerate() {
        let key = k.as_str();

        match (
            base_map.get(key).copied(),
            left_map.get(key).copied(),
            right_map.get(key).copied(),
        ) {
            (Some(b), Some(l), Some(r)) => {
                if l == r || r == b {
                    let tier = param_order_tier(lang, key, l, idx);
                    left_resolved.push((k.clone(), l.to_string(), tier));
                } else if l == b {
                    let tier = param_order_tier(lang, key, r, idx);
                    left_resolved.push((k.clone(), r.to_string(), tier));
                } else {
                    return None;
                }
            }
            (None, Some(l), Some(r)) => {
                if l == r {
                    let tier = param_order_tier(lang, key, l, idx);
                    left_resolved.push((k.clone(), l.to_string(), tier));
                } else {
                    return None;
                }
            }
            (None, Some(l), None) => {
                let tier = param_order_tier(lang, key, l, idx);
                left_resolved.push((k.clone(), l.to_string(), tier));
            }
            (Some(b), Some(l), None) if l != b => {
                return None;
            }
            _ => {}
        }
    }

    // Collect right-only additions and verify right-only delete/modify invariants.
    let mut right_only: Vec<(String, String, u8)> = Vec::new();

    for (idx, (k, _)) in right.iter().enumerate() {
        let key = k.as_str();

        if left_map.contains_key(key) {
            continue;
        }

        match (base_map.get(key).copied(), right_map.get(key).copied()) {
            (None, Some(r)) => {
                let tier = param_order_tier(lang, key, r, idx);
                right_only.push((k.clone(), r.to_string(), tier));
            }
            (Some(b), Some(r)) if r != b => {
                return None;
            }
            _ => {}
        }
    }

    // Insert right-only additions at the end of their matching tier so `self` stays first and
    // `*args`/`**kwargs`/`...rest` stay last.
    let mut combined = left_resolved;

    for (k, raw, tier) in right_only {
        if tier == 0 {
            if combined.first().is_some_and(|(_, _, t)| *t == 0) {
                return None;
            }

            combined.insert(0, (k, raw, tier));
        } else {
            let insert_pos = combined
                .iter()
                .position(|(_, _, t)| *t > tier)
                .unwrap_or(combined.len());
            combined.insert(insert_pos, (k, raw, tier));
        }
    }

    // Verify monotonic tier invariant across the final merged parameter list.
    for pair in combined.windows(2) {
        if pair[0].2 > pair[1].2 {
            return None;
        }
    }

    Some(combined.into_iter().map(|(_, raw, _)| raw).collect())
}

/// Returns `true` if `line` is a standard diff3 conflict marker.
fn is_conflict_marker_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("<<<<<<< ")
        || t.starts_with("||||||| ")
        || t == "======="
        || t.starts_with(">>>>>>> ")
}

/// Tracks multiline string literal state across lines so column-0 content inside multiline
/// strings (`r#"..."#`, `"""..."""`, `'''...'''`, `` `...` ``) is never indented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultilineStringState {
    /// Not inside a multiline string literal.
    None,
    /// Inside a Rust raw string `r###"..."###` with `hashes` `#` delimiters.
    RustRaw(usize),
    /// Inside a JS/TS template literal `` `...` ``.
    Backtick,
    /// Inside a Python triple-double-quoted string `"""..."""`.
    TripleDouble,
    /// Inside a Python triple-single-quoted string `'''...'''`.
    TripleSingle,
}

/// Advances [`MultilineStringState`] across a single source line.
fn advance_multiline_string_state(line: &str, state: &mut MultilineStringState) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        match *state {
            MultilineStringState::None => {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
                    break;
                }

                if bytes[i] == b'#' {
                    // Skip Python comment only if not Rust `#[` attribute.
                    if bytes.get(i + 1) != Some(&b'[') && bytes.get(i + 1) != Some(&b'!') {
                        break;
                    }
                }

                if bytes[i] == b'r' {
                    let prev_ident =
                        i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');

                    if !prev_ident {
                        let mut j = i + 1;

                        while j < len && bytes[j] == b'#' {
                            j += 1;
                        }

                        if j < len && bytes[j] == b'"' {
                            let hashes = j - (i + 1);
                            *state = MultilineStringState::RustRaw(hashes);
                            i = j + 1;
                            continue;
                        }
                    }
                }

                if i + 2 < len && &bytes[i..i + 3] == b"\"\"\"" {
                    *state = MultilineStringState::TripleDouble;
                    i += 3;
                    continue;
                }

                if i + 2 < len && &bytes[i..i + 3] == b"'''" {
                    *state = MultilineStringState::TripleSingle;
                    i += 3;
                    continue;
                }

                if bytes[i] == b'`' {
                    *state = MultilineStringState::Backtick;
                    i += 1;
                    continue;
                }

                if bytes[i] == b'"' {
                    i += 1;

                    while i < len {
                        if bytes[i] == b'\\' {
                            i += 2;
                            continue;
                        }

                        if bytes[i] == b'"' {
                            i += 1;
                            break;
                        }

                        i += 1;
                    }

                    continue;
                }

                i += 1;
            }
            MultilineStringState::RustRaw(hashes) => {
                if bytes[i] == b'"' {
                    let end_hashes = i + 1 + hashes;

                    if end_hashes <= len && bytes[i + 1..end_hashes].iter().all(|&b| b == b'#') {
                        *state = MultilineStringState::None;
                        i = end_hashes;
                        continue;
                    }
                }

                i += 1;
            }
            MultilineStringState::Backtick => {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }

                if bytes[i] == b'`' {
                    *state = MultilineStringState::None;
                }

                i += 1;
            }
            MultilineStringState::TripleDouble => {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }

                if i + 2 < len && &bytes[i..i + 3] == b"\"\"\"" {
                    *state = MultilineStringState::None;
                    i += 3;
                    continue;
                }

                i += 1;
            }
            MultilineStringState::TripleSingle => {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }

                if i + 2 < len && &bytes[i..i + 3] == b"'''" {
                    *state = MultilineStringState::None;
                    i += 3;
                    continue;
                }

                i += 1;
            }
        }
    }
}

/// Indents a container member block using `indent`, keeping conflict markers at column 0 and
/// preserving column-0 lines inside multiline string literals verbatim.
fn indent_member(text: &str, indent: &str) -> String {
    let mut out = Vec::new();
    let mut ml_state = MultilineStringState::None;

    for (idx, line) in text.lines().enumerate() {
        if is_conflict_marker_line(line) {
            out.push(line.trim_start().to_string());
            continue;
        }

        let started_in_multiline = ml_state != MultilineStringState::None;
        advance_multiline_string_state(line, &mut ml_state);

        if started_in_multiline {
            out.push(line.to_string());
        } else if line.trim().is_empty() {
            out.push(String::new());
        } else if idx == 0 {
            if line.starts_with(indent) || line.starts_with('\t') {
                out.push(line.to_string());
            } else {
                out.push(format!("{indent}{line}"));
            }
        } else if line.starts_with(' ') || line.starts_with('\t') {
            out.push(line.to_string());
        } else {
            out.push(format!("{indent}{line}"));
        }
    }

    out.join("\n")
}

/// Detects the single-level indentation string of the first member inside `body_node`.
fn detect_container_indent(lang: AstLanguage, source: &str, children: &[Node<'_>]) -> String {
    if let Some(first) = children.first() {
        let start = first.start_byte();
        let line_start = source[..start].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let prefix = &source[line_start..start];

        if !prefix.is_empty() && prefix.chars().all(|c| c == ' ' || c == '\t') {
            return prefix.to_string();
        }
    }

    lang.default_indent().to_string()
}

/// Parses a container declaration (`impl`, `trait`, `class`, `interface`, `struct`, `enum`, `mod`)
/// into a [`ParsedContainer`].
fn parse_container_members(lang: AstLanguage, source: &str) -> Option<ParsedContainer> {
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

    let actual_container = unwrap_declaration_wrapper(container_node);

    if !is_container_kind(actual_container.kind()) {
        return None;
    }

    let body_node = find_container_body_node(actual_container)?;
    let is_comma_separated = matches!(
        body_node.kind(),
        "field_declaration_list" | "enum_variant_list"
    );

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
        .filter(|n| !matches!(n.kind(), "{" | "}" | ";" | ","))
        .collect();

    let indent = detect_container_indent(lang, source, &children);
    let items = collect_items_from_nodes(lang, &children, source, bytes, false);

    if items.is_empty() {
        return None;
    }

    Some(ParsedContainer {
        header,
        items,
        footer,
        indent,
        is_comma_separated,
    })
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

/// Builds a map from declaration key to `(symbol_label, source_text)` for all non-import,
/// non-preamble items.
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
            TopItem::Preamble(_) | TopItem::Import(_) => {}
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
    Some(collect_items_from_nodes(
        lang, &children, source, bytes, true,
    ))
}

/// Returns `true` if `node` is a top-of-file preamble item (Go `package_clause`, JS/TS directive
/// prologue `"use client"` / `"use strict"`, Python module docstring, or Rust `//!`/`#![...]`).
fn is_preamble_node(
    lang: AstLanguage,
    node: Node<'_>,
    node_slice: &str,
    at_top_of_file: bool,
) -> bool {
    if is_inner_crate_header(node.kind(), node_slice) {
        return true;
    }

    if !at_top_of_file {
        return false;
    }

    match lang {
        AstLanguage::Go => node.kind() == "package_clause",
        AstLanguage::TypeScript | AstLanguage::Tsx | AstLanguage::JavaScript => {
            if node.kind() == "expression_statement" {
                let core = node_slice.trim().trim_end_matches(';').trim();
                matches!(
                    core,
                    "\"use client\""
                        | "'use client'"
                        | "\"use strict\""
                        | "'use strict'"
                        | "\"use server\""
                        | "'use server'"
                )
            } else {
                false
            }
        }
        AstLanguage::Python => {
            if node.kind() == "comment"
                && (node_slice.starts_with("#!") || node_slice.contains("coding"))
            {
                return true;
            }

            if node.kind() == "expression_statement" {
                let mut c = node.walk();
                let named: Vec<Node<'_>> = node.named_children(&mut c).collect();
                named.len() == 1 && named[0].kind() == "string"
            } else {
                false
            }
        }
        AstLanguage::Rust => false,
    }
}

/// Extracts normalized `#[cfg(...)]` attribute discriminators from leading annotation nodes
/// attached to a declaration so platform-conditional items (`#[cfg(unix)]` vs `#[cfg(windows)]`)
/// receive distinct canonical keys.
fn extract_cfg_discriminator(annotations: &[Node<'_>], source: &str) -> Option<String> {
    let mut cfgs = Vec::new();

    for ann in annotations {
        if ann.kind() == "attribute_item" {
            let text = source[ann.start_byte()..ann.end_byte()].trim();

            if text.starts_with("#[cfg(") && text.ends_with(")]") {
                let norm: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
                cfgs.push(norm);
            }
        }
    }

    if cfgs.is_empty() {
        None
    } else {
        Some(cfgs.join("&"))
    }
}

/// Groups a slice of sibling Tree-sitter nodes (attaching leading comments/attributes to their
/// target declaration) into [`TopItem`]s and disambiguates any duplicate declaration keys.
fn collect_items_from_nodes(
    lang: AstLanguage,
    children: &[Node<'_>],
    source: &str,
    bytes: &[u8],
    is_file_root: bool,
) -> Vec<TopItem> {
    let mut items = Vec::new();
    let mut idx = 0usize;
    let mut seen_body_item = false;

    while idx < children.len() {
        let node = children[idx];
        let kind = node.kind();
        let node_slice = source[node.start_byte()..node.end_byte()].trim();

        if is_preamble_node(lang, node, node_slice, is_file_root && !seen_body_item) {
            if !node_slice.is_empty() {
                items.push(TopItem::Preamble(node_slice.to_string()));
            }

            idx += 1;
            continue;
        }

        if is_leading_annotation(kind) && idx + 1 < children.len() {
            let start_idx = idx;
            let start_byte = node.start_byte();

            while idx + 1 < children.len() {
                let cur = children[idx];
                let cur_slice = source[cur.start_byte()..cur.end_byte()].trim();

                if is_leading_annotation(cur.kind())
                    && !is_preamble_node(lang, cur, cur_slice, is_file_root && !seen_body_item)
                {
                    idx += 1;
                } else {
                    break;
                }
            }

            let decl_node = children[idx];
            let decl_slice = source[decl_node.start_byte()..decl_node.end_byte()].trim();
            let full_text = source[start_byte..decl_node.end_byte()].trim().to_string();

            if is_preamble_node(lang, decl_node, decl_slice, is_file_root && !seen_body_item) {
                items.push(TopItem::Preamble(full_text));
            } else if is_import_node(decl_node.kind()) {
                seen_body_item = true;
                items.push(TopItem::Import(full_text));
            } else if let Some((prefix, name)) = classify_declaration(decl_node, bytes) {
                seen_body_item = true;
                let mut key = format!("{prefix}::{name}");

                if let Some(cfg_disc) = extract_cfg_discriminator(&children[start_idx..idx], source)
                {
                    key.push_str(&format!("@@{cfg_disc}"));
                }

                let label = format!("{prefix} {name}");
                items.push(TopItem::Decl {
                    key,
                    label,
                    text: full_text,
                });
            } else if !full_text.is_empty() {
                seen_body_item = true;
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
            seen_body_item = true;
            items.push(TopItem::Import(slice));
        } else if let Some((prefix, name)) = classify_declaration(node, bytes) {
            seen_body_item = true;
            let key = format!("{prefix}::{name}");
            let label = format!("{prefix} {name}");
            items.push(TopItem::Decl {
                key,
                label,
                text: slice,
            });
        } else {
            seen_body_item = true;
            items.push(TopItem::Other(slice));
        }

        idx += 1;
    }

    disambiguate_duplicate_decl_keys(&mut items);
    items
}

/// Ensures no two sibling [`TopItem::Decl`] entries share the exact same key (which would
/// otherwise overwrite one another in `BTreeMap`), disambiguating TypeScript overload signatures
/// by normalized signature and repeated `impl` blocks by occurrence ordinal.
fn disambiguate_duplicate_decl_keys(items: &mut [TopItem]) {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for item in items.iter() {
        if let TopItem::Decl { key, .. } = item {
            *counts.entry(key.clone()).or_insert(0) += 1;
        }
    }

    let mut seen: BTreeMap<String, usize> = BTreeMap::new();

    for item in items.iter_mut() {
        if let TopItem::Decl { key, text, .. } = item {
            if counts.get(key.as_str()).copied().unwrap_or(0) <= 1 {
                continue;
            }

            if key.starts_with("fn_sig::") {
                let sig_norm: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
                *key = format!("{key}@@{sig_norm}");
                continue;
            }

            let occ = seen.entry(key.clone()).or_insert(0);
            *occ += 1;

            if *occ > 1 {
                *key = format!("{key}#{occ}");
            }
        }
    }
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
        "use_declaration" | "import_statement" | "import_from_statement" | "import_declaration"
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
        | "method_definition" => {
            let name = find_identifier(node, source)?;
            Some(("fn", name))
        }
        "function_signature" => {
            let name = find_identifier(node, source)?;
            Some(("fn_sig", name))
        }
        "method_signature" => {
            let name = find_identifier(node, source)?;
            // Inside a TypeScript class, a `method_signature` is an overload signature distinct from
            // `method_definition`; inside an `interface_declaration`, treat it as a normal method.
            let in_class = node.parent().is_some_and(|p| p.kind() == "class_body");

            if in_class {
                Some(("fn_sig", name))
            } else {
                Some(("fn", name))
            }
        }
        "method_declaration" => {
            let name = find_identifier(node, source)?;

            if let Some(recv_type) = extract_go_receiver_type(node, source) {
                Some(("fn", format!("({recv_type}).{name}")))
            } else {
                Some(("fn", name))
            }
        }
        "field_declaration"
        | "public_field_definition"
        | "field_definition"
        | "property_signature" => {
            let name = find_identifier(node, source)?;
            Some(("field", name))
        }
        "enum_variant" => {
            let name = find_identifier(node, source)?;
            Some(("variant", name))
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
        "export_statement" | "decorated_definition" => {
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

/// Extracts the receiver type string (e.g., `*Server` or `User`) from a Go `method_declaration`,
/// ignoring the receiver variable name so `func (a *Server) String()` and `func (s *Server) String()`
/// align to the same key while `func (a *A) String()` and `func (b *B) String()` stay distinct.
fn extract_go_receiver_type(node: Node<'_>, source: &[u8]) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();

    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            if let Some(ty) = child.child_by_field_name("type") {
                if let Ok(text) = ty.utf8_text(source) {
                    let norm: String = text.split_whitespace().collect::<Vec<_>>().join(" ");

                    if !norm.is_empty() {
                        return Some(norm);
                    }
                }
            }
        }
    }

    None
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

/// Extracts a canonical identifier for a Rust `impl` block capturing trait, full target type
/// (including `&str`, `[u8]`, `()`, generics), and optional `where` clause.
fn extract_impl_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let norm_text = |n: Node<'_>| -> Option<String> {
        let s = n.utf8_text(source).ok()?;
        let norm: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
        (!norm.is_empty()).then_some(norm)
    };

    let trait_txt = node.child_by_field_name("trait").and_then(norm_text);
    let type_txt = node.child_by_field_name("type").and_then(norm_text);

    let mut where_txt = None;
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if child.kind() == "where_clause" {
            where_txt = norm_text(child);
            break;
        }
    }

    let mut base = match (trait_txt, type_txt) {
        (Some(tr), Some(ty)) => format!("{tr}_for_{ty}"),
        (None, Some(ty)) => ty,
        (Some(tr), None) => tr,
        (None, None) => return None,
    };

    if let Some(w) = where_txt {
        base.push_str(&format!(" {w}"));
    }

    Some(base)
}

/// Splits leading attribute or comment lines (such as `#[cfg(unix)]`) from an import statement
/// so attributes are preserved verbatim and never mangled by delimited-skeleton merging.
fn split_import_leading_attrs(imp: &str) -> (String, String) {
    let lines: Vec<&str> = imp.lines().collect();
    let mut split_at = 0usize;

    while split_at < lines.len() {
        let t = lines[split_at].trim();

        if t.starts_with("#[") || t.starts_with("//") || t.starts_with("/*") || t.is_empty() {
            split_at += 1;
        } else {
            break;
        }
    }

    if split_at == 0 || split_at >= lines.len() {
        (String::new(), imp.trim().to_string())
    } else {
        (
            lines[..split_at].join("\n").trim().to_string(),
            lines[split_at..].join("\n").trim().to_string(),
        )
    }
}

/// Commutatively 3-way merges import declarations, combining specifier lists imported from the
/// same module path, handling Go `import (...)` groups natively, preserving `#[cfg(...)]`
/// attributes, and propagating conflicts cleanly.
fn merge_imports_3way(
    lang: AstLanguage,
    base_items: &[TopItem],
    left_items: &[TopItem],
    right_items: &[TopItem],
    left_label: &str,
    right_label: &str,
) -> MergedImports {
    if lang == AstLanguage::Go {
        return merge_go_imports_3way(base_items, left_items, right_items, left_label, right_label);
    }

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
    let mut has_conflicts = false;
    let mut base_snippet = String::new();
    let mut left_snippet = String::new();
    let mut right_snippet = String::new();

    for key in ordered_keys {
        match (base_map.get(&key), left_map.get(&key), right_map.get(&key)) {
            (Some(b), Some(l), Some(r)) => {
                if l == r || r == b {
                    merged.push(l.clone());
                } else if l == b {
                    merged.push(r.clone());
                } else if let Some(combined) = try_merge_import_with_attrs(Some(b), l, r) {
                    merged.push(combined);
                } else {
                    let res = merge_text_3way(b, l, r, left_label, right_label);

                    if res.has_conflicts {
                        has_conflicts = true;

                        if base_snippet.is_empty() {
                            base_snippet = b.clone();
                            left_snippet = l.clone();
                            right_snippet = r.clone();
                        }
                    }

                    merged.push(res.content.trim().to_string());
                }
            }
            (None, Some(l), Some(r)) => {
                if l == r {
                    merged.push(l.clone());
                } else if let Some(combined) = try_merge_import_with_attrs(None, l, r) {
                    merged.push(combined);
                } else {
                    has_conflicts = true;

                    if base_snippet.is_empty() {
                        left_snippet = l.clone();
                        right_snippet = r.clone();
                    }

                    merged.push(format_explicit_conflict(l, r, left_label, right_label));
                }
            }
            (None, Some(l), None) => merged.push(l.clone()),
            (None, None, Some(r)) => merged.push(r.clone()),
            (Some(b), Some(l), None) => {
                if l != b {
                    has_conflicts = true;

                    if base_snippet.is_empty() {
                        base_snippet = b.clone();
                        left_snippet = l.clone();
                    }

                    let res = merge_text_3way(b, l, "", left_label, right_label);
                    merged.push(res.content.trim().to_string());
                }
            }
            (Some(b), None, Some(r)) => {
                if r != b {
                    has_conflicts = true;

                    if base_snippet.is_empty() {
                        base_snippet = b.clone();
                        right_snippet = r.clone();
                    }

                    let res = merge_text_3way(b, "", r, left_label, right_label);
                    merged.push(res.content.trim().to_string());
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    MergedImports {
        lines: merged,
        has_conflicts,
        base_snippet,
        left_snippet,
        right_snippet,
    }
}

/// Merges Go `import "pkg"` and grouped `import (\n\t"pkg"\n)` declarations using newline-separated
/// Go import specs instead of comma-joining them.
fn merge_go_imports_3way(
    base_items: &[TopItem],
    left_items: &[TopItem],
    right_items: &[TopItem],
    left_label: &str,
    right_label: &str,
) -> MergedImports {
    let collect_raw = |items: &[TopItem]| -> Vec<String> {
        items
            .iter()
            .filter_map(|it| match it {
                TopItem::Import(s) => Some(s.trim().to_string()),
                _ => None,
            })
            .collect()
    };

    let b_raw = collect_raw(base_items);
    let l_raw = collect_raw(left_items);
    let r_raw = collect_raw(right_items);

    if l_raw == r_raw || r_raw == b_raw {
        return MergedImports {
            lines: l_raw,
            has_conflicts: false,
            base_snippet: String::new(),
            left_snippet: String::new(),
            right_snippet: String::new(),
        };
    }

    if l_raw == b_raw {
        return MergedImports {
            lines: r_raw,
            has_conflicts: false,
            base_snippet: String::new(),
            left_snippet: String::new(),
            right_snippet: String::new(),
        };
    }

    let b_specs = parse_go_import_specs(&b_raw);
    let l_specs = parse_go_import_specs(&l_raw);
    let r_specs = parse_go_import_specs(&r_raw);

    let (Some(b_list), Some(l_list), Some(r_list)) = (b_specs, l_specs, r_specs) else {
        let b_txt = b_raw.join("\n");
        let l_txt = l_raw.join("\n");
        let r_txt = r_raw.join("\n");
        let res = merge_text_3way(&b_txt, &l_txt, &r_txt, left_label, right_label);
        return MergedImports {
            lines: vec![res.content.trim().to_string()],
            has_conflicts: res.has_conflicts,
            base_snippet: if res.has_conflicts {
                b_txt
            } else {
                String::new()
            },
            left_snippet: if res.has_conflicts {
                l_txt
            } else {
                String::new()
            },
            right_snippet: if res.has_conflicts {
                r_txt
            } else {
                String::new()
            },
        };
    };

    let b_map: BTreeMap<&str, &str> = b_list
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let l_map: BTreeMap<&str, &str> = l_list
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let r_map: BTreeMap<&str, &str> = r_list
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut ordered_keys: Vec<&str> = Vec::new();

    for (k, _) in l_list.iter().chain(r_list.iter()) {
        if !ordered_keys.contains(&k.as_str()) {
            ordered_keys.push(k.as_str());
        }
    }

    let mut merged_specs: Vec<String> = Vec::new();
    let mut has_conflicts = false;
    let mut base_snippet = String::new();
    let mut left_snippet = String::new();
    let mut right_snippet = String::new();

    for key in ordered_keys {
        match (
            b_map.get(key).copied(),
            l_map.get(key).copied(),
            r_map.get(key).copied(),
        ) {
            (Some(b), Some(l), Some(r)) => {
                if l == r || r == b {
                    merged_specs.push(l.to_string());
                } else if l == b {
                    merged_specs.push(r.to_string());
                } else {
                    has_conflicts = true;

                    if base_snippet.is_empty() {
                        base_snippet = b.to_string();
                        left_snippet = l.to_string();
                        right_snippet = r.to_string();
                    }

                    merged_specs.push(format_explicit_conflict(l, r, left_label, right_label));
                }
            }
            (None, Some(l), Some(r)) => {
                if l == r {
                    merged_specs.push(l.to_string());
                } else {
                    has_conflicts = true;

                    if base_snippet.is_empty() {
                        left_snippet = l.to_string();
                        right_snippet = r.to_string();
                    }

                    merged_specs.push(format_explicit_conflict(l, r, left_label, right_label));
                }
            }
            (None, Some(l), None) => merged_specs.push(l.to_string()),
            (None, None, Some(r)) => merged_specs.push(r.to_string()),
            (Some(b), Some(l), None) => {
                if l != b {
                    has_conflicts = true;
                    merged_specs.push(l.to_string());
                }
            }
            (Some(b), None, Some(r)) => {
                if r != b {
                    has_conflicts = true;
                    merged_specs.push(r.to_string());
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    if merged_specs.is_empty() {
        return MergedImports {
            lines: Vec::new(),
            has_conflicts,
            base_snippet,
            left_snippet,
            right_snippet,
        };
    }

    let was_grouped = b_raw.iter().any(|s| s.contains('('))
        || l_raw.iter().any(|s| s.contains('('))
        || r_raw.iter().any(|s| s.contains('('));

    let formatted = if merged_specs.len() == 1 && !was_grouped && !has_conflicts {
        vec![format!("import {}", merged_specs[0])]
    } else {
        let mut lines = Vec::with_capacity(merged_specs.len() + 2);
        lines.push("import (".to_string());

        for spec in merged_specs {
            if spec.contains('\n') {
                lines.push(spec);
            } else {
                lines.push(format!("\t{spec}"));
            }
        }

        lines.push(")".to_string());
        vec![lines.join("\n")]
    };

    MergedImports {
        lines: formatted,
        has_conflicts,
        base_snippet,
        left_snippet,
        right_snippet,
    }
}

/// Parses Go `import` declarations into `(import_path_key, raw_spec_line)` pairs.
fn parse_go_import_specs(raw_imports: &[String]) -> Option<Vec<(String, String)>> {
    let mut specs = Vec::new();
    let mut seen = BTreeSet::new();

    for decl in raw_imports {
        let trimmed = decl.trim();
        let rest = trimmed.strip_prefix("import")?.trim();

        if rest.starts_with('(') && rest.ends_with(')') {
            let inner = &rest[1..rest.len() - 1];

            for line in inner.lines() {
                let t = line.trim();

                if t.is_empty() {
                    continue;
                }

                if t.starts_with("//") || t.starts_with("/*") {
                    return None;
                }

                let key = extract_go_import_path(t)?;

                if seen.insert(key.clone()) {
                    specs.push((key, t.to_string()));
                }
            }
        } else {
            let key = extract_go_import_path(rest)?;

            if seen.insert(key.clone()) {
                specs.push((key, rest.to_string()));
            }
        }
    }

    Some(specs)
}

/// Extracts the quoted package path (e.g., `"net/http"`) from a single Go import spec line.
fn extract_go_import_path(spec: &str) -> Option<String> {
    let start = spec.find('"')?;
    let end_rel = spec[start + 1..].find('"')?;
    Some(spec[start..=start + 1 + end_rel].to_string())
}

/// Merges two import statements that share identical leading attributes (such as `#[cfg(unix)]`),
/// applying skeleton merging solely to the underlying `use` / `import` statement.
fn try_merge_import_with_attrs(base: Option<&str>, left: &str, right: &str) -> Option<String> {
    let (l_attrs, l_stmt) = split_import_leading_attrs(left);
    let (r_attrs, r_stmt) = split_import_leading_attrs(right);

    if l_attrs != r_attrs {
        return None;
    }

    let b_stmt_owned = base.map(|b| split_import_leading_attrs(b).1);
    let merged_stmt =
        crate::text_merge::try_merge_same_skeleton_line(b_stmt_owned.as_deref(), &l_stmt, &r_stmt)?;

    if l_attrs.is_empty() {
        Some(merged_stmt)
    } else {
        Some(format!("{l_attrs}\n{merged_stmt}"))
    }
}

/// Builds a map of import statements keyed by their module skeleton (plus any leading attributes)
/// so duplicate imports from the same module under the same `#[cfg(...)]` are coalesced.
fn extract_import_map(items: &[TopItem]) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();

    for item in items {
        if let TopItem::Import(s) = item {
            let trimmed = s.trim().to_string();
            let key = extract_import_module_key(&trimmed);

            if let Some(existing) = map.get(&key).cloned() {
                if let Some(combined) = try_merge_import_with_attrs(None, &existing, &trimmed) {
                    map.insert(key, combined);
                    continue;
                }
            }

            map.insert(key, trimmed);
        }
    }

    map
}

/// Computes the canonical skeleton key for an import statement, preserving any leading `#[cfg(...)]`
/// attributes separately so conditional imports never collide or get mangled.
fn extract_import_module_key(imp: &str) -> String {
    let (attrs, stmt) = split_import_leading_attrs(imp);
    let stmt_key = crate::text_merge::delimited_skeleton_key(&stmt).unwrap_or(stmt);

    if attrs.is_empty() {
        stmt_key
    } else {
        let attrs_norm: String = attrs.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("{attrs_norm}\n{stmt_key}")
    }
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

        let res = merge_file_3way(
            "src/lib.rs",
            base,
            left,
            right,
            "agent-auth",
            "agent-billing",
        );
        assert!(
            !res.has_conflicts,
            "expected clean AST merge, got:\n{}",
            res.content
        );
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
    fn rust_struct_and_enum_commas_and_variant_conflicts_are_preserved() {
        let base_struct = "pub struct User {\n    pub id: u64,\n}\n";
        let left_struct = "pub struct User {\n    pub id: u64,\n    pub name: String,\n}\n";
        let right_struct = "pub struct User {\n    pub id: u64,\n    pub email: String,\n}\n";

        let res_struct = merge_file_3way(
            "src/user.rs",
            base_struct,
            left_struct,
            right_struct,
            "a",
            "b",
        );
        assert!(!res_struct.has_conflicts, "got:\n{}", res_struct.content);
        assert!(res_struct.content.contains("pub id: u64,"));
        assert!(res_struct.content.contains("pub name: String,"));
        assert!(res_struct.content.contains("pub email: String,"));
        assert!(!res_struct.content.contains("\n    ,\n"));

        let base_enum = "pub enum State {\n    Idle,\n    Pending(u16),\n}\n";
        let left_enum = "pub enum State {\n    Idle,\n    Pending(u32),\n}\n";
        let right_enum = "pub enum State {\n    Idle,\n    Pending(u64),\n}\n";

        let res_enum = merge_file_3way("src/state.rs", base_enum, left_enum, right_enum, "a", "b");
        assert!(
            res_enum.has_conflicts,
            "expected variant conflict, got:\n{}",
            res_enum.content
        );
        assert!(res_enum.content.contains("Idle,"));
    }

    #[test]
    fn rust_mod_inner_use_imports_are_preserved_and_merged() {
        let base = "pub fn root() {}\n\nmod tests {\n    use super::*;\n\n    fn test_a() {}\n}\n";
        let left = "pub fn root() {}\n\nmod tests {\n    use super::*;\n    use std::sync::Arc;\n\n    fn test_a() {}\n\n    fn test_b() {}\n}\n";
        let right = "pub fn root() {}\n\nmod tests {\n    use super::*;\n    use std::path::Path;\n\n    fn test_a() {}\n\n    fn test_c() {}\n}\n";

        let res = merge_file_3way("src/lib.rs", base, left, right, "a", "b");
        assert!(!res.has_conflicts, "got:\n{}", res.content);
        assert!(res.content.contains("use super::*;"));
        assert!(res.content.contains("use std::sync::Arc;"));
        assert!(res.content.contains("use std::path::Path;"));
        assert!(res.content.contains("fn test_b()"));
        assert!(res.content.contains("fn test_c()"));
    }

    #[test]
    fn go_package_stays_above_multiline_imports_and_methods_disambiguate_receivers() {
        let base = "package server\n\nimport (\n\t\"fmt\"\n)\n\nfunc (a *Alpha) String() string {\n\treturn fmt.Sprint(a)\n}\n\nfunc (b *Beta) String() string {\n\treturn fmt.Sprint(b)\n}\n";
        let left = "package server\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc (a *Alpha) String() string {\n\treturn fmt.Sprintf(\"A:%v\", a)\n}\n\nfunc (b *Beta) String() string {\n\treturn fmt.Sprint(b)\n}\n";
        let right = "package server\n\nimport (\n\t\"fmt\"\n\t\"net/http\"\n)\n\nfunc (a *Alpha) String() string {\n\treturn fmt.Sprint(a)\n}\n\nfunc (b *Beta) String() string {\n\treturn fmt.Sprintf(\"B:%v\", b)\n}\n";

        let res = merge_file_3way("pkg/server.go", base, left, right, "a", "b");
        assert!(!res.has_conflicts, "got:\n{}", res.content);
        assert!(
            res.content.starts_with("package server\n"),
            "package clause must stay at top, got:\n{}",
            res.content
        );
        assert!(res.content.contains("\t\"os\""));
        assert!(res.content.contains("\t\"net/http\""));
        assert!(!res.content.contains("\"os\","));
        assert!(res.content.contains("A:%v"));
        assert!(res.content.contains("B:%v"));
    }

    #[test]
    fn js_use_client_python_docstring_and_rust_deleted_header_are_handled_correctly() {
        let ts_base = "\"use client\";\n\nimport { useState } from \"react\";\n\nexport function A() {\n  return 1;\n}\n";
        let ts_left = "\"use client\";\n\nimport { useState, useEffect } from \"react\";\n\nexport function A() {\n  return 1;\n}\n\nexport function B() {\n  return 2;\n}\n";
        let ts_right = "\"use client\";\n\nimport { useState, useMemo } from \"react\";\n\nexport function A() {\n  return 1;\n}\n\nexport function C() {\n  return 3;\n}\n";
        let ts_res = merge_file_3way("app/Page.tsx", ts_base, ts_left, ts_right, "a", "b");
        assert!(!ts_res.has_conflicts, "got:\n{}", ts_res.content);
        assert!(ts_res.content.starts_with("\"use client\";\n"));

        let py_base = "\"\"\"Module docstring.\"\"\"\n\nimport os\n\nclass Svc:\n    @staticmethod\n    def run():\n        return 1\n";
        let py_left = "\"\"\"Module docstring.\"\"\"\n\nimport os\nimport sys\n\nclass Svc:\n    @staticmethod\n    def run():\n        return 1\n\n    @classmethod\n    def build(cls):\n        return cls()\n";
        let py_right = "\"\"\"Module docstring.\"\"\"\n\nimport os\nimport json\n\nclass Svc:\n    @staticmethod\n    def run():\n        return 1\n\n    @property\n    def ready(self):\n        return True\n";
        let py_res = merge_file_3way("svc.py", py_base, py_left, py_right, "a", "b");
        assert!(!py_res.has_conflicts, "got:\n{}", py_res.content);
        assert!(
            py_res
                .content
                .starts_with("\"\"\"Module docstring.\"\"\"\n")
        );
        assert!(py_res.content.contains("@classmethod"));
        assert!(py_res.content.contains("@property"));

        let rs_base = "//! Crate docs.\n#![allow(dead_code)]\n\npub fn a() {}\n";
        let rs_left = "//! Crate docs.\n\npub fn a() {}\npub fn b() {}\n";
        let rs_right = "//! Crate docs.\n#![allow(dead_code)]\n\npub fn a() {}\npub fn c() {}\n";
        let rs_res = merge_file_3way("src/lib.rs", rs_base, rs_left, rs_right, "a", "b");
        assert!(!rs_res.has_conflicts, "got:\n{}", rs_res.content);
        assert!(
            !rs_res.content.contains("#![allow(dead_code)]"),
            "deleted inner attribute must not be resurrected:\n{}",
            rs_res.content
        );
    }

    #[test]
    fn rust_cfg_items_impls_and_nested_use_trees_and_same_name_add_add_behave_safely() {
        let base_cfg = "#[cfg(unix)]\npub fn open() -> i32 { 1 }\n\n#[cfg(windows)]\npub fn open() -> i32 { 2 }\n";
        let left_cfg = "#[cfg(unix)]\npub fn open() -> i32 { 10 }\n\n#[cfg(windows)]\npub fn open() -> i32 { 2 }\n";
        let right_cfg = "#[cfg(unix)]\npub fn open() -> i32 { 1 }\n\n#[cfg(windows)]\npub fn open() -> i32 { 20 }\n";
        let res_cfg = merge_file_3way("src/fs.rs", base_cfg, left_cfg, right_cfg, "a", "b");
        assert!(!res_cfg.has_conflicts, "got:\n{}", res_cfg.content);
        assert!(res_cfg.content.contains("10"));
        assert!(res_cfg.content.contains("20"));

        let base_use = "use std::{collections::{BTreeMap}, io};\n\npub fn f() {}\n";
        let left_use = "use std::{collections::{BTreeMap, HashMap}, io};\n\npub fn f() {}\n";
        let right_use = "use std::{collections::{BTreeMap, BTreeSet}, io};\n\npub fn f() {}\n";
        let res_use = merge_file_3way("src/u.rs", base_use, left_use, right_use, "a", "b");
        assert!(
            res_use.has_conflicts,
            "conflicting nested use trees must propagate conflict instead of duplicating collections::{{...}}:\n{}",
            res_use.content
        );

        let base_add = "pub fn existing() {}\n";
        let left_add = "pub fn existing() {}\n\npub fn helper(a: i32) -> i32 { a }\n";
        let right_add = "pub fn existing() {}\n\npub fn helper(b: u64) -> u64 { b }\n";
        let res_add = merge_file_3way("src/dup.rs", base_add, left_add, right_add, "a", "b");
        assert!(
            res_add.has_conflicts,
            "same-name add/add functions must conflict:\n{}",
            res_add.content
        );

        let base_cfg_use = "#[cfg(unix)]\nuse std::os::unix::fs;\n\npub fn f() {}\n";
        let left_cfg_use = "#[cfg(unix)]\nuse std::os::unix::fs;\n\npub fn f() {}\npub fn g() {}\n";
        let right_cfg_use = "#[cfg(unix)]\nuse std::os::unix::fs;\n#[cfg(windows)]\nuse std::os::windows::fs;\n\npub fn f() {}\n";
        let res_cfg_use = merge_file_3way(
            "src/u2.rs",
            base_cfg_use,
            left_cfg_use,
            right_cfg_use,
            "a",
            "b",
        );
        assert!(!res_cfg_use.has_conflicts, "got:\n{}", res_cfg_use.content);
        assert!(
            res_cfg_use
                .content
                .contains("#[cfg(unix)]\nuse std::os::unix::fs;")
        );
        assert!(
            res_cfg_use
                .content
                .contains("#[cfg(windows)]\nuse std::os::windows::fs;")
        );

        let base_multi_impl = "pub struct S;\n\nimpl S {\n    pub fn a() -> i32 { 1 }\n}\n\nimpl S {\n    pub fn b() -> i32 { 2 }\n}\n";
        let left_multi_impl = "pub struct S;\n\nimpl S {\n    pub fn a() -> i32 { 10 }\n}\n\nimpl S {\n    pub fn b() -> i32 { 2 }\n}\n";
        let right_multi_impl = "pub struct S;\n\nimpl S {\n    pub fn a() -> i32 { 1 }\n}\n\nimpl S {\n    pub fn b() -> i32 { 20 }\n}\n";
        let res_multi_impl = merge_file_3way(
            "src/m.rs",
            base_multi_impl,
            left_multi_impl,
            right_multi_impl,
            "a",
            "b",
        );
        assert!(
            !res_multi_impl.has_conflicts,
            "got:\n{}",
            res_multi_impl.content
        );
        assert!(res_multi_impl.content.contains("pub fn a() -> i32 { 10 }"));
        assert!(res_multi_impl.content.contains("pub fn b() -> i32 { 20 }"));

        let base_ovl = "export function parse(x: string): number;\nexport function parse(x: number): string;\nexport function parse(x: string | number): number | string {\n  return typeof x === \"string\" ? Number(x) : String(x);\n}\n";
        let left_ovl = "export function parse(x: string): number;\nexport function parse(x: number): string;\nexport function parse(x: boolean): number;\nexport function parse(x: string | number | boolean): number | string {\n  return typeof x === \"string\" ? Number(x) : String(x);\n}\n";
        let right_ovl = "export function parse(x: string): number;\nexport function parse(x: number): string;\nexport function parse(x: string | number): number | string {\n  return typeof x === \"string\" ? Number(x) : String(x);\n}\n\nexport function helper() {\n  return true;\n}\n";
        let res_ovl = merge_file_3way("src/ovl.ts", base_ovl, left_ovl, right_ovl, "a", "b");
        assert!(!res_ovl.has_conflicts, "got:\n{}", res_ovl.content);
        assert!(
            res_ovl
                .content
                .contains("export function parse(x: string): number;")
        );
        assert!(
            res_ovl
                .content
                .contains("export function parse(x: number): string;")
        );
        assert!(
            res_ovl
                .content
                .contains("export function parse(x: boolean): number;")
        );
        assert!(res_ovl.content.contains("export function helper()"));
    }

    #[test]
    fn nested_symbol_extraction_detects_method_level_edits_without_whole_class_collisions() {
        let base = "pub struct Svc;\n\nimpl Svc {\n    pub fn a(&self) -> i32 { 1 }\n    pub fn b(&self) -> i32 { 2 }\n}\n";
        let left = "pub struct Svc;\n\nimpl Svc {\n    pub fn a(&self) -> i32 { 10 }\n    pub fn b(&self) -> i32 { 2 }\n}\n";
        let right = "pub struct Svc;\n\nimpl Svc {\n    pub fn a(&self) -> i32 { 1 }\n    pub fn b(&self) -> i32 { 20 }\n}\n";

        let left_syms: BTreeSet<String> = diff_modified_symbols("src/svc.rs", base, left)
            .into_iter()
            .map(|s| s.label)
            .collect();
        let right_syms: BTreeSet<String> = diff_modified_symbols("src/svc.rs", base, right)
            .into_iter()
            .map(|s| s.label)
            .collect();

        assert!(left_syms.contains("impl Svc::a"));
        assert!(right_syms.contains("impl Svc::b"));
        assert!(
            left_syms.is_disjoint(&right_syms),
            "disjoint method edits inside same impl must not collide: left={left_syms:?}, right={right_syms:?}"
        );
        assert!(!extract_ast_symbols("mod.mts", "export class Svc { handle() {} }").is_empty());
        assert!(
            !extract_ast_symbols(
                "stub.pyi",
                "class Svc:\n    def handle(self) -> None: ...\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn multiline_raw_string_and_container_indent_and_param_tiers_are_preserved() {
        let base_raw =
            "impl Sql {\n    pub fn q() -> &'static str {\n        r#\"\nSELECT 1\n\"#\n    }\n}\n";
        let left_raw = "impl Sql {\n    pub fn q() -> &'static str {\n        r#\"\nSELECT 1\n\"#\n    }\n\n    pub fn a() -> i32 { 1 }\n}\n";
        let right_raw = "impl Sql {\n    pub fn q() -> &'static str {\n        r#\"\nSELECT 1\n\"#\n    }\n\n    pub fn b() -> i32 { 2 }\n}\n";
        let res_raw = merge_file_3way("src/sql.rs", base_raw, left_raw, right_raw, "a", "b");
        assert!(!res_raw.has_conflicts, "got:\n{}", res_raw.content);
        assert!(
            res_raw.content.contains("\nSELECT 1\n\"#"),
            "column-0 raw string line must not be indented:\n{}",
            res_raw.content
        );

        let base_rest = "export function log(a: string, ...rest: string[]) {\n  return a;\n}\n";
        let left_rest =
            "export function log(a: string, b: number, ...rest: string[]) {\n  return a + b;\n}\n";
        let right_rest =
            "export function log(a: string, c: boolean, ...rest: string[]) {\n  return a;\n}\n";
        let res_rest = merge_file_3way("src/log.ts", base_rest, left_rest, right_rest, "a", "b");
        assert!(!res_rest.has_conflicts, "got:\n{}", res_rest.content);
        assert!(
            res_rest
                .content
                .contains("log(a: string, b: number, c: boolean, ...rest: string[])"),
            "...rest must remain last:\n{}",
            res_rest.content
        );

        let base_2sp = "export class Api {\n  get() {\n    return 1;\n  }\n}\n";
        let left_2sp = "export class Api {\n  get() {\n    return 10;\n  }\n}\n";
        let right_2sp = "export class Api {\n  get() {\n    return 20;\n  }\n}\n";
        let union_2sp = merge_ast_union("src/api.ts", base_2sp, left_2sp, right_2sp);
        assert!(!union_2sp.contains("<<<<<<<"));
        assert!(union_2sp.contains("  get()"));
        assert!(union_2sp.contains("return 10;"));
        assert!(union_2sp.contains("return 20;"));
    }

    #[test]
    fn typescript_concurrent_sibling_functions_and_imports_merge_cleanly() {
        let base =
            "import { db } from './db';\n\nexport function healthCheck() {\n  return true;\n}\n";
        let left = "import { db } from './db';\nimport { verifyJwt } from './jwt';\n\nexport function healthCheck() {\n  return true;\n}\n\nexport function loginUser(token: string) {\n  return verifyJwt(token);\n}\n";
        let right = "import { db } from './db';\nimport { stripe } from './stripe';\n\nexport function healthCheck() {\n  return true;\n}\n\nexport function createInvoice(total: number) {\n  return total;\n}\n";

        let res = merge_file_3way(
            "packages/api/routes.ts",
            base,
            left,
            right,
            "claude",
            "antigravity",
        );
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

        let py_res = merge_file_3way(
            "app/handlers.py",
            py_base,
            py_left,
            py_right,
            "agent-a",
            "agent-b",
        );
        assert!(
            !py_res.has_conflicts,
            "expected clean Python AST merge, got:\n{}",
            py_res.content
        );
        assert!(py_res.content.contains("import json"));
        assert!(py_res.content.contains("import hashlib"));
        assert!(py_res.content.contains("def parse_payload"));
        assert!(py_res.content.contains("def hash_token"));

        let go_base = "package main\n\nfunc Health() string {\n\treturn \"ok\"\n}\n";
        let go_left = "package main\n\nfunc Health() string {\n\treturn \"ok\"\n}\n\nfunc VerifyToken(tok string) bool {\n\treturn len(tok) > 0\n}\n";
        let go_right = "package main\n\nfunc Health() string {\n\treturn \"ok\"\n}\n\nfunc ChargeInvoice(cents int) int {\n\treturn cents\n}\n";

        let go_res = merge_file_3way(
            "cmd/server/main.go",
            go_base,
            go_left,
            go_right,
            "agent-a",
            "agent-b",
        );
        assert!(
            !go_res.has_conflicts,
            "expected clean Go AST merge, got:\n{}",
            go_res.content
        );
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

        let res = merge_file_3way(
            "src/app.ts",
            base,
            left,
            right,
            "agent-locale",
            "agent-decimals",
        );
        assert!(
            !res.has_conflicts,
            "expected clean AST parameter + body merge, got:\n{}",
            res.content
        );
        assert!(res.content.contains(
            "export function formatCurrency(amount: number, currency: string, locale: string = \"en-US\", decimals: number = 2): string"
        ));
        assert!(res.content.contains("new Intl.NumberFormat(locale, {"));
        assert!(
            res.content
                .contains("return `${currency} ${amount.toFixed(decimals)}`;")
        );
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
        assert!(
            ts_res
                .content
                .contains("import { readFile, writeFile, stat } from \"./fs\";")
        );
        assert!(
            ts_res
                .content
                .contains("export const ROLES = [\"admin\", \"user\", \"editor\", \"auditor\"];")
        );
        assert!(ts_res.content.contains(
            "export const CONFIG = { host: \"localhost\", port: 9000, tls: true, timeout: 5000 };"
        ));

        let php_base = "<?php\nfunction sendMail($to, $subject) {\n    $headers = ['From' => 'noreply@example.com'];\n    return mail($to, $subject);\n}\n";
        let php_left = "<?php\nfunction sendMail($to, $subject, $cc = null) {\n    $headers = ['From' => 'noreply@example.com', 'Cc' => $cc];\n    return mail($to, $subject);\n}\n";
        let php_right = "<?php\nfunction sendMail($to, $subject, $bcc = null) {\n    $headers = ['From' => 'noreply@example.com', 'Bcc' => $bcc];\n    return mail($to, $subject);\n}\n";
        let php_res = merge_file_3way("app/Mail.php", php_base, php_left, php_right, "a", "b");
        assert!(!php_res.has_conflicts, "got:\n{}", php_res.content);
        assert!(
            php_res
                .content
                .contains("function sendMail($to, $subject, $cc = null, $bcc = null)")
        );
        assert!(
            php_res
                .content
                .contains("'From' => 'noreply@example.com', 'Cc' => $cc, 'Bcc' => $bcc")
        );

        let md_base = "# Architecture\n\nTessera merges code across agents in a single working tree.\n\n## Features\n- Fast snapshots\n- Lock-free oplog\n";
        let md_left = "# Architecture\n\nTessera merges code and prose across agents in a single working tree.\n\n## Features\n- Fast snapshots\n- Lock-free oplog\n- Sub-line token merging\n";
        let md_right = "# Architecture\n\nTessera merges code across agents in a single shared working tree.\n\n## Features\n- Fast snapshots\n- Lock-free oplog\n- Delimited collection merging\n";
        let md_res = merge_file_3way("README.md", md_base, md_left, md_right, "a", "b");
        assert!(!md_res.has_conflicts, "got:\n{}", md_res.content);
        assert!(md_res.content.contains(
            "Tessera merges code and prose across agents in a single shared working tree."
        ));
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

    #[test]
    fn vue_sfc_reports_before_script_conflicts_and_parses_tsx_scripts() {
        let base = "<template>\n  <h1>Title</h1>\n</template>\n\n<script lang=\"tsx\">\nexport function Badge() {\n  return <span>ok</span>;\n}\n</script>\n";
        let left = "<template>\n  <h1>Left Title</h1>\n</template>\n\n<script lang=\"tsx\">\nexport function Badge() {\n  return <span>ok</span>;\n}\n\nexport function LeftView() {\n  return <div>left</div>;\n}\n</script>\n";
        let right = "<template>\n  <h1>Right Title</h1>\n</template>\n\n<script lang=\"tsx\">\nexport function Badge() {\n  return <span>ok</span>;\n}\n\nexport function RightView() {\n  return <div>right</div>;\n}\n</script>\n";

        let res = merge_file_3way("src/Header.vue", base, left, right, "a", "b");
        assert!(res.has_conflicts);
        assert!(
            res.conflicted_symbols
                .contains(&"<template>/<before-script>".to_string())
        );
        assert!(res.left_conflict_snippet.contains("Left Title"));
        assert!(res.right_conflict_snippet.contains("Right Title"));
        assert!(res.content.contains("export function LeftView()"));
        assert!(res.content.contains("export function RightView()"));
    }
}
