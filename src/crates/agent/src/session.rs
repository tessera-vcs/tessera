//! Session detection, process-tree inspection, navigation waypoints (`focus_session_waypoint`),
//! repository initialization, session lifecycle (`ensure_session`, `spawn_cow_agent`,
//! `snapshot_agent`, `record_single_path_edit`, `claim_*`, `radar`, `watch_tick`, `focus_session`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use core::{
    AgentMetadata, ChangeId, ConflictRecord, CoreError, CoreResult, EntryMode, FileEntry,
    GitBackend, ObjectId, OpId, OpLogStore, SessionWaypoint, View, is_valid_sandbox_repo_root,
    now_millis, validate_ref_name, validate_repo_rel_path, validate_safe_identifier,
};
use merge::{attribute_agent_hunks, diff_modified_symbols, merge_ast_union};
use serde::{Deserialize, Serialize};
use workspace::{build_agent_env_with_config, create_cow_workspace, snapshot_working_dir};

use crate::harness::{
    KNOWN_HARNESS_PATHS, add_paths_to_git_exclude, install_harness_integrations_for,
};
use crate::hooks::{PostToolOutcome, handle_post_tool_hook, handle_pre_tool_hook};
use crate::worktree::{
    acquire_file_lock, is_ai_session_id, is_binary_bytes, is_harness_config_path,
    session_claims_path, session_may_sweep, write_worktree_file,
};
use crate::{Coordinator, FileLineAttribution, InitReport, RadarReport, SymbolCollision};

/// Result of focusing an agent session's terminal tab, multiplexer pane, or IDE window via `tes session focus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusSessionReport {
    /// Target agent or session identifier.
    pub session_id: String,
    /// Harness identifier (`"antigravity"`, `"claude-code"`, `"cursor"`, `"terminal"`, etc.).
    pub harness: String,
    /// `true` if a terminal tab, multiplexer pane, or IDE window was actively brought to the foreground.
    pub focused: bool,
    /// Mechanism used to focus the session (`"tmux"`, `"wezterm"`, `"kitty"`, `"apple-terminal"`, `"iterm2"`, `"ide-window"`, or `"waypoint-info"`).
    pub mechanism: String,
    /// Human-readable description of the focus action or navigation hint.
    pub detail: String,
    /// Recorded session waypoint metadata, if available.
    pub waypoint: Option<SessionWaypoint>,
}

/// Safely truncates `raw` on Unicode scalar boundaries (never slicing mid-UTF-8 code point),
/// appending a deterministic short Blake3 hash suffix whenever truncation occurs so UUIDv7 or
/// timestamp-prefixed tokens sharing a leading prefix never collide.
pub(crate) fn truncate_session_token(raw: &str, max_chars: usize) -> String {
    let char_count = raw.chars().count();

    if char_count <= max_chars {
        return raw.to_string();
    }

    let hash = blake3::hash(raw.as_bytes()).to_hex();
    let keep = max_chars.saturating_sub(7).max(1);
    let prefix: String = raw.chars().take(keep).collect();
    format!("{prefix}-{}", &hash[..6])
}

/// Normalizes a raw session identifier into lowercase ASCII alphanumeric, `-`, and `_` characters,
/// ensuring the result never starts with `-` (which would be rejected by identifier validation).
fn sanitize_id(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    if sanitized.is_empty() || sanitized.starts_with('-') {
        format!("s{sanitized}")
    } else {
        sanitized
    }
}

/// Percent-encodes `raw` for safe inclusion in a URI query parameter (RFC 3986 unreserved set).
pub(crate) fn percent_encode_uri_component(raw: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(raw.len());

    for &b in raw.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }

    out
}

/// Automatically detects the current AI agent session identifier without requiring manual
/// `tes agent spawn` invocations.
///
/// Checks harness environment variables in priority order (`TES_SESSION_ID`,
/// `ANTIGRAVITY_CONVERSATION_ID`, `CLAUDE_SESSION_ID`, `CURSOR_SESSION_ID`,
/// `WINDSURF_SESSION_ID`, `KIRO_SESSION_ID`, `OPENCODE_SESSION_ID`, `CODEX_THREAD_ID`,
/// `CODEX_SESSION_ID`, `CURSOR_TRACE_ID`, `TERM_SESSION_ID`) and falls back to the ancestor
/// harness or parent process ID (`getppid`).
pub fn detect_session_id(explicit: Option<&str>) -> String {
    detect_session_id_with_env(explicit, |key| std::env::var(key).ok())
}

/// Pure, testable core of [`detect_session_id`] parameterized by an environment lookup function.
pub(crate) fn detect_session_id_with_env<F>(explicit: Option<&str>, get_env: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(exp) = explicit {
        let trimmed = exp.trim();

        if !trimmed.is_empty() {
            return sanitize_id(trimmed);
        }
    }

    if get_env("TES_FROM_IDE_EXTENSION").is_some() {
        return "editor".to_string();
    }

    // SAFETY: `libc::getppid()` is a pure POSIX syscall with no arguments or memory dereferences.
    #[cfg(unix)]
    let ppid = unsafe { libc::getppid() };
    #[cfg(not(unix))]
    let ppid = i32::try_from(std::process::id()).unwrap_or(1);

    let has_harness_env = [
        "ANTIGRAVITY_AGENT",
        "ANTIGRAVITY_CONVERSATION_ID",
        "CLAUDECODE",
        "CLAUDE_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_SESSION_ID",
        "OPENAI_CODEX_SESSION_ID",
        "CODEX_SANDBOX",
        "CODEX_CI",
        "OPENCODE_SESSION_ID",
        "CURSOR_SESSION_ID",
        "CURSOR_TRACE_ID",
        "WINDSURF_SESSION_ID",
        "CODEIUM_SESSION_ID",
        "KIRO_SESSION_ID",
        "COPILOT_SESSION_ID",
        "GEMINI_CLI_SESSION_ID",
        "GEMINI_SESSION_ID",
        "AIDER_SESSION_ID",
        "AMP_SESSION_ID",
        "GOOSE_SESSION_ID",
        "AUGMENT_SESSION_ID",
    ]
    .iter()
    .any(|k| get_env(k).is_some());

    if let Some(val) = get_env("TES_SESSION_ID") {
        let trimmed = val.trim();

        if !trimmed.is_empty() && !trimmed.starts_with("term-") {
            let short = truncate_session_token(trimmed, 32);
            return sanitize_id(&short);
        }

        if !trimmed.is_empty() && !has_harness_env && trimmed == format!("term-{ppid}") {
            let short = truncate_session_token(trimmed, 32);
            return sanitize_id(&short);
        }
    }

    // Prefer stable per-session IDs (e.g. `CURSOR_SESSION_ID`) over per-request IDs (`CURSOR_TRACE_ID`).
    for (env_key, prefix) in [
        ("ANTIGRAVITY_CONVERSATION_ID", "antigravity-"),
        ("CLAUDE_SESSION_ID", "claude-"),
        ("CURSOR_SESSION_ID", "cursor-"),
        ("WINDSURF_SESSION_ID", "windsurf-"),
        ("CODEIUM_SESSION_ID", "windsurf-"),
        ("KIRO_SESSION_ID", "kiro-"),
        ("OPENCODE_SESSION_ID", "opencode-"),
        ("CODEX_THREAD_ID", "codex-"),
        ("CODEX_SESSION_ID", "codex-"),
        ("OPENAI_CODEX_SESSION_ID", "codex-"),
        ("COPILOT_SESSION_ID", "copilot-"),
        ("GEMINI_CLI_SESSION_ID", "gemini-"),
        ("GEMINI_SESSION_ID", "gemini-"),
        ("AIDER_SESSION_ID", "aider-"),
        ("AMP_SESSION_ID", "amp-"),
        ("GOOSE_SESSION_ID", "goose-"),
        ("AUGMENT_SESSION_ID", "augment-"),
        ("CURSOR_TRACE_ID", "cursor-"),
    ] {
        if let Some(val) = get_env(env_key) {
            let trimmed = val.trim();

            if !trimmed.is_empty() {
                let short = truncate_session_token(trimmed, 12);
                return sanitize_id(&format!("{prefix}{short}"));
            }
        }
    }

    let ancestor_harness = detect_ancestor_harness_process(ppid);

    if let Some(val) = get_env("TES_SESSION_ID") {
        let trimmed = val.trim();

        if !trimmed.is_empty() && !has_harness_env && ancestor_harness.is_none() {
            let short = truncate_session_token(trimmed, 32);
            return sanitize_id(&short);
        }
    }

    if let Some((harness_prefix, harness_pid)) = ancestor_harness {
        return format!("{harness_prefix}-{harness_pid}");
    }

    let stable_pid = find_stable_session_ancestor_pid(ppid);

    if get_env("CODEX_SANDBOX").is_some() || get_env("CODEX_CI").is_some() {
        return format!("codex-{stable_pid}");
    }

    if get_env("CLAUDECODE").is_some() {
        return format!("claude-{stable_pid}");
    }

    if get_env("ANTIGRAVITY_AGENT").is_some() {
        return format!("antigravity-{stable_pid}");
    }

    for (env_key, prefix) in [
        ("TES_SESSION_ID", ""),
        ("TERM_SESSION_ID", "term-"),
        ("ITERM_SESSION_ID", "term-iterm-"),
        ("TMUX_PANE", "term-tmux-"),
        ("ZELLIJ_PANE_ID", "term-zellij-"),
        ("WEZTERM_PANE", "term-wezterm-"),
        ("KITTY_WINDOW_ID", "term-kitty-"),
        ("GHOSTTY_SURFACE_ID", "term-ghostty-"),
        ("ALACRITTY_WINDOW_ID", "term-alacritty-"),
        ("KONSOLE_DBUS_SESSION", "term-konsole-"),
        ("GNOME_TERMINAL_SCREEN", "term-gnome-"),
        ("TILIX_ID", "term-tilix-"),
        ("WT_SESSION", "term-wt-"),
    ] {
        if let Some(val) = get_env(env_key) {
            let trimmed = val.trim();

            if !trimmed.is_empty() {
                let effective = if env_key == "TERM_SESSION_ID" || env_key == "ITERM_SESSION_ID" {
                    trimmed.rsplit(':').next().unwrap_or(trimmed)
                } else if env_key == "GNOME_TERMINAL_SCREEN" || env_key == "KONSOLE_DBUS_SESSION" {
                    trimmed.rsplit('/').next().unwrap_or(trimmed)
                } else {
                    trimmed
                };
                let max_len = if env_key == "TES_SESSION_ID" { 32 } else { 12 };
                let short = truncate_session_token(effective, max_len);
                return sanitize_id(&format!("{prefix}{short}"));
            }
        }
    }

    format!("session-{stable_pid}")
}

/// Maps a normalized binary or script basename to its canonical CLI agent harness identifier.
fn classify_cli_harness_token(bin_name: &str) -> Option<&'static str> {
    if bin_name == "codex" || bin_name.starts_with("codex-") {
        return Some("codex");
    }

    match bin_name {
        "claude" | "claude-code" => Some("claude"),
        "opencode" => Some("opencode"),
        "kiro-cli" => Some("kiro"),
        "cursor-agent" => Some("cursor"),
        "aider" | "aider-chat" => Some("aider"),
        "gemini" | "gemini-cli" => Some("gemini"),
        "copilot" => Some("copilot"),
        "amp" => Some("amp"),
        "goose" => Some("goose"),
        "crush" => Some("crush"),
        "auggie" | "augment" => Some("augment"),
        "droid" => Some("droid"),
        "cline" => Some("cline"),
        "openhands" => Some("openhands"),
        "plandex" => Some("plandex"),
        _ => None,
    }
}

/// Queries the full command line arguments for `pid` when inspecting script runtimes (`node`, `python`, `bun`).
fn query_process_args(pid: i32) -> Option<String> {
    if pid <= 1 {
        return None;
    }

    #[cfg(unix)]
    {
        if let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) {
            if !raw.is_empty() {
                let joined: String = raw
                    .split(|&b| b == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ");

                if !joined.trim().is_empty() {
                    return Some(joined);
                }
            }
        }

        if let Ok(out) = Command::new("ps")
            .args(["-o", "args=", "-p", &pid.to_string()])
            .output()
        {
            if out.status.success() {
                let line = String::from_utf8_lossy(&out.stdout).trim().to_string();

                if !line.is_empty() {
                    return Some(line);
                }
            }
        }
    }

    None
}

/// Walks up the process tree from `start_pid` to check if the current command was spawned by a
/// CLI coding agent (`codex`, `claude`, `opencode`, `kiro-cli`, `aider`, `gemini`,
/// `cursor-agent`, `amp`, `goose`, `crush`, `auggie`, etc.), returning `(harness_name, pid)`.
fn detect_ancestor_harness_process(start_pid: i32) -> Option<(&'static str, i32)> {
    let mut curr = start_pid;

    for _ in 0..6 {
        if curr <= 1 {
            break;
        }

        let Some((next_ppid, comm)) = query_process_parent_and_name(curr) else {
            break;
        };

        if comm.contains("/target/") {
            break;
        }

        let bin_name = comm
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&comm)
            .trim_start_matches('-')
            .trim_end_matches(".exe")
            .trim_end_matches(".EXE")
            .to_ascii_lowercase();

        if bin_name == "cargo" {
            break;
        }

        if let Some(harness) = classify_cli_harness_token(&bin_name) {
            return Some((harness, curr));
        }

        if matches!(bin_name.as_str(), "node" | "nodejs" | "bun" | "deno")
            || bin_name.starts_with("python")
        {
            if let Some(args_line) = query_process_args(curr) {
                for tok in args_line.split_whitespace().skip(1) {
                    if tok.starts_with('-') {
                        continue;
                    }

                    let script_base = tok
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(tok)
                        .trim_end_matches(".js")
                        .trim_end_matches(".mjs")
                        .trim_end_matches(".cjs")
                        .trim_end_matches(".py")
                        .to_ascii_lowercase();

                    if let Some(harness) = classify_cli_harness_token(&script_base) {
                        return Some((harness, curr));
                    }
                }
            }
        }

        if next_ppid <= 1 || next_ppid == curr {
            break;
        }

        curr = next_ppid;
    }

    None
}

/// Returns `true` if `comm` is an interactive shell, transient tool subshell, or CLI agent runtime.
pub(crate) fn is_shell_or_cli_harness_process(comm: &str) -> bool {
    if comm.contains(".app/") {
        return false;
    }

    let bin_name = comm
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(comm)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_ascii_lowercase();

    matches!(
        bin_name.as_str(),
        "sh" | "zsh"
            | "bash"
            | "dash"
            | "ash"
            | "fish"
            | "ksh"
            | "ksh93"
            | "pdksh"
            | "mksh"
            | "oksh"
            | "yash"
            | "tcsh"
            | "csh"
            | "nu"
            | "elvish"
            | "xonsh"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "env"
            | "tes"
            | "agy"
            | "antigravity-cli"
            | "claude"
            | "codex"
            | "opencode"
            | "aider"
            | "gemini"
            | "cursor-agent"
            | "kiro-cli"
            | "copilot"
            | "amp"
            | "goose"
            | "crush"
            | "node"
            | "nodejs"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "bun"
            | "deno"
            | "uv"
            | "uvx"
            | "pipx"
    ) || bin_name.starts_with("codex-")
        || bin_name.starts_with("python")
}

/// Walks up the process tree from `start_pid` across transient tool subshells (`sh`, `bash`, `zsh`,
/// etc.) and CLI agent processes (`agy`, `claude`, `codex`, `opencode`, `node`, etc.) to find the
/// persistent interactive terminal shell or host daemon process ID for the session.
fn find_stable_session_ancestor_pid(start_pid: i32) -> i32 {
    let mut curr = start_pid;

    for _ in 0..10 {
        if curr <= 1 {
            break;
        }

        let Some((next_ppid, comm)) = query_process_parent_and_name(curr) else {
            break;
        };

        if next_ppid <= 1 || next_ppid == curr || !is_shell_or_cli_harness_process(&comm) {
            break;
        }

        let Some((_, parent_comm)) = query_process_parent_and_name(next_ppid) else {
            break;
        };

        let parent_lower = parent_comm.to_ascii_lowercase();

        if parent_lower.contains("language_server") || parent_lower.contains("extensionhost") {
            curr = next_ppid;
            break;
        }

        if is_shell_or_cli_harness_process(&parent_comm) && query_process_tty(next_ppid).is_some() {
            curr = next_ppid;
            continue;
        }

        if curr == start_pid && query_process_tty(curr).is_none() {
            curr = next_ppid;
        }

        break;
    }

    curr
}

/// Queries `(parent_pid, command_name)` for `pid` using `/proc/<pid>/stat` when available,
/// or portable `ps` invocations on macOS and BSDs.
fn query_process_parent_and_name(pid: i32) -> Option<(i32, String)> {
    #[cfg(unix)]
    {
        if let Ok(stat_line) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let (Some(open_paren), Some(close_paren)) =
                (stat_line.find('('), stat_line.rfind(')'))
            {
                if close_paren > open_paren {
                    let comm = stat_line[open_paren + 1..close_paren].trim().to_string();
                    let after = stat_line[close_paren + 1..].trim();
                    let mut fields = after.split_whitespace();
                    let _state = fields.next()?;

                    if let Some(ppid_str) = fields.next() {
                        if let Ok(ppid) = ppid_str.parse::<i32>() {
                            return Some((ppid, comm));
                        }
                    }
                }
            }
        }

        for format_arg in ["ppid=,comm=", "ppid=,ucomm="] {
            let Ok(out) = Command::new("ps")
                .args(["-o", format_arg, "-p", &pid.to_string()])
                .output()
            else {
                continue;
            };

            if !out.status.success() {
                continue;
            }

            let line = String::from_utf8_lossy(&out.stdout);
            let trimmed = line.trim();
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let Some(ppid_str) = parts.next() else {
                continue;
            };
            let comm = parts.next().unwrap_or("").trim().to_string();

            if let Ok(ppid) = ppid_str.trim().parse::<i32>() {
                if !comm.is_empty() {
                    return Some((ppid, comm));
                }
            }
        }
    }

    #[cfg(not(unix))]
    let _ = pid;

    None
}

/// Captures the navigation waypoint ([`SessionWaypoint`]) for the current agent or terminal process.
///
/// Inspects harness environment variables, checks multiplexer pane variables (`TMUX_PANE`,
/// `ZELLIJ_PANE_ID`) before outer `TERM_PROGRAM`, and walks the process tree for `shell_pid`,
/// controlling `tty`, and ancestor `ide_pid`.
pub fn detect_session_waypoint(session_id: &str) -> SessionWaypoint {
    detect_session_waypoint_with_env(session_id, |key| std::env::var(key).ok())
}

/// Pure environment-parameterized implementation backing [`detect_session_waypoint`].
pub(crate) fn detect_session_waypoint_with_env<F>(session_id: &str, get_env: F) -> SessionWaypoint
where
    F: Fn(&str) -> Option<String>,
{
    let prefix_harness = if session_id.starts_with("antigravity-") {
        Some("antigravity")
    } else if session_id.starts_with("claude-") {
        Some("claude-code")
    } else if session_id.starts_with("cursor-") {
        Some("cursor")
    } else if session_id.starts_with("windsurf-") {
        Some("windsurf")
    } else if session_id.starts_with("kiro-") {
        Some("kiro")
    } else if session_id.starts_with("opencode-") {
        Some("opencode")
    } else if session_id.starts_with("codex-") {
        Some("codex")
    } else if session_id.starts_with("copilot-") {
        Some("copilot")
    } else if session_id.starts_with("gemini-") {
        Some("gemini")
    } else if session_id.starts_with("aider-") {
        Some("aider")
    } else if session_id.starts_with("amp-") {
        Some("amp")
    } else if session_id.starts_with("goose-") {
        Some("goose")
    } else if session_id.starts_with("crush-") {
        Some("crush")
    } else if session_id.starts_with("augment-") {
        Some("augment")
    } else if session_id.starts_with("droid-") {
        Some("droid")
    } else if session_id.starts_with("cline-") {
        Some("cline")
    } else if session_id.starts_with("openhands-") {
        Some("openhands")
    } else if session_id.starts_with("plandex-") {
        Some("plandex")
    } else {
        None
    };

    let mut harness = prefix_harness.map(ToString::to_string);
    let mut conversation_id = None;

    for (env_key, harness_name) in [
        ("ANTIGRAVITY_CONVERSATION_ID", "antigravity"),
        ("CLAUDE_SESSION_ID", "claude-code"),
        ("CURSOR_SESSION_ID", "cursor"),
        ("WINDSURF_SESSION_ID", "windsurf"),
        ("CODEIUM_SESSION_ID", "windsurf"),
        ("KIRO_SESSION_ID", "kiro"),
        ("OPENCODE_SESSION_ID", "opencode"),
        ("CODEX_THREAD_ID", "codex"),
        ("CODEX_SESSION_ID", "codex"),
        ("OPENAI_CODEX_SESSION_ID", "codex"),
        ("COPILOT_SESSION_ID", "copilot"),
        ("GEMINI_CLI_SESSION_ID", "gemini"),
        ("GEMINI_SESSION_ID", "gemini"),
        ("AIDER_SESSION_ID", "aider"),
        ("AMP_SESSION_ID", "amp"),
        ("GOOSE_SESSION_ID", "goose"),
        ("AUGMENT_SESSION_ID", "augment"),
        ("CURSOR_TRACE_ID", "cursor"),
    ] {
        if prefix_harness.is_some_and(|ph| ph != harness_name) {
            continue;
        }

        if let Some(val) = get_env(env_key) {
            let trimmed = val.trim();

            if !trimmed.is_empty() {
                harness = Some(harness_name.to_string());
                conversation_id = Some(trimmed.to_string());
                break;
            }
        }
    }

    if harness.is_none() {
        if get_env("ANTIGRAVITY_AGENT").is_some() {
            harness = Some("antigravity".to_string());
        } else if get_env("CLAUDECODE").is_some() {
            harness = Some("claude-code".to_string());
        } else if get_env("CODEX_SANDBOX").is_some() || get_env("CODEX_CI").is_some() {
            harness = Some("codex".to_string());
        }
    }

    // Check inner terminal multiplexers (`TMUX_PANE`, `ZELLIJ_PANE_ID`) BEFORE outer `TERM_PROGRAM`
    // so sessions running inside tmux or zellij under iTerm/Ghostty record the multiplexer first.
    let mut term_program = if get_env("TMUX_PANE").is_some_and(|v| !v.trim().is_empty()) {
        Some("tmux".to_string())
    } else if get_env("ZELLIJ_PANE_ID").is_some_and(|v| !v.trim().is_empty()) {
        Some("zellij".to_string())
    } else {
        get_env("TERM_PROGRAM")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    if term_program.is_none() {
        for (env_key, prog_name) in [
            ("WEZTERM_PANE", "WezTerm"),
            ("KITTY_WINDOW_ID", "kitty"),
            ("GHOSTTY_SURFACE_ID", "ghostty"),
            ("ITERM_SESSION_ID", "iTerm.app"),
            ("WT_SESSION", "Windows_Terminal"),
        ] {
            if get_env(env_key).is_some_and(|v| !v.trim().is_empty()) {
                term_program = Some(prog_name.to_string());
                break;
            }
        }
    }

    let mut term_session_id = None;

    for env_key in [
        "TMUX_PANE",
        "ZELLIJ_PANE_ID",
        "WEZTERM_PANE",
        "KITTY_WINDOW_ID",
        "ITERM_SESSION_ID",
        "TERM_SESSION_ID",
        "GHOSTTY_SURFACE_ID",
        "ALACRITTY_WINDOW_ID",
        "WT_SESSION",
    ] {
        if let Some(val) = get_env(env_key) {
            let trimmed = val.trim();

            if !trimmed.is_empty() {
                term_session_id = Some(trimmed.to_string());
                break;
            }
        }
    }

    // SAFETY: `libc::getppid()` is a pure POSIX syscall with no arguments or memory dereferences.
    #[cfg(unix)]
    let ppid = unsafe { libc::getppid() };
    #[cfg(not(unix))]
    let ppid = i32::try_from(std::process::id()).unwrap_or(1);

    let explicit_term_pid = session_id
        .strip_prefix("term-")
        .or_else(|| session_id.strip_prefix("session-"))
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|&pid| pid > 1 && is_pid_or_ancestor(pid, ppid));

    if harness.is_none() && explicit_term_pid.is_none() {
        if let Some((prefix, _pid)) = detect_ancestor_harness_process(ppid) {
            let mapped = if prefix == "claude" {
                "claude-code"
            } else {
                prefix
            };
            harness = Some(mapped.to_string());
        }
    }

    let stable_pid = explicit_term_pid.unwrap_or_else(|| find_stable_session_ancestor_pid(ppid));
    let shell_pid = u32::try_from(stable_pid).ok();
    let tty = query_process_tty(stable_pid);
    let ide_pid = detect_ancestor_ide_pid_with_env(stable_pid, &get_env);

    if ide_pid.is_some() && tty.is_none() {
        term_program = None;
        term_session_id = None;
    }

    SessionWaypoint {
        harness: harness.unwrap_or_else(|| "terminal".to_string()),
        conversation_id,
        term_program,
        term_session_id,
        tty,
        shell_pid,
        ide_pid,
    }
}

/// Returns `true` if `target_pid` equals `start_pid` or is an ancestor process of `start_pid`.
fn is_pid_or_ancestor(target_pid: i32, start_pid: i32) -> bool {
    if target_pid <= 1 || start_pid <= 1 {
        return false;
    }

    if target_pid == start_pid {
        return true;
    }

    let mut curr = start_pid;

    for _ in 0..8 {
        let Some((next_ppid, _)) = query_process_parent_and_name(curr) else {
            break;
        };

        if next_ppid == target_pid {
            return true;
        }

        if next_ppid <= 1 || next_ppid == curr {
            break;
        }

        curr = next_ppid;
    }

    false
}

/// Queries the controlling TTY device path (e.g. `"/dev/ttys003"` or `"/dev/pts/2"`) for `pid`.
fn query_process_tty(pid: i32) -> Option<String> {
    if pid <= 1 {
        return None;
    }

    #[cfg(unix)]
    if let Ok(out) = Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
    {
        if out.status.success() {
            let raw = String::from_utf8_lossy(&out.stdout);
            let trimmed = raw.trim();

            if !trimmed.is_empty() && trimmed != "?" && trimmed != "??" && trimmed != "-" {
                return if trimmed.starts_with("/dev/") {
                    Some(trimmed.to_string())
                } else {
                    Some(format!("/dev/{trimmed}"))
                };
            }
        }
    }

    #[cfg(target_os = "windows")]
    if std::env::var_os("WT_SESSION").is_some() {
        return Some(format!("conpty:{pid}"));
    }

    None
}

/// Returns `true` if `comm` identifies an IDE host process (Electron/VS Code/Cursor/Windsurf/Antigravity/Zed).
pub(crate) fn is_ide_process_name(comm: &str) -> bool {
    let lower = comm.to_ascii_lowercase();

    if lower.contains(".app/") {
        return lower.contains("antigravity")
            || lower.contains("cursor.app")
            || lower.contains("windsurf.app")
            || lower.contains("visual studio code.app")
            || lower.contains("vs code.app")
            || lower.contains("vscodium.app")
            || lower.contains("code - insiders.app")
            || lower.contains("kiro.app")
            || lower.contains("zed.app");
    }

    let bin_name = lower
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&lower)
        .trim_end_matches(".exe");

    matches!(
        bin_name,
        "electron"
            | "antigravity"
            | "antigravity-ide"
            | "cursor"
            | "windsurf"
            | "code"
            | "code-insiders"
            | "codium"
            | "kiro"
            | "zed"
            | "zed-editor"
    )
}

/// Resolves the host IDE process ID from `VSCODE_PID` or by walking up the process tree from `start_pid`.
fn detect_ancestor_ide_pid_with_env<F>(start_pid: i32, get_env: &F) -> Option<u32>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(val) = get_env("VSCODE_PID") {
        if let Ok(pid) = val.trim().parse::<u32>() {
            if pid > 1 {
                return Some(pid);
            }
        }
    }

    let mut curr = start_pid;
    let mut matched_ide_pid = None;

    for _ in 0..8 {
        if curr <= 1 {
            break;
        }

        let Some((next_ppid, comm)) = query_process_parent_and_name(curr) else {
            break;
        };

        if is_ide_process_name(&comm) {
            if let Ok(pid_u32) = u32::try_from(curr) {
                matched_ide_pid = Some(pid_u32);
            }
        }

        if next_ppid <= 1 || next_ppid == curr {
            break;
        }

        curr = next_ppid;
    }

    matched_ide_pid
}

/// Returns `true` if an operating system process with `pid` is currently alive.
pub(crate) fn is_process_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }

    #[cfg(unix)]
    {
        // SAFETY: `libc::kill(pid, 0)` with signal 0 checks process existence without sending a signal.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .ok()
            .is_some_and(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
            })
    }

    #[cfg(not(any(unix, windows)))]
    {
        true
    }
}

/// Brings the terminal tab, multiplexer pane, or terminal emulator window for `meta` to the foreground.
///
/// Checks `tmux` / `zellij` multiplexer pane coordinates before outer terminal emulator windows,
/// percent-encodes `meta.agent_id` in IDE URIs, and returns a `waypoint-info` report for
/// non-terminal IDE sessions (`ide_pid.is_some() && tty.is_none()`).
///
/// # Errors
/// Returns [`CoreError`] only if an internal invariant fails.
pub fn focus_session_waypoint(meta: &AgentMetadata) -> CoreResult<FocusSessionReport> {
    let fallback_harness = if meta.agent_id.starts_with("antigravity-") {
        "antigravity"
    } else if meta.agent_id.starts_with("claude-") {
        "claude-code"
    } else if meta.agent_id.starts_with("cursor-") {
        "cursor"
    } else if meta.agent_id.starts_with("windsurf-") {
        "windsurf"
    } else if meta.agent_id.starts_with("kiro-") {
        "kiro"
    } else if meta.agent_id.starts_with("opencode-") {
        "opencode"
    } else if meta.agent_id.starts_with("codex-") {
        "codex"
    } else if meta.agent_id.starts_with("copilot-") {
        "copilot"
    } else {
        "terminal"
    };

    let wp = meta.waypoint.clone().unwrap_or_else(|| SessionWaypoint {
        harness: fallback_harness.to_string(),
        conversation_id: None,
        term_program: None,
        term_session_id: None,
        tty: None,
        shell_pid: None,
        ide_pid: None,
    });

    let is_ide_agent = wp.ide_pid.is_some() && wp.tty.is_none();
    let term_prog = if is_ide_agent {
        ""
    } else {
        wp.term_program.as_deref().unwrap_or("")
    };

    let is_tmux_pane = term_prog.eq_ignore_ascii_case("tmux")
        || meta.agent_id.starts_with("term-tmux-")
        || wp
            .term_session_id
            .as_deref()
            .is_some_and(|id| id.starts_with('%'));

    if !is_ide_agent && is_tmux_pane {
        if let Some(pane) = wp.term_session_id.as_deref() {
            let _ = Command::new("tmux")
                .args(["select-window", "-t", pane])
                .output();
            let status = Command::new("tmux")
                .args(["select-pane", "-t", pane])
                .output();

            if status.is_ok_and(|o| o.status.success()) {
                let _ = activate_host_terminal_window(&wp);
                return Ok(FocusSessionReport {
                    session_id: meta.agent_id.clone(),
                    harness: wp.harness.clone(),
                    focused: true,
                    mechanism: "tmux".to_string(),
                    detail: format!("Focused tmux pane {pane} for session {}", meta.agent_id),
                    waypoint: Some(wp),
                });
            }
        }
    }

    let is_zellij_pane =
        term_prog.eq_ignore_ascii_case("zellij") || meta.agent_id.starts_with("term-zellij-");

    if !is_ide_agent && is_zellij_pane && wp.term_session_id.is_some() {
        if let Some(pane) = wp.term_session_id.as_deref() {
            if activate_host_terminal_window(&wp).is_some() {
                return Ok(FocusSessionReport {
                    session_id: meta.agent_id.clone(),
                    harness: wp.harness.clone(),
                    focused: true,
                    mechanism: "zellij".to_string(),
                    detail: format!(
                        "Focused host terminal for zellij pane {pane} (session {})",
                        meta.agent_id
                    ),
                    waypoint: Some(wp),
                });
            }
        }
    }

    if !is_ide_agent
        && (term_prog.eq_ignore_ascii_case("WezTerm") || meta.agent_id.starts_with("term-wezterm-"))
        && wp.term_session_id.is_some()
    {
        if let Some(pane) = wp.term_session_id.as_deref() {
            let status = Command::new("wezterm")
                .args(["cli", "activate-pane", "--pane-id", pane])
                .output();

            if status.is_ok_and(|o| o.status.success()) {
                let _ = activate_host_terminal_window(&wp);
                return Ok(FocusSessionReport {
                    session_id: meta.agent_id.clone(),
                    harness: wp.harness.clone(),
                    focused: true,
                    mechanism: "wezterm".to_string(),
                    detail: format!("Focused WezTerm pane {pane} for session {}", meta.agent_id),
                    waypoint: Some(wp),
                });
            }
        }
    }

    if !is_ide_agent
        && (term_prog.eq_ignore_ascii_case("kitty") || meta.agent_id.starts_with("term-kitty-"))
        && wp.term_session_id.is_some()
    {
        if let Some(win_id) = wp.term_session_id.as_deref() {
            let status = Command::new("kitty")
                .args(["@", "focus-window", "--match", &format!("id:{win_id}")])
                .output();

            if status.is_ok_and(|o| o.status.success()) {
                let _ = activate_host_terminal_window(&wp);
                return Ok(FocusSessionReport {
                    session_id: meta.agent_id.clone(),
                    harness: wp.harness.clone(),
                    focused: true,
                    mechanism: "kitty".to_string(),
                    detail: format!(
                        "Focused kitty window {win_id} for session {}",
                        meta.agent_id
                    ),
                    waypoint: Some(wp),
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if !is_ide_agent && (term_prog == "iTerm.app" || meta.agent_id.starts_with("term-iterm-")) {
            let safe_tty = wp
                .tty
                .as_deref()
                .filter(|s| is_safe_applescript_token(s))
                .unwrap_or("");
            let safe_guid = wp
                .term_session_id
                .as_deref()
                .map(|s| s.rsplit(':').next().unwrap_or(s))
                .filter(|s| is_safe_applescript_token(s))
                .unwrap_or("");

            if !safe_tty.is_empty() || !safe_guid.is_empty() {
                let script = format!(
                    r#"tell application "iTerm2"
    repeat with w in windows
        repeat with t in tabs of w
            repeat with s in sessions of t
                if ("{safe_tty}" is not "" and tty of s is "{safe_tty}") or ("{safe_guid}" is not "" and unique ID of s is "{safe_guid}") then
                    select t
                    select s
                    set index of w to 1
                    activate
                    return "focused"
                end if
            end repeat
        end repeat
    end repeat
end tell
return "not-found""#
                );
                let mut tab_selected = false;

                if let Ok(out) = Command::new("osascript").args(["-e", &script]).output() {
                    if out.status.success()
                        && String::from_utf8_lossy(&out.stdout).trim() == "focused"
                    {
                        tab_selected = true;
                    }
                }

                if tab_selected || term_prog == "iTerm.app" {
                    let opened = Command::new("open")
                        .args(["-b", "com.googlecode.iterm2"])
                        .output()
                        .is_ok_and(|o| o.status.success());

                    if tab_selected || opened {
                        return Ok(FocusSessionReport {
                            session_id: meta.agent_id.clone(),
                            harness: wp.harness.clone(),
                            focused: true,
                            mechanism: "iterm2".to_string(),
                            detail: format!(
                                "Focused iTerm2 session ({}) for {}",
                                if !safe_tty.is_empty() {
                                    safe_tty
                                } else {
                                    safe_guid
                                },
                                meta.agent_id
                            ),
                            waypoint: Some(wp),
                        });
                    }
                }
            }
        }

        if !is_ide_agent {
            if let Some(safe_tty) = wp.tty.as_deref().filter(|s| is_safe_applescript_token(s)) {
                if term_prog == "Apple_Terminal" || term_prog.is_empty() || wp.ide_pid.is_none() {
                    let script = format!(
                        r#"tell application "Terminal"
    repeat with w in windows
        repeat with t in tabs of w
            if tty of t is "{safe_tty}" then
                set selected of t to true
                set frontmost of w to true
                activate
                return "focused"
            end if
        end repeat
    end repeat
end tell
return "not-found""#
                    );

                    if let Ok(out) = Command::new("osascript").args(["-e", &script]).output() {
                        if out.status.success()
                            && String::from_utf8_lossy(&out.stdout).trim() == "focused"
                        {
                            let _ = Command::new("open")
                                .args(["-b", "com.apple.Terminal"])
                                .output();
                            return Ok(FocusSessionReport {
                                session_id: meta.agent_id.clone(),
                                harness: wp.harness.clone(),
                                focused: true,
                                mechanism: "apple-terminal".to_string(),
                                detail: format!(
                                    "Focused Terminal.app tab ({safe_tty}) for {}",
                                    meta.agent_id
                                ),
                                waypoint: Some(wp),
                            });
                        }
                    }
                }
            }
        }
    }

    if !is_ide_agent
        && (wp.tty.is_some() || wp.term_session_id.is_some() || meta.agent_id.starts_with("term-"))
    {
        if let Some((mechanism, detail)) = activate_host_terminal_window(&wp) {
            return Ok(FocusSessionReport {
                session_id: meta.agent_id.clone(),
                harness: wp.harness.clone(),
                focused: true,
                mechanism,
                detail: format!("{detail} for {}", meta.agent_id),
                waypoint: Some(wp),
            });
        }

        #[cfg(target_os = "macos")]
        if let Some(ide_pid) = wp.ide_pid {
            if std::env::var_os("TES_FROM_IDE_EXTENSION").is_none() {
                let comm_lower = query_process_parent_and_name(ide_pid as i32)
                    .map(|(_, c)| c.to_ascii_lowercase())
                    .unwrap_or_default();

                let scheme = if comm_lower.contains("antigravity") || wp.harness == "antigravity" {
                    "antigravity-ide"
                } else if comm_lower.contains("cursor") || wp.harness == "cursor" {
                    "cursor"
                } else if comm_lower.contains("windsurf") || wp.harness == "windsurf" {
                    "windsurf"
                } else {
                    "vscode"
                };

                let encoded_id = percent_encode_uri_component(&meta.agent_id);
                let uri = format!("{scheme}://Lunaris.tessera/focus?session={encoded_id}");

                if Command::new("open")
                    .arg(&uri)
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    return Ok(FocusSessionReport {
                        session_id: meta.agent_id.clone(),
                        harness: wp.harness.clone(),
                        focused: true,
                        mechanism: format!("{scheme}-uri"),
                        detail: format!(
                            "Focused IDE integrated terminal via {scheme}:// for {}",
                            meta.agent_id
                        ),
                        waypoint: Some(wp),
                    });
                }
            }
        }
    }

    let mut summary_parts = vec![format!("harness={}", wp.harness)];

    if let Some(conv) = &wp.conversation_id {
        summary_parts.push(format!("conversation_id={conv}"));
    }

    if let Some(prog) = &wp.term_program {
        summary_parts.push(format!("term_program={prog}"));
    }

    if let Some(tty) = &wp.tty {
        summary_parts.push(format!("tty={tty}"));
    }

    if let Some(pid) = wp.shell_pid {
        summary_parts.push(format!("shell_pid={pid}"));
    }

    Ok(FocusSessionReport {
        session_id: meta.agent_id.clone(),
        harness: wp.harness.clone(),
        focused: false,
        mechanism: "waypoint-info".to_string(),
        detail: format!(
            "Session {} waypoint ({})",
            meta.agent_id,
            summary_parts.join(", ")
        ),
        waypoint: Some(wp),
    })
}

/// Activates the standalone terminal emulator window hosting `wp` across macOS, Linux, and Windows.
fn activate_host_terminal_window(wp: &SessionWaypoint) -> Option<(String, String)> {
    #[cfg(target_os = "macos")]
    {
        if let Some(start_pid) = wp.shell_pid.and_then(|p| i32::try_from(p).ok()) {
            let mut curr = start_pid;

            for _ in 0..10 {
                if curr <= 1 {
                    break;
                }

                let Ok(ps_out) = Command::new("ps")
                    .args(["-ww", "-o", "ppid=,comm=", "-p", &curr.to_string()])
                    .output()
                else {
                    break;
                };

                if !ps_out.status.success() {
                    break;
                }

                let line = String::from_utf8_lossy(&ps_out.stdout);
                let mut parts = line.trim().splitn(2, char::is_whitespace);
                let next_ppid = parts
                    .next()
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(0);
                let comm = parts.next().unwrap_or("").trim();
                let lower = comm.to_ascii_lowercase();

                if is_ide_process_name(&lower) {
                    break;
                }

                if let Some(app_idx) = comm.find(".app/") {
                    let app_bundle = &comm[..app_idx + 4];

                    if Command::new("open")
                        .args(["-a", app_bundle])
                        .output()
                        .is_ok_and(|o| o.status.success())
                    {
                        let app_name = app_bundle.rsplit('/').next().unwrap_or(app_bundle);
                        return Some((
                            "terminal-window".to_string(),
                            format!("Focused {app_name}"),
                        ));
                    }
                }

                if next_ppid <= 1 || next_ppid == curr {
                    break;
                }

                curr = next_ppid;
            }
        }

        if let Some(prog) = wp.term_program.as_deref() {
            let candidate_app = match prog {
                "iTerm.app" | "iTerm" => Some("iTerm"),
                "Apple_Terminal" | "Terminal" => Some("Terminal"),
                "ghostty" | "Ghostty" => Some("Ghostty"),
                "WarpTerminal" | "Warp" => Some("Warp"),
                "alacritty" | "Alacritty" => Some("Alacritty"),
                "WezTerm" | "wezterm" => Some("WezTerm"),
                "kitty" => Some("kitty"),
                "Hyper" => Some("Hyper"),
                "rio" => Some("rio"),
                "Tabby" => Some("Tabby"),
                "cmux" => Some("cmux"),
                _ => None,
            };

            if let Some(app_name) = candidate_app {
                if Command::new("open")
                    .args(["-a", app_name])
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    return Some(("terminal-window".to_string(), format!("Focused {app_name}")));
                }
            }
        }
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        if let Some(start_pid) = wp.shell_pid.and_then(|p| i32::try_from(p).ok()) {
            let mut ancestor_pids = Vec::new();
            let mut curr = start_pid;

            for _ in 0..10 {
                if curr <= 1 || wp.ide_pid.is_some_and(|ide| ide == curr as u32) {
                    break;
                }

                ancestor_pids.push(curr);
                let Some((next_ppid, comm)) = query_process_parent_and_name(curr) else {
                    break;
                };

                if is_ide_process_name(&comm.to_ascii_lowercase())
                    || next_ppid <= 1
                    || next_ppid == curr
                {
                    break;
                }

                curr = next_ppid;
            }

            if let Ok(wm_out) = Command::new("wmctrl").arg("-lp").output() {
                if wm_out.status.success() {
                    let listing = String::from_utf8_lossy(&wm_out.stdout);

                    for pid in &ancestor_pids {
                        let pid_str = pid.to_string();

                        for line in listing.lines() {
                            let mut cols = line.split_whitespace();
                            let win_id = cols.next().unwrap_or("");
                            let _desktop = cols.next();
                            let win_pid = cols.next().unwrap_or("");

                            if !win_id.is_empty() && win_pid == pid_str {
                                if Command::new("wmctrl")
                                    .args(["-ia", win_id])
                                    .output()
                                    .is_ok_and(|o| o.status.success())
                                {
                                    return Some((
                                        "wmctrl".to_string(),
                                        format!("Focused terminal window {win_id} (PID {pid})"),
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            for pid in &ancestor_pids {
                let pid_str = pid.to_string();

                if Command::new("xdotool")
                    .args(["search", "--pid", &pid_str, "windowactivate"])
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    return Some((
                        "xdotool".to_string(),
                        format!("Focused terminal window (PID {pid})"),
                    ));
                }

                if Command::new("hyprctl")
                    .args(["dispatch", "focuswindow", &format!("pid:{pid}")])
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    return Some((
                        "hyprctl".to_string(),
                        format!("Focused Hyprland terminal window (PID {pid})"),
                    ));
                }

                if Command::new("swaymsg")
                    .arg(format!("[pid={pid}] focus"))
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    return Some((
                        "swaymsg".to_string(),
                        format!("Focused Sway terminal window (PID {pid})"),
                    ));
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(start_pid) = wp.shell_pid.and_then(|p| i32::try_from(p).ok()) {
            let stop_ide_pid = wp.ide_pid.unwrap_or(0);
            let ps_script = format!(
                "$ws = New-Object -ComObject WScript.Shell; $curr = {start_pid}; for ($i = 0; $i -lt 8; $i++) {{ if ($curr -le 1 -or $curr -eq {stop_ide_pid}) {{ break }}; if ($ws.AppActivate($curr)) {{ Write-Output \"focused:$curr\"; exit 0 }}; $proc = Get-CimInstance Win32_Process -Filter \"ProcessId = $curr\" -ErrorAction SilentlyContinue; if (-not $proc -or $proc.ParentProcessId -le 1 -or $proc.ParentProcessId -eq $curr) {{ break }}; $curr = $proc.ParentProcessId }}; if ($ws.AppActivate('WindowsTerminal')) {{ Write-Output 'focused:wt'; exit 0 }}; exit 1"
            );

            if let Ok(out) = Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
                .output()
            {
                if out.status.success() {
                    let stdout = String::from_utf8_lossy(&out.stdout);

                    if stdout.trim().starts_with("focused:") {
                        return Some((
                            "windows-terminal".to_string(),
                            format!("Focused Windows terminal ({})", stdout.trim()),
                        ));
                    }
                }
            }
        }
    }

    None
}

/// Validates that `token` contains only safe path/identifier characters before interpolating into AppleScript.
#[cfg(target_os = "macos")]
fn is_safe_applescript_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | ':' | '.'))
}

impl Coordinator {
    /// Opens an existing Tessera-enabled repository by discovering `.git` from `start_dir`.
    ///
    /// Supports standard repositories, linked Git worktrees, and Git submodules via
    /// [`GitBackend::common_git_dir`].
    ///
    /// # Errors
    /// Returns [`CoreError::RepoNotFound`] if `.git` or `.git/tessera` is not initialized.
    pub fn open(start_dir: &Path) -> CoreResult<Self> {
        let git = GitBackend::discover(start_dir)?;
        let oplog = OpLogStore::new_with_repo_root(git.common_git_dir(), git.repo_root());

        if !oplog.is_initialized() {
            return Err(CoreError::RepoNotFound(
                oplog.store_dir().display().to_string(),
            ));
        }

        let invocation_dir = Self::discover_sandbox_root(start_dir, git.repo_root());
        let coord = Self {
            git,
            oplog,
            invocation_dir,
        };
        coord.prune_dead_terminal_sessions();
        Ok(coord)
    }

    /// Walks upward from `start_dir` until `repo_root` to detect whether the caller is executing
    /// inside an isolated Copy-on-Write branch sandbox (`.tessera/repo_root`).
    fn discover_sandbox_root(start_dir: &Path, repo_root: &Path) -> Option<PathBuf> {
        let mut curr = start_dir.to_path_buf();

        loop {
            if curr == repo_root {
                return None;
            }

            if is_valid_sandbox_repo_root(&curr, Some(repo_root)).is_some() {
                return Some(curr);
            }

            if !curr.pop() {
                return None;
            }
        }
    }

    /// Returns the Copy-on-Write sandbox root directory if `tes` was invoked from inside a sandbox.
    #[must_use]
    pub fn invocation_dir(&self) -> Option<&Path> {
        self.invocation_dir.as_deref()
    }

    /// Automatically removes stale passive `term-<pid>` and `session-<pid>` entries whose OS process
    /// is no longer running, as well as clean idle sessions older than 30 minutes, so dead or
    /// abandoned sessions never pollute radar or conflict resolution.
    pub fn prune_dead_terminal_sessions(&self) {
        const IDLE_TTL_MS: u64 = 30 * 60 * 1000;

        let Ok(current) = self.oplog.load_current_reconciled() else {
            return;
        };

        let now = now_millis();
        let mut dead_ids = Vec::new();

        for (id, agent) in &current.view.agents {
            let pid_opt = id
                .strip_prefix("term-")
                .or_else(|| id.strip_prefix("session-"))
                .and_then(|s| s.parse::<i32>().ok());

            if let Some(pid) = pid_opt {
                let matches_shell_pid = agent
                    .waypoint
                    .as_ref()
                    .and_then(|wp| wp.shell_pid)
                    .is_some_and(|spid| spid == pid as u32);

                if pid > 1
                    && matches_shell_pid
                    && agent.intent_inferred
                    && agent.workspace_path.is_none()
                {
                    let alive = is_process_alive(pid);

                    if !alive {
                        dead_ids.push(id.clone());
                        continue;
                    }
                }
            }

            if !agent.has_own_changes()
                && agent.head_commit == agent.base_commit
                && agent.workspace_path.is_none()
                && now.saturating_sub(agent.updated_at_ms) > IDLE_TTL_MS
            {
                dead_ids.push(id.clone());
            }
        }

        if !dead_ids.is_empty() {
            let _ = self.oplog.record_tx(
                "gc",
                format!("prune dead shell sessions {:?}", dead_ids),
                true,
                |view| {
                    for id in &dead_ids {
                        view.agents.remove(id);
                    }

                    Ok(())
                },
            );

            for id in &dead_ids {
                self.git.delete_ref(&format!("refs/tessera/agents/{id}"));
            }
        }
    }

    /// Initializes `.git` (if needed), `.git/tessera`, snapshots the initial working tree if untracked
    /// files exist on a fresh repo, and installs repo-local harness integrations.
    ///
    /// # Errors
    /// Returns [`CoreError`] if repository or hook initialization fails.
    pub fn init_repo(repo_root: &Path) -> CoreResult<(Self, InitReport)> {
        Self::init_repo_with_full_options(repo_root, None, None, false)
    }

    /// Initializes `.git` (if needed), `.git/tessera`, optionally sets the initial/target branch to
    /// `branch_override`, snapshots the initial working tree, and installs harness integrations.
    ///
    /// # Errors
    /// Returns [`CoreError`] if repository or hook initialization fails.
    pub fn init_repo_with_branch(
        repo_root: &Path,
        branch_override: Option<&str>,
    ) -> CoreResult<(Self, InitReport)> {
        Self::init_repo_with_full_options(repo_root, branch_override, None, false)
    }

    /// Initializes `.git` (if needed), `.git/tessera`, optionally sets the initial/target branch to
    /// `branch_override`, and installs harness integrations filtered by `harness_filter`
    /// (`"antigravity"`, `"claude"`, `"cursor"`, `"all"`, or `None` for auto-detection).
    ///
    /// # Errors
    /// Returns [`CoreError`] if repository or hook initialization fails.
    pub fn init_repo_with_options(
        repo_root: &Path,
        branch_override: Option<&str>,
        harness_filter: Option<&str>,
    ) -> CoreResult<(Self, InitReport)> {
        Self::init_repo_with_full_options(repo_root, branch_override, harness_filter, false)
    }

    /// Initializes `.git` (if needed), `.git/tessera`, optionally sets the initial/target branch to
    /// `branch_override`, installs harness integrations filtered by `harness_filter`, and when
    /// `local_only` is `true`, records all newly created harness rule/config paths in
    /// `<common_git_dir>/info/exclude` so mixed teams do not accidentally commit Tessera-only files.
    ///
    /// # Errors
    /// Returns [`CoreError`] if repository or hook initialization fails.
    pub fn init_repo_with_full_options(
        repo_root: &Path,
        branch_override: Option<&str>,
        harness_filter: Option<&str>,
        local_only: bool,
    ) -> CoreResult<(Self, InitReport)> {
        let git = GitBackend::init_or_open(repo_root)?;
        let common_git_dir = git.common_git_dir().to_path_buf();
        let oplog = OpLogStore::new_with_repo_root(&common_git_dir, git.repo_root());

        if let Some(b) = branch_override.map(str::trim).filter(|s| !s.is_empty()) {
            if let Ok(Some(head_oid)) = git.resolve_head_commit() {
                git.update_ref(&format!("refs/heads/{b}"), &head_oid)?;
            }

            git.set_head_branch(b)?;
        }

        let pre_existing_harness: BTreeSet<&str> = KNOWN_HARNESS_PATHS
            .iter()
            .copied()
            .filter(|rel| git.repo_root().join(rel).exists())
            .collect();

        // Install harness auto-discovery files first so they can be excluded or snapshotted accurately.
        let harness_files =
            install_harness_integrations_for(git.repo_root(), &common_git_dir, harness_filter)?;

        if local_only {
            let newly_installed_excludes: Vec<String> = harness_files
                .iter()
                .filter(|rel| {
                    !rel.starts_with(".git/") && !pre_existing_harness.contains(rel.as_str())
                })
                .cloned()
                .collect();
            add_paths_to_git_exclude(&common_git_dir, &newly_installed_excludes)?;
        }

        fs::create_dir_all(common_git_dir.join("tessera/stat_cache")).map_err(|source| {
            CoreError::Io {
                path: common_git_dir.display().to_string(),
                source,
            }
        })?;

        let branch = git
            .current_branch_name()
            .unwrap_or_else(|_| "main".to_string());
        let current_head = git
            .resolve_head_commit()?
            .ok_or_else(|| CoreError::NotFound("HEAD commit".to_string()))?;
        let head_commit_obj = git.read_commit(&current_head)?;
        let head_files = git.read_tree_to_map(&head_commit_obj.tree_id)?;

        let final_head = if head_files.is_empty() {
            let snap = snapshot_working_dir(&git, git.repo_root(), "root", &[], None)?;

            if !snap.files.is_empty() {
                let parents: Vec<ObjectId> = if head_commit_obj.parents.is_empty() {
                    Vec::new()
                } else {
                    vec![current_head]
                };
                let author = git.resolve_git_author();
                let updated_commit =
                    git.write_commit(&snap.root_tree_id, &parents, &author, "Initial commit")?;
                git.update_ref(&format!("refs/heads/{branch}"), &updated_commit)?;
                git.sync_git_index(&updated_commit);
                updated_commit
            } else {
                current_head
            }
        } else {
            current_head
        };

        let is_fresh_init = !oplog.is_initialized();
        let mut initial_view = View::default();
        initial_view.branches.insert(branch.clone(), final_head);
        initial_view.next_agent_slot = 1;

        let root_op = oplog.init(initial_view)?;
        let mut cfg = oplog.load_config()?;

        if cfg.trunk_branch != branch {
            cfg.trunk_branch = branch.clone();
            oplog.save_config(&cfg)?;
        }

        let coord = Self {
            git,
            oplog,
            invocation_dir: None,
        };

        if is_fresh_init {
            let _ = fs::remove_dir_all(coord.sandboxes_root());
        }

        let report = InitReport {
            repo_root: coord.git.repo_root().display().to_string(),
            trunk_branch: branch,
            head_commit: final_head,
            root_op_id: root_op.id,
            harness_files_installed: harness_files,
        };
        Ok((coord, report))
    }

    /// Ensures that an AI agent session (`session_id`) is registered in `.git/tessera`.
    ///
    /// If the session is already active, updates its `intent` if `explicit_intent` is `Some`.
    /// If the session is new, allocates a unique runtime slot (`1..=N`), generates a reverse-hex
    /// [`ChangeId`], and branches from the current trunk HEAD.
    ///
    /// Placeholder adoption is restricted strictly to clean `mcp-*` sessions (`!a.has_own_changes()`)
    /// so dirty `mcp-*` edits are never discarded and explicit human `term-*` claims are never stolen.
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading HEAD or writing to the `OpLog` fails.
    pub fn ensure_session(
        &self,
        session_id: &str,
        explicit_intent: Option<&str>,
    ) -> CoreResult<AgentMetadata> {
        validate_safe_identifier("session_id", session_id)?;
        let head_commit = self
            .git
            .resolve_head_commit()?
            .ok_or_else(|| CoreError::NotFound("HEAD commit".to_string()))?;
        let commit_obj = self.git.read_commit(&head_commit)?;
        let head_branch = self.git.current_branch_name().unwrap_or_default();
        let cfg = self.oplog.load_config().unwrap_or_default();
        let current_op = self.oplog.load_current_reconciled().ok();

        let is_ai_agent = is_ai_session_id(session_id);
        let can_adopt_placeholder = is_ai_agent && !session_id.starts_with("mcp-");
        let is_term_session = session_id.starts_with("term-") || session_id.starts_with("session-");

        if let Some(ref cur) = current_op {
            if let Some(existing) = cur.view.agents.get(session_id) {
                let non_trunk_branch = existing
                    .target_branch
                    .as_deref()
                    .filter(|b| !b.is_empty() && *b != cfg.trunk_branch);

                let slot_ok = if let Some(br) = non_trunk_branch {
                    existing.slot == self.branch_slot(&cur.view, br, &cfg.trunk_branch)
                } else {
                    let used_by_others: BTreeSet<u16> = cur
                        .view
                        .agents
                        .iter()
                        .filter(|(id, a)| {
                            *id != session_id
                                && (is_ai_session_id(id) == is_ai_agent
                                    || a.target_branch
                                        .as_deref()
                                        .is_some_and(|b| !b.is_empty() && b != cfg.trunk_branch))
                        })
                        .map(|(_, a)| a.slot)
                        .collect();

                    let max_compact_slot = (used_by_others.len() as u16).saturating_add(1).max(1);
                    existing.slot > 0
                        && !used_by_others.contains(&existing.slot)
                        && (existing.workspace_path.is_some() || existing.slot <= max_compact_slot)
                };

                let has_placeholder = can_adopt_placeholder
                    && existing.targets_head_branch(&head_branch)
                    && existing.intent_inferred
                    && !existing.has_own_changes()
                    && cur
                        .view
                        .agents
                        .iter()
                        .any(|(id, a)| id.starts_with("mcp-") && !a.has_own_changes());

                let branch_ok = existing.target_branch.is_some() || !is_term_session;

                let commit_ok = !(existing.targets_head_branch(&head_branch)
                    && !existing.has_own_changes()
                    && existing.claimed_symbols.is_empty()
                    && existing.read_blobs.is_empty()
                    && existing.head_commit == existing.base_commit
                    && existing.base_commit != head_commit);

                let intent_ok = explicit_intent.is_none_or(|s| {
                    let trimmed = s.trim();
                    trimmed.is_empty() || (!existing.intent_inferred && existing.intent == trimmed)
                });

                let waypoint_ok = session_id == "editor" || existing.waypoint.is_some();

                if slot_ok && !has_placeholder && branch_ok && commit_ok && intent_ok && waypoint_ok
                {
                    return Ok(existing.clone());
                }
            }
        }

        let foreign_baseline = if is_ai_agent {
            match current_op.as_ref() {
                Some(cur) if !cur.view.agents.contains_key(session_id) => {
                    self.unowned_worktree_changes(&cur.view, &commit_obj.tree_id)
                }
                _ => BTreeSet::new(),
            }
        } else {
            BTreeSet::new()
        };

        let mut out_agent: Option<AgentMetadata> = None;
        let mut adopted_placeholder_id: Option<String> = None;

        self.oplog.record_tx(
            session_id,
            format!("ensure session {session_id}"),
            true,
            |view| {
                let adopted_placeholder = if can_adopt_placeholder {
                    let existing_needs_adoption = view.agents.get(session_id).is_none_or(|a| {
                        a.targets_head_branch(&head_branch)
                            && a.intent_inferred
                            && !a.has_own_changes()
                    });

                    if existing_needs_adoption {
                        let placeholder_id = view
                            .agents
                            .iter()
                            .filter(|(id, a)| id.starts_with("mcp-") && !a.has_own_changes())
                            .max_by_key(|(_, a)| a.updated_at_ms)
                            .map(|(id, _)| id.clone());

                        if let Some(pid) = placeholder_id {
                            adopted_placeholder_id = Some(pid.clone());
                            view.agents.remove(&pid)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let initial_branch = if is_term_session {
                    self.git.current_branch_name().ok()
                } else {
                    None
                };

                let inherited_term_branch = if is_ai_agent {
                    let env_root_matches = std::env::var("TES_REPO_ROOT").ok().is_some_and(|r| {
                        let env_path = Path::new(r.trim());
                        env_path == self.git.repo_root()
                            || (env_path.canonicalize().ok()
                                == self.git.repo_root().canonicalize().ok()
                                && env_path.canonicalize().is_ok())
                    });

                    if env_root_matches {
                        std::env::var("TES_BRANCH")
                            .ok()
                            .map(|b| b.trim().to_string())
                            .filter(|b| !b.is_empty() && validate_ref_name(b).is_ok())
                            .or_else(|| {
                                std::env::var("TES_SESSION_ID")
                                    .ok()
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| s.starts_with("term-") || s.starts_with("session-"))
                                    .and_then(|s| {
                                        view.agents.get(&s).and_then(|a| a.target_branch.clone())
                                    })
                            })
                    } else {
                        None
                    }
                } else {
                    None
                };

                let needs_waypoint = session_id != "editor"
                    && view
                        .agents
                        .get(session_id)
                        .and_then(|a| a.waypoint.as_ref())
                        .is_none();

                let detected_waypoint = needs_waypoint.then(|| detect_session_waypoint(session_id));

                let used_by_others: BTreeSet<u16> = view
                    .agents
                    .iter()
                    .filter(|(id, a)| {
                        *id != session_id
                            && (is_ai_session_id(id) == is_ai_agent
                                || a.target_branch
                                    .as_deref()
                                    .is_some_and(|b| !b.is_empty() && b != cfg.trunk_branch))
                    })
                    .map(|(_, a)| a.slot)
                    .collect();

                let lowest_free_slot = (1u16..=u16::MAX)
                    .find(|s| !used_by_others.contains(s))
                    .unwrap_or(1);

                if let Some(existing) = view.agents.get_mut(session_id) {
                    let max_compact_slot = (used_by_others.len() as u16).saturating_add(1).max(1);

                    if existing.workspace_path.is_none()
                        && existing
                            .target_branch
                            .as_deref()
                            .is_none_or(|b| b.is_empty() || b == cfg.trunk_branch)
                        && (existing.slot == 0
                            || used_by_others.contains(&existing.slot)
                            || existing.slot > max_compact_slot)
                    {
                        existing.slot = lowest_free_slot;
                    }

                    if let Some(adopted) = adopted_placeholder {
                        if existing.intent_inferred && !adopted.intent_inferred {
                            existing.intent = adopted.intent;
                            existing.intent_inferred = false;
                        }

                        if existing.targets_head_branch(&head_branch) {
                            existing.target_branch = adopted.target_branch;

                            if !existing.has_own_changes() {
                                existing.base_commit = adopted.base_commit;
                                existing.head_commit = adopted.head_commit;
                                existing.head_tree = adopted.head_tree;
                            }
                        }

                        existing.claimed_symbols.extend(adopted.claimed_symbols);

                        for (k, v) in adopted.read_blobs {
                            existing.read_blobs.entry(k).or_insert(v);
                        }
                    }

                    if existing.target_branch.is_none() && is_term_session {
                        existing.target_branch = initial_branch;
                    }

                    let resolved_non_trunk = existing
                        .target_branch
                        .as_deref()
                        .filter(|b| !b.is_empty() && *b != cfg.trunk_branch)
                        .map(str::to_string);

                    if existing.targets_head_branch(&head_branch)
                        && !existing.has_own_changes()
                        && existing.claimed_symbols.is_empty()
                        && existing.read_blobs.is_empty()
                        && existing.head_commit == existing.base_commit
                        && existing.base_commit != head_commit
                    {
                        existing.base_commit = head_commit;
                        existing.head_commit = head_commit;
                        existing.head_tree = commit_obj.tree_id;
                        existing.read_blobs.clear();
                        existing.integrated_peer_blobs.clear();
                    }

                    if let Some(intent_str) = explicit_intent {
                        if !intent_str.trim().is_empty() {
                            existing.intent = intent_str.trim().to_string();
                            existing.intent_inferred = false;
                        }
                    }

                    if existing.waypoint.is_none() {
                        if let Some(wp) = detected_waypoint.clone() {
                            existing.waypoint = Some(wp);
                        }
                    }

                    existing.updated_at_ms = now_millis();
                    let mut active_slot = existing.slot;
                    let mut cloned = existing.clone();

                    if let Some(ref br) = resolved_non_trunk {
                        let br_slot = self.branch_slot(view, br, &cfg.trunk_branch);

                        if let Some(ex) = view.agents.get_mut(session_id) {
                            ex.slot = br_slot;
                        }

                        cloned.slot = br_slot;
                        active_slot = br_slot;
                    }

                    out_agent = Some(cloned);

                    view.next_agent_slot = used_by_others
                        .iter()
                        .copied()
                        .max()
                        .unwrap_or(0)
                        .max(active_slot)
                        .saturating_add(1);

                    return Ok(());
                }

                let mut slot = lowest_free_slot;

                view.next_agent_slot = used_by_others
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(0)
                    .max(slot)
                    .saturating_add(1);

                let change_id =
                    ChangeId::from_seed(&format!("{session_id}-{}", head_commit.as_str()));

                let (mut intent, mut intent_inferred) = match explicit_intent {
                    Some(s) if !s.trim().is_empty() => (s.trim().to_string(), false),
                    _ => (format!("Active session {session_id}"), true),
                };

                let mut base_commit = head_commit;
                let mut head_commit_val = head_commit;
                let mut head_tree = commit_obj.tree_id;
                let mut claimed_symbols = BTreeSet::new();
                let mut read_blobs = BTreeMap::new();
                let mut target_branch = initial_branch;
                let mut waypoint = detected_waypoint;

                if let Some(adopted) = adopted_placeholder {
                    if intent_inferred && !adopted.intent_inferred {
                        intent = adopted.intent;
                        intent_inferred = false;
                    }

                    base_commit = adopted.base_commit;
                    head_commit_val = adopted.head_commit;
                    head_tree = adopted.head_tree;
                    claimed_symbols = adopted.claimed_symbols;
                    read_blobs = adopted.read_blobs;
                    target_branch = adopted.target_branch;

                    if waypoint.is_none() {
                        waypoint = adopted.waypoint;
                    }
                } else if let Some(env_b) = inherited_term_branch {
                    let resolved_cid = self
                        .git
                        .resolve_ref(&format!("refs/heads/{env_b}"))
                        .ok()
                        .flatten()
                        .or_else(|| view.branches.get(&env_b).copied());

                    if let Some(b_cid) = resolved_cid {
                        if let Ok(b_commit) = self.git.read_commit(&b_cid) {
                            target_branch = Some(env_b);
                            base_commit = b_cid;
                            head_commit_val = b_cid;
                            head_tree = b_commit.tree_id;
                        }
                    }
                }

                if let Some(ref br) = target_branch {
                    if !br.is_empty() && br != &cfg.trunk_branch {
                        slot = self.branch_slot(view, br, &cfg.trunk_branch);
                    }
                }

                let meta = AgentMetadata {
                    agent_id: session_id.to_string(),
                    slot,
                    change_id: change_id.clone(),
                    base_commit,
                    head_commit: head_commit_val,
                    head_tree,
                    intent,
                    intent_inferred,
                    claimed_symbols,
                    scopes: Vec::new(),
                    workspace_path: None,
                    read_blobs,
                    own_blobs: BTreeMap::new(),
                    deleted_paths: BTreeSet::new(),
                    foreign_baseline: foreign_baseline.clone(),
                    integrated_peer_blobs: BTreeMap::new(),
                    target_branch,
                    checkpoints: BTreeMap::new(),
                    waypoint,
                    updated_at_ms: now_millis(),
                };

                view.changes.insert(change_id, head_commit_val);
                view.agents.insert(session_id.to_string(), meta.clone());
                out_agent = Some(meta);

                Ok(())
            },
        )?;

        if let Some(pid) = adopted_placeholder_id {
            self.git.delete_ref(&format!("refs/tessera/agents/{pid}"));
        }

        let meta = out_agent.ok_or_else(|| CoreError::NotFound(session_id.to_string()))?;
        self.git.update_ref(
            &format!("refs/tessera/agents/{session_id}"),
            &meta.head_commit,
        )?;
        self.git.update_ref(
            &format!("refs/tessera/changes/{}", meta.change_id.as_str()),
            &meta.head_commit,
        )?;
        Ok(meta)
    }

    /// Spawns an isolated Copy-on-Write workspace at `.tessera/workspaces/<agent_id>` for an agent,
    /// cloning warm build caches (`target/`, `node_modules/`) via OS COW and sanitizing `.d` stamps.
    ///
    /// # Errors
    /// Returns [`CoreError`] if workspace creation or `OpLog` recording fails.
    pub fn spawn_cow_agent(
        &self,
        agent_id: &str,
        intent: Option<&str>,
        scopes: Vec<String>,
    ) -> CoreResult<AgentMetadata> {
        validate_safe_identifier("agent_id", agent_id)?;
        let cfg = self.oplog.load_config()?;
        let ws_dir = self
            .git
            .repo_root()
            .join(".tessera")
            .join("workspaces")
            .join(agent_id);

        create_cow_workspace(self.git.repo_root(), &ws_dir, &scopes, &cfg.cow_cache_dirs)?;

        let _ = self.ensure_session(agent_id, intent)?;
        let ws_path_str = ws_dir.display().to_string();
        let mut updated_meta = None;

        self.oplog.record_tx(
            agent_id,
            format!("spawn COW workspace for {agent_id}"),
            false,
            |view| {
                if let Some(agent) = view.agents.get_mut(agent_id) {
                    agent.scopes = scopes.clone();
                    agent.workspace_path = Some(ws_path_str.clone());
                    agent.updated_at_ms = now_millis();
                    updated_meta = Some(agent.clone());
                }

                Ok(())
            },
        )?;

        updated_meta.ok_or_else(|| CoreError::NotFound(agent_id.to_string()))
    }

    /// Incrementally snapshots the working directory of `agent_id` (either its isolated COW workspace
    /// or the shared repository root), extracts modified AST symbols via Tree-sitter, auto-infers
    /// semantic intent if needed, and updates the agent's Git commit and `ChangeId` ref.
    ///
    /// # Errors
    /// Returns [`CoreError`] if snapshotting or Git commit creation fails.
    pub fn snapshot_agent(&self, agent_id: &str, is_ephemeral: bool) -> CoreResult<AgentMetadata> {
        let meta = self.ensure_session(agent_id, None)?;
        let work_dir = meta
            .workspace_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.git.repo_root().to_path_buf());

        let base_commit_obj = self.git.read_commit(&meta.base_commit)?;
        let mut snap = snapshot_working_dir(
            &self.git,
            &work_dir,
            agent_id,
            &meta.scopes,
            Some(&base_commit_obj.tree_id),
        )?;

        let base_files = self.git.read_tree_to_map(&base_commit_obj.tree_id)?;
        let mut sandbox_own_blobs: BTreeMap<String, ObjectId> = BTreeMap::new();
        let mut snapshot_deleted_paths: BTreeSet<String> = BTreeSet::new();

        if meta.workspace_path.is_none() {
            let current = self.oplog.load_current_reconciled()?;
            let other_owned_paths: BTreeSet<String> = current
                .view
                .agents
                .values()
                .filter(|a| a.agent_id != agent_id)
                .flat_map(|a| a.owned_paths().cloned())
                .collect();
            let other_touched_paths: BTreeSet<String> = current
                .view
                .agents
                .values()
                .filter(|a| a.agent_id != agent_id)
                .flat_map(|a| {
                    a.owned_paths()
                        .cloned()
                        .chain(a.read_blobs.keys().cloned())
                        .chain(a.claimed_symbols.iter().map(|s| {
                            s.split("::")
                                .next()
                                .unwrap_or(s.as_str())
                                .trim()
                                .to_string()
                        }))
                })
                .collect();

            let current_head_branch = self.git.current_branch_name().unwrap_or_default();
            let eff_tb = meta.effective_branch(&current_head_branch).to_string();

            if eff_tb == current_head_branch {
                for rel_path in meta.own_blobs.keys().chain(base_files.keys()) {
                    if !snap.files.contains_key(rel_path)
                        && !other_owned_paths.contains(rel_path)
                        && !is_harness_config_path(rel_path)
                        && session_may_sweep(&meta, rel_path, &base_files)
                    {
                        snapshot_deleted_paths.insert(rel_path.clone());
                    }
                }

                let isolated_files = Self::isolate_head_branch_tree(
                    &meta,
                    &snap.files,
                    &base_files,
                    &other_owned_paths,
                    &other_touched_paths,
                );

                if !eff_tb.is_empty() {
                    let branch_ws = self.branch_sandbox_dir(&eff_tb);

                    if branch_ws.is_dir() {
                        for rel_path in base_files.keys().chain(meta.own_blobs.keys()) {
                            if !isolated_files.contains_key(rel_path)
                                && !snap.files.contains_key(rel_path)
                            {
                                let _ = fs::remove_file(branch_ws.join(rel_path));
                            }
                        }
                    }
                }

                snap.files = isolated_files;
                snap.root_tree_id = self.git.write_tree_from_map(&snap.files)?;
            } else {
                let branch_ws = self.branch_sandbox_dir(&eff_tb);

                if branch_ws.is_dir() {
                    let safe_b = Self::sanitize_branch_name(&eff_tb);
                    let cache_key = format!("{agent_id}-branch-{safe_b}");

                    if let Ok(branch_snap) = snapshot_working_dir(
                        &self.git,
                        &branch_ws,
                        &cache_key,
                        &meta.scopes,
                        Some(&base_commit_obj.tree_id),
                    ) {
                        for rel_path in base_files.keys().chain(meta.own_blobs.keys()) {
                            if !branch_snap.files.contains_key(rel_path)
                                && !other_owned_paths.contains(rel_path)
                                && !is_harness_config_path(rel_path)
                            {
                                snapshot_deleted_paths.insert(rel_path.clone());
                            }
                        }

                        if !meta.has_own_changes() {
                            for (rel_path, entry) in branch_snap.files {
                                if !is_harness_config_path(&rel_path)
                                    && base_files.get(&rel_path) != Some(&entry)
                                    && !other_owned_paths.contains(&rel_path)
                                {
                                    sandbox_own_blobs.insert(rel_path.clone(), entry.blob_id);
                                    snap.files.insert(rel_path, entry);
                                }
                            }
                        }
                    }
                }

                let mut isolated_files = base_files.clone();

                for del_path in &snapshot_deleted_paths {
                    isolated_files.remove(del_path);
                }

                for (rel_path, blob_id) in &sandbox_own_blobs {
                    let mode = snap
                        .files
                        .get(rel_path)
                        .or_else(|| base_files.get(rel_path))
                        .map_or(EntryMode::NormalFile, FileEntry::entry_mode);
                    isolated_files.insert(rel_path.clone(), FileEntry::from_mode(*blob_id, mode));
                }

                let head_files = self
                    .git
                    .read_tree_to_map(&meta.head_tree)
                    .unwrap_or_default();

                for (rel_path, own_blob_id) in &meta.own_blobs {
                    if snapshot_deleted_paths.contains(rel_path) {
                        continue;
                    }

                    let mode = head_files
                        .get(rel_path)
                        .or_else(|| base_files.get(rel_path))
                        .map_or(EntryMode::NormalFile, FileEntry::entry_mode);
                    isolated_files
                        .insert(rel_path.clone(), FileEntry::from_mode(*own_blob_id, mode));
                }

                for del_path in &meta.deleted_paths {
                    isolated_files.remove(del_path);
                }

                snap.files = isolated_files;
                snap.root_tree_id = self.git.write_tree_from_map(&snap.files)?;
            }
        }

        let mut detected_symbols = BTreeSet::new();
        let mut first_touched_path = None;
        let scan_whole_tree_symbols = meta.workspace_path.is_some()
            || meta.read_blobs.is_empty()
            || !sandbox_own_blobs.is_empty();

        if scan_whole_tree_symbols {
            for (path, entry) in &snap.files {
                let base_blob_id = base_files.get(path).map(|e| &e.blob_id);

                if base_blob_id != Some(&entry.blob_id) {
                    if first_touched_path.is_none() {
                        first_touched_path = Some(path.clone());
                    }

                    if entry.is_symlink() || entry.is_gitlink() {
                        continue;
                    }

                    let base_bytes = match base_blob_id {
                        Some(bid) => self.git.read_blob(bid)?,
                        None => Vec::new(),
                    };
                    let curr_bytes = self.git.read_blob(&entry.blob_id)?;

                    if is_binary_bytes(&base_bytes) || is_binary_bytes(&curr_bytes) {
                        continue;
                    }

                    let syms = diff_modified_symbols(
                        path,
                        &String::from_utf8_lossy(&base_bytes),
                        &String::from_utf8_lossy(&curr_bytes),
                    );

                    for sym in syms {
                        detected_symbols.insert(format!("{path}::{}", sym.label));
                    }
                }
            }
        }

        let mut effective_intent = meta.intent.clone();

        if meta.intent_inferred {
            if !detected_symbols.is_empty() {
                let list: Vec<&str> = detected_symbols
                    .iter()
                    .take(3)
                    .map(|s| s.rsplit("::").next().unwrap_or(s.as_str()))
                    .collect();

                if let Some(p) = &first_touched_path {
                    effective_intent = format!("Update {p} ({})", list.join(", "));
                } else {
                    effective_intent = format!("Update {}", list.join(", "));
                }
            } else if let Some(p) = &first_touched_path {
                effective_intent = format!("Update {p}");
            }
        }

        let author = self.git.resolve_git_author();
        let new_commit = if snap.root_tree_id == meta.head_tree {
            meta.head_commit
        } else {
            self.git.write_commit(
                &snap.root_tree_id,
                std::slice::from_ref(&meta.base_commit),
                &author,
                &effective_intent,
            )?
        };

        let mut updated_meta = None;
        self.oplog.record_tx(
            agent_id,
            format!("snapshot {agent_id} -> {}", new_commit.short()),
            is_ephemeral,
            |view| {
                if let Some(agent) = view.agents.get_mut(agent_id) {
                    agent.head_commit = new_commit;
                    agent.head_tree = snap.root_tree_id;

                    for del_path in &snapshot_deleted_paths {
                        agent.own_blobs.remove(del_path);

                        if base_files.contains_key(del_path) {
                            agent.deleted_paths.insert(del_path.clone());
                        } else {
                            agent.deleted_paths.remove(del_path);
                        }
                    }

                    for (rel_path, blob_id) in &sandbox_own_blobs {
                        agent.deleted_paths.remove(rel_path);
                        agent.own_blobs.insert(rel_path.clone(), *blob_id);
                    }

                    for sym in &detected_symbols {
                        agent.claimed_symbols.insert(sym.clone());
                    }

                    if agent.intent_inferred && effective_intent != agent.intent {
                        agent.intent = effective_intent.clone();
                    }

                    agent.updated_at_ms = now_millis();
                    view.changes.insert(agent.change_id.clone(), new_commit);
                    updated_meta = Some(agent.clone());
                }

                Ok(())
            },
        )?;

        let out = updated_meta.ok_or_else(|| CoreError::NotFound(agent_id.to_string()))?;
        self.git
            .update_ref(&format!("refs/tessera/agents/{agent_id}"), &out.head_commit)?;
        self.git.update_ref(
            &format!("refs/tessera/changes/{}", out.change_id.as_str()),
            &out.head_commit,
        )?;
        Ok(out)
    }

    /// Builds the tree of a shared-root session that targets the branch checked out in `repo_root`.
    pub(crate) fn isolate_head_branch_tree(
        meta: &AgentMetadata,
        disk_files: &BTreeMap<String, FileEntry>,
        base_files: &BTreeMap<String, FileEntry>,
        other_owned_paths: &BTreeSet<String>,
        other_touched_paths: &BTreeSet<String>,
    ) -> BTreeMap<String, FileEntry> {
        let mut tree = disk_files.clone();

        let attributable = |rel_path: &String| {
            !other_owned_paths.contains(rel_path) && session_may_sweep(meta, rel_path, base_files)
        };

        let moved_away_blobs: BTreeSet<ObjectId> = base_files
            .iter()
            .filter(|(p, _)| !disk_files.contains_key(*p) && (meta.owns_path(p) || attributable(p)))
            .map(|(_, e)| e.blob_id)
            .collect();

        let changed_paths: BTreeSet<&String> = disk_files
            .iter()
            .filter(|(p, e)| base_files.get(*p) != Some(*e))
            .map(|(p, _)| p)
            .chain(base_files.keys().filter(|p| !disk_files.contains_key(*p)))
            .collect();

        for rel_path in changed_paths {
            let moved_in = !base_files.contains_key(rel_path)
                && !other_owned_paths.contains(rel_path)
                && disk_files
                    .get(rel_path)
                    .is_some_and(|e| moved_away_blobs.contains(&e.blob_id));

            if moved_in || attributable(rel_path) || meta.owns_path(rel_path) {
                continue;
            }

            match base_files.get(rel_path) {
                Some(base_entry) => {
                    tree.insert(rel_path.clone(), base_entry.clone());
                }
                None => {
                    tree.remove(rel_path);
                }
            }
        }

        for (rel_path, own_blob_id) in &meta.own_blobs {
            match disk_files.get(rel_path) {
                Some(disk_entry) if other_touched_paths.contains(rel_path) => {
                    tree.insert(
                        rel_path.clone(),
                        FileEntry::from_mode(*own_blob_id, disk_entry.entry_mode()),
                    );
                }
                Some(disk_entry) => {
                    tree.insert(rel_path.clone(), disk_entry.clone());
                }
                None => {
                    tree.remove(rel_path);
                }
            }
        }

        for rel_path in &meta.deleted_paths {
            tree.remove(rel_path);
        }

        tree
    }

    /// Returns repository-relative paths that differ between `repo_root` and the checked-out `HEAD`
    /// tree (content, mode, addition or deletion) but are authored by no session.
    ///
    /// # Errors
    /// Returns [`CoreError`] if `HEAD` or the `OpLog` cannot be read.
    pub fn unattributed_worktree_changes(&self) -> CoreResult<BTreeSet<String>> {
        let current = self.oplog.load_current_reconciled()?;
        let head_cid = self
            .git
            .resolve_head_commit()?
            .ok_or_else(|| CoreError::NotFound("HEAD commit".to_string()))?;
        let head_tree = self.git.read_commit(&head_cid)?.tree_id;
        Ok(self.unowned_worktree_changes(&current.view, &head_tree))
    }

    /// Returns repository-relative paths whose working-tree state in `repo_root` differs from
    /// `head_tree` and that no session in `view` authors.
    pub(crate) fn unowned_worktree_changes(
        &self,
        view: &View,
        head_tree: &ObjectId,
    ) -> BTreeSet<String> {
        let Ok(head_files) = self.git.read_tree_to_map(head_tree) else {
            return BTreeSet::new();
        };
        let Ok(snap) = snapshot_working_dir(
            &self.git,
            self.git.repo_root(),
            "worktree-baseline",
            &[],
            Some(head_tree),
        ) else {
            return BTreeSet::new();
        };

        let owned: BTreeSet<&String> = view.agents.values().flat_map(|a| a.owned_paths()).collect();
        let changed = snap
            .files
            .iter()
            .filter(|(p, e)| head_files.get(*p) != Some(*e))
            .map(|(p, _)| p)
            .chain(head_files.keys().filter(|p| !snap.files.contains_key(*p)));

        changed
            .filter(|p| !owned.contains(p) && !is_harness_config_path(p))
            .cloned()
            .collect()
    }

    /// Incrementally updates `agent_id`'s Git tree and commit for a single modified or deleted
    /// repository-relative path in O(depth) time without walking the working directory.
    ///
    /// Serialized under `<common_git_dir>/tessera/edit.lock` so parallel tool calls from one
    /// agent cannot clobber `head_tree`. Falls back to [`Self::snapshot_agent`] if `agent_id`
    /// uses an isolated Copy-on-Write workspace (`workspace_path.is_some()`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if updating the Git tree, commit, or `OpLog` fails.
    pub fn record_single_path_edit(
        &self,
        agent_id: &str,
        rel_path: &str,
        new_entry: Option<FileEntry>,
    ) -> CoreResult<AgentMetadata> {
        let _edit_lock =
            acquire_file_lock(&self.git.common_git_dir().join("tessera").join("edit.lock"))?;

        let current = self.oplog.load_current_reconciled()?;
        let meta = match current.view.agents.get(agent_id) {
            Some(a) => a.clone(),
            None => self.ensure_session(agent_id, None)?,
        };

        if meta.workspace_path.is_some() {
            drop(_edit_lock);
            return self.snapshot_agent(agent_id, true);
        }

        let norm_path = validate_repo_rel_path(rel_path)?;
        let base_commit_obj = self.git.read_commit(&meta.base_commit)?;
        let base_tree_for_update = if meta.head_commit != meta.base_commit
            && self
                .git
                .read_commit(&meta.head_commit)
                .ok()
                .is_some_and(|c| c.parents.first() == Some(&meta.base_commit))
        {
            meta.head_tree
        } else {
            base_commit_obj.tree_id
        };
        let exists_in_base = self
            .git
            .read_tree_entry_at_path(&base_commit_obj.tree_id, &norm_path)?
            .is_some();

        let head_branch = self.git.current_branch_name().unwrap_or_default();
        let targets_head = !head_branch.is_empty() && meta.targets_head_branch(&head_branch);
        let mut evicted_own_paths: BTreeSet<String> = BTreeSet::new();

        let mut updates: BTreeMap<String, Option<FileEntry>> = BTreeMap::new();

        for (own_path, own_blob_id) in &meta.own_blobs {
            if own_path == &norm_path {
                continue;
            }

            if targets_head && !self.git.repo_root().join(own_path).is_file() {
                evicted_own_paths.insert(own_path.clone());
                updates.insert(own_path.clone(), None);
                let branch_ws = self.branch_sandbox_dir(&head_branch);

                if branch_ws.is_dir() {
                    let _ = fs::remove_file(branch_ws.join(own_path));
                }

                continue;
            }

            let mode = match self
                .git
                .read_tree_entry_at_path(&meta.head_tree, own_path)?
                .or(self
                    .git
                    .read_tree_entry_at_path(&base_tree_for_update, own_path)?)
            {
                Some(entry) => entry.entry_mode(),
                None => self
                    .git
                    .read_tree_entry_at_path(&base_commit_obj.tree_id, own_path)?
                    .map_or(EntryMode::NormalFile, |e| e.entry_mode()),
            };
            updates.insert(
                own_path.clone(),
                Some(FileEntry::from_mode(*own_blob_id, mode)),
            );
        }

        for deleted_path in &meta.deleted_paths {
            if deleted_path != &norm_path {
                updates.insert(deleted_path.clone(), None);
            }
        }

        let mut evicted_in_base: BTreeSet<String> = BTreeSet::new();

        for ev_path in &evicted_own_paths {
            if self
                .git
                .read_tree_entry_at_path(&base_commit_obj.tree_id, ev_path)?
                .is_some()
            {
                evicted_in_base.insert(ev_path.clone());
            }
        }

        updates.insert(norm_path.clone(), new_entry.clone());
        let new_tree_id = self
            .git
            .update_tree_entries(&base_tree_for_update, &updates)?;

        let author = self.git.resolve_git_author();
        let new_commit = if new_tree_id == meta.head_tree {
            meta.head_commit
        } else {
            self.git.write_commit(
                &new_tree_id,
                std::slice::from_ref(&meta.base_commit),
                &author,
                &meta.intent,
            )?
        };

        let mut updated_meta = None;
        self.oplog.record_tx(
            agent_id,
            format!("snapshot {agent_id} -> {}", new_commit.short()),
            true,
            |view| {
                if let Some(agent) = view.agents.get_mut(agent_id) {
                    agent.head_commit = new_commit;
                    agent.head_tree = new_tree_id;

                    for ev_path in &evicted_own_paths {
                        agent.own_blobs.remove(ev_path);

                        if evicted_in_base.contains(ev_path) {
                            agent.deleted_paths.insert(ev_path.clone());
                        } else {
                            agent.deleted_paths.remove(ev_path);
                        }
                    }

                    match &new_entry {
                        Some(entry) => {
                            agent.deleted_paths.remove(&norm_path);
                            agent.own_blobs.insert(norm_path.clone(), entry.blob_id);
                        }
                        None => {
                            agent.own_blobs.remove(&norm_path);

                            if exists_in_base {
                                agent.deleted_paths.insert(norm_path.clone());
                            } else {
                                agent.deleted_paths.remove(&norm_path);
                            }
                        }
                    }

                    agent.updated_at_ms = now_millis();
                    view.changes.insert(agent.change_id.clone(), new_commit);
                    updated_meta = Some(agent.clone());
                }

                Ok(())
            },
        )?;

        let out = updated_meta.ok_or_else(|| CoreError::NotFound(agent_id.to_string()))?;
        self.git
            .update_ref(&format!("refs/tessera/agents/{agent_id}"), &out.head_commit)?;
        self.git.update_ref(
            &format!("refs/tessera/changes/{}", out.change_id.as_str()),
            &out.head_commit,
        )?;
        Ok(out)
    }

    /// Explicitly updates the `intent` and claimed AST/file symbols for `agent_id`, and
    /// automatically records pre-edit baselines (`read_blobs`) for any target file paths.
    ///
    /// # Errors
    /// Returns [`CoreError`] if updating the `OpLog` fails.
    pub fn claim_intent_and_symbols(
        &self,
        agent_id: &str,
        intent: Option<&str>,
        symbols: &[String],
    ) -> CoreResult<AgentMetadata> {
        self.claim_intent_and_symbols_with_branch(agent_id, intent, symbols, None)
    }

    /// Explicitly updates the `intent`, claimed AST/file symbols, and optional per-session
    /// `target_branch` (`--branch <name>`) for `agent_id`, recording pre-edit baselines (`read_blobs`).
    ///
    /// # Errors
    /// Returns [`CoreError`] if updating the `OpLog` fails.
    pub fn claim_intent_and_symbols_with_branch(
        &self,
        agent_id: &str,
        intent: Option<&str>,
        symbols: &[String],
        target_branch: Option<&str>,
    ) -> CoreResult<AgentMetadata> {
        self.claim_intent_and_symbols_with_branch_and_base(
            agent_id,
            intent,
            symbols,
            target_branch,
            None,
        )
    }

    /// Explicitly updates the `intent`, claimed AST/file symbols, optional per-session
    /// `target_branch` (`--branch <name>`), and optional parent revision `from_branch`
    /// (`--from <base>`) for `agent_id`, recording pre-edit baselines (`read_blobs`).
    ///
    /// Refuses to silently reset an existing branch when `from_branch` points to a different commit.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidArgument`] if `target_branch` already exists at a different
    /// commit than `from_branch`, or [`CoreError`] if arguments are invalid/missing or updating the `OpLog` fails.
    pub fn claim_intent_and_symbols_with_branch_and_base(
        &self,
        agent_id: &str,
        intent: Option<&str>,
        symbols: &[String],
        target_branch: Option<&str>,
        from_branch: Option<&str>,
    ) -> CoreResult<AgentMetadata> {
        let requested_branch = target_branch.map(str::trim).filter(|s| !s.is_empty());

        if let Some(b) = requested_branch {
            validate_ref_name(b)?;
        }

        let explicit_from_cid = match from_branch.map(str::trim).filter(|s| !s.is_empty()) {
            Some(base_spec) => Some(self.git.resolve_revision(base_spec)?),
            None => None,
        };

        let meta_now = self.ensure_session(agent_id, intent)?;
        let cfg = self.oplog.load_config()?;
        let head_branch = self.git.current_branch_name().unwrap_or_default();
        let pristine = !meta_now.has_own_changes() && meta_now.targets_head_branch(&head_branch);

        let head_cid = self.git.resolve_head_commit().ok().flatten();
        let head_tree = head_cid.and_then(|cid| self.git.read_commit(&cid).ok().map(|c| c.tree_id));
        let current = self.oplog.load_current_reconciled()?;
        let unowned_changes = match (&head_tree, pristine && is_ai_session_id(agent_id)) {
            (Some(tree), true) => self.unowned_worktree_changes(&current.view, tree),
            _ => BTreeSet::new(),
        };

        let auto_branch = if requested_branch.is_none()
            && cfg.auto_isolate_dirty_claims
            && pristine
            && is_ai_session_id(agent_id)
            && !head_branch.is_empty()
        {
            let mut dirty: BTreeSet<String> = unowned_changes.clone();

            for peer in current.view.agents.values() {
                if peer.agent_id != agent_id && peer.targets_head_branch(&head_branch) {
                    dirty.extend(peer.owned_paths().cloned());
                }
            }

            let claimed_targets: Vec<&str> = symbols
                .iter()
                .map(|s| {
                    s.split("::")
                        .next()
                        .unwrap_or(s)
                        .trim()
                        .trim_end_matches('/')
                })
                .filter(|s| !s.is_empty())
                .collect();
            let overlaps = dirty.iter().any(|d| {
                claimed_targets
                    .iter()
                    .any(|c| d == c || d.starts_with(&format!("{c}/")))
            });
            let candidate = format!("agent/{agent_id}");
            (overlaps && validate_ref_name(&candidate).is_ok()).then_some(candidate)
        } else {
            None
        };

        let clean_branch: Option<&str> = requested_branch.or(auto_branch.as_deref());
        let branch_commit_opt = if let Some(b) = clean_branch {
            let existing_ref = self
                .git
                .resolve_ref(&format!("refs/heads/{b}"))
                .ok()
                .flatten();
            let resolved_cid = match (existing_ref, explicit_from_cid) {
                (Some(cid), Some(fcid)) if cid != fcid => {
                    return Err(CoreError::InvalidArgument(format!(
                        "branch '{b}' already exists at {} (cannot reset it to {} via --from; delete or use `tes restore` first)",
                        cid.short(),
                        fcid.short()
                    )));
                }
                (Some(cid), _) => Some(cid),
                (None, Some(fcid)) => {
                    let _ = self.git.update_ref(&format!("refs/heads/{b}"), &fcid);
                    Some(fcid)
                }
                (None, None) => {
                    let remote_ref = self
                        .git
                        .resolve_ref(&format!("refs/remotes/origin/{b}"))
                        .ok()
                        .flatten();
                    let session_prev_cid = meta_now
                        .target_branch
                        .as_deref()
                        .filter(|prev_b| *prev_b != b && *prev_b != cfg.trunk_branch)
                        .and_then(|prev_b| {
                            self.git
                                .resolve_ref(&format!("refs/heads/{prev_b}"))
                                .ok()
                                .flatten()
                                .or(Some(meta_now.base_commit))
                        });
                    let fork_cid = if auto_branch.is_some() {
                        head_cid
                    } else if remote_ref.is_some() {
                        remote_ref
                    } else if session_prev_cid.is_some() {
                        session_prev_cid
                    } else {
                        self.git
                            .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))
                            .ok()
                            .flatten()
                            .or(head_cid)
                    };

                    if let Some(tcid) = &fork_cid {
                        let _ = self.git.update_ref(&format!("refs/heads/{b}"), tcid);
                    }

                    fork_cid
                }
            };
            resolved_cid.and_then(|cid| self.git.read_commit(&cid).ok().map(|c| (cid, c.tree_id)))
        } else {
            None
        };

        self.oplog.record_tx(
            agent_id,
            format!("claim intent/symbols for {agent_id}"),
            false,
            |view| {
                if let Some((b, (b_cid, _))) = clean_branch.zip(branch_commit_opt.as_ref()) {
                    view.branches.entry(b.to_string()).or_insert_with(|| *b_cid);
                }

                if let Some(agent) = view.agents.get_mut(agent_id) {
                    if let Some(i) = intent {
                        if !i.trim().is_empty() {
                            agent.intent = i.trim().to_string();
                            agent.intent_inferred = false;
                        }
                    }

                    if pristine && is_ai_session_id(agent_id) {
                        agent.foreign_baseline = unowned_changes.clone();
                    }

                    if let Some(b) = clean_branch {
                        agent.target_branch = Some(b.to_string());

                        if let Some((b_cid, b_tid)) = &branch_commit_opt {
                            if !agent.has_own_changes() {
                                agent.base_commit = *b_cid;
                                agent.head_commit = *b_cid;
                                agent.head_tree = *b_tid;

                                if explicit_from_cid.is_some() {
                                    agent.read_blobs.clear();
                                    agent.integrated_peer_blobs.clear();
                                }
                            }
                        }
                    }

                    for s in symbols {
                        agent.claimed_symbols.insert(s.clone());
                    }

                    agent.updated_at_ms = now_millis();
                }

                if let Some(b) = clean_branch {
                    let b_slot = self.branch_slot(view, b, &cfg.trunk_branch);

                    if b != cfg.trunk_branch {
                        for a in view.agents.values_mut() {
                            if a.target_branch.as_deref() == Some(b) {
                                a.slot = b_slot;
                            }
                        }
                    } else if let Some(agent) = view.agents.get_mut(agent_id) {
                        agent.slot = b_slot;
                    }
                }

                Ok(())
            },
        )?;

        if clean_branch.is_some() {
            let _ = self.prepare_branch_sandbox(agent_id, clean_branch);
        }

        for sym in symbols {
            let file_part = sym.split("::").next().unwrap_or(sym.as_str()).trim();
            let exists_on_disk_or_branch = self.git.repo_root().join(file_part).is_file()
                || branch_commit_opt.as_ref().is_some_and(|(_, b_tid)| {
                    self.git
                        .read_tree_entry_at_path(b_tid, file_part)
                        .ok()
                        .flatten()
                        .is_some()
                });

            if !file_part.is_empty() && exists_on_disk_or_branch {
                let _ = handle_pre_tool_hook(self, agent_id, file_part);
            }
        }

        let refreshed = self.oplog.load_current_reconciled()?;
        refreshed
            .view
            .agents
            .get(agent_id)
            .cloned()
            .ok_or_else(|| CoreError::NotFound(agent_id.to_string()))
    }

    /// Saves a named checkpoint in the `OpLog` for `agent_id` so the agent can instantly roll back
    /// before a risky multi-file refactor.
    ///
    /// # Errors
    /// Returns [`CoreError`] if snapshotting or recording the checkpoint fails.
    pub fn create_checkpoint(&self, agent_id: &str, label: &str) -> CoreResult<OpId> {
        let _ = self.snapshot_agent(agent_id, false)?;
        let current = self.oplog.load_current_reconciled()?;
        let cp_id = current.id.clone();

        self.oplog.record_tx(
            agent_id,
            format!("checkpoint '{label}' for {agent_id}"),
            false,
            |view| {
                if let Some(agent) = view.agents.get_mut(agent_id) {
                    agent.checkpoints.insert(label.to_string(), cp_id.clone());
                }

                Ok(())
            },
        )?;
        Ok(cp_id)
    }

    /// Executes a command inside `agent_id`'s workspace with deterministic runtime environment
    /// isolation (`PORT`, `TES_DB_NAME`, `TMPDIR`), snapshotting before and after execution.
    ///
    /// # Errors
    /// Returns [`CoreError`] if snapshotting or spawning the child process fails.
    pub fn exec_in_agent(&self, agent_id: &str, command_args: &[String]) -> CoreResult<i32> {
        if command_args.is_empty() {
            return Err(CoreError::InvalidArgument(
                "empty command arguments passed to exec".to_string(),
            ));
        }

        let cfg = self.oplog.load_config()?;
        let meta = self.snapshot_agent(agent_id, true)?;
        let work_dir = meta
            .workspace_path
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.git.repo_root().to_path_buf());

        let env_map = build_agent_env_with_config(&meta, &cfg, &work_dir);

        if let Some(tmp_dir) = env_map.get("TMPDIR") {
            let _ = fs::create_dir_all(tmp_dir);
        }

        let status = Command::new(&command_args[0])
            .args(&command_args[1..])
            .current_dir(&work_dir)
            .envs(&env_map)
            .status()
            .map_err(|source| CoreError::Io {
                path: command_args[0].clone(),
                source,
            })?;

        let _ = self.snapshot_agent(agent_id, false)?;
        Ok(status.code().unwrap_or(1))
    }

    /// Computes a live preview of the `ast-union` resolution for `record` so visual IDE tools
    /// can display the combined AST output before the user accepts it.
    #[must_use]
    pub fn compute_ast_union_preview(&self, record: &ConflictRecord) -> String {
        let base_bytes = match record.merge.removes.first().and_then(Option::as_ref) {
            Some(bid) => self.git.read_blob(bid).unwrap_or_default(),
            None => Vec::new(),
        };
        let ours_bytes = match record.merge.adds.first().and_then(Option::as_ref) {
            Some(bid) => self.git.read_blob(bid).unwrap_or_default(),
            None => record.left_snippet.as_bytes().to_vec(),
        };
        let theirs_bytes = match record.merge.adds.get(1).and_then(Option::as_ref) {
            Some(bid) => self.git.read_blob(bid).unwrap_or_default(),
            None => record.right_snippet.as_bytes().to_vec(),
        };
        merge_ast_union(
            &record.path,
            &String::from_utf8_lossy(&base_bytes),
            &String::from_utf8_lossy(&ours_bytes),
            &String::from_utf8_lossy(&theirs_bytes),
        )
    }

    /// Computes the live **Symbol Collision Radar** across all active agents, surfacing any
    /// overlapping AST symbols, claimed contracts, or touched files along with each agent's intent.
    ///
    /// # Errors
    /// Returns [`CoreError`] if reading the `OpLog` or conflict records fails.
    pub fn radar(&self) -> CoreResult<RadarReport> {
        let cfg = self.oplog.load_config()?;
        let default_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| cfg.trunk_branch.clone());
        let current = self.oplog.load_current_reconciled()?;

        let mut symbol_to_agents: BTreeMap<(String, String), BTreeMap<String, String>> =
            BTreeMap::new();
        let mut active_agents = Vec::new();
        let mut file_attributions = Vec::new();
        let mut working_cache: BTreeMap<String, Option<String>> = BTreeMap::new();

        let mut read_working_file = |rel_path: &str| -> Option<String> {
            if let Some(cached) = working_cache.get(rel_path) {
                return cached.clone();
            }

            let full_path = self.git.repo_root().join(rel_path);
            let loaded = fs::read(&full_path)
                .ok()
                .filter(|bytes| !is_binary_bytes(bytes))
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            working_cache.insert(rel_path.to_string(), loaded.clone());
            loaded
        };

        for agent in current.view.agents.values() {
            if agent.agent_id == "editor" && !agent.has_own_changes() {
                continue;
            }

            let is_idle_shell = (agent.agent_id.starts_with("term-")
                || agent.agent_id.starts_with("session-"))
                && agent.intent_inferred
                && agent.claimed_symbols.is_empty()
                && !agent.has_own_changes()
                && agent.head_commit == agent.base_commit;

            if is_idle_shell && agent.target_branch.is_none() {
                continue;
            }

            active_agents.push(agent.clone());

            if is_idle_shell {
                continue;
            }

            let eff_branch = agent
                .target_branch
                .as_deref()
                .unwrap_or(default_branch.as_str())
                .to_string();

            for sym in &agent.claimed_symbols {
                symbol_to_agents
                    .entry((eff_branch.clone(), sym.clone()))
                    .or_default()
                    .insert(agent.agent_id.clone(), agent.intent.clone());
            }

            for path in agent.read_blobs.keys().chain(agent.own_blobs.keys()) {
                symbol_to_agents
                    .entry((eff_branch.clone(), path.clone()))
                    .or_default()
                    .insert(agent.agent_id.clone(), agent.intent.clone());
            }

            if eff_branch != default_branch {
                continue;
            }

            let base_tree = self
                .git
                .read_commit(&agent.base_commit)
                .ok()
                .and_then(|c| self.git.read_tree_to_map(&c.tree_id).ok())
                .unwrap_or_default();

            for (rel_path, own_blob_id) in &agent.own_blobs {
                let Some(working_str) = read_working_file(rel_path) else {
                    continue;
                };

                let base_bytes = base_tree
                    .get(rel_path)
                    .and_then(|entry| self.git.read_blob(&entry.blob_id).ok())
                    .unwrap_or_default();

                let Ok(own_bytes) = self.git.read_blob(own_blob_id) else {
                    continue;
                };

                if is_binary_bytes(&base_bytes) || is_binary_bytes(&own_bytes) {
                    continue;
                }

                let base_str = String::from_utf8_lossy(&base_bytes);
                let own_str = String::from_utf8_lossy(&own_bytes);

                if own_str == base_str {
                    continue;
                }

                let hunks = attribute_agent_hunks(&base_str, &own_str, &working_str);

                for (start_line, end_line, in_working_tree) in hunks {
                    if !in_working_tree {
                        continue;
                    }

                    file_attributions.push(FileLineAttribution {
                        path: rel_path.clone(),
                        agent_id: agent.agent_id.clone(),
                        branch: eff_branch.clone(),
                        intent: agent.intent.clone(),
                        slot: agent.slot,
                        start_line,
                        end_line,
                        in_working_tree: true,
                        symbol: None,
                    });
                }
            }

            for sym in &agent.claimed_symbols {
                let (rel_path, sym_name_opt) = match sym.split_once("::") {
                    Some((p, s)) => (p.trim(), Some(s.trim())),
                    None => (sym.trim(), None),
                };

                if agent.own_blobs.contains_key(rel_path) {
                    continue;
                }

                let Some(working_str) = read_working_file(rel_path) else {
                    continue;
                };

                let line_no = sym_name_opt
                    .and_then(|sym_name| {
                        working_str
                            .lines()
                            .enumerate()
                            .find(|(_, line)| line.contains(sym_name))
                            .map(|(idx, _)| idx + 1)
                    })
                    .unwrap_or(1);

                file_attributions.push(FileLineAttribution {
                    path: rel_path.to_string(),
                    agent_id: agent.agent_id.clone(),
                    branch: eff_branch.clone(),
                    intent: agent.intent.clone(),
                    slot: agent.slot,
                    start_line: line_no,
                    end_line: line_no,
                    in_working_tree: true,
                    symbol: sym_name_opt.map(str::to_string),
                });
            }
        }

        let mut collisions = Vec::new();

        for ((_, target), agents_map) in symbol_to_agents {
            if agents_map.len() > 1 {
                collisions.push(SymbolCollision {
                    target,
                    competing_agents: agents_map,
                });
            }
        }

        let mut active_conflicts = Vec::new();

        for cid in current.view.conflicts.values() {
            if let Ok(rec) = self.oplog.load_conflict(cid) {
                active_conflicts.push(rec);
            }
        }

        Ok(RadarReport {
            active_agents,
            collisions,
            active_conflicts,
            file_attributions,
        })
    }

    /// Closes and cleans up an active agent session (`agent_id`), removing its isolated COW workspace
    /// (`.tessera/workspaces/<agent_id>`) if present, deleting `refs/tessera/agents/<agent_id>`, and
    /// removing the session from the active `View`.
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if `agent_id` is not an active session.
    pub fn close_agent(&self, agent_id: &str) -> CoreResult<AgentMetadata> {
        let mut removed_meta: Option<AgentMetadata> = None;
        self.oplog.record_tx(
            "agent-close",
            format!("close agent session {agent_id}"),
            false,
            |view| {
                removed_meta = view.agents.remove(agent_id);
                Ok(())
            },
        )?;

        let meta = removed_meta
            .ok_or_else(|| CoreError::NotFound(format!("agent session '{agent_id}'")))?;
        self.git
            .delete_ref(&format!("refs/tessera/agents/{agent_id}"));

        if let Some(ws_path) = &meta.workspace_path {
            let p = Path::new(ws_path);

            if p.exists() {
                let _ = fs::remove_dir_all(p);
            }
        }

        Ok(meta)
    }

    /// Clears active agent and terminal sessions from `View::agents`.
    ///
    /// When `include_dirty` is `false`, only sessions with no uncommitted `own_blobs` are removed.
    /// When `include_dirty` is `true`, all sessions in `View::agents` are closed and their refs
    /// (`refs/tessera/agents/<id>`) and dedicated agent COW workspaces are cleaned up.
    ///
    /// # Errors
    /// Returns [`CoreError`] if recording the transaction in the `OpLog` fails.
    pub fn clear_agents(&self, include_dirty: bool) -> CoreResult<Vec<String>> {
        let mut removed = Vec::new();
        let mut removed_workspaces = Vec::new();

        self.oplog.record_tx(
            "agent-clear",
            if include_dirty {
                "clear all agent sessions".to_string()
            } else {
                "clear idle agent sessions".to_string()
            },
            false,
            |view| {
                let to_remove: Vec<String> = view
                    .agents
                    .iter()
                    .filter(|(_, a)| include_dirty || !a.has_own_changes())
                    .map(|(k, _)| k.clone())
                    .collect();

                for id in to_remove {
                    if let Some(meta) = view.agents.remove(&id) {
                        if let Some(ws) = meta.workspace_path {
                            removed_workspaces.push(ws);
                        }

                        removed.push(id);
                    }
                }

                if include_dirty {
                    view.observed_blobs.clear();
                }

                Ok(())
            },
        )?;

        for id in &removed {
            self.git.delete_ref(&format!("refs/tessera/agents/{id}"));
        }

        for ws_path in &removed_workspaces {
            let p = Path::new(ws_path);

            if p.exists() {
                let _ = fs::remove_dir_all(p);
            }
        }

        Ok(removed)
    }

    /// Reconciles any in-flight files written to `repo_root` by active sessions before switching
    /// branches or materializing branch sandboxes.
    pub fn reconcile_in_flight_sessions(&self) {
        let Ok(current) = self.oplog.load_current_reconciled() else {
            return;
        };

        let mut active_ids: Vec<(u8, u64, String)> = current
            .view
            .agents
            .values()
            .filter(|a| {
                a.agent_id != "editor"
                    && (!a.intent_inferred
                        || !a.claimed_symbols.is_empty()
                        || a.has_own_changes()
                        || !a.read_blobs.is_empty())
            })
            .map(|a| {
                let priority = if !a.claimed_symbols.is_empty() || a.has_own_changes() {
                    0
                } else if !a.read_blobs.is_empty() {
                    1
                } else {
                    2
                };
                (priority, a.updated_at_ms, a.agent_id.clone())
            })
            .collect();

        active_ids.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));

        for (_, _, aid) in active_ids {
            let _ = self.watch_tick(&aid);
        }
    }

    /// Scans the repository working tree for files modified since `session_id`'s last recorded
    /// baseline (`read_blobs` or `base_commit`) and reconciles each modified file through
    /// [`handle_post_tool_hook`].
    ///
    /// # Errors
    /// Returns [`CoreError`] if snapshotting or hook reconciliation fails.
    pub fn watch_tick(&self, session_id: &str) -> CoreResult<Vec<PostToolOutcome>> {
        if session_id == "editor" {
            return Ok(Vec::new());
        }

        let meta = self.ensure_session(session_id, None)?;
        let current = self.oplog.load_current_reconciled()?;

        let is_unclaimed_session = meta.intent_inferred
            && meta.claimed_symbols.is_empty()
            && !meta.has_own_changes()
            && meta.read_blobs.is_empty();
        let has_active_peer = current.view.agents.values().any(|a| {
            a.agent_id != session_id
                && (!a.intent_inferred
                    || !a.claimed_symbols.is_empty()
                    || a.has_own_changes()
                    || !a.read_blobs.is_empty())
        });

        if is_unclaimed_session && has_active_peer {
            return Ok(Vec::new());
        }

        let current_head_branch = self
            .git
            .current_branch_name()
            .unwrap_or_else(|_| "main".to_string());
        let eff_branch = meta.effective_branch(&current_head_branch);

        let base_commit = self.git.read_commit(&meta.base_commit)?;
        let base_files = self.git.read_tree_to_map(&base_commit.tree_id)?;

        let mut committed_branch_trees: Vec<BTreeMap<String, FileEntry>> = Vec::new();
        let mut seen_branch_cids: BTreeSet<ObjectId> = BTreeSet::new();
        seen_branch_cids.insert(meta.base_commit);

        for b_cid in current.view.branches.values() {
            if seen_branch_cids.insert(*b_cid) {
                if let Ok(bc) = self.git.read_commit(b_cid) {
                    if let Ok(bm) = self.git.read_tree_to_map(&bc.tree_id) {
                        committed_branch_trees.push(bm);
                    }
                }
            }
        }

        for local_br in self.git.list_local_branches() {
            if let Ok(Some(b_cid)) = self.git.resolve_ref(&format!("refs/heads/{local_br}")) {
                if seen_branch_cids.insert(b_cid) {
                    if let Ok(bc) = self.git.read_commit(&b_cid) {
                        if let Ok(bm) = self.git.read_tree_to_map(&bc.tree_id) {
                            committed_branch_trees.push(bm);
                        }
                    }
                }
            }
        }

        if let Ok(Some(head_cid)) = self.git.resolve_head_commit() {
            if seen_branch_cids.insert(head_cid) {
                if let Ok(hc) = self.git.read_commit(&head_cid) {
                    if let Ok(hm) = self.git.read_tree_to_map(&hc.tree_id) {
                        committed_branch_trees.push(hm);
                    }
                }
            }
        }

        if let Some(inv_dir) = self.invocation_dir() {
            if inv_dir != self.git.repo_root() && inv_dir.is_dir() {
                let inv_key = format!("{session_id}-inv");

                if let Ok(inv_snap) = snapshot_working_dir(
                    &self.git,
                    inv_dir,
                    &inv_key,
                    &meta.scopes,
                    Some(&base_commit.tree_id),
                ) {
                    for (rel_path, entry) in &inv_snap.files {
                        let is_claimed_by_me = session_claims_path(&meta, rel_path, &base_files);
                        let is_owned_by_other = current.view.agents.values().any(|a| {
                            a.agent_id != session_id && a.own_blobs.contains_key(rel_path)
                        });

                        if (is_claimed_by_me || !is_owned_by_other)
                            && base_files.get(rel_path).map(|e| &e.blob_id) != Some(&entry.blob_id)
                        {
                            let inv_file = inv_dir.join(rel_path);
                            let repo_file = self.git.repo_root().join(rel_path);

                            if let Ok(bytes) = fs::read(&inv_file) {
                                write_worktree_file(&repo_file, &bytes, entry.executable);
                            }
                        }
                    }
                }
            }
        }

        let snap = snapshot_working_dir(
            &self.git,
            self.git.repo_root(),
            session_id,
            &meta.scopes,
            Some(&base_commit.tree_id),
        )?;

        let mut outcomes = Vec::new();

        for (rel_path, entry) in &snap.files {
            let claimed_by_me = session_claims_path(&meta, rel_path, &base_files);
            let has_unreconciled_observed_blob = !meta.own_blobs.contains_key(rel_path)
                && claimed_by_me
                && current
                    .view
                    .observed_blobs
                    .get(rel_path)
                    .is_some_and(|obs_list| {
                        obs_list.iter().any(|obs_b| {
                            base_files.get(rel_path).map(|e| &e.blob_id) != Some(obs_b)
                                && meta.read_blobs.get(rel_path) != Some(obs_b)
                                && !committed_branch_trees
                                    .iter()
                                    .any(|bm| bm.get(rel_path).map(|e| &e.blob_id) == Some(obs_b))
                                && !current
                                    .view
                                    .agents
                                    .values()
                                    .any(|a| a.own_blobs.get(rel_path) == Some(obs_b))
                        })
                    });

            if !has_unreconciled_observed_blob {
                if base_files.get(rel_path).map(|e| &e.blob_id) == Some(&entry.blob_id) {
                    continue;
                }

                if meta.own_blobs.get(rel_path) == Some(&entry.blob_id) {
                    continue;
                }

                let baseline_blob = meta
                    .read_blobs
                    .get(rel_path)
                    .or_else(|| base_files.get(rel_path).map(|e| &e.blob_id));

                if baseline_blob == Some(&entry.blob_id) {
                    continue;
                }

                if committed_branch_trees
                    .iter()
                    .any(|bm| bm.get(rel_path).map(|e| &e.blob_id) == Some(&entry.blob_id))
                {
                    continue;
                }

                let already_reconciled = current.view.agents.values().any(|a| {
                    a.own_blobs.get(rel_path) == Some(&entry.blob_id)
                        || (a.owns_path(rel_path)
                            && a.read_blobs.get(rel_path) == Some(&entry.blob_id))
                });

                if already_reconciled {
                    continue;
                }
            }

            let claimed_by_other =
                current.view.agents.values().any(|a| {
                    a.agent_id != session_id && session_claims_path(a, rel_path, &base_files)
                });
            let other_claimer_missing_own_blob = current.view.agents.values().any(|a| {
                a.agent_id != session_id
                    && !a.own_blobs.contains_key(rel_path)
                    && session_claims_path(a, rel_path, &base_files)
            });

            if claimed_by_me
                && claimed_by_other
                && meta.own_blobs.contains_key(rel_path)
                && other_claimer_missing_own_blob
            {
                continue;
            }

            if !claimed_by_me
                && (claimed_by_other
                    || eff_branch != current_head_branch
                    || is_harness_config_path(rel_path))
            {
                continue;
            }

            if !session_may_sweep(&meta, rel_path, &base_files) {
                continue;
            }

            let outcome = handle_post_tool_hook(self, session_id, rel_path)?;
            outcomes.push(outcome);
        }

        Ok(outcomes)
    }

    /// Resolves `session_id` (matching by `agent_id`, untruncated `waypoint.conversation_id`, or
    /// unique prefix/substring) and brings its terminal tab, multiplexer pane, or IDE window to
    /// the foreground.
    ///
    /// # Errors
    /// Returns [`CoreError::NotFound`] if no active session matches `session_id`, or
    /// [`CoreError::InvalidArgument`] if `session_id` is empty or matches multiple sessions ambiguously.
    pub fn focus_session(&self, session_id: &str) -> CoreResult<FocusSessionReport> {
        let query = session_id.trim();

        if query.is_empty() {
            return Err(CoreError::InvalidArgument(
                "session ID cannot be empty".to_string(),
            ));
        }

        let current = self.oplog.load_current_reconciled()?;

        if let Some(exact) = current.view.agents.get(query) {
            return focus_session_waypoint(exact);
        }

        if let Some(by_conv) = current.view.agents.values().find(|a| {
            a.waypoint
                .as_ref()
                .and_then(|w| w.conversation_id.as_deref())
                .is_some_and(|c| c == query)
        }) {
            return focus_session_waypoint(by_conv);
        }

        let matches: Vec<&AgentMetadata> = current
            .view
            .agents
            .values()
            .filter(|a| {
                a.agent_id.starts_with(query)
                    || a.agent_id.contains(query)
                    || a.waypoint
                        .as_ref()
                        .and_then(|w| w.conversation_id.as_deref())
                        .is_some_and(|c| c.starts_with(query))
            })
            .collect();

        match matches.len() {
            0 => Err(CoreError::NotFound(format!("agent session '{query}'"))),
            1 => focus_session_waypoint(matches[0]),
            n => {
                let ids: Vec<&str> = matches.iter().map(|a| a.agent_id.as_str()).collect();
                Err(CoreError::InvalidArgument(format!(
                    "ambiguous session query '{query}' matches {n} active sessions ({}); provide a more specific ID",
                    ids.join(", ")
                )))
            }
        }
    }
}
