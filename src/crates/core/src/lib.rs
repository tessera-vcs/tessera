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
    is_valid_sandbox_repo_root, CommitGraphNode, GitBackend, GitCommit, GitTreeNode,
};
pub use model::{
    atomic_write_file, atomic_write_file_relaxed, now_millis, resolve_confined_repo_path,
    validate_hex_id, validate_ref_name, validate_repo_rel_path, validate_safe_identifier,
    write_working_tree_file_safe, AgentMetadata, ChangeId, ConflictId, ConflictKind,
    ConflictRecord, CoreError, CoreResult, EntryMode, FileEntry, Merge, ObjectId, OpId, RepoConfig,
    SessionWaypoint, StashEntry,
};
pub use oplog::{OpLogStore, Operation, View};
