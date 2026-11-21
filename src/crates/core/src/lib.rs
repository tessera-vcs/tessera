//! Core storage, native Git ODB bridge, lock-free Operation Log, and algebraic conflict models for Tessera.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

extern crate core as rust_core;
#[doc(hidden)]
pub use rust_core::*;

pub mod git_backend;
pub mod model;
pub mod oplog;

pub use git_backend::{
    CommitGraphNode, GitBackend, GitCommit, GitTreeNode, is_valid_sandbox_repo_root,
    resolve_dot_git,
};
pub use model::{
    AgentMetadata, ChangeId, ConflictId, ConflictKind, ConflictRecord, CoreError, CoreResult,
    EntryMode, FileEntry, FileEntryKind, Merge, ObjectId, OpId, RepoConfig, SessionWaypoint,
    StashEntry, atomic_write_file, atomic_write_file_relaxed, now_millis,
    resolve_confined_repo_path, validate_hex_id, validate_ref_name, validate_repo_rel_path,
    validate_safe_identifier, write_working_tree_file_safe,
};
pub use oplog::{OpLogStore, Operation, View};
