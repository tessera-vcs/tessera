//! Copy-on-Write agent sandboxes, build cache path hygiene, deterministic runtime environment
//! slot isolation, and parallel stat-cached working-copy snapshotting for Tessera.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

pub mod cow;
pub mod env;
pub mod snapshot;

pub use cow::{
    clone_path_cow, create_cow_workspace, materialize_tree_to_dir, sanitize_build_cache,
    sync_missing_cow_caches, CowStrategy,
};
pub use env::{
    build_agent_env, build_agent_env_with_config, ensure_polyglot_port_shims,
    is_forbidden_custom_env_key, polyglot_port_env_vars, POLYGLOT_PORT_ENV_KEYS,
};
pub use snapshot::{
    is_ignored_or_secret_path, snapshot_working_dir, CachedStat, SnapshotOutcome,
};
