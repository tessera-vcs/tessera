//! Tree-sitter AST 3-way semantic merge engine, manifest/lockfile merge drivers,
//! sequential migration resequencer, and cross-directory rename tracker for Tessera.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

pub mod ast_merge;
pub mod drivers;
pub mod rename;
pub mod text_merge;
pub mod tree_merge;

pub use ast_merge::{
    diff_modified_symbols, extract_ast_symbols, merge_ast_union, merge_file_3way, AstLanguage,
    AstMergeOutcome, AstSymbol,
};
pub use drivers::{
    merge_cargo_lock_3way, merge_json_3way, merge_toml_3way, try_merge_with_driver,
    DriverMergeOutcome, MigrationResequence,
};
pub use rename::{content_similarity_bps, detect_renames, RenameMatch};
pub use text_merge::{attribute_agent_hunks, merge_text_3way, unified_diff, TextMergeOutcome};
pub use tree_merge::{merge_trees_3way, TreeMergeOutcome};
