//! Backwards-compatible public facade re-exporting Zero-Command Harness Auto-Pilot, session
//! detection, harness configuration installer, pre/post-tool lifecycle hooks, and background
//! micro-watcher daemon APIs.

pub use crate::harness::{install_harness_integrations, install_harness_integrations_for};
pub use crate::hooks::{
    PostToolOutcome, PreToolOutcome, handle_post_tool_hook, handle_pre_tool_hook,
};
pub use crate::session::{
    FocusSessionReport, detect_session_id, detect_session_waypoint, focus_session_waypoint,
};
pub use crate::watcher::{ensure_background_watcher, run_auto_watch_daemon};
