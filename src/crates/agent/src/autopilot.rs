//! Zero-Command Harness Auto-Pilot, session detection, harness configuration installer,
//! pre/post-tool lifecycle hooks, and background micro-watcher daemon.

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use core::{
    atomic_write_file, is_valid_sandbox_repo_root, now_millis, resolve_confined_repo_path,
    AgentMetadata, ConflictKind, ConflictRecord, CoreError, CoreResult, FileEntry, Merge, ObjectId,
    SessionWaypoint,
};
use imara_diff::{Algorithm, Diff, InternedInput};
use merge::{diff_modified_symbols, merge_file_3way};
use serde::{Deserialize, Serialize};
use workspace::is_ignored_or_secret_path;

use crate::Coordinator;

/// Maximum number of intermediate observed blobs retained per file path in the `OpLog` `View`.
const MAX_OBSERVED_BLOBS_PER_PATH: usize = 64;

/// Default polling interval in milliseconds for the background micro-watcher daemon.
const DEFAULT_WATCH_INTERVAL_MS: u64 = 75;

/// Outcome of `tes hook pre-tool` before an agent reads or edits a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreToolOutcome {
    /// Detected or explicit session ID.
    pub session_id: String,
    /// Repository-relative file path inspected.
    pub path: String,
    /// Optional warning if another active agent session is concurrently editing `path`.
    pub radar_warning: Option<String>,
}

/// Outcome of `tes hook post-tool` after an agent edits a file in the shared working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostToolOutcome {
    /// Detected or explicit session ID.
    pub session_id: String,
    /// Repository-relative file path reconciled.
    pub path: String,
    /// `true` if another session concurrently modified `path` and Tessera performed an automatic 3-way AST merge on disk.
    pub auto_merged_concurrent_edit: bool,
    /// `true` if the concurrent edit had an overlapping AST node conflict recorded in `.git/tessera/conflicts/`.
    pub has_conflict: bool,
    /// Notice message injected back into the AI harness context window.
    pub context_notice: Option<String>,
    /// Current intent string for this session (captured from prompt or inferred from AST).
    pub session_intent: String,
}

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

/// Automatically detects the current AI agent session identifier without requiring manual
/// `tes agent spawn` invocations.
///
/// Checks harness environment variables in priority order (`TES_SESSION_ID`,
/// `ANTIGRAVITY_CONVERSATION_ID`, `CLAUDE_SESSION_ID`, `CURSOR_TRACE_ID`, `WINDSURF_SESSION_ID`,
/// `KIRO_SESSION_ID`, `OPENCODE_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_SESSION_ID`,
/// `TERM_SESSION_ID`) and falls back to the ancestor harness or parent process ID (`getppid`).
pub fn detect_session_id(explicit: Option<&str>) -> String {
    if let Some(exp) = explicit {
        let trimmed = exp.trim();
        if !trimmed.is_empty() {
            return sanitize_id(trimmed);
        }
    }

    // SAFETY: `libc::getppid()` is a pure POSIX syscall with no arguments or memory dereferences.
    #[cfg(unix)]
    let ppid = unsafe { libc::getppid() };
    #[cfg(not(unix))]
    let ppid = i32::try_from(std::process::id()).unwrap_or(1);

    let has_harness_env = std::env::var_os("ANTIGRAVITY_AGENT").is_some()
        || std::env::var_os("ANTIGRAVITY_CONVERSATION_ID").is_some()
        || std::env::var_os("CLAUDECODE").is_some()
        || std::env::var_os("CLAUDE_SESSION_ID").is_some()
        || std::env::var_os("CODEX_THREAD_ID").is_some()
        || std::env::var_os("CODEX_SESSION_ID").is_some()
        || std::env::var_os("OPENAI_CODEX_SESSION_ID").is_some()
        || std::env::var_os("CODEX_SANDBOX").is_some()
        || std::env::var_os("CODEX_CI").is_some()
        || std::env::var_os("OPENCODE_SESSION_ID").is_some()
        || std::env::var_os("CURSOR_TRACE_ID").is_some()
        || std::env::var_os("CURSOR_SESSION_ID").is_some()
        || std::env::var_os("WINDSURF_SESSION_ID").is_some()
        || std::env::var_os("CODEIUM_SESSION_ID").is_some()
        || std::env::var_os("KIRO_SESSION_ID").is_some()
        || std::env::var_os("COPILOT_SESSION_ID").is_some()
        || std::env::var_os("GEMINI_CLI_SESSION_ID").is_some()
        || std::env::var_os("GEMINI_SESSION_ID").is_some()
        || std::env::var_os("AIDER_SESSION_ID").is_some()
        || std::env::var_os("AMP_SESSION_ID").is_some()
        || std::env::var_os("GOOSE_SESSION_ID").is_some()
        || std::env::var_os("AUGMENT_SESSION_ID").is_some();

    if let Ok(val) = std::env::var("TES_SESSION_ID") {
        let trimmed = val.trim();
        if !trimmed.is_empty() && !trimmed.starts_with("term-") {
            let short = &trimmed[..32.min(trimmed.len())];
            return sanitize_id(short);
        }

        if !trimmed.is_empty() && !has_harness_env && trimmed == format!("term-{ppid}") {
            let short = &trimmed[..32.min(trimmed.len())];
            return sanitize_id(short);
        }
    }

    for (env_key, prefix) in [
        ("ANTIGRAVITY_CONVERSATION_ID", "antigravity-"),
        ("CLAUDE_SESSION_ID", "claude-"),
        ("CURSOR_TRACE_ID", "cursor-"),
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
    ] {
        if let Ok(val) = std::env::var(env_key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                let short = &trimmed[..12.min(trimmed.len())];
                return sanitize_id(&format!("{prefix}{short}"));
            }
        }
    }

    let ancestor_harness = detect_ancestor_harness_process(ppid);

    if let Ok(val) = std::env::var("TES_SESSION_ID") {
        let trimmed = val.trim();
        if !trimmed.is_empty() && !has_harness_env && ancestor_harness.is_none() {
            let short = &trimmed[..32.min(trimmed.len())];
            return sanitize_id(short);
        }
    }

    if let Some((harness_prefix, harness_pid)) = ancestor_harness {
        return format!("{harness_prefix}-{harness_pid}");
    }

    let stable_pid = find_stable_session_ancestor_pid(ppid);

    if std::env::var_os("CODEX_SANDBOX").is_some() || std::env::var_os("CODEX_CI").is_some() {
        return format!("codex-{stable_pid}");
    }

    if std::env::var_os("CLAUDECODE").is_some() {
        return format!("claude-{stable_pid}");
    }

    if std::env::var_os("ANTIGRAVITY_AGENT").is_some() {
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
        if let Ok(val) = std::env::var(env_key) {
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
                let short = &effective[..max_len.min(effective.len())];
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

        if let Ok(out) = std::process::Command::new("ps")
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
fn is_shell_or_cli_harness_process(comm: &str) -> bool {
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
        // Fast path on Linux / NetBSD procfs: read `/proc/<pid>/stat` without spawning a subprocess.
        if let Ok(stat_line) = fs::read_to_string(format!("/proc/{pid}/stat")) {
            if let (Some(open_paren), Some(close_paren)) =
                (stat_line.find('('), stat_line.rfind(')'))
            {
                if close_paren > open_paren {
                    let comm = stat_line[open_paren + 1..close_paren].trim().to_string();
                    let after = stat_line[close_paren + 1..].trim();
                    // Fields after `(comm)` in `/proc/<pid>/stat`: `state ppid pgrp session ...`
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

        // Portable POSIX / BSD `ps`: try `ppid=,comm=` (macOS, FreeBSD, NetBSD, Linux) and fall back to
        // `ppid=,ucomm=` (OpenBSD, where `ucomm` is the short command keyword).
        for format_arg in ["ppid=,comm=", "ppid=,ucomm="] {
            let Ok(out) = std::process::Command::new("ps")
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

/// Normalizes a raw session identifier into lowercase ASCII alphanumeric, `-`, and `_` characters.
fn sanitize_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Captures the navigation waypoint ([`SessionWaypoint`]) for the current agent or terminal process.
///
/// Inspects harness environment variables (`ANTIGRAVITY_CONVERSATION_ID`, `CLAUDE_SESSION_ID`,
/// `CURSOR_TRACE_ID`, `WINDSURF_SESSION_ID`, `KIRO_SESSION_ID`, `OPENCODE_SESSION_ID`,
/// `CODEX_THREAD_ID`, `CODEX_SESSION_ID`, `COPILOT_SESSION_ID`), terminal/multiplexer variables
/// (`TERM_PROGRAM`, `TERM_SESSION_ID`, `ITERM_SESSION_ID`, `TMUX_PANE`, `WEZTERM_PANE`,
/// `KITTY_WINDOW_ID`, `ZELLIJ_PANE_ID`, `GHOSTTY_SURFACE_ID`, `WT_SESSION`), and the process tree
/// (`shell_pid`, controlling `tty`, and ancestor `ide_pid`).
pub fn detect_session_waypoint(session_id: &str) -> SessionWaypoint {
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
        ("CURSOR_TRACE_ID", "cursor"),
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
    ] {
        if prefix_harness.is_some_and(|ph| ph != harness_name) {
            continue;
        }

        if let Ok(val) = std::env::var(env_key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                harness = Some(harness_name.to_string());
                conversation_id = Some(trimmed.to_string());
                break;
            }
        }
    }

    if harness.is_none() {
        if std::env::var_os("ANTIGRAVITY_AGENT").is_some() {
            harness = Some("antigravity".to_string());
        } else if std::env::var_os("CLAUDECODE").is_some() {
            harness = Some("claude-code".to_string());
        } else if std::env::var_os("CODEX_SANDBOX").is_some()
            || std::env::var_os("CODEX_CI").is_some()
        {
            harness = Some("codex".to_string());
        }
    }

    let mut term_program = std::env::var("TERM_PROGRAM")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if term_program.is_none() {
        for (env_key, prog_name) in [
            ("TMUX_PANE", "tmux"),
            ("WEZTERM_PANE", "WezTerm"),
            ("KITTY_WINDOW_ID", "kitty"),
            ("ZELLIJ_PANE_ID", "zellij"),
            ("GHOSTTY_SURFACE_ID", "ghostty"),
            ("ITERM_SESSION_ID", "iTerm.app"),
            ("WT_SESSION", "Windows_Terminal"),
        ] {
            if std::env::var(env_key)
                .ok()
                .is_some_and(|v| !v.trim().is_empty())
            {
                term_program = Some(prog_name.to_string());
                break;
            }
        }
    }

    let mut term_session_id = None;
    for env_key in [
        "TMUX_PANE",
        "WEZTERM_PANE",
        "KITTY_WINDOW_ID",
        "ZELLIJ_PANE_ID",
        "ITERM_SESSION_ID",
        "TERM_SESSION_ID",
        "GHOSTTY_SURFACE_ID",
        "ALACRITTY_WINDOW_ID",
        "WT_SESSION",
    ] {
        if let Ok(val) = std::env::var(env_key) {
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
    let ide_pid = detect_ancestor_ide_pid(stable_pid);

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
    if let Ok(out) = std::process::Command::new("ps")
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
fn is_ide_process_name(comm: &str) -> bool {
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
fn detect_ancestor_ide_pid(start_pid: i32) -> Option<u32> {
    if let Ok(val) = std::env::var("VSCODE_PID") {
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

/// Brings the terminal tab, multiplexer pane, or terminal emulator window for `meta` to the foreground.
///
/// Supports `tmux`, `WezTerm`, `kitty`, macOS `Terminal.app` / `iTerm2` / third-party `.app`
/// terminals (`Ghostty`, `Warp`, `Alacritty`, `cmux`, `Hyper`, `Rio`, `Tabby`), Linux desktop
/// terminals (`wmctrl`, `xdotool`, `hyprctl`, `swaymsg`), and Windows terminals (`AppActivate`).
/// Non-terminal IDE sessions (`ide_pid.is_some() && tty.is_none()`) are not navigated and return
/// a `waypoint-info` report containing the recorded session coordinates.
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

    if !is_ide_agent
        && (term_prog.eq_ignore_ascii_case("tmux") || meta.agent_id.starts_with("term-tmux-"))
        && wp.term_session_id.is_some()
    {
        if let Some(pane) = wp.term_session_id.as_deref() {
            let _ = std::process::Command::new("tmux")
                .args(["select-window", "-t", pane])
                .output();
            let status = std::process::Command::new("tmux")
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

    if !is_ide_agent
        && (term_prog.eq_ignore_ascii_case("WezTerm") || meta.agent_id.starts_with("term-wezterm-"))
        && wp.term_session_id.is_some()
    {
        if let Some(pane) = wp.term_session_id.as_deref() {
            let status = std::process::Command::new("wezterm")
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
            let status = std::process::Command::new("kitty")
                .args(["@", "focus-window", "--match", &format!("id:{win_id}")])
                .output();

            if status.is_ok_and(|o| o.status.success()) {
                let _ = activate_host_terminal_window(&wp);
                return Ok(FocusSessionReport {
                    session_id: meta.agent_id.clone(),
                    harness: wp.harness.clone(),
                    focused: true,
                    mechanism: "kitty".to_string(),
                    detail: format!("Focused kitty window {win_id} for session {}", meta.agent_id),
                    waypoint: Some(wp),
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        if !is_ide_agent
            && (term_prog == "iTerm.app" || meta.agent_id.starts_with("term-iterm-"))
        {
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
                if let Ok(out) = std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .output()
                {
                    if out.status.success()
                        && String::from_utf8_lossy(&out.stdout).trim() == "focused"
                    {
                        tab_selected = true;
                    }
                }

                if tab_selected || term_prog == "iTerm.app" {
                    let opened = std::process::Command::new("open")
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
                    if let Ok(out) = std::process::Command::new("osascript")
                        .args(["-e", &script])
                        .output()
                    {
                        if out.status.success()
                            && String::from_utf8_lossy(&out.stdout).trim() == "focused"
                        {
                            let _ = std::process::Command::new("open")
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

                let uri = format!("{scheme}://Lunaris.tessera/focus?session={}", meta.agent_id);

                if std::process::Command::new("open")
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

                let Ok(ps_out) = std::process::Command::new("ps")
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
                    if std::process::Command::new("open")
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
                if std::process::Command::new("open")
                    .args(["-a", app_name])
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    return Some((
                        "terminal-window".to_string(),
                        format!("Focused {app_name}"),
                    ));
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

            if let Ok(wm_out) = std::process::Command::new("wmctrl").arg("-lp").output() {
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
                                if std::process::Command::new("wmctrl")
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
                if std::process::Command::new("xdotool")
                    .args(["search", "--pid", &pid_str, "windowactivate"])
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    return Some((
                        "xdotool".to_string(),
                        format!("Focused terminal window (PID {pid})"),
                    ));
                }

                if std::process::Command::new("hyprctl")
                    .args(["dispatch", "focuswindow", &format!("pid:{pid}")])
                    .output()
                    .is_ok_and(|o| o.status.success())
                {
                    return Some((
                        "hyprctl".to_string(),
                        format!("Focused Hyprland terminal window (PID {pid})"),
                    ));
                }

                if std::process::Command::new("swaymsg")
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

            if let Ok(out) = std::process::Command::new("powershell")
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

/// Installs repo-local harness discovery rules, lifecycle hooks, MCP server configurations,
/// and Git hooks during `tes init` using automatic harness detection (or `all` when requested).
///
/// # Errors
/// Returns [`CoreError::Io`] if any configuration file cannot be written.
pub fn install_harness_integrations(repo_root: &Path, git_dir: &Path) -> CoreResult<Vec<String>> {
    install_harness_integrations_for(repo_root, git_dir, None)
}

/// Installs repo-local harness discovery rules, lifecycle hooks, MCP server configurations,
/// and Git hooks for the specified `harness_filter` (`"antigravity"`, `"claude"`, `"cursor"`,
/// `"windsurf"`, `"kiro"`, `"opencode"`, `"codex"`, `"vscode"`, `"all"`, or `None` for smart auto-detection).
///
/// # Errors
/// Returns [`CoreError::Io`] if any configuration file cannot be written.
pub fn install_harness_integrations_for(
    repo_root: &Path,
    git_dir: &Path,
    harness_filter: Option<&str>,
) -> CoreResult<Vec<String>> {
    let mut installed = Vec::new();
    let mode = harness_filter
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    let has_tag = |tag: &str, alias: &str| -> bool {
        match mode.as_deref() {
            Some("all") => true,
            Some(csv) => csv
                .split(',')
                .map(str::trim)
                .any(|p| p == tag || (!alias.is_empty() && p == alias) || p == "all"),
            None => false,
        }
    };

    let is_auto = mode.is_none();
    let want_antigravity = if is_auto {
        true
    } else {
        has_tag("antigravity", "agy")
    };
    let want_claude = if is_auto {
        repo_root.join(".claude").exists()
            || repo_root.join("CLAUDE.md").exists()
            || std::env::var("CLAUDE_SESSION_ID").is_ok()
    } else {
        has_tag("claude", "")
    };
    let want_cursor = if is_auto {
        repo_root.join(".cursor").exists() || std::env::var("CURSOR_TRACE_ID").is_ok()
    } else {
        has_tag("cursor", "")
    };
    let want_windsurf = if is_auto {
        repo_root.join(".windsurf").exists()
            || repo_root.join(".windsurfrules").exists()
            || std::env::var("WINDSURF_SESSION_ID").is_ok()
    } else {
        has_tag("windsurf", "")
    };
    let want_kiro = if is_auto {
        repo_root.join(".kiro").exists() || std::env::var("KIRO_SESSION_ID").is_ok()
    } else {
        has_tag("kiro", "")
    };
    let want_opencode = if is_auto {
        repo_root.join("opencode.json").exists()
            || repo_root.join(".opencode").exists()
            || std::env::var("OPENCODE_SESSION_ID").is_ok()
    } else {
        has_tag("opencode", "")
    };
    let want_codex = if is_auto {
        repo_root.join(".codex").exists()
            || std::env::var("CODEX_THREAD_ID").is_ok()
            || std::env::var("CODEX_SESSION_ID").is_ok()
            || std::env::var("CODEX_SANDBOX").is_ok()
            || std::env::var("CODEX_CI").is_ok()
    } else {
        has_tag("codex", "openai")
    };
    let want_vscode = if is_auto {
        repo_root.join(".vscode/mcp.json").exists()
    } else {
        has_tag("vscode", "copilot")
    };

    // Clean up any legacy pre-rename rule files so agents never read stale rules.
    for legacy_rel in [
        ".agents/rules/lunr.md",
        ".cursor/rules/lunr.mdc",
        ".windsurf/rules/lunr.md",
        ".kiro/steering/lunr.md",
    ] {
        let _ = fs::remove_file(repo_root.join(legacy_rel));
    }
    let _ = fs::remove_dir_all(repo_root.join(".lunr"));

    let doc_block = r#"<!-- tessera:start -->
## Tessera Multi-Agent Source Control (Autonomous Mode)

This repository is version-controlled with **Tessera** (`.git/tessera`). Note: Tessera (`tes`) is strictly the source-control tool managing this repository. Never add Tessera/`tes` branding or copy into the user's application UI or code unless explicitly asked.
**IMPORTANT: Do NOT run raw `git` commands (`git status`, `git diff`, `git log`, `git branch`, `git checkout`, `git switch`, `git stash`, `git reset`, `git add`, `git commit`, `git cherry-pick`). Always use `tes` CLI commands instead:**
1. **Before editing files**: Run `tes claim [--branch <branch>] [--from <base-branch>] --intent "<your task goal>" <target-files...>` (automatically creates/switches your session to `<branch>`, optionally branching off `<base-branch>`, and records baselines; pass `--branch <branch>` explicitly to collaborate on a branch that already has uncommitted edits). Batch `tes claim` with your initial `read_file` calls in the same turn to minimize round-trips.
2. **After editing files**: Batch related `write_file` / `edit_file` calls in the same turn, then run `tes hook post-tool --path <edited-files...>` to auto-reconcile concurrent edits via 3-way AST & token merge. Never revert or delete lines added by concurrent peer sessions.
3. **Running builds, tests, or dev servers on a branch**: Run `tes exec [-b <branch>] -- <command>` to execute in an isolated Copy-on-Write sandbox with cloned dependencies and isolated `TES_PORT` / `DATABASE_URL` (plus full `PORT` and dev-server port isolation when `port_isolation = true`).
4. **Branch, Stash, Restore & History**: Use `tes status`, `tes diff`, `tes log`, `tes branch [-d|-D|-m]`, `tes stash [push|pop|apply|list|drop]`, `tes restore`, `tes cherry-pick`, `tes pull`, and `tes push`.
5. **When finished**: Run `tes commit [-m "<commit message>" | --amend]` (or chain `tes hook post-tool --path <edited-files...> && tes commit -m "<commit message>"` in a single command) to commit your session's isolated changes onto its branch.
<!-- tessera:end -->
"#;

    let always_on_rule_body = r#"# Tessera Multi-Agent Source Control (Autonomous Mode)

This repository is version-controlled by **Tessera** (`tes`). Note: Tessera (`tes`) is strictly the source-control tool managing this repository. Never add Tessera/`tes` branding or copy into the user's application UI or code unless explicitly asked.
Multiple AI agents may be editing this codebase concurrently in separate chat tabs or terminals, either on the same branch or on separate per-session branches.
**The user will give you normal coding prompts without mentioning `tes`. You MUST automatically run Tessera CLI commands on every coding task without waiting to be asked:**

## Critical Rules
- **NEVER run raw `git` commands** (`git status`, `git diff`, `git log`, `git branch`, `git checkout`, `git switch`, `git stash`, `git reset`, `git add`, `git commit`, `git cherry-pick`). Tessera manages branches and virtual working trees in `.git/tessera` without altering `.git/HEAD` for other concurrent agents. Always use `tes status`, `tes diff`, `tes log`, `tes branch`, `tes switch`, `tes stash`, `tes restore`, `tes cherry-pick`, `tes pull`, `tes push`, `tes commit`, and `tes merge`.
- **Never Revert Concurrent Peer Edits**: When multiple agents edit the same file on the same branch, `tes hook post-tool` automatically 3-way AST-merges their edits on disk. `tes status` and `tes diff` show only your session's own changes (`Staged / Session Changes`) and list concurrent `Peer Session Changes` separately. Never delete or revert a peer agent's code simply because you did not author it.
- **Use `tes` CLI commands in terminal tool calls** rather than duplicating with MCP calls so your per-tab session ID (`ANTIGRAVITY_CONVERSATION_ID`) is automatically isolated.
- **Batch Tool Calls for Fast Execution**: Minimize sequential LLM turns by calling `tes claim` alongside your initial `read_file` calls in Turn 1, batching your `write_file` / `edit_file` calls together in Turn 2, and chaining `tes hook post-tool --path <edited-files...> && tes commit -m "<commit message>"` in a single command in Turn 3 whenever no intermediate build check is needed.

## Mandatory Autonomous Workflow for Every Coding Task
1. **Before Editing Files**:
   - Claim your task intent, target branch, and target files via CLI (in parallel with your initial `read_file` calls):
     `tes claim [--branch <branch>] [--from <base-branch>] --intent "<concise summary of your task>" <target-file-paths...>`
   - (`tes claim --branch <branch> [--from <base-branch>]` automatically creates and binds your session to `<branch>`, optionally forking from `<base-branch>`. If a claimed file already has uncommitted changes from the user or a peer and you omit `--branch`, Tessera auto-isolates your session onto `agent/<session_id>`; pass `--branch <branch>` explicitly when same-branch collaboration is intended.)
2. **After Editing Files**:
   - Reconcile your edited files with any concurrent agent changes via CLI (passing all edited paths at once):
     `tes hook post-tool --path <edited-file-1> <edited-file-2>...`
   - Tessera automatically 3-way AST-merges non-overlapping changes on disk while preserving per-branch session isolation.
3. **Running Builds, Tests, or Servers on a Branch**:
   - Use `tes exec [-b <branch>] -- <command>` to run builds, tests, or dev servers in an isolated Copy-on-Write branch sandbox with cloned dependencies (`node_modules`, `target`, `.venv`, `vendor`, etc.) and isolated `TES_PORT` / `DATABASE_URL` (plus full `PORT` and dev-server port shims when `tes config set port_isolation true` is enabled).
4. **If a Conflict Is Reported**:
   - Standard Git conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) are written directly into the affected file on disk.
   - Edit the file to resolve the conflict markers and run `tes hook post-tool --path <path>` (or run `tes resolve <path> --strategy <working|ast-union|ours|theirs>`) to clear the conflict automatically. Never manually delete files inside `.git/tessera/`.
5. **Branch Lifecycle, Stash, Cherry-Pick & Restore**:
   - Delete or rename branches via `tes branch -d <branch>`, `tes branch -D <branch>`, or `tes branch -m <old> <new>`.
   - Stash, cherry-pick, or restore changes via `tes stash [push|pop|apply|list|drop]`, `tes cherry-pick <commit>`, or `tes restore [<rev>] [<paths...>]`.
6. **When Your Coding Task Is Complete**:
   - Commit your session's isolated changes cleanly onto its branch via CLI (can be chained directly after `tes hook post-tool --path <files...> && tes commit -m "..."`):
     `tes commit -m "<clean conventional commit message>"` (or `tes commit --amend -m "..."` / `tes merge -m "<clean conventional commit message>"`)
   - To AST-merge multiple branches into a target branch, run:
     `tes merge <branch1> <branch2> --into <target-branch> -m "<commit message>"`
"#;

    if want_antigravity {
        let agy_rule_path = repo_root.join(".agents/rules/tessera.md");
        let agy_rule_content = format!("---\ntrigger: always_on\n---\n\n{always_on_rule_body}");
        atomic_write_file(&agy_rule_path, agy_rule_content.as_bytes())?;
        installed.push(".agents/rules/tessera.md".to_string());
    }

    if want_claude {
        let claude_settings_path = repo_root.join(".claude/settings.json");
        upsert_claude_hooks_json(&claude_settings_path)?;
        installed.push(".claude/settings.json".to_string());
    }

    if want_claude {
        upsert_mcp_json(&repo_root.join(".mcp.json"))?;
        installed.push(".mcp.json".to_string());
    }

    if want_cursor {
        upsert_mcp_json(&repo_root.join(".cursor/mcp.json"))?;
        installed.push(".cursor/mcp.json".to_string());

        let cursor_rule = r#"---
description: Tessera multi-agent source control integration
globs: *
alwaysApply: true
---
This repository uses Tessera (`tes`) for concurrent multi-agent source control (never add Tessera/`tes` branding into application UI/code unless asked, and never run raw `git` commands). On every coding task, automatically: (1) run `tes claim [--branch <branch>] --intent "<goal>" <files>` before editing, (2) run `tes hook post-tool --path <file>` immediately after editing each file to AST-merge concurrent changes, (3) use `tes exec [-b <branch>] -- <cmd>` to run isolated branch tests/servers, and (4) run `tes commit -m "<commit message>"` (or `tes merge -m "<commit message>"`) when finished.
"#;
        atomic_write_file(
            &repo_root.join(".cursor/rules/tessera.mdc"),
            cursor_rule.as_bytes(),
        )?;
        installed.push(".cursor/rules/tessera.mdc".to_string());
    }

    if want_windsurf {
        let ws_rule_path = repo_root.join(".windsurf/rules/tessera.md");
        let ws_rule_content = format!("---\ntrigger: always_on\n---\n\n{always_on_rule_body}");
        atomic_write_file(&ws_rule_path, ws_rule_content.as_bytes())?;
        installed.push(".windsurf/rules/tessera.md".to_string());
    }

    if want_kiro {
        let kiro_rule_path = repo_root.join(".kiro/steering/tessera.md");
        let kiro_rule_content = format!("---\ninclusion: always\n---\n\n{always_on_rule_body}");
        atomic_write_file(&kiro_rule_path, kiro_rule_content.as_bytes())?;
        installed.push(".kiro/steering/tessera.md".to_string());

        upsert_mcp_json(&repo_root.join(".kiro/settings/mcp.json"))?;
        installed.push(".kiro/settings/mcp.json".to_string());
    }

    if want_opencode {
        upsert_opencode_json(&repo_root.join("opencode.json"))?;
        installed.push("opencode.json".to_string());
    }

    if want_codex {
        upsert_codex_config_toml(&repo_root.join(".codex/config.toml"))?;
        installed.push(".codex/config.toml".to_string());

        upsert_claude_hooks_json(&repo_root.join(".codex/hooks.json"))?;
        installed.push(".codex/hooks.json".to_string());
    }

    if want_vscode {
        upsert_vscode_mcp_json(&repo_root.join(".vscode/mcp.json"))?;
        installed.push(".vscode/mcp.json".to_string());
    }

    if want_opencode
        || want_codex
        || want_vscode
        || repo_root.join("AGENTS.md").exists()
    {
        upsert_marked_section(&repo_root.join("AGENTS.md"), doc_block)?;
        installed.push("AGENTS.md".to_string());
    }

    if want_claude {
        upsert_marked_section(&repo_root.join("CLAUDE.md"), doc_block)?;
        installed.push("CLAUDE.md".to_string());
    }

    let hooks_dir = git_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).map_err(|source| CoreError::Io {
        path: hooks_dir.display().to_string(),
        source,
    })?;
    let git_hook_snippet = "# tessera:start\nif command -v tes >/dev/null 2>&1; then\n  tes hook git-sync >/dev/null 2>&1 || true\nfi\n# tessera:end\n";
    for hook_name in ["post-commit", "post-merge", "post-checkout"] {
        let hook_path = hooks_dir.join(hook_name);
        upsert_git_hook(&hook_path, git_hook_snippet)?;
        installed.push(format!(".git/hooks/{hook_name}"));
    }

    // Ensure `.git/info/exclude` ignores `.tessera/` so COW workspaces and temp dirs never pollute `git status`.
    let exclude_path = git_dir.join("info/exclude");
    let existing_exclude = fs::read_to_string(&exclude_path).unwrap_or_default();
    if !existing_exclude
        .lines()
        .any(|l| l.trim() == ".tessera/" || l.trim() == ".tessera")
    {
        let updated_exclude = if existing_exclude.is_empty() || existing_exclude.ends_with('\n') {
            format!("{existing_exclude}.tessera/\n")
        } else {
            format!("{existing_exclude}\n.tessera/\n")
        };
        atomic_write_file(&exclude_path, updated_exclude.as_bytes())?;
    }

    Ok(installed)
}

/// Inserts or updates a `<!-- tessera:start --> ... <!-- tessera:end -->` markdown section in `path`.
fn upsert_marked_section(path: &Path, block: &str) -> CoreResult<()> {
    let mut existing = if path.is_file() {
        fs::read_to_string(path).map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?
    } else {
        String::new()
    };

    if let (Some(l_start), Some(l_end)) = (
        existing.find("<!-- lunr:start -->"),
        existing.find("<!-- lunr:end -->"),
    ) {
        let after = l_end + "<!-- lunr:end -->".len();
        existing = format!("{}{}", &existing[..l_start], &existing[after..])
            .trim()
            .to_string();
    }

    let updated = if let (Some(start), Some(end)) = (
        existing.find("<!-- tessera:start -->"),
        existing.find("<!-- tessera:end -->"),
    ) {
        let after_end = end + "<!-- tessera:end -->".len();
        format!(
            "{}{}{}",
            &existing[..start],
            block.trim_end(),
            &existing[after_end..]
        )
    } else if existing.trim().is_empty() {
        block.to_string()
    } else {
        format!("{}\n\n{}", existing.trim_end(), block)
    };

    atomic_write_file(path, updated.as_bytes())
}

/// Upserts the `tessera` MCP server entry under `mcpServers` in a standard `.mcp.json` file.
fn upsert_mcp_json(path: &Path) -> CoreResult<()> {
    let mut root: serde_json::Value = if path.is_file() {
        let raw = fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if !root.is_object() {
        root = serde_json::json!({});
    }
    let obj = root
        .as_object_mut()
        .expect("root is guaranteed to be a JSON object");
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        *servers = serde_json::json!({});
    }

    if let Some(servers_obj) = servers.as_object_mut() {
        if servers_obj.contains_key("tessera") && path.is_file() {
            return Ok(());
        }
        servers_obj.insert(
            "tessera".to_string(),
            serde_json::json!({
                "command": "tes",
                "args": ["mcp"]
            }),
        );
    }

    let formatted = format!(
        "{}\n",
        serde_json::to_string_pretty(&root)
            .map_err(|e| CoreError::Serialization(e.to_string()))?
    );
    atomic_write_file(path, formatted.as_bytes())
}

/// Upserts the `tessera` MCP server entry in an OpenCode `opencode.json` file.
fn upsert_opencode_json(path: &Path) -> CoreResult<()> {
    let mut root: serde_json::Value = if path.is_file() {
        let raw = fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({ "$schema": "https://opencode.ai/config.json" })
    };

    if !root.is_object() {
        root = serde_json::json!({ "$schema": "https://opencode.ai/config.json" });
    }
    let obj = root
        .as_object_mut()
        .expect("root is guaranteed to be a JSON object");
    let mcp = obj.entry("mcp").or_insert_with(|| serde_json::json!({}));
    if !mcp.is_object() {
        *mcp = serde_json::json!({});
    }

    if let Some(mcp_obj) = mcp.as_object_mut() {
        if mcp_obj.contains_key("tessera") && path.is_file() {
            return Ok(());
        }
        mcp_obj.insert(
            "tessera".to_string(),
            serde_json::json!({
                "type": "local",
                "command": ["tes", "mcp"],
                "enabled": true
            }),
        );
    }

    let formatted = format!(
        "{}\n",
        serde_json::to_string_pretty(&root)
            .map_err(|e| CoreError::Serialization(e.to_string()))?
    );
    atomic_write_file(path, formatted.as_bytes())
}

/// Upserts the `[mcp_servers.tessera]` table in an OpenAI Codex `.codex/config.toml` file.
fn upsert_codex_config_toml(path: &Path) -> CoreResult<()> {
    let existing = if path.is_file() {
        fs::read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    };

    if existing.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "[mcp_servers.tessera]" || trimmed == "[mcp_servers.\"tessera\"]"
    }) {
        return Ok(());
    }

    let block = "[mcp_servers.tessera]\ncommand = \"tes\"\nargs = [\"mcp\"]\n";
    let updated = if existing.trim().is_empty() {
        block.to_string()
    } else {
        format!("{}\n\n{block}", existing.trim_end())
    };

    atomic_write_file(path, updated.as_bytes())
}

/// Upserts the `tessera` MCP server entry under `servers` in `.vscode/mcp.json`.
fn upsert_vscode_mcp_json(path: &Path) -> CoreResult<()> {
    let mut root: serde_json::Value = if path.is_file() {
        let raw = fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if !root.is_object() {
        root = serde_json::json!({});
    }
    let obj = root
        .as_object_mut()
        .expect("root is guaranteed to be a JSON object");
    let servers = obj
        .entry("servers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        *servers = serde_json::json!({});
    }

    if let Some(servers_obj) = servers.as_object_mut() {
        if servers_obj.contains_key("tessera") && path.is_file() {
            return Ok(());
        }
        servers_obj.insert(
            "tessera".to_string(),
            serde_json::json!({
                "type": "stdio",
                "command": "tes",
                "args": ["mcp"]
            }),
        );
    }

    let formatted = format!(
        "{}\n",
        serde_json::to_string_pretty(&root)
            .map_err(|e| CoreError::Serialization(e.to_string()))?
    );
    atomic_write_file(path, formatted.as_bytes())
}

/// Upserts lifecycle hooks (`UserPromptSubmit`, `PreToolUse`, `PostToolUse`) in `.claude/settings.json` or `.codex/hooks.json`.
fn upsert_claude_hooks_json(path: &Path) -> CoreResult<()> {
    let mut root: serde_json::Value = if path.is_file() {
        let raw = fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if !root.is_object() {
        root = serde_json::json!({});
    }
    let obj = root
        .as_object_mut()
        .expect("root is guaranteed to be a JSON object");
    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        *hooks = serde_json::json!({});
    }

    if let Some(hooks_obj) = hooks.as_object_mut() {
        hooks_obj.insert(
            "UserPromptSubmit".to_string(),
            serde_json::json!([{
                "hooks": [{ "type": "command", "command": "tes hook prompt" }]
            }]),
        );
        hooks_obj.insert(
            "PreToolUse".to_string(),
            serde_json::json!([{
                "matcher": "Read|Edit|Write|MultiEdit|apply_patch",
                "hooks": [{ "type": "command", "command": "tes hook pre-tool" }]
            }]),
        );
        hooks_obj.insert(
            "PostToolUse".to_string(),
            serde_json::json!([{
                "matcher": "Edit|Write|MultiEdit|apply_patch",
                "hooks": [{ "type": "command", "command": "tes hook post-tool" }]
            }]),
        );
    }

    let formatted = format!(
        "{}\n",
        serde_json::to_string_pretty(&root)
            .map_err(|e| CoreError::Serialization(e.to_string()))?
    );
    atomic_write_file(path, formatted.as_bytes())
}

/// Inserts or updates a `# tessera:start ... # tessera:end` shell hook block in `.git/hooks/<hook>`.
fn upsert_git_hook(path: &Path, snippet: &str) -> CoreResult<()> {
    let existing = if path.is_file() {
        fs::read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    };

    let updated = if let (Some(start), Some(end)) = (
        existing.find("# tessera:start"),
        existing.find("# tessera:end"),
    ) {
        let after_end = end + "# tessera:end".len();
        format!(
            "{}{}{}",
            &existing[..start],
            snippet.trim_end(),
            &existing[after_end..]
        )
    } else if existing.trim().is_empty() {
        format!("#!/bin/sh\n{snippet}")
    } else {
        format!("{}\n\n{}", existing.trim_end(), snippet)
    };

    atomic_write_file(path, updated.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Handles `tes hook pre-tool`: records the baseline blob of `rel_path` seen by `session_id`
/// before an edit occurs, and warns if another active session is concurrently modifying `rel_path`.
///
/// # Errors
/// Returns [`CoreError`] if reading or updating the `OpLog` fails.
pub fn handle_pre_tool_hook(
    coord: &Coordinator,
    session_id: &str,
    rel_path: &str,
) -> CoreResult<PreToolOutcome> {
    let normalized_path = normalize_repo_rel_path(coord.git().repo_root(), rel_path)?;
    let meta = coord.ensure_session(session_id, None)?;
    let base_tree_blob = coord
        .git()
        .read_commit(&meta.base_commit)
        .ok()
        .and_then(|c| {
            coord
                .git()
                .read_tree_entry_at_path(&c.tree_id, &normalized_path)
                .ok()
                .flatten()
        })
        .map(|e| e.blob_id);

    if is_ignored_or_secret_path(
        coord.git().repo_root(),
        coord.git().git_dir(),
        &normalized_path,
        base_tree_blob.is_some(),
    ) {
        return Ok(PreToolOutcome {
            session_id: session_id.to_string(),
            path: normalized_path,
            radar_warning: None,
        });
    }

    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| "main".to_string());
    let my_eff_branch = meta
        .target_branch
        .as_deref()
        .unwrap_or(default_branch.as_str())
        .to_string();

    let full_path = coord.git().repo_root().join(&normalized_path);
    let source_path = coord
        .invocation_dir()
        .map(|d| d.join(&normalized_path))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| full_path.clone());

    let cur_op_opt = coord.oplog().load_current_reconciled().ok();

    // If `source_path` is in `repo_root` and currently holds an uncommitted blob from a foreign
    // branch session (or `session_id` is on a branch whose base commit differs from `HEAD` while
    // no other active session owns or awaits `disk_blob`), restore `session_id`'s own branch
    // version to `repo_root` before reading.
    if source_path == full_path && full_path.is_file() {
        if let (Ok(disk_bytes), Some(cur_op)) = (fs::read(&full_path), cur_op_opt.as_ref()) {
            if let Ok(disk_blob) = coord.git().write_blob(&disk_bytes) {
                let my_expected = meta
                    .own_blobs
                    .get(&normalized_path)
                    .or(base_tree_blob.as_ref());
                let owned_by_foreign_branch = cur_op.view.agents.values().any(|other| {
                    other.agent_id != session_id
                        && other.effective_branch(&default_branch) != my_eff_branch
                        && other.own_blobs.get(&normalized_path) == Some(&disk_blob)
                });
                let head_cid_opt = coord.git().resolve_head_commit().ok().flatten();
                let foreign_peer_awaiting_disk = cur_op.view.agents.values().any(|other| {
                    let is_passive = (other.agent_id.starts_with("term-")
                        || other.agent_id.starts_with("session-")
                        || other.agent_id == "editor")
                        && other.intent_inferred
                        && other.claimed_symbols.is_empty()
                        && !other.has_own_changes();
                    !is_passive
                        && other.agent_id != session_id
                        && other.effective_branch(&default_branch) != my_eff_branch
                        && (other.own_blobs.get(&normalized_path) == Some(&disk_blob)
                            || (!other.owns_path(&normalized_path)
                                && other.read_blobs.get(&normalized_path) == Some(&disk_blob)))
                });
                let diverged_branch_base = my_eff_branch != default_branch
                    && head_cid_opt.is_some_and(|hcid| hcid != meta.base_commit)
                    && !foreign_peer_awaiting_disk;

                if (owned_by_foreign_branch || diverged_branch_base)
                    && my_expected != Some(&disk_blob)
                {
                    if let Some(exp_blob) = my_expected {
                        if let Ok(exp_bytes) = coord.git().read_blob(exp_blob) {
                            let _ = atomic_write_file(&full_path, &exp_bytes);
                        }
                    }
                }
            }
        }
    } else if source_path == full_path
        && !full_path.is_file()
        && my_eff_branch != default_branch
        && !meta.deleted_paths.contains(&normalized_path)
        && !cur_op_opt.as_ref().is_some_and(|cur_op| {
            cur_op.view.agents.values().any(|other| {
                other.effective_branch(&default_branch) == my_eff_branch
                    && other.deleted_paths.contains(&normalized_path)
            })
        })
    {
        if let Some(exp_blob) = meta
            .own_blobs
            .get(&normalized_path)
            .or(base_tree_blob.as_ref())
        {
            if let Ok(exp_bytes) = coord.git().read_blob(exp_blob) {
                let _ = atomic_write_file(&full_path, &exp_bytes);
            }
        }
    }

    let current_blob_opt = if source_path.is_file() {
        let bytes = fs::read(&source_path).map_err(|source| CoreError::Io {
            path: source_path.display().to_string(),
            source,
        })?;
        Some(coord.git().write_blob(&bytes)?)
    } else {
        None
    };

    let mut warning: Option<String> = None;

    coord.oplog().record_tx(
        session_id,
        format!("pre-tool read baseline for {normalized_path}"),
        true,
        |view| {
            // Check if another active session on the same branch has already touched `normalized_path`.
            for (other_id, other_agent) in &view.agents {
                let is_passive = (other_id.starts_with("term-")
                    || other_id.starts_with("session-"))
                    && other_agent.intent_inferred
                    && other_agent.claimed_symbols.is_empty()
                    && !other_agent.has_own_changes();
                let other_b = other_agent.effective_branch(&default_branch);
                if other_id != session_id
                    && !is_passive
                    && other_b == my_eff_branch
                    && (other_agent.read_blobs.contains_key(&normalized_path)
                        || other_agent.owns_path(&normalized_path))
                {
                    warning = Some(format!(
                        "Session '{}' (Intent: \"{}\") is also active on '{}'",
                        other_id, other_agent.intent, normalized_path
                    ));
                }
            }

            let initial_baseline = if my_eff_branch != default_branch || warning.is_some() {
                base_tree_blob.or(current_blob_opt)
            } else {
                current_blob_opt
            };

            if let Some(blob_id) = initial_baseline {
                let obs = view
                    .observed_blobs
                    .entry(normalized_path.clone())
                    .or_default();
                if !obs.contains(&blob_id) {
                    if obs.len() >= MAX_OBSERVED_BLOBS_PER_PATH {
                        obs.remove(0);
                    }
                    obs.push(blob_id);
                }

                if let Some(agent) = view.agents.get_mut(session_id) {
                    // Only initialize baseline if this session does not already have an older baseline recorded.
                    agent
                        .read_blobs
                        .entry(normalized_path.clone())
                        .or_insert(blob_id);
                    agent.updated_at_ms = now_millis();
                }
            }
            Ok(())
        },
    )?;

    ensure_background_watcher(coord.git().repo_root(), coord.git().git_dir());

    Ok(PreToolOutcome {
        session_id: session_id.to_string(),
        path: normalized_path,
        radar_warning: warning,
    })
}

/// Handles `tes hook post-tool`: reconciles an in-place edit to `rel_path` in the shared working
/// directory when multiple sessions edit the same file without separate worktrees.
///
/// - Compares `session_id`'s baseline (`read_blobs[rel_path]`) against all intermediate blobs
///   captured in `observed_blobs[rel_path]` (by the background watcher), `other_agent.read_blobs`, and `HEAD`.
/// - If another session on the same branch modified `rel_path` after `session_id` read it, runs
///   [`merge_file_3way`] (Tree-sitter AST 3-way merge + manifest drivers) and writes the merged file to disk atomically.
/// - Automatically infers or updates `session_id`'s intent from the modified AST symbols if
///   no explicit prompt intent was set, and records each session's unmerged `own_blobs` for
///   per-branch isolation.
///
/// # Errors
/// Returns [`CoreError`] if reading/writing blobs or updating the `OpLog` fails.
pub fn handle_post_tool_hook(
    coord: &Coordinator,
    session_id: &str,
    rel_path: &str,
) -> CoreResult<PostToolOutcome> {
    let normalized_path = normalize_repo_rel_path(coord.git().repo_root(), rel_path)?;
    coord.ensure_session(session_id, None)?;

    let full_path = coord.git().repo_root().join(&normalized_path);

    if let Some(dir_outcome) = reconcile_directory_post_tool(coord, session_id, &normalized_path)? {
        return Ok(dir_outcome);
    }

    let session_could_delete = coord
        .oplog()
        .load_current_reconciled()
        .ok()
        .and_then(|cur| cur.view.agents.get(session_id).cloned())
        .is_some_and(|a| {
            a.own_blobs.contains_key(&normalized_path)
                || coord
                    .git()
                    .read_commit(&a.base_commit)
                    .ok()
                    .and_then(|c| {
                        coord
                            .git()
                            .read_tree_entry_at_path(&c.tree_id, &normalized_path)
                            .ok()
                            .flatten()
                    })
                    .is_some()
        });

    // If `tes hook post-tool` was invoked from inside a COW sandbox directory (or with an absolute
    // sandbox file path), sync the edited file (and its mode) from the sandbox into `repo_root` first.
    let raw_p = Path::new(rel_path);
    let mut deleted_in_sandbox = false;
    if raw_p.is_absolute() && raw_p != full_path {
        if raw_p.is_file() {
            if let Ok(raw_bytes) = fs::read(raw_p) {
                let _ = atomic_write_file(&full_path, &raw_bytes);
                if let Ok(m) = fs::metadata(raw_p) {
                    let _ = fs::set_permissions(&full_path, m.permissions());
                }
            }
        } else {
            deleted_in_sandbox = session_could_delete;
        }
    } else if let Some(inv_dir) = coord.invocation_dir() {
        let inv_file = inv_dir.join(&normalized_path);
        if inv_file != full_path {
            if inv_file.is_file() {
                if let Ok(inv_bytes) = fs::read(&inv_file) {
                    let _ = atomic_write_file(&full_path, &inv_bytes);
                    if let Ok(m) = fs::metadata(&inv_file) {
                        let _ = fs::set_permissions(&full_path, m.permissions());
                    }
                }
            } else {
                deleted_in_sandbox = session_could_delete;
            }
        }
    }

    if !full_path.is_file() || deleted_in_sandbox {
        let meta = coord.record_single_path_edit(session_id, &normalized_path, None)?;
        let default_branch = coord
            .git()
            .current_branch_name()
            .unwrap_or_else(|_| "main".to_string());
        let eff_branch = meta.effective_branch(&default_branch);
        if eff_branch == default_branch && full_path.is_file() {
            let _ = fs::remove_file(&full_path);
        }
        let branch_ws = coord.branch_sandbox_dir(eff_branch);
        if branch_ws.is_dir() {
            let _ = fs::remove_file(branch_ws.join(&normalized_path));
        }

        return Ok(PostToolOutcome {
            session_id: session_id.to_string(),
            path: normalized_path,
            auto_merged_concurrent_edit: false,
            has_conflict: false,
            context_notice: None,
            session_intent: meta.intent,
        });
    }

    let current_op = coord.oplog().load_current_reconciled()?;
    let my_agent = current_op
        .view
        .agents
        .get(session_id)
        .cloned()
        .ok_or_else(|| CoreError::NotFound(format!("session {session_id}")))?;

    let default_branch = coord
        .git()
        .current_branch_name()
        .unwrap_or_else(|_| "main".to_string());
    let my_eff_branch = my_agent.effective_branch(&default_branch);

    // Determine the baseline blob `session_id` started from on its `base_commit`.
    let base_commit_obj_opt = coord.git().read_commit(&my_agent.base_commit).ok();
    let base_tree_entry = base_commit_obj_opt.as_ref().and_then(|c| {
        coord
            .git()
            .read_tree_entry_at_path(&c.tree_id, &normalized_path)
            .ok()
            .flatten()
    });
    let base_tree_blob = base_tree_entry.as_ref().map(|e| e.blob_id);

    if is_ignored_or_secret_path(
        coord.git().repo_root(),
        coord.git().git_dir(),
        &normalized_path,
        base_tree_blob.is_some(),
    ) {
        return Ok(PostToolOutcome {
            session_id: session_id.to_string(),
            path: normalized_path,
            auto_merged_concurrent_edit: false,
            has_conflict: false,
            context_notice: None,
            session_intent: my_agent.intent,
        });
    }

    #[cfg(unix)]
    let mut executable = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(&full_path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or_else(|_| base_tree_entry.as_ref().is_some_and(|e| e.executable))
    };
    #[cfg(not(unix))]
    let mut executable = base_tree_entry.as_ref().is_some_and(|e| e.executable);

    let mut disk_after_bytes = fs::read(&full_path).map_err(|source| CoreError::Io {
        path: full_path.display().to_string(),
        source,
    })?;
    let mut disk_after_blob = coord.git().write_blob(&disk_after_bytes)?;

    let base_blob = base_tree_blob.or_else(|| my_agent.read_blobs.get(&normalized_path).copied());

    // If this session is on a non-default branch and `_tes_preexec` redirected the shell command
    // into the branch sandbox (`$TES_WORKSPACE`), read the edited bytes directly from the branch sandbox.
    let mut edited_in_branch_sandbox = false;
    if my_eff_branch != default_branch {
        let sb_file = coord.branch_sandbox_dir(my_eff_branch).join(&normalized_path);
        if sb_file.is_file() && sb_file != full_path {
            if let Ok(sb_bytes) = fs::read(&sb_file) {
                if let Ok(sb_blob) = coord.git().write_blob(&sb_bytes) {
                    let prev_expected = my_agent
                        .own_blobs
                        .get(&normalized_path)
                        .or(base_blob.as_ref());
                    if Some(&sb_blob) != prev_expected && Some(&sb_blob) != base_blob.as_ref() {
                        disk_after_bytes = sb_bytes;
                        disk_after_blob = sb_blob;
                        edited_in_branch_sandbox = true;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            if let Ok(m) = fs::metadata(&sb_file) {
                                executable = m.permissions().mode() & 0o111 != 0;
                            }
                        }
                    }
                }
            }
        }
    }
    let disk_after_str = String::from_utf8_lossy(&disk_after_bytes);

    let base_bytes = match &base_blob {
        Some(bid) => coord.git().read_blob(bid)?,
        None => Vec::new(),
    };
    let base_str = String::from_utf8_lossy(&base_bytes);

    let is_active_peer = |other_id: &str, other_agent: &core::AgentMetadata| -> bool {
        if other_id == session_id {
            return false;
        }

        let is_passive = (other_id.starts_with("term-") || other_id.starts_with("session-"))
            && other_agent.intent_inferred
            && other_agent.claimed_symbols.is_empty()
            && !other_agent.has_own_changes();
        if is_passive {
            return false;
        }

        other_agent.owns_path(&normalized_path)
            || other_agent.read_blobs.contains_key(&normalized_path)
            || other_agent.claimed_symbols.iter().any(|s| {
                s.split("::").next().unwrap_or(s.as_str()).trim() == normalized_path
            })
    };

    let is_same_branch_peer = |other_id: &str, other_agent: &core::AgentMetadata| -> bool {
        if !is_active_peer(other_id, other_agent) {
            return false;
        }

        let other_eff_branch = other_agent.effective_branch(&default_branch);
        other_eff_branch == my_eff_branch
    };

    // If a peer session already recorded `own_blobs[normalized_path]` and `session_id` edited
    // the shared file on disk on top of that peer's edits, strip the peer's edits via 3-way
    // patch inversion (`merge_file_3way(peer_blob, my_prev_own_blob, disk_after)`) so `session_id`'s
    // `own_blobs` contains ONLY `session_id`'s isolated changes!
    let mut foreign_blobs: Vec<ObjectId> = Vec::new();
    for (other_id, other_agent) in &current_op.view.agents {
        if !is_active_peer(other_id, other_agent) {
            continue;
        }

        if let Some(peer_own_blob) = other_agent.own_blobs.get(&normalized_path) {
            if Some(peer_own_blob) != base_blob.as_ref()
                && *peer_own_blob != disk_after_blob
                && !foreign_blobs.contains(peer_own_blob)
            {
                foreign_blobs.push(*peer_own_blob);
            }
        }
    }

    let trunk_branch = coord
        .oplog()
        .load_config()
        .map(|c| c.trunk_branch)
        .unwrap_or_else(|_| default_branch.clone());
    if my_eff_branch != default_branch {
        for (b_name, b_cid) in &current_op.view.branches {
            if b_name == my_eff_branch
                || b_name == &default_branch
                || b_name == &trunk_branch
                || *b_cid == my_agent.base_commit
            {
                continue;
            }

            if let Ok(b_commit) = coord.git().read_commit(b_cid) {
                if let Ok(Some(b_entry)) = coord
                    .git()
                    .read_tree_entry_at_path(&b_commit.tree_id, &normalized_path)
                {
                    if Some(&b_entry.blob_id) != base_blob.as_ref()
                        && b_entry.blob_id != disk_after_blob
                        && !foreign_blobs.contains(&b_entry.blob_id)
                    {
                        foreign_blobs.push(b_entry.blob_id);
                    }
                }
            }
        }
    }

    let mut effective_my_blob = disk_after_blob;
    let mut current_my_str = disk_after_str.to_string();
    let mut untangled_cross_branch = false;

    // If a cross-branch peer's `post-tool` already rescued our blob into `my_agent.own_blobs`
    // and wrote that peer's own isolated blob to `repo_root`, preserve our rescued `own_blob`.
    let disk_is_foreign_peer_own_blob = current_op.view.agents.iter().any(|(other_id, other_agent)| {
        is_active_peer(other_id, other_agent)
            && !is_same_branch_peer(other_id, other_agent)
            && other_agent.own_blobs.get(&normalized_path) == Some(&disk_after_blob)
    }) || foreign_blobs.contains(&disk_after_blob)
        || (my_eff_branch != default_branch
            && Some(&disk_after_blob) == base_blob.as_ref());
    if disk_is_foreign_peer_own_blob || crate::has_git_conflict_markers(&disk_after_str) {
        if let Some(existing_own) = my_agent.own_blobs.get(&normalized_path) {
            if *existing_own != disk_after_blob {
                if let Ok(existing_bytes) = coord.git().read_blob(existing_own) {
                    let existing_str = String::from_utf8_lossy(&existing_bytes).into_owned();
                    if !crate::has_git_conflict_markers(&existing_str) {
                        effective_my_blob = *existing_own;
                        current_my_str = existing_str;
                        if disk_is_foreign_peer_own_blob {
                            untangled_cross_branch = true;
                        }
                    }
                }
            }
        } else if disk_is_foreign_peer_own_blob {
            if let Some(obs_list) = current_op.view.observed_blobs.get(&normalized_path) {
                let mut best_rescued: Option<(isize, usize, ObjectId, String)> = None;
                for obs_blob in obs_list.iter().rev() {
                    let owned_by_other = current_op
                        .view
                        .agents
                        .values()
                        .any(|a| a.own_blobs.get(&normalized_path) == Some(obs_blob));
                    if *obs_blob != disk_after_blob
                        && Some(obs_blob) != base_blob.as_ref()
                        && Some(obs_blob) != base_tree_blob.as_ref()
                        && !foreign_blobs.contains(obs_blob)
                        && !owned_by_other
                    {
                        if let Ok(obs_bytes) = coord.git().read_blob(obs_blob) {
                            let obs_str = String::from_utf8_lossy(&obs_bytes).into_owned();
                            if !crate::has_git_conflict_markers(&obs_str) {
                                let my_score = score_blob_for_agent(
                                    &base_str,
                                    &obs_str,
                                    &my_agent.intent,
                                    Some(my_eff_branch),
                                );
                                let max_other_score = current_op
                                    .view
                                    .agents
                                    .iter()
                                    .filter(|(other_id, other_agent)| {
                                        is_active_peer(other_id, other_agent)
                                            && !is_same_branch_peer(other_id, other_agent)
                                            && !other_agent.owns_path(&normalized_path)
                                    })
                                    .map(|(_, other_agent)| {
                                        score_blob_for_agent(
                                            &base_str,
                                            &obs_str,
                                            &other_agent.intent,
                                            Some(other_agent.effective_branch(&default_branch)),
                                        )
                                    })
                                    .max()
                                    .unwrap_or(0);
                                let net_score =
                                    (my_score as isize) - (max_other_score as isize);

                                if best_rescued.as_ref().is_none_or(|(best_net, best_raw, _, _)| {
                                    (net_score, my_score) > (*best_net, *best_raw)
                                }) {
                                    best_rescued =
                                        Some((net_score, my_score, *obs_blob, obs_str));
                                }
                            }
                        }
                    }
                }

                if let Some((_, _, rescued_blob, rescued_str)) = best_rescued {
                    effective_my_blob = rescued_blob;
                    current_my_str = rescued_str;
                    untangled_cross_branch = true;
                }
            }
        }
    }

    let my_prev_blob = my_agent
        .own_blobs
        .get(&normalized_path)
        .cloned()
        .or(base_blob);
    let my_prev_bytes = match &my_prev_blob {
        Some(bid) => coord.git().read_blob(bid).unwrap_or_default(),
        None => Vec::new(),
    };
    let my_prev_str = String::from_utf8_lossy(&my_prev_bytes);

    let mut stripped_peer_blobs: std::collections::BTreeSet<ObjectId> =
        std::collections::BTreeSet::new();
    for peer_own_blob in &foreign_blobs {
        if let Ok(peer_bytes) = coord.git().read_blob(peer_own_blob) {
            let peer_str = String::from_utf8_lossy(&peer_bytes);
            if is_contaminated_by_foreign_peer(&base_str, &my_prev_str, &peer_str, &current_my_str)
            {
                let stripped = merge_file_3way(
                    &normalized_path,
                    &peer_str,
                    &my_prev_str,
                    &current_my_str,
                    session_id,
                    session_id,
                );
                let isolated_candidate = if !stripped.has_conflicts && stripped.content != *base_str
                {
                    Some(stripped.content)
                } else {
                    strip_peer_inserted_lines(&base_str, &my_prev_str, &peer_str, &current_my_str)
                        .filter(|s| s != &*base_str)
                };

                if let Some(clean_str) = isolated_candidate {
                    if let Ok(isolated_id) = coord.git().write_blob(clean_str.as_bytes()) {
                        effective_my_blob = isolated_id;
                        current_my_str = clean_str;
                        untangled_cross_branch = true;
                        stripped_peer_blobs.insert(*peer_own_blob);
                    }
                }
            }
        }
    }

    // First, check if any active peer (on any branch) is missing `own_blobs[normalized_path]`
    // while `observed_blobs` has unattributed intermediate writes on disk, and attribute each
    // observed blob to its matching session via `score_blob_for_agent`.
    let any_active_peer_missing_own_blob =
        current_op.view.agents.iter().any(|(other_id, other_agent)| {
            is_active_peer(other_id, other_agent)
                && !other_agent.owns_path(&normalized_path)
        });
    let mut assigned_own_blobs: std::collections::BTreeMap<String, ObjectId> =
        std::collections::BTreeMap::new();
    if effective_my_blob != disk_after_blob {
        assigned_own_blobs.insert(session_id.to_string(), effective_my_blob);
    }

    if any_active_peer_missing_own_blob {
        let mut attribution_blobs: Vec<ObjectId> = Vec::new();
        if let Some(obs_list) = current_op.view.observed_blobs.get(&normalized_path) {
            for obs_blob in obs_list {
                if Some(obs_blob) != base_blob.as_ref()
                    && Some(obs_blob) != base_tree_blob.as_ref()
                    && !foreign_blobs.contains(obs_blob)
                    && !attribution_blobs.contains(obs_blob)
                {
                    attribution_blobs.push(*obs_blob);
                }
            }
        }

        for extra_blob in [effective_my_blob, disk_after_blob] {
            if Some(&extra_blob) != base_blob.as_ref()
                && Some(&extra_blob) != base_tree_blob.as_ref()
                && !foreign_blobs.contains(&extra_blob)
                && !attribution_blobs.contains(&extra_blob)
            {
                attribution_blobs.push(extra_blob);
            }
        }

        if attribution_blobs.len() >= 2 {
            for (aid, agent_meta) in &current_op.view.agents {
                if aid != session_id && !is_active_peer(aid, agent_meta) {
                    continue;
                }

                if aid == session_id && assigned_own_blobs.contains_key(session_id) {
                    continue;
                }

                if aid != session_id && agent_meta.owns_path(&normalized_path) {
                    continue;
                }

                let mut best_blob: Option<(usize, ObjectId)> = None;
                for cand_blob in &attribution_blobs {
                    if effective_my_blob != disk_after_blob
                        && *cand_blob == effective_my_blob
                        && aid != session_id
                    {
                        continue;
                    }

                    if let Ok(cand_bytes) = coord.git().read_blob(cand_blob) {
                        let cand_str = String::from_utf8_lossy(&cand_bytes);
                        let score = score_blob_for_agent(
                            &base_str,
                            &cand_str,
                            &agent_meta.intent,
                            Some(agent_meta.effective_branch(&default_branch)),
                        );
                        if score > 0
                            && best_blob
                                .as_ref()
                                .is_none_or(|(best_s, _)| score > *best_s)
                        {
                            best_blob = Some((score, *cand_blob));
                        }
                    }
                }

                if let Some((_, matched_blob)) = best_blob {
                    assigned_own_blobs.insert(aid.clone(), matched_blob);
                }
            }

            if assigned_own_blobs.len() >= 2 {
                untangled_cross_branch = true;
            }
        }
    }

    if !assigned_own_blobs.contains_key(session_id) {
        assigned_own_blobs.insert(session_id.to_string(), effective_my_blob);
    }

    let my_own_blob = *assigned_own_blobs
        .get(session_id)
        .or_else(|| my_agent.own_blobs.get(&normalized_path))
        .unwrap_or(&effective_my_blob);

    // Collect all distinct non-base blobs observed for `normalized_path` on the SAME branch across:
    // - Background watcher (`observed_blobs` when same-branch peers are active),
    // - Active same-branch peer sessions (`other_agent.own_blobs`),
    // - Active branch `HEAD` commit (if a peer on the same branch already merged and closed),
    // - Current session's isolated edit (`my_own_blob`).
    let mut candidate_blobs: Vec<(String, String, ObjectId)> = Vec::new();
    let push_candidate = |list: &mut Vec<(String, String, ObjectId)>,
                          id: String,
                          intent: String,
                          blob: ObjectId| {
        if Some(&blob) != base_blob.as_ref()
            && Some(&blob) != base_tree_blob.as_ref()
            && !list.iter().any(|(_, _, b)| *b == blob)
        {
            list.push((id, intent, blob));
        }
    };

    let mut reconciled_head_commit: Option<ObjectId> = None;
    let mut head_changed_norm_path = false;
    let target_branch_ref = format!("refs/heads/{my_eff_branch}");
    let head_commit_opt = coord
        .git()
        .resolve_ref(&target_branch_ref)
        .ok()
        .flatten()
        .or_else(|| coord.git().resolve_head_commit().ok().flatten());

    let resolving_existing_conflict = current_op.view.conflicts.contains_key(&normalized_path)
        && !crate::has_git_conflict_markers(&disk_after_str);

    let has_concurrent_same_branch_peer = head_commit_opt.as_ref() != Some(&my_agent.base_commit)
        || current_op
            .view
            .agents
            .iter()
            .any(|(other_id, other_agent)| is_same_branch_peer(other_id, other_agent));

    let any_same_branch_peer_missing_own_blob =
        current_op.view.agents.iter().any(|(other_id, other_agent)| {
            is_same_branch_peer(other_id, other_agent)
                && !other_agent.owns_path(&normalized_path)
        });

    if !resolving_existing_conflict
        && has_concurrent_same_branch_peer
        && any_same_branch_peer_missing_own_blob
    {
        if let Some(obs_list) = current_op.view.observed_blobs.get(&normalized_path) {
            for obs_blob in obs_list {
                let assigned_to_foreign = assigned_own_blobs.iter().any(|(aid, b)| {
                    b == obs_blob
                        && aid != session_id
                        && current_op
                            .view
                            .agents
                            .get(aid)
                            .is_some_and(|a| !is_same_branch_peer(aid, a))
                });
                if *obs_blob != my_own_blob
                    && my_agent.own_blobs.get(&normalized_path) != Some(obs_blob)
                    && !foreign_blobs.contains(obs_blob)
                    && !assigned_to_foreign
                {
                    push_candidate(
                        &mut candidate_blobs,
                        "peer".to_string(),
                        "concurrent edit".to_string(),
                        *obs_blob,
                    );
                }
            }
        }
    }

    let mut peers_integrated_now: std::collections::BTreeMap<String, ObjectId> =
        std::collections::BTreeMap::new();
    let integrated_peers = my_agent.integrated_peer_blobs.get(&normalized_path);

    if !resolving_existing_conflict {
        for (other_id, other_agent) in &current_op.view.agents {
            if !is_same_branch_peer(other_id, other_agent) {
                continue;
            }

            if let Some(other_own) = other_agent
                .own_blobs
                .get(&normalized_path)
                .or_else(|| assigned_own_blobs.get(other_id))
            {
                let candidate_opt =
                    if base_tree_blob.is_some() || stripped_peer_blobs.contains(other_own) {
                        Some(*other_own)
                    } else {
                        match integrated_peers.and_then(|m| m.get(other_id)) {
                            Some(seen) if seen == other_own => None,
                            Some(seen) => {
                                let seen_bytes = coord.git().read_blob(seen).unwrap_or_default();
                                let seen_str = String::from_utf8_lossy(&seen_bytes);
                                let peer_bytes =
                                    coord.git().read_blob(other_own).unwrap_or_default();
                                let peer_str = String::from_utf8_lossy(&peer_bytes);
                                let rebased = merge_file_3way(
                                    &normalized_path,
                                    &seen_str,
                                    &base_str,
                                    &peer_str,
                                    "base",
                                    other_id,
                                );
                                coord.git().write_blob(rebased.content.as_bytes()).ok()
                            }
                            None => Some(*other_own),
                        }
                    };
                peers_integrated_now.insert(other_id.clone(), *other_own);
                if let Some(cand_blob) = candidate_opt {
                    push_candidate(
                        &mut candidate_blobs,
                        other_id.clone(),
                        other_agent.intent.clone(),
                        cand_blob,
                    );
                }
            } else if let Some(other_blob) = other_agent.read_blobs.get(&normalized_path) {
                if !other_agent.deleted_paths.contains(&normalized_path) {
                    peers_integrated_now.insert(other_id.clone(), *other_blob);
                    if *other_blob != my_own_blob
                        && my_agent.own_blobs.get(&normalized_path) != Some(other_blob)
                        && integrated_peers.and_then(|m| m.get(other_id)) != Some(other_blob)
                    {
                        push_candidate(
                            &mut candidate_blobs,
                            other_id.clone(),
                            other_agent.intent.clone(),
                            *other_blob,
                        );
                    }
                }
            }
        }
    }

    if let Some(head_commit_id) = head_commit_opt {
        if head_commit_id != my_agent.base_commit {
            reconciled_head_commit = Some(head_commit_id);
        }

        if !resolving_existing_conflict {
            if let Ok(head_commit) = coord.git().read_commit(&head_commit_id) {
                if let Ok(Some(head_entry)) = coord
                    .git()
                    .read_tree_entry_at_path(&head_commit.tree_id, &normalized_path)
                {
                    if Some(&head_entry.blob_id) != base_blob.as_ref()
                        && head_entry.blob_id != my_own_blob
                    {
                        let head_bytes = coord
                            .git()
                            .read_blob(&head_entry.blob_id)
                            .unwrap_or_default();
                        let head_str = String::from_utf8_lossy(&head_bytes);
                        let already_based_on_head = crate::has_git_conflict_markers(&head_str)
                            || is_contaminated_by_foreign_peer(
                                &base_str,
                                &base_str,
                                &head_str,
                                &current_my_str,
                            );
                        if !already_based_on_head {
                            let branch_name = my_eff_branch.to_string();
                            let summary = head_commit
                                .message
                                .lines()
                                .next()
                                .unwrap_or("merged commit")
                                .to_string();
                            push_candidate(
                                &mut candidate_blobs,
                                branch_name,
                                summary,
                                head_entry.blob_id,
                            );
                            head_changed_norm_path = true;
                        }
                        reconciled_head_commit = Some(head_commit_id);
                    }
                }
            }
        }
    }

    if !crate::has_git_conflict_markers(&current_my_str) {
        push_candidate(
            &mut candidate_blobs,
            session_id.to_string(),
            my_agent.intent.clone(),
            my_own_blob,
        );
    }

    let my_own_bytes = coord
        .git()
        .read_blob(&my_own_blob)
        .unwrap_or_else(|_| disk_after_bytes.clone());
    let my_own_str = String::from_utf8_lossy(&my_own_bytes);

    // Auto-extract modified AST symbols from this session's own edit.
    let touched_syms = diff_modified_symbols(&normalized_path, &base_str, &my_own_str);
    let mut final_blob = if crate::has_git_conflict_markers(&disk_after_str) {
        disk_after_blob
    } else {
        my_own_blob
    };
    let mut final_disk_bytes = if crate::has_git_conflict_markers(&disk_after_str) {
        disk_after_bytes.clone()
    } else {
        my_own_bytes.clone()
    };
    let mut auto_merged = untangled_cross_branch;
    let mut has_conflict = crate::has_git_conflict_markers(&disk_after_str);
    let mut context_notice = None;
    let mut conflict_to_save: Option<ConflictRecord> = None;
    let mut merged_peer_blobs: Vec<ObjectId> = Vec::new();

    let waiting_peer_baseline_blob: Option<ObjectId> = if my_eff_branch != default_branch {
        current_op
            .view
            .agents
            .iter()
            .filter(|(other_id, other)| {
                is_active_peer(other_id, other)
                    && !is_same_branch_peer(other_id, other)
                    && (other.targets_head_branch(&default_branch)
                        || (!other.owns_path(&normalized_path)
                            && !assigned_own_blobs.contains_key(*other_id)))
            })
            .find_map(|(_, other)| {
                other
                    .own_blobs
                    .get(&normalized_path)
                    .or_else(|| other.read_blobs.get(&normalized_path))
                    .copied()
                    .or(base_blob)
            })
    } else {
        None
    };
    let preserve_head_baseline_for_peer = waiting_peer_baseline_blob.is_some();

    if candidate_blobs.len() >= 2 {
        let (first_id, first_intent, first_blob_id) = &candidate_blobs[0];
        let first_bytes = coord.git().read_blob(first_blob_id)?;
        let mut acc_str = String::from_utf8_lossy(&first_bytes).into_owned();
        let mut acc_blob_id = *first_blob_id;
        let mut acc_label = format!("{first_id} ({first_intent})");
        merged_peer_blobs.push(*first_blob_id);

        for (next_id, next_intent, next_blob_id) in candidate_blobs.iter().skip(1) {
            merged_peer_blobs.push(*next_blob_id);
            let next_bytes = coord.git().read_blob(next_blob_id)?;
            let next_str = String::from_utf8_lossy(&next_bytes);
            let next_label = format!("{next_id} ({next_intent})");

            let merge_res = merge_file_3way(
                &normalized_path,
                &base_str,
                &acc_str,
                &next_str,
                &acc_label,
                &next_label,
            );

            acc_str = merge_res.content;
            acc_blob_id = coord.git().write_blob(acc_str.as_bytes())?;
            acc_label = format!("{acc_label} + {next_label}");
            auto_merged = true;

            if merge_res.has_conflicts {
                has_conflict = true;
                let symbol = merge_res.conflicted_symbols.first().cloned();
                let blob_merge = Merge {
                    removes: vec![base_blob],
                    adds: vec![Some(*first_blob_id), Some(*next_blob_id)],
                };
                let cid =
                    ConflictRecord::compute_id(&normalized_path, symbol.as_deref(), &blob_merge);
                let mut intents = std::collections::BTreeMap::new();
                intents.insert(first_id.clone(), first_intent.clone());
                intents.insert(session_id.to_string(), my_agent.intent.clone());

                let record = ConflictRecord {
                    id: cid,
                    path: normalized_path.clone(),
                    symbol: symbol.clone(),
                    kind: if symbol.is_some() {
                        ConflictKind::AstNode
                    } else {
                        ConflictKind::TextHunk
                    },
                    merge: blob_merge,
                    agent_intents: intents,
                    base_snippet: merge_res.base_conflict_snippet,
                    left_snippet: merge_res.left_conflict_snippet,
                    right_snippet: merge_res.right_conflict_snippet,
                    diagnostics: None,
                    created_at_ms: now_millis(),
                };
                coord.oplog().save_conflict(&record)?;
                conflict_to_save = Some(record);

                context_notice = Some(format!(
                    "[Tessera Conflict]: Concurrent edit on '{}' with session '{}' (Intent: \"{}\") overlapped on {:?}. Inspect with `tes conflicts --json`.",
                    normalized_path, next_id, next_intent, merge_res.conflicted_symbols
                ));
            }
        }

        let default_branch_peer_owns_path = edited_in_branch_sandbox
            && current_op.view.agents.values().any(|other| {
                other.agent_id != session_id
                    && other.targets_head_branch(&default_branch)
                    && other.owns_path(&normalized_path)
            });
        if !default_branch_peer_owns_path && !preserve_head_baseline_for_peer {
            atomic_write_file(&full_path, acc_str.as_bytes())?;
            crate::set_worktree_executable(&full_path, executable);
        } else if let Some(b_id) = waiting_peer_baseline_blob {
            if disk_after_blob != b_id && !crate::has_git_conflict_markers(&disk_after_str) {
                if let Ok(peer_base_bytes) = coord.git().read_blob(&b_id) {
                    let _ = atomic_write_file(&full_path, &peer_base_bytes);
                }
            }
        }

        final_disk_bytes = acc_str.into_bytes();
        final_blob = acc_blob_id;
        if !has_conflict {
            context_notice = Some(format!(
                "[Tessera Auto-Merge]: Cleanly AST-merged concurrent edits to '{}'.",
                normalized_path
            ));
        }
    } else if let Some(b_id) = waiting_peer_baseline_blob {
        if disk_after_blob != b_id && !crate::has_git_conflict_markers(&disk_after_str) {
            if let Ok(peer_base_bytes) = coord.git().read_blob(&b_id) {
                let _ = atomic_write_file(&full_path, &peer_base_bytes);
            }
        }
    } else if my_own_blob != disk_after_blob && !crate::has_git_conflict_markers(&disk_after_str) {
        let default_branch_peer_owns_path = edited_in_branch_sandbox
            && current_op.view.agents.values().any(|other| {
                other.agent_id != session_id
                    && other.targets_head_branch(&default_branch)
                    && other.owns_path(&normalized_path)
            });
        if !default_branch_peer_owns_path {
            let _ = atomic_write_file(&full_path, &my_own_bytes);
            crate::set_worktree_executable(&full_path, executable);
        }
    }

    // If `session_id` has other claimed files in `repo_root` that currently hold a foreign branch
    // session's `own_blobs`, restore `session_id`'s branch version so subsequent `view_file` calls
    // in `session_id` see `session_id`'s own branch state rather than a peer branch's edits.
    for sym in &my_agent.claimed_symbols {
        let claimed_file = sym.split("::").next().unwrap_or(sym.as_str()).trim();
        if claimed_file.is_empty()
            || claimed_file == normalized_path
            || my_agent.deleted_paths.contains(claimed_file)
        {
            continue;
        }

        let Ok((_, claimed_full)) =
            resolve_confined_repo_path(coord.git().repo_root(), claimed_file)
        else {
            continue;
        };
        if !claimed_full.is_file() {
            continue;
        }

        if let Ok(c_bytes) = fs::read(&claimed_full) {
            if let Ok(c_blob) = coord.git().write_blob(&c_bytes) {
                let claimed_base_blob = base_commit_obj_opt
                    .as_ref()
                    .and_then(|c| {
                        coord
                            .git()
                            .read_tree_entry_at_path(&c.tree_id, claimed_file)
                            .ok()
                            .flatten()
                    })
                    .map(|e| e.blob_id);
                let my_expected = my_agent
                    .own_blobs
                    .get(claimed_file)
                    .copied()
                    .or(claimed_base_blob);
                let is_foreign = current_op.view.agents.values().any(|other| {
                    let other_b = other.effective_branch(&default_branch);
                    other.agent_id != session_id
                        && other_b != my_eff_branch
                        && other.own_blobs.get(claimed_file) == Some(&c_blob)
                });
                if is_foreign && my_expected != Some(c_blob) {
                    if let Some(exp_blob) = &my_expected {
                        if let Ok(exp_bytes) = coord.git().read_blob(exp_blob) {
                            let _ = atomic_write_file(&claimed_full, &exp_bytes);
                        }
                    }
                }
            }
        }
    }

    // Whenever `reconciled_head_commit` advances `agent.base_commit`, rebase ALL files in
    // `my_agent.own_blobs` (including `normalized_path`) from `my_agent.base_commit` onto `new_base`
    // so no file in `agent.own_blobs` remains anchored to an older commit after `base_commit` advances.
    let mut rebased_session_own_blobs: Option<std::collections::BTreeMap<String, ObjectId>> = None;
    let mut rebased_session_deleted_paths: Option<std::collections::BTreeSet<String>> = None;
    if !has_conflict {
        if let Some(new_base) = &reconciled_head_commit {
            if let (Ok(old_base_c), Ok(new_base_c)) = (
                coord.git().read_commit(&my_agent.base_commit),
                coord.git().read_commit(new_base),
            ) {
                if let (Ok(old_map), Ok(new_map)) = (
                    coord.git().read_tree_to_map(&old_base_c.tree_id),
                    coord.git().read_tree_to_map(&new_base_c.tree_id),
                ) {
                    let mut candidate_own = my_agent.own_blobs.clone();
                    candidate_own.insert(normalized_path.clone(), my_own_blob);

                    let mut rebased_map = std::collections::BTreeMap::new();
                    let mut all_rebased_cleanly = true;

                    for (p, own_bid) in &candidate_own {
                        let old_bid = old_map.get(p).map(|e| e.blob_id);
                        let new_bid = new_map.get(p).map(|e| e.blob_id);

                        if old_bid == new_bid {
                            if p == &normalized_path || new_bid.as_ref() != Some(own_bid) {
                                rebased_map.insert(p.clone(), *own_bid);
                            }
                            continue;
                        }

                        if p != &normalized_path
                            && (Some(*own_bid) == old_bid || Some(*own_bid) == new_bid)
                        {
                            continue;
                        }

                        if p == &normalized_path
                            && head_changed_norm_path
                            && foreign_blobs.is_empty()
                            && candidate_blobs.len() <= 2
                        {
                            rebased_map.insert(p.clone(), final_blob);
                            continue;
                        }

                        let old_b = old_bid
                            .and_then(|id| coord.git().read_blob(&id).ok())
                            .unwrap_or_default();
                        let new_b = new_bid
                            .and_then(|id| coord.git().read_blob(&id).ok())
                            .unwrap_or_default();
                        let own_b = coord.git().read_blob(own_bid).unwrap_or_default();

                        let reb = merge_file_3way(
                            p,
                            &String::from_utf8_lossy(&old_b),
                            &String::from_utf8_lossy(&new_b),
                            &String::from_utf8_lossy(&own_b),
                            my_eff_branch,
                            session_id,
                        );

                        if reb.has_conflicts || crate::has_git_conflict_markers(&reb.content) {
                            all_rebased_cleanly = false;
                            break;
                        }

                        if let Ok(reb_bid) = coord.git().write_blob(reb.content.as_bytes()) {
                            if p == &normalized_path || new_bid.as_ref() != Some(&reb_bid) {
                                rebased_map.insert(p.clone(), reb_bid);
                            }
                        } else {
                            all_rebased_cleanly = false;
                            break;
                        }
                    }

                    if all_rebased_cleanly {
                        let mut rebased_del = my_agent.deleted_paths.clone();
                        rebased_del.retain(|p| p != &normalized_path && new_map.contains_key(p));
                        rebased_session_own_blobs = Some(rebased_map);
                        rebased_session_deleted_paths = Some(rebased_del);
                    }
                }
            }
        }
    }

    let mut updated_intent = my_agent.intent.clone();
    coord.oplog().record_tx(
        session_id,
        format!("post-tool reconcile {normalized_path}"),
        true,
        |view| {
            let mut next_obs = Vec::new();
            let candidate_obs = view
                .observed_blobs
                .get(&normalized_path)
                .into_iter()
                .flatten()
                .copied()
                .chain(std::iter::once(disk_after_blob));
            for b in candidate_obs {
                let is_owned = assigned_own_blobs.values().any(|ob| *ob == b)
                    || view
                        .agents
                        .values()
                        .any(|a| a.own_blobs.get(&normalized_path) == Some(&b));
                if !is_owned
                    && Some(&b) != base_blob.as_ref()
                    && Some(&b) != base_tree_blob.as_ref()
                    && b != final_blob
                    && b != my_own_blob
                    && !next_obs.contains(&b)
                {
                    next_obs.push(b);
                }
            }

            if !next_obs.contains(&final_blob) {
                next_obs.push(final_blob);
            }

            view.observed_blobs
                .insert(normalized_path.clone(), next_obs);

            for (aid, own_b) in &assigned_own_blobs {
                if let Some(agent) = view.agents.get_mut(aid) {
                    if aid == session_id {
                        if !crate::has_git_conflict_markers(&current_my_str) {
                            agent.own_blobs.insert(normalized_path.clone(), *own_b);
                            agent.deleted_paths.remove(&normalized_path);
                        }
                    } else if !agent.deleted_paths.contains(&normalized_path) {
                        agent
                            .own_blobs
                            .entry(normalized_path.clone())
                            .or_insert(*own_b);
                    }
                }
            }

            for agent in view.agents.values_mut() {
                let contributed = agent
                    .read_blobs
                    .get(&normalized_path)
                    .is_some_and(|b| merged_peer_blobs.contains(b));
                let same_branch = agent.effective_branch(&default_branch) == my_eff_branch;
                if agent.agent_id == session_id || (auto_merged && contributed) {
                    agent.read_blobs.insert(normalized_path.clone(), final_blob);
                } else if resolving_existing_conflict && same_branch {
                    if agent.own_blobs.contains_key(&normalized_path) {
                        agent.own_blobs.insert(normalized_path.clone(), final_blob);
                    }
                    if agent.read_blobs.contains_key(&normalized_path) {
                        agent.read_blobs.insert(normalized_path.clone(), final_blob);
                    }
                }
            }

            if let Some(agent) = view.agents.get_mut(session_id) {
                if let (Some(new_base), Some(rebased_map)) = (
                    reconciled_head_commit.as_ref(),
                    rebased_session_own_blobs.as_ref(),
                ) {
                    agent.base_commit = *new_base;
                    agent.own_blobs = rebased_map.clone();
                    if let Some(rebased_del) = &rebased_session_deleted_paths {
                        agent.deleted_paths = rebased_del.clone();
                    }
                }

                if !peers_integrated_now.is_empty() {
                    let entry = agent
                        .integrated_peer_blobs
                        .entry(normalized_path.clone())
                        .or_default();
                    for (pid, pblob) in &peers_integrated_now {
                        entry.insert(pid.clone(), *pblob);
                    }
                }

                for sym in &touched_syms {
                    agent
                        .claimed_symbols
                        .insert(format!("{}::{}", normalized_path, sym.label));
                }

                if agent.intent_inferred && !touched_syms.is_empty() {
                    let sym_list = touched_syms
                        .iter()
                        .take(3)
                        .map(|s| s.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    agent.intent = format!("Update {normalized_path} ({sym_list})");
                }
                updated_intent = agent.intent.clone();
                agent.updated_at_ms = now_millis();
            }

            if let Some(conflict) = &conflict_to_save {
                view.conflicts
                    .insert(normalized_path.clone(), conflict.id.clone());
            } else if !crate::has_git_conflict_markers(&String::from_utf8_lossy(&final_disk_bytes)) {
                if let Some(old_cid) = view.conflicts.remove(&normalized_path) {
                    coord.oplog().delete_conflict(&old_cid);
                }

                if !view.conflicts.keys().any(|k| k != "<speculative-verify>") {
                    if let Some(spec_cid) = view.conflicts.remove("<speculative-verify>") {
                        coord.oplog().delete_conflict(&spec_cid);
                    }
                }
            }
            Ok(())
        },
    )?;

    // Update the session's Git commit/tree in O(depth) without walking the working directory.
    let final_own_blob = rebased_session_own_blobs
        .as_ref()
        .and_then(|m| m.get(&normalized_path).copied())
        .unwrap_or(my_own_blob);
    if !crate::has_git_conflict_markers(&current_my_str) {
        let _ = coord.record_single_path_edit(
            session_id,
            &normalized_path,
            Some(FileEntry {
                blob_id: final_own_blob,
                executable,
            }),
        )?;
    }

    for (aid, own_b) in &assigned_own_blobs {
        if aid != session_id {
            let _ = coord.record_single_path_edit(
                aid,
                &normalized_path,
                Some(FileEntry {
                    blob_id: *own_b,
                    executable,
                }),
            );

            if let Some(peer_meta) = current_op.view.agents.get(aid) {
                let peer_branch = peer_meta.effective_branch(&default_branch);
                let peer_ws = coord.branch_sandbox_dir(peer_branch);
                if peer_ws.is_dir() {
                    if let Ok(peer_bytes) = coord.git().read_blob(own_b) {
                        crate::write_worktree_rel(&peer_ws, &normalized_path, &peer_bytes, executable);
                    }
                }
            }
        }
    }

    // Live-sync the edited file into the session's target branch sandbox (if materialized)
    // so any running dev server on that branch hot-reloads immediately without affecting other branches.
    let eff_branch = my_eff_branch.to_string();
    let branch_ws = coord.branch_sandbox_dir(&eff_branch);
    if branch_ws.is_dir() {
        let ws_bytes = if eff_branch == default_branch {
            &final_disk_bytes
        } else {
            &my_own_bytes
        };
        crate::write_worktree_rel(&branch_ws, &normalized_path, ws_bytes, executable);
    }

    // If invoked from inside another sandbox (such as `branches/main`), keep that sandbox file synchronized too.
    if let Some(inv_dir) = coord.invocation_dir() {
        if inv_dir != coord.git().repo_root() && inv_dir != branch_ws {
            crate::write_worktree_rel(inv_dir, &normalized_path, &final_disk_bytes, executable);
        }
    }

    Ok(PostToolOutcome {
        session_id: session_id.to_string(),
        path: normalized_path,
        auto_merged_concurrent_edit: auto_merged,
        has_conflict,
        context_notice,
        session_intent: updated_intent,
    })
}

/// Expands `tes hook post-tool` on a directory path into per-file reconciliation.
///
/// Harnesses report directory-level operations (`mv dirA dirB`, `rm -r dir`, `cp -r src dst`) with
/// the directory path. Treating that path as one file would drop every file under it from the
/// session tree without per-file tombstones (so the next rebuild resurrects them) and would never
/// record the files that appeared under the destination directory.
///
/// Returns `Ok(None)` when `normalized_path` is neither an existing directory nor a vanished
/// directory that held tracked or session-owned files, so the caller handles it as a single file.
///
/// # Errors
/// Propagates any error from reconciling an individual file under the directory.
fn reconcile_directory_post_tool(
    coord: &Coordinator,
    session_id: &str,
    normalized_path: &str,
) -> CoreResult<Option<PostToolOutcome>> {
    let repo_root = coord.git().repo_root();
    let full_path = repo_root.join(normalized_path);
    let prefix = format!("{normalized_path}/");

    let current = coord.oplog().load_current_reconciled()?;
    let Some(meta) = current.view.agents.get(session_id) else {
        return Ok(None);
    };
    let base_map = coord
        .git()
        .read_commit(&meta.base_commit)
        .and_then(|c| coord.git().read_tree_to_map(&c.tree_id))
        .unwrap_or_default();

    let mut paths: std::collections::BTreeSet<String> = base_map
        .keys()
        .chain(meta.own_blobs.keys())
        .filter(|p| p.starts_with(&prefix))
        .cloned()
        .collect();

    if full_path.is_dir() {
        collect_worktree_files(repo_root, coord.git().git_dir(), &full_path, &base_map, &mut paths);
    } else if full_path.exists() || paths.is_empty() {
        return Ok(None);
    }

    let mut outcome = PostToolOutcome {
        session_id: session_id.to_string(),
        path: normalized_path.to_string(),
        auto_merged_concurrent_edit: false,
        has_conflict: false,
        context_notice: None,
        session_intent: meta.intent.clone(),
    };
    let mut notices = Vec::new();

    for rel_path in &paths {
        let file_outcome = handle_post_tool_hook(coord, session_id, rel_path)?;
        outcome.auto_merged_concurrent_edit |= file_outcome.auto_merged_concurrent_edit;
        outcome.has_conflict |= file_outcome.has_conflict;
        outcome.session_intent = file_outcome.session_intent;
        if let Some(notice) = file_outcome.context_notice {
            notices.push(notice);
        }
    }

    if !notices.is_empty() {
        outcome.context_notice = Some(notices.join("\n"));
    }

    Ok(Some(outcome))
}

/// Recursively collects repository-relative paths of regular files under `dir`, skipping symbolic
/// links and untracked paths excluded by `.gitignore`, `.git/info/exclude` or built-in secret rules.
fn collect_worktree_files(
    repo_root: &Path,
    git_dir: &Path,
    dir: &Path,
    base_map: &std::collections::BTreeMap<String, FileEntry>,
    out: &mut std::collections::BTreeSet<String>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Ok(rel) = path.strip_prefix(repo_root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if file_type.is_dir() {
            let dir_prefix = format!("{rel_str}/");
            let tracked = base_map.keys().any(|k| k.starts_with(&dir_prefix));
            if !is_ignored_or_secret_path(repo_root, git_dir, &rel_str, tracked) {
                collect_worktree_files(repo_root, git_dir, &path, base_map, out);
            }
        } else if file_type.is_file()
            && !is_ignored_or_secret_path(repo_root, git_dir, &rel_str, base_map.contains_key(&rel_str))
        {
            out.insert(rel_str);
        }
    }
}

/// Returns `true` if `trimmed` is a short delimiter or standard single-file component wrapper tag
/// that should not be treated as a substantive peer-only addition during cross-branch untangling.
fn is_trivial_boilerplate_line(trimmed: &str) -> bool {
    trimmed.len() <= 3
        || matches!(
            trimmed,
            "<template>"
                | "</template>"
                | "<script>"
                | "<script setup>"
                | "<script setup lang=\"ts\">"
                | "<script lang=\"ts\">"
                | "</script>"
                | "<style>"
                | "<style scoped>"
                | "</style>"
        )
}

/// Returns `true` if `current_my_str` contains at least two-thirds of the substantive lines
/// introduced by `peer_str` (relative to `base_str` and `my_prev_str`), indicating that the
/// caller's edit was authored on top of a foreign branch peer's uncommitted disk buffer.
fn is_contaminated_by_foreign_peer(
    base_str: &str,
    my_prev_str: &str,
    peer_str: &str,
    current_my_str: &str,
) -> bool {
    let baseline_lines: std::collections::BTreeSet<&str> = base_str
        .lines()
        .chain(my_prev_str.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let my_lines: std::collections::BTreeSet<&str> = current_my_str
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let peer_substantive_added: Vec<&str> = peer_str
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty() && !baseline_lines.contains(l) && !is_trivial_boilerplate_line(l)
        })
        .collect();

    if peer_substantive_added.is_empty() {
        return false;
    }

    let matched = peer_substantive_added
        .iter()
        .filter(|l| my_lines.contains(**l))
        .count();
    matched * 3 >= peer_substantive_added.len() * 2
}

/// Strips contiguous hunks inserted by a foreign peer (`peer_str` relative to `base_str` and
/// `my_prev_str`) from `disk_after_str` using LCS line alignment, handling adjacent insertions
/// (such as two sessions appending to the end of `styles.css`, `app.js`, or before `</main>` in
/// `index.html`) where a standard 3-way merge reports an adjacent-hunk conflict, while never
/// stripping coincidental matching lines from full-file replacements on separate branches.
fn strip_peer_inserted_lines(
    base_str: &str,
    my_prev_str: &str,
    peer_str: &str,
    disk_after_str: &str,
) -> Option<String> {
    let b_lines: Vec<&str> = base_str.lines().collect();
    let m_lines: Vec<&str> = my_prev_str.lines().collect();
    let p_lines: Vec<&str> = peer_str.lines().collect();
    let d_lines: Vec<&str> = disk_after_str.lines().collect();

    if p_lines.is_empty() || d_lines.is_empty() {
        return None;
    }

    let bp_pairs = lcs_line_pairs(&b_lines, &p_lines);
    let mp_pairs = lcs_line_pairs(&m_lines, &p_lines);
    let bd_pairs = lcs_line_pairs(&b_lines, &d_lines);
    let mut b_in_p = vec![false; b_lines.len()];
    let mut in_base_or_mine = vec![false; p_lines.len()];
    for &(b_idx, p_idx) in &bp_pairs {
        b_in_p[b_idx] = true;
        in_base_or_mine[p_idx] = true;
    }

    for &(_, p_idx) in &mp_pairs {
        in_base_or_mine[p_idx] = true;
    }

    let mut b_in_d = vec![false; b_lines.len()];
    for &(b_idx, _) in &bd_pairs {
        b_in_d[b_idx] = true;
    }

    let pd_pairs = lcs_line_pairs(&p_lines, &d_lines);
    let mut p_to_d: Vec<Option<usize>> = vec![None; p_lines.len()];
    for (p_idx, d_idx) in pd_pairs {
        p_to_d[p_idx] = Some(d_idx);
    }

    let mut drop_d = vec![false; d_lines.len()];
    let mut restore_at_d: Vec<Vec<&str>> = vec![Vec::new(); d_lines.len()];
    let mut b_restored = vec![false; b_lines.len()];
    let mut dropped_any = false;
    let mut p_idx = 0;
    while p_idx < p_lines.len() {
        if in_base_or_mine[p_idx] {
            p_idx += 1;
            continue;
        }

        let p_start = p_idx;
        while p_idx < p_lines.len() && !in_base_or_mine[p_idx] {
            p_idx += 1;
        }
        let p_end = p_idx;
        let hunk_len = p_end - p_start;

        let has_substantive = (p_start..p_end)
            .any(|i| !is_trivial_boilerplate_line(p_lines[i].trim()));
        if !has_substantive {
            continue;
        }

        if let (Some(d_first), Some(d_last)) = (p_to_d[p_start], p_to_d[p_end - 1]) {
            let all_matched = (p_start..p_end).all(|i| p_to_d[i].is_some());
            if all_matched && d_last + 1 == d_first + hunk_len {
                for flag in &mut drop_d[d_first..(d_first + hunk_len)] {
                    *flag = true;
                }

                let bp_range_start = bp_pairs
                    .iter()
                    .rev()
                    .find(|&&(_, pi)| pi < p_start)
                    .map_or(0, |&(bi, _)| bi + 1);
                let bp_range_end = bp_pairs
                    .iter()
                    .find(|&&(_, pi)| pi >= p_end)
                    .map_or(b_lines.len(), |&(bi, _)| bi);
                let bd_range_start = bd_pairs
                    .iter()
                    .rev()
                    .find(|&&(_, di)| di < d_first)
                    .map_or(0, |&(bi, _)| bi + 1);
                let bd_range_end = bd_pairs
                    .iter()
                    .find(|&&(_, di)| di >= d_first + hunk_len)
                    .map_or(b_lines.len(), |&(bi, _)| bi);
                let rep_start = bp_range_start.max(bd_range_start);
                let rep_end = bp_range_end.min(bd_range_end);
                for b_idx in rep_start..rep_end {
                    if !b_in_p[b_idx] && !b_in_d[b_idx] && !b_restored[b_idx] {
                        restore_at_d[d_first].push(b_lines[b_idx]);
                        b_restored[b_idx] = true;
                    }
                }
                dropped_any = true;
            }
        }
    }

    if !dropped_any {
        return None;
    }

    let mut out_lines: Vec<&str> = Vec::with_capacity(d_lines.len());
    let mut dropped_since_last_kept = false;
    for (idx, line) in d_lines.iter().enumerate() {
        for &restored_line in &restore_at_d[idx] {
            out_lines.push(restored_line);
        }

        if drop_d[idx] {
            dropped_since_last_kept = true;
            continue;
        }

        if dropped_since_last_kept {
            let trimmed = line.trim();
            if !trimmed.is_empty()
                && !matches!(trimmed, "}" | "};" | ")" | ");" | "]" | "];")
                && out_lines.last().copied() == Some(*line)
            {
                let had_consecutive_in_base =
                    b_lines.windows(2).any(|w| w[0] == *line && w[1] == *line);
                if !had_consecutive_in_base {
                    dropped_since_last_kept = false;
                    continue;
                }
            }
            dropped_since_last_kept = false;
        }
        out_lines.push(*line);
    }

    let mut out = out_lines.join("\n");
    if disk_after_str.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    Some(out)
}

/// Computes exact longest common subsequence (LCS) index pairs on a bounded slice pair.
fn lcs_small_slice(
    a: &[&str],
    b: &[&str],
    a_offset: usize,
    b_offset: usize,
    out: &mut Vec<(usize, usize)>,
) {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            if a[i] == b[j] {
                dp[i][j] = 1 + dp[i + 1][j + 1];
            } else {
                dp[i][j] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((a_offset + i, b_offset + j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
}

/// Computes the longest common subsequence (LCS) index pairs `(a_idx, b_idx)` between two line slices
/// in $O(N + M)$ memory using `imara_diff`'s Histogram algorithm with bounded intra-hunk refinement.
fn lcs_line_pairs(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    let n = a.len();
    let m = b.len();
    if n == 0 || m == 0 {
        return Vec::new();
    }

    let mut pairs = Vec::with_capacity(n.min(m));
    if n.saturating_mul(m) <= 16_384 {
        lcs_small_slice(a, b, 0, 0, &mut pairs);
        return pairs;
    }

    let mut a_text = String::new();
    for line in a {
        a_text.push_str(line);
        a_text.push('\n');
    }

    let mut b_text = String::new();
    for line in b {
        b_text.push_str(line);
        b_text.push('\n');
    }

    let input = InternedInput::new(a_text.as_str(), b_text.as_str());
    let computed_diff = Diff::compute(Algorithm::Histogram, &input);

    let mut a_idx = 0usize;
    let mut b_idx = 0usize;
    for hunk in computed_diff.hunks() {
        let a_hunk_start = hunk.before.start as usize;
        let a_hunk_end = hunk.before.end as usize;
        let b_hunk_start = hunk.after.start as usize;
        let b_hunk_end = hunk.after.end as usize;

        while a_idx < a_hunk_start && b_idx < b_hunk_start {
            pairs.push((a_idx, b_idx));
            a_idx += 1;
            b_idx += 1;
        }

        let hunk_n = a_hunk_end.saturating_sub(a_hunk_start);
        let hunk_m = b_hunk_end.saturating_sub(b_hunk_start);
        if hunk_n > 0 && hunk_m > 0 && hunk_n.saturating_mul(hunk_m) <= 16_384 {
            lcs_small_slice(
                &a[a_hunk_start..a_hunk_end],
                &b[b_hunk_start..b_hunk_end],
                a_hunk_start,
                b_hunk_start,
                &mut pairs,
            );
        }

        a_idx = a_hunk_end;
        b_idx = b_hunk_end;
    }

    while a_idx < n && b_idx < m {
        pairs.push((a_idx, b_idx));
        a_idx += 1;
        b_idx += 1;
    }

    pairs
}

/// Scores how strongly the added/modified lines in `candidate_str` (relative to `base_str`)
/// correlate with an agent's `intent` and `target_branch` keywords.
fn score_blob_for_agent(
    base_str: &str,
    candidate_str: &str,
    intent: &str,
    target_branch: Option<&str>,
) -> usize {
    let base_lines: std::collections::BTreeSet<&str> = base_str.lines().map(str::trim).collect();
    let mut added_lower = String::new();
    for line in candidate_str.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !base_lines.contains(trimmed) {
            added_lower.push_str(&trimmed.to_ascii_lowercase());
            added_lower.push(' ');
        }
    }

    if added_lower.is_empty() {
        return 0;
    }

    let stop_words = [
        "add",
        "optional",
        "parameter",
        "function",
        "update",
        "the",
        "and",
        "for",
        "into",
        "with",
        "from",
        "feat",
        "fix",
        "chore",
        "src",
        "app",
    ];
    let combined_meta = format!("{} {}", intent, target_branch.unwrap_or_default()).to_ascii_lowercase();
    let mut keywords = std::collections::BTreeSet::new();
    for token in combined_meta.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.len() >= 3 && !stop_words.contains(&token) {
            keywords.insert(token);
        }
    }

    keywords
        .into_iter()
        .filter(|kw| added_lower.contains(kw))
        .count()
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
        std::process::Command::new("tasklist")
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

/// Attempts to acquire a non-blocking exclusive advisory file lock on `lock_path`.
///
/// Returns `Some(File)` if this process acquired the lock, or `None` if another watcher
/// process currently holds the lock.
fn try_acquire_watcher_lock(lock_path: &Path) -> Option<fs::File> {
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `file.as_raw_fd()` is a valid open file descriptor owned by `file`.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return None;
        }
    }

    Some(file)
}

/// Ensures a background file watcher (`tes watch --auto-daemon`) is running for `repo_root`
/// so rapid concurrent file writes are captured in `.git/objects` before `post-tool` hooks execute.
pub fn ensure_background_watcher(repo_root: &Path, git_dir: &Path) {
    #[cfg(unix)]
    {
        let lock_file = git_dir.join("tessera").join("watch.lock");
        let probe = try_acquire_watcher_lock(&lock_file);
        if probe.is_none() {
            return;
        }
        drop(probe);

        let pid_file = git_dir.join("tessera").join("watch.pid");
        if let Ok(raw_pid) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = raw_pid.trim().parse::<i32>() {
                if is_process_alive(pid) {
                    return;
                }
            }
        }

        let exe_opt = std::env::current_exe().ok().filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem == "tes")
        });

        if let Some(bin) = exe_opt {
            use std::os::unix::process::CommandExt;

            let mut cmd = std::process::Command::new(bin);
            cmd.args(["watch", "--auto-daemon"])
                .current_dir(repo_root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0);

            let _ = cmd.spawn();
        }
    }

    #[cfg(not(unix))]
    let _ = (repo_root, git_dir);
}

/// Runs the lightweight background micro-watcher that records intermediate disk blobs for
/// claimed files into `view.observed_blobs`, exiting automatically once all active sessions
/// close or after 90 seconds.
///
/// # Errors
/// Returns [`CoreError`] if reading or updating the `OpLog` fails.
pub fn run_auto_watch_daemon(coord: &Coordinator) -> CoreResult<()> {
    let lock_file = coord.git().git_dir().join("tessera").join("watch.lock");
    let Some(_lock_guard) = try_acquire_watcher_lock(&lock_file) else {
        return Ok(());
    };

    let pid_file = coord.git().git_dir().join("tessera").join("watch.pid");
    let my_pid = std::process::id().to_string();
    let _ = atomic_write_file(&pid_file, my_pid.as_bytes());

    let poll_ms = coord
        .oplog()
        .load_config()
        .map(|c| c.watch_interval_ms.max(15))
        .unwrap_or(DEFAULT_WATCH_INTERVAL_MS);
    let max_ticks = (90_000 / poll_ms).max(1);

    let mut last_stat: std::collections::BTreeMap<String, (u64, u64, u32, ObjectId)> =
        std::collections::BTreeMap::new();

    for _ in 0..max_ticks {
        let Ok(current) = coord.oplog().load_current_reconciled() else {
            std::thread::sleep(std::time::Duration::from_millis(poll_ms));
            continue;
        };

        if current.view.agents.is_empty() {
            break;
        }

        let default_branch = coord
            .git()
            .current_branch_name()
            .unwrap_or_else(|_| "main".to_string());

        let mut watched_paths = std::collections::BTreeSet::new();
        for agent in current.view.agents.values() {
            for p in agent.read_blobs.keys() {
                watched_paths.insert(p.clone());
            }

            for sym in &agent.claimed_symbols {
                let file_part = sym.split("::").next().unwrap_or(sym.as_str()).trim();
                if !file_part.is_empty() {
                    watched_paths.insert(file_part.to_string());
                }
            }
        }

        for rel_path in watched_paths {
            let Ok((clean_rel, full_path)) =
                resolve_confined_repo_path(coord.git().repo_root(), &rel_path)
            else {
                continue;
            };

            let Ok(meta) = fs::symlink_metadata(&full_path) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }

            let size = meta.len();
            let (mtime_secs, mtime_nanos) = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| (d.as_secs(), d.subsec_nanos()))
                .unwrap_or((0, 0));

            if let Some(&(prev_size, prev_secs, prev_nanos, prev_blob)) = last_stat.get(&clean_rel)
            {
                if prev_size == size && prev_secs == mtime_secs && prev_nanos == mtime_nanos {
                    let already_recorded = current
                        .view
                        .observed_blobs
                        .get(&clean_rel)
                        .is_some_and(|list| list.contains(&prev_blob));
                    if already_recorded {
                        continue;
                    }
                }
            }

            let is_tracked = current
                .view
                .agents
                .values()
                .any(|a| a.read_blobs.contains_key(&clean_rel));
            if is_ignored_or_secret_path(
                coord.git().repo_root(),
                coord.git().git_dir(),
                &clean_rel,
                is_tracked,
            ) {
                continue;
            }

            let Ok(bytes) = fs::read(&full_path) else {
                continue;
            };

            let Ok(blob_id) = coord.git().write_blob(&bytes) else {
                continue;
            };

            last_stat.insert(clean_rel.clone(), (size, mtime_secs, mtime_nanos, blob_id));

            let already_recorded = current
                .view
                .observed_blobs
                .get(&clean_rel)
                .is_some_and(|list| list.contains(&blob_id));
            if !already_recorded {
                let _ = coord.oplog().record_tx(
                    "watcher",
                    format!("observe intermediate blob for {clean_rel}"),
                    true,
                    |view| {
                        let list = view.observed_blobs.entry(clean_rel.clone()).or_default();
                        if !list.contains(&blob_id) {
                            if list.len() >= MAX_OBSERVED_BLOBS_PER_PATH {
                                list.remove(0);
                            }
                            list.push(blob_id);
                        }
                        Ok(())
                    },
                );
            }

            let active_on_path: Vec<&core::AgentMetadata> = current
                .view
                .agents
                .values()
                .filter(|a| {
                    let is_passive = (a.agent_id.starts_with("term-")
                        || a.agent_id.starts_with("session-")
                        || a.agent_id == "editor")
                        && a.intent_inferred
                        && a.claimed_symbols.is_empty()
                        && !a.has_own_changes();
                    !is_passive
                        && (a.owns_path(&clean_rel)
                            || a.read_blobs.contains_key(&clean_rel)
                            || a.claimed_symbols.iter().any(|s| {
                                s.split("::").next().unwrap_or(s.as_str()).trim() == clean_rel
                            }))
                })
                .collect();

            let distinct_branches: std::collections::BTreeSet<&str> = active_on_path
                .iter()
                .map(|a| a.effective_branch(&default_branch))
                .collect();

            if distinct_branches.len() >= 2
                && !active_on_path
                    .iter()
                    .any(|a| a.targets_head_branch(&default_branch))
            {
                let baseline_opt = active_on_path
                    .iter()
                    .filter(|a| !a.owns_path(&clean_rel))
                    .find_map(|a| a.read_blobs.get(&clean_rel).copied());

                if let Some(base_b) = baseline_opt {
                    if base_b != blob_id {
                        if let Ok(base_bytes) = coord.git().read_blob(&base_b) {
                            if atomic_write_file(&full_path, &base_bytes).is_ok() {
                                if let Ok(restored_meta) = fs::symlink_metadata(&full_path) {
                                    let r_size = restored_meta.len();
                                    let (r_secs, r_nanos) = restored_meta
                                        .modified()
                                        .ok()
                                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                        .map(|d| (d.as_secs(), d.subsec_nanos()))
                                        .unwrap_or((0, 0));
                                    last_stat.insert(
                                        clean_rel.clone(),
                                        (r_size, r_secs, r_nanos, base_b),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(poll_ms));
    }

    let _ = fs::remove_file(&pid_file);
    Ok(())
}

/// Normalizes and validates an absolute or relative path (including paths inside a Copy-on-Write
/// branch sandbox) into a confined forward-slash repository-relative path.
///
/// # Errors
/// Returns [`CoreError::InvalidArgument`] if `raw_path` escapes `repo_root`, targets `.git`,
/// or traverses a symlink outside `repo_root`.
fn normalize_repo_rel_path(repo_root: &Path, raw_path: &str) -> CoreResult<String> {
    let p = Path::new(raw_path);
    if p.is_absolute() {
        if let Ok(stripped) = p.strip_prefix(repo_root) {
            let rel = stripped.to_string_lossy().replace('\\', "/");
            let (norm, _) = resolve_confined_repo_path(repo_root, &rel)?;
            return Ok(norm);
        }

        let mut anc = p.parent();
        while let Some(dir) = anc {
            if is_valid_sandbox_repo_root(dir, Some(repo_root)).is_some() {
                if let Ok(stripped) = p.strip_prefix(dir) {
                    let rel = stripped.to_string_lossy().replace('\\', "/");
                    let _ = resolve_confined_repo_path(dir, &rel)?;
                    let (norm, _) = resolve_confined_repo_path(repo_root, &rel)?;
                    return Ok(norm);
                }
            }
            anc = dir.parent();
        }

        return Err(CoreError::InvalidArgument(format!(
            "absolute path '{}' is outside repository root '{}'",
            raw_path,
            repo_root.display()
        )));
    }

    let (norm, _) = resolve_confined_repo_path(repo_root, raw_path)?;
    Ok(norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_eof_edits_on_separate_branches_stay_isolated_and_preserve_harness_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        // Simulate an existing repo that already has an initial commit BEFORE `tes init` is run.
        let git = core::GitBackend::init_or_open(root).expect("init git");
        let base_css = "body {\n  margin: 0;\n}\n";
        let base_html = "<main>\n  <section id=\"hero\">Hero</section>\n</main>\n";
        fs::write(root.join("styles.css"), base_css).expect("write styles.css");
        fs::write(root.join("index.html"), base_html).expect("write index.html");
        let snap = workspace::snapshot_working_dir(&git, root, "init", &[], None).expect("snap");
        let author = git.resolve_git_author();
        let c0 = git
            .write_commit(&snap.root_tree_id, &[], &author, "Initial commit")
            .expect("commit");
        git.update_ref("refs/heads/main", &c0).expect("update main");
        git.sync_git_index(&c0);

        // Now run `tes init` on the existing repo (so .agents/rules/tessera.md is untracked).
        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");
        assert!(root.join(".agents/rules/tessera.md").is_file());

        // Agent 1 claims `feat/pricing-calculator` and Agent 2 claims `feat/testimonials-faq`.
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-pricing",
                Some("Add pricing calculator"),
                &["styles.css".to_string(), "index.html".to_string()],
                Some("feat/pricing-calculator"),
            )
            .expect("claim pricing");
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-faq",
                Some("Add testimonials and FAQ"),
                &["styles.css".to_string(), "index.html".to_string()],
                Some("feat/testimonials-faq"),
            )
            .expect("claim faq");

        // Agent 1 appends `.pricing` to `styles.css` and inserts `<section id="pricing">` before `</main>`.
        let css_after_a1 = "body {\n  margin: 0;\n}\n.pricing {\n  display: grid;\n}\n";
        fs::write(root.join("styles.css"), css_after_a1).expect("write a1 css");
        handle_post_tool_hook(&coord, "agent-pricing", "styles.css").expect("post-tool a1 css");

        let html_after_a1 = "<main>\n  <section id=\"hero\">Hero</section>\n  <section id=\"pricing\">Pricing</section>\n</main>\n";
        fs::write(root.join("index.html"), html_after_a1).expect("write a1 html");
        handle_post_tool_hook(&coord, "agent-pricing", "index.html").expect("post-tool a1 html");

        // Agent 2 reads the file on disk (which already contains Agent 1's lines!) and appends right after Agent 1's lines.
        let css_after_a2 = "body {\n  margin: 0;\n}\n.pricing {\n  display: grid;\n}\n.testimonials {\n  display: flex;\n}\n";
        fs::write(root.join("styles.css"), css_after_a2).expect("write a2 css");
        handle_post_tool_hook(&coord, "agent-faq", "styles.css").expect("post-tool a2 css");

        let html_after_a2 = "<main>\n  <section id=\"hero\">Hero</section>\n  <section id=\"pricing\">Pricing</section>\n  <section id=\"testimonials\">Testimonials</section>\n</main>\n";
        fs::write(root.join("index.html"), html_after_a2).expect("write a2 html");
        handle_post_tool_hook(&coord, "agent-faq", "index.html").expect("post-tool a2 html");

        // Both agents commit to their respective branches.
        coord
            .merge_agents_with_message(
                &["agent-pricing".to_string()],
                Some("feat/pricing-calculator"),
                None,
                false,
                Some("feat: add pricing calculator"),
            )
            .expect("commit pricing");
        coord
            .merge_agents_with_message(
                &["agent-faq".to_string()],
                Some("feat/testimonials-faq"),
                None,
                false,
                Some("feat: add testimonials and faq"),
            )
            .expect("commit faq");

        // Verify harness files (`.agents/rules/tessera.md`) were NOT deleted by materialization!
        assert!(root.join(".agents/rules/tessera.md").is_file());

        // Verify `feat/pricing-calculator` has ONLY pricing and NOT testimonials.
        let (_, pricing_ws, _) = coord
            .prepare_branch_sandbox("term-1", Some("feat/pricing-calculator"))
            .expect("pricing sandbox");
        let pricing_css = fs::read_to_string(pricing_ws.join("styles.css")).expect("read pricing css");
        let pricing_html = fs::read_to_string(pricing_ws.join("index.html")).expect("read pricing html");
        assert!(pricing_css.contains(".pricing"), "pricing_css: {pricing_css}");
        assert!(!pricing_css.contains(".testimonials"), "pricing_css leaked testimonials: {pricing_css}");
        assert!(pricing_html.contains("id=\"pricing\""), "pricing_html: {pricing_html}");
        assert!(!pricing_html.contains("id=\"testimonials\""), "pricing_html leaked testimonials: {pricing_html}");

        // Verify `feat/testimonials-faq` has ONLY testimonials and NOT pricing.
        let (_, faq_ws, _) = coord
            .prepare_branch_sandbox("term-2", Some("feat/testimonials-faq"))
            .expect("faq sandbox");
        let faq_css = fs::read_to_string(faq_ws.join("styles.css")).expect("read faq css");
        let faq_html = fs::read_to_string(faq_ws.join("index.html")).expect("read faq html");
        assert!(faq_css.contains(".testimonials"), "faq_css: {faq_css}");
        assert!(!faq_css.contains(".pricing"), "faq_css leaked pricing: {faq_css}");
        assert!(faq_html.contains("id=\"testimonials\""), "faq_html: {faq_html}");
        assert!(!faq_html.contains("id=\"pricing\""), "faq_html leaked pricing: {faq_html}");
    }

    #[test]
    fn cross_branch_vue_edits_never_conflict_and_late_node_modules_syncs_to_sandbox() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let git = core::GitBackend::init_or_open(root).expect("init git");
        fs::create_dir_all(root.join("src/components")).expect("mkdir src/components");
        let base_app_vue = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\n</script>\n\n<template>\n  <HelloWorld />\n  <div class=\"ticks\"></div>\n  <section id=\"spacer\"></section>\n</template>\n";
        fs::write(root.join("src/App.vue"), base_app_vue).expect("write App.vue");
        let snap = workspace::snapshot_working_dir(&git, root, "init", &[], None).expect("snap");
        let author = git.resolve_git_author();
        let c0 = git
            .write_commit(&snap.root_tree_id, &[], &author, "Initial commit")
            .expect("commit");
        git.update_ref("refs/heads/main", &c0).expect("update main");
        git.sync_git_index(&c0);

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        // Both agents claim their branches BEFORE `node_modules` exists in `root`.
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-pricing",
                Some("Add pricing calculator"),
                &[
                    "src/components/PricingCalculator.vue".to_string(),
                    "src/App.vue".to_string(),
                ],
                Some("feat/pricing-calculator"),
            )
            .expect("claim pricing");
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-faq",
                Some("Add testimonials and FAQ"),
                &[
                    "src/components/TestimonialsSection.vue".to_string(),
                    "src/App.vue".to_string(),
                ],
                Some("feat/testimonials-faq"),
            )
            .expect("claim faq");

        // Now `npm install` runs in `root`, creating `node_modules/.bin/vue-tsc`.
        fs::create_dir_all(root.join("node_modules/.bin")).expect("mkdir node_modules/.bin");
        fs::write(root.join("node_modules/.bin/vue-tsc"), "#!/bin/sh\nexit 0\n")
            .expect("write vue-tsc");

        // Agent 1 creates PricingCalculator.vue and edits App.vue.
        fs::write(
            root.join("src/components/PricingCalculator.vue"),
            "<template><div>Pricing</div></template>\n",
        )
        .expect("write PricingCalculator.vue");
        handle_post_tool_hook(
            &coord,
            "agent-pricing",
            "src/components/PricingCalculator.vue",
        )
        .expect("post-tool PricingCalculator.vue");

        let app_after_a1 = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\nimport PricingCalculator from './components/PricingCalculator.vue'\n</script>\n\n<template>\n  <HelloWorld />\n  <div class=\"ticks\"></div>\n  <PricingCalculator />\n  <div class=\"ticks\"></div>\n  <section id=\"spacer\"></section>\n</template>\n";
        fs::write(root.join("src/App.vue"), app_after_a1).expect("write a1 App.vue");
        let a1_outcome =
            handle_post_tool_hook(&coord, "agent-pricing", "src/App.vue").expect("post-tool a1");
        assert!(!a1_outcome.has_conflict, "a1 should not conflict");

        // Agent 2 creates TestimonialsSection.vue and edits App.vue on top of disk (which has PricingCalculator).
        fs::write(
            root.join("src/components/TestimonialsSection.vue"),
            "<template><div>Testimonials</div></template>\n",
        )
        .expect("write TestimonialsSection.vue");
        handle_post_tool_hook(
            &coord,
            "agent-faq",
            "src/components/TestimonialsSection.vue",
        )
        .expect("post-tool TestimonialsSection.vue");

        let app_after_a2 = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\nimport PricingCalculator from './components/PricingCalculator.vue'\nimport TestimonialsSection from './components/TestimonialsSection.vue'\n</script>\n\n<template>\n  <HelloWorld />\n  <div class=\"ticks\"></div>\n  <PricingCalculator />\n  <div class=\"ticks\"></div>\n  <TestimonialsSection />\n  <div class=\"ticks\"></div>\n  <section id=\"spacer\"></section>\n</template>\n";
        fs::write(root.join("src/App.vue"), app_after_a2).expect("write a2 App.vue");
        let a2_outcome =
            handle_post_tool_hook(&coord, "agent-faq", "src/App.vue").expect("post-tool a2");
        assert!(
            !a2_outcome.has_conflict,
            "cross-branch post-tool must never report a conflict"
        );

        // Prepare branch sandbox for `feat/testimonials-faq` and verify:
        // - `node_modules/.bin/vue-tsc` was synced into the already-created sandbox.
        // - `src/App.vue` on `feat/testimonials-faq` does NOT import PricingCalculator.vue and does not duplicate `<div class="ticks"></div>`.
        let (_, faq_ws, _) = coord
            .prepare_branch_sandbox("agent-faq", Some("feat/testimonials-faq"))
            .expect("faq sandbox");
        assert!(
            faq_ws.join("node_modules/.bin/vue-tsc").is_file(),
            "late node_modules/.bin/vue-tsc must be synced into existing branch sandbox"
        );
        let faq_app = fs::read_to_string(faq_ws.join("src/App.vue")).expect("read faq App.vue");
        assert!(
            faq_app.contains("TestimonialsSection"),
            "faq App.vue missing TestimonialsSection: {faq_app}"
        );
        assert!(
            !faq_app.contains("PricingCalculator"),
            "faq App.vue leaked PricingCalculator: {faq_app}"
        );
        assert!(
            !faq_app.contains("<div class=\"ticks\"></div>\n  <div class=\"ticks\"></div>"),
            "faq App.vue has duplicate consecutive ticks separator: {faq_app}"
        );
    }

    #[test]
    fn terminals_and_editor_branches_stay_isolated_and_sandbox_deletions_persist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let git = core::GitBackend::init_or_open(root).expect("init git");
        fs::write(root.join("README.md"), "# Main Readme\n").expect("write README.md");
        fs::write(root.join("index.html"), "<h1>Main</h1>\n").expect("write index.html");
        let snap = workspace::snapshot_working_dir(&git, root, "init", &[], None).expect("snap");
        let author = git.resolve_git_author();
        let c0 = git
            .write_commit(&snap.root_tree_id, &[], &author, "Initial commit")
            .expect("commit");
        git.update_ref("refs/heads/main", &c0).expect("update main");
        git.sync_git_index(&c0);

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        // Terminal 1 opens on `main` and switches to `feat/testimonials-faq`.
        let t1_init = coord.ensure_session("term-1001", None).expect("t1 init");
        assert_eq!(t1_init.target_branch.as_deref(), Some("main"));
        let t1 = coord
            .switch_branch("term-1001", "feat/testimonials-faq")
            .expect("t1 switch");
        assert_eq!(t1.target_branch.as_deref(), Some("feat/testimonials-faq"));

        // Terminal 2 opens and switches to `feat/pricing-calculator`.
        let _ = coord.ensure_session("term-1002", None).expect("t2 init");
        let t2 = coord
            .switch_branch("term-1002", "feat/pricing-calculator")
            .expect("t2 switch");
        assert_eq!(t2.target_branch.as_deref(), Some("feat/pricing-calculator"));

        // Terminal 3 opens on `main` without running `tes switch`.
        let t3 = coord.ensure_session("term-1003", None).expect("t3 init");
        assert_eq!(t3.target_branch.as_deref(), Some("main"));

        // Switching the VS Code extension editor branch must only change `repo_root` (.git/HEAD)
        // and must never alter Terminal 1, Terminal 2, or Terminal 3's branches.
        let switched_ed = coord
            .switch_editor_branch("feat/pricing-calculator")
            .expect("switch editor");
        assert_eq!(switched_ed, "feat/pricing-calculator");

        let cur = coord.oplog().load_current_reconciled().expect("load current");
        assert_eq!(
            cur.view.agents.get("term-1001").and_then(|a| a.target_branch.as_deref()),
            Some("feat/testimonials-faq"),
            "Terminal 1 must stay on feat/testimonials-faq"
        );
        assert_eq!(
            cur.view.agents.get("term-1002").and_then(|a| a.target_branch.as_deref()),
            Some("feat/pricing-calculator"),
            "Terminal 2 must stay on feat/pricing-calculator"
        );
        assert_eq!(
            cur.view.agents.get("term-1003").and_then(|a| a.target_branch.as_deref()),
            Some("main"),
            "Terminal 3 must stay pinned to main even after VS Code switches editor branch"
        );

        // Switch editor back to `main`, then delete `README.md` inside `main`'s branch sandbox
        // (simulating `rm README.md` in Terminal 3).
        coord.switch_editor_branch("main").expect("switch editor to main");
        let (_, main_ws, _) = coord
            .prepare_branch_sandbox("term-1003", Some("main"))
            .expect("prepare main sandbox");
        assert!(main_ws.join("README.md").is_file());
        fs::remove_file(main_ws.join("README.md")).expect("rm README.md in sandbox");

        // Next prompt's `prepare_branch_sandbox` must keep `README.md` deleted in both the sandbox
        // and `repo_root` (since editor is on `main`), and subsequent prompts must not resurrect it.
        let _ = coord
            .prepare_branch_sandbox("term-1003", Some("main"))
            .expect("prepare main after rm #1");
        assert!(
            !main_ws.join("README.md").exists(),
            "README.md must stay deleted in sandbox after prompt #1"
        );
        assert!(
            !root.join("README.md").exists(),
            "README.md must also be deleted from repo_root when editor is on main"
        );

        let _ = coord
            .prepare_branch_sandbox("term-1003", Some("main"))
            .expect("prepare main after rm #2");
        assert!(
            !main_ws.join("README.md").exists(),
            "README.md must not be resurrected on prompt #2"
        );
    }

    #[test]
    fn full_file_vue_replacements_on_separate_branches_preserve_all_lines_and_editor_head() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let git = core::GitBackend::init_or_open(root).expect("init git");
        fs::create_dir_all(root.join("src")).expect("mkdir src");
        let base_app_vue = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\n</script>\n\n<template>\n  <HelloWorld msg=\"Vite + Vue\" />\n</template>\n\n<style scoped>\n.logo {\n  height: 6em;\n}\n</style>\n";
        fs::write(root.join("src/App.vue"), base_app_vue).expect("write base App.vue");
        let snap = workspace::snapshot_working_dir(&git, root, "init", &[], None).expect("snap");
        let author = git.resolve_git_author();
        let c0 = git
            .write_commit(&snap.root_tree_id, &[], &author, "Initial commit")
            .expect("commit");
        git.update_ref("refs/heads/main", &c0).expect("update main");
        git.sync_git_index(&c0);

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        coord
            .claim_intent_and_symbols_with_branch(
                "agent-orange",
                Some("Replace App.vue with orange todo page"),
                &["src/App.vue".to_string()],
                Some("feat/orange-todo"),
            )
            .expect("claim orange");
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-blue",
                Some("Replace App.vue with royal blue counter page"),
                &["src/App.vue".to_string()],
                Some("feat/blue-counter"),
            )
            .expect("claim blue");

        let orange_vue = "<script setup lang=\"ts\">\nimport { ref } from 'vue'\n\nconst newTodo = ref('')\nconst todos = ref<string[]>([])\n\nfunction addTodo() {\n  if (newTodo.value.trim()) {\n    todos.value.push(newTodo.value.trim())\n    newTodo.value = ''\n  }\n}\n</script>\n\n<template>\n  <div class=\"container\">\n    <h1>ORANGE TODO BRANCH (<code>feat/orange-todo</code>)</h1>\n  </div>\n</template>\n\n<style>\nhtml,\nbody {\n  margin: 0;\n  padding: 0;\n  min-height: 100vh;\n}\n#app {\n  min-height: 100vh;\n}\n</style>\n\n<style scoped>\n.container {\n  display: flex;\n  flex-direction: column;\n  align-items: center;\n  color: #ffffff;\n}\nh1 {\n  font-size: 2.25rem;\n  font-weight: 700;\n  color: #ffffff;\n  margin: 0 0 2rem 0;\n  text-align: center;\n}\n</style>\n";
        fs::write(root.join("src/App.vue"), orange_vue).expect("write orange App.vue");
        handle_post_tool_hook(&coord, "agent-orange", "src/App.vue").expect("post-tool orange");

        let blue_vue = "<script setup lang=\"ts\">\nimport { ref } from 'vue'\n\nconst count = ref(0)\n\nfunction increment() {\n  count.value += 1\n}\n\nfunction decrement() {\n  count.value -= 1\n}\n\nfunction reset() {\n  count.value = 0\n}\n</script>\n\n<template>\n  <main class=\"counter-page\">\n    <h1>BLUE COUNTER BRANCH (<code>feat/blue-counter</code>)</h1>\n    <div class=\"count\">{{ count }}</div>\n    <button @click=\"increment\">+1</button>\n    <button @click=\"decrement\">-1</button>\n    <button @click=\"reset\">Reset</button>\n  </main>\n</template>\n\n<style>\nhtml,\nbody {\n  margin: 0;\n  padding: 0;\n  min-height: 100vh;\n}\n#app {\n  min-height: 100vh;\n}\n</style>\n\n<style scoped>\n.counter-page {\n  display: flex;\n  flex-direction: column;\n  align-items: center;\n  background-color: #1d4ed8;\n  color: #ffffff;\n}\nh1 {\n  font-size: 2.25rem;\n  font-weight: 700;\n  color: #ffffff;\n  margin: 0 0 2rem 0;\n  text-align: center;\n}\n</style>\n";
        fs::write(root.join("src/App.vue"), blue_vue).expect("write blue App.vue");
        handle_post_tool_hook(&coord, "agent-blue", "src/App.vue").expect("post-tool blue");

        // Radar must not report collisions across separate branches.
        let radar = coord.radar().expect("radar");
        assert!(
            radar.collisions.is_empty(),
            "sessions on separate branches must not report radar collisions: {:?}",
            radar.collisions
        );

        // Blue Counter's App.vue must be preserved 100% byte-for-byte after post-tool!
        let disk_after_blue = fs::read_to_string(root.join("src/App.vue")).expect("read disk");
        assert_eq!(
            disk_after_blue, blue_vue,
            "post-tool hook must never strip shared lines (import ref, braces, <style>) from a full-file replacement on another branch"
        );

        // Both agents merge into their target branches; editor HEAD on `root` must stay on `main`.
        coord
            .merge_agents_with_message(
                &["agent-orange".to_string()],
                Some("feat/orange-todo"),
                None,
                false,
                Some("feat: orange todo"),
            )
            .expect("merge orange");
        coord
            .merge_agents_with_message(
                &["agent-blue".to_string()],
                Some("feat/blue-counter"),
                None,
                false,
                Some("feat: blue counter"),
            )
            .expect("merge blue");

        assert_eq!(
            coord.git().current_branch_name().expect("branch"),
            "main",
            "merging into feature branches must keep repo_root (.git/HEAD) on main"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/App.vue")).expect("read main App.vue"),
            base_app_vue,
            "repo_root/src/App.vue on main must remain main's original App.vue"
        );
    }

    #[test]
    fn editor_branch_switching_during_parallel_sessions_and_merge_into_main() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        let git = core::GitBackend::init_or_open(root).expect("init git");
        fs::create_dir_all(root.join("src")).expect("mkdir src");
        let base_app_vue = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\n</script>\n\n<template>\n  <HelloWorld />\n</template>\n";
        fs::write(root.join("src/App.vue"), base_app_vue).expect("write base App.vue");
        let snap = workspace::snapshot_working_dir(&git, root, "init", &[], None).expect("snap");
        let author = git.resolve_git_author();
        let c0 = git
            .write_commit(&snap.root_tree_id, &[], &author, "Initial commit")
            .expect("commit");
        git.update_ref("refs/heads/main", &c0).expect("update main");
        git.sync_git_index(&c0);

        let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

        // Session 1 claims feat/pomodoro-timer, Session 2 claims feat/habit-tracker
        coord
            .claim_intent_and_symbols_with_branch(
                "session-pomodoro",
                Some("Add Emerald Focus Timer to App.vue"),
                &["src/App.vue".to_string()],
                Some("feat/pomodoro-timer"),
            )
            .expect("claim pomodoro");
        coord
            .claim_intent_and_symbols_with_branch(
                "session-habits",
                Some("Add Purple Daily Habits Tracker to App.vue"),
                &["src/App.vue".to_string()],
                Some("feat/habit-tracker"),
            )
            .expect("claim habits");

        // Session 2 edits and commits to feat/habit-tracker
        let habits_vue = "<script setup lang=\"ts\">\nimport { ref } from 'vue'\nimport HelloWorld from './components/HelloWorld.vue'\nconst habits = ref([])\n</script>\n\n<template>\n  <HelloWorld />\n  <section id=\"habit-tracker\" class=\"habits-panel\">\n    <h2>Daily Habits (feat/habit-tracker)</h2>\n  </section>\n</template>\n";
        fs::write(root.join("src/App.vue"), habits_vue).expect("write habits App.vue");
        handle_post_tool_hook(&coord, "session-habits", "src/App.vue").expect("post-tool habits");
        coord
            .merge_agents_with_message(
                &["session-habits".to_string()],
                Some("feat/habit-tracker"),
                None,
                true,
                Some("feat(habits): add purple daily habits tracker section"),
            )
            .expect("commit habits");

        // Session 1 edits App.vue on feat/pomodoro-timer (uncommitted!)
        let pomodoro_vue = "<script setup lang=\"ts\">\nimport { ref } from 'vue'\nimport HelloWorld from './components/HelloWorld.vue'\nconst secondsLeft = ref(1500)\n</script>\n\n<template>\n  <HelloWorld />\n  <section id=\"pomodoro-timer\" class=\"timer-panel\">\n    <h2>Focus Timer (feat/pomodoro-timer)</h2>\n  </section>\n</template>\n";
        fs::write(root.join("src/App.vue"), pomodoro_vue).expect("write pomodoro App.vue");
        handle_post_tool_hook(&coord, "session-pomodoro", "src/App.vue")
            .expect("post-tool pomodoro");

        // User switches editor status bar across feat/habit-tracker -> feat/pomodoro-timer -> main
        // WHILE session-pomodoro still has uncommitted own_blobs!
        coord
            .switch_editor_branch("feat/habit-tracker")
            .expect("switch editor to habits");
        coord
            .switch_editor_branch("feat/pomodoro-timer")
            .expect("switch editor to pomodoro");
        assert!(
            fs::read_to_string(root.join("src/App.vue"))
                .expect("read pomodoro preview")
                .contains("Focus Timer"),
            "switching editor to feat/pomodoro-timer must show uncommitted pomodoro own_blobs"
        );
        coord
            .switch_editor_branch("main")
            .expect("switch editor to main");

        // Now session-pomodoro runs watch_tick + snapshot_agent + commit (tes status / tes commit)
        let _ = coord.watch_tick("session-pomodoro").expect("watch_tick");
        let pom_meta = coord
            .snapshot_agent("session-pomodoro", true)
            .expect("snapshot pomodoro");
        assert!(
            !pom_meta.own_blobs.is_empty(),
            "editor switching to main must NEVER wipe out session-pomodoro's uncommitted own_blobs!"
        );
        coord
            .merge_agents_with_message(
                &["session-pomodoro".to_string()],
                Some("feat/pomodoro-timer"),
                None,
                true,
                Some("feat(timer): add emerald focus timer section"),
            )
            .expect("commit pomodoro");

        // User switches editor to feat/pomodoro-timer before Session 3 starts
        coord
            .switch_editor_branch("feat/pomodoro-timer")
            .expect("switch editor to pomodoro");

        // Session 3 starts and runs `tes merge feat/pomodoro-timer feat/habit-tracker --into main`
        let _ = coord.ensure_session("session-merge", None).expect("ensure session-merge");
        let report = coord
            .merge_agents_with_message(
                &[
                    "feat/pomodoro-timer".to_string(),
                    "feat/habit-tracker".to_string(),
                ],
                Some("main"),
                None,
                true,
                Some("Merge feat/pomodoro-timer and feat/habit-tracker into main"),
            )
            .expect("merge into main");
        assert!(report.conflicts.is_empty(), "merge into main must have 0 conflicts");
        assert_eq!(
            coord.git().current_branch_name().expect("head branch"),
            "main",
            "merging into main with --into main must switch .git/HEAD to main"
        );
        let merged_disk = fs::read_to_string(root.join("src/App.vue")).expect("read merged main");
        assert!(
            merged_disk.contains("Focus Timer") && merged_disk.contains("Daily Habits"),
            "repo_root/src/App.vue must immediately contain both merged sections"
        );
        assert_eq!(
            merged_disk.matches("from 'vue'").count(),
            1,
            "merged App.vue must deduplicate `import {{ ... }} from 'vue'` into a single statement:\n{merged_disk}"
        );

        // Session 3 runs `tes switch main` and `tes status` (watch_tick + snapshot_agent)
        coord
            .switch_branch("session-merge", "main")
            .expect("switch session-merge to main");
        let _ = coord.watch_tick("session-merge").expect("watch_tick merge");
        let merge_meta = coord
            .snapshot_agent("session-merge", true)
            .expect("snapshot session-merge");
        assert!(
            merge_meta.own_blobs.is_empty(),
            "session-merge must have a clean working tree on main after merging"
        );
    }

    #[test]
    fn conflict_markers_written_to_working_file_and_auto_cleared_on_edit_and_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        fs::create_dir_all(root.join("src")).expect("mkdir src");
        fs::write(
            root.join("src/config.ts"),
            "export const MAX_RETRIES = 3;\n",
        )
        .expect("write initial config.ts");

        let (coord, _) = Coordinator::init_repo(root).expect("init repo");

        // Branch A sets MAX_RETRIES = 5
        coord
            .switch_branch("session-a", "feat/retries-5")
            .expect("switch a");
        coord
            .claim_intent_and_symbols("session-a", Some("Set MAX_RETRIES to 5"), &["src/config.ts".to_string()])
            .expect("claim a");
        fs::write(root.join("src/config.ts"), "export const MAX_RETRIES = 5;\n").expect("write a");
        handle_post_tool_hook(&coord, "session-a", "src/config.ts").expect("post-tool a");
        coord
            .merge_agents_with_message(
                &["session-a".to_string()],
                Some("feat/retries-5"),
                None,
                true,
                Some("feat: set MAX_RETRIES to 5"),
            )
            .expect("commit a");

        // Branch B sets MAX_RETRIES = 10
        coord
            .switch_branch("session-b", "feat/retries-10")
            .expect("switch b");
        coord
            .claim_intent_and_symbols("session-b", Some("Set MAX_RETRIES to 10"), &["src/config.ts".to_string()])
            .expect("claim b");
        fs::write(root.join("src/config.ts"), "export const MAX_RETRIES = 10;\n").expect("write b");
        handle_post_tool_hook(&coord, "session-b", "src/config.ts").expect("post-tool b");
        coord
            .merge_agents_with_message(
                &["session-b".to_string()],
                Some("feat/retries-10"),
                None,
                true,
                Some("feat: set MAX_RETRIES to 10"),
            )
            .expect("commit b");

        // Merge both into main -> produces a conflict on src/config.ts
        let report = coord
            .merge_agents_with_message(
                &["feat/retries-5".to_string(), "feat/retries-10".to_string()],
                Some("main"),
                None,
                true,
                Some("Merge retries branches"),
            )
            .expect("merge into main");
        assert_eq!(report.conflicts.len(), 1);

        // Verify standard Git 2-way conflict markers are written directly into repo_root/src/config.ts
        let conflicted_disk = fs::read_to_string(root.join("src/config.ts")).expect("read conflicted");
        assert!(
            conflicted_disk.contains("<<<<<<< ")
                && conflicted_disk.contains("=======")
                && conflicted_disk.contains(">>>>>>> ")
                && !conflicted_disk.contains("||||||| base"),
            "working file must contain standard Git 2-way conflict markers:\n{conflicted_disk}"
        );

        // User or agent edits src/config.ts directly to resolve the conflict and runs post-tool
        coord
            .switch_branch("session-resolve", "main")
            .expect("switch resolve");
        fs::write(
            root.join("src/config.ts"),
            "export const MAX_RETRIES = 10;\n",
        )
        .expect("write resolved config.ts");
        handle_post_tool_hook(&coord, "session-resolve", "src/config.ts").expect("post-tool resolve");

        // Verify conflict is automatically cleared from View and .git/tessera/conflicts/
        let radar = coord.radar().expect("radar");
        assert!(
            radar.active_conflicts.is_empty(),
            "editing the file to remove conflict markers and running post-tool must auto-clear the conflict"
        );
    }

    #[test]
    fn strip_peer_inserted_lines_restores_replaced_baseline_lines() {
        let base_str = "function add(a, b) {\n  return a + b;\n}\n\nfunction multiply(a, b) {\n  return a * b;\n}\n\nmodule.exports = { add };\n";
        let peer_str = "function add(a, b) {\n  return a + b;\n}\n\nfunction subtract(a, b) {\n  return a - b;\n}\n\nmodule.exports = { add, subtract };\n";
        let disk_after_str = "function add(a, b) {\n  return a + b;\n}\n\nfunction subtract(a, b) {\n  return a - b;\n}\n\nfunction multiply(a, b) {\n  return a * b;\n}\n\nmodule.exports = { add, subtract };\n\nfunction divide(a, b) {\n  return a / b;\n}\n";

        let stripped = strip_peer_inserted_lines(base_str, base_str, peer_str, disk_after_str)
            .expect("strip foreign peer lines");
        assert!(
            !stripped.contains("subtract"),
            "foreign peer subtract function must be stripped:\n{stripped}"
        );
        assert!(
            stripped.contains("module.exports = { add };"),
            "baseline export line replaced by foreign peer must be restored:\n{stripped}"
        );
        assert!(
            stripped.contains("function multiply(a, b)") && stripped.contains("function divide(a, b)"),
            "both branch baseline multiply and new divide must be preserved:\n{stripped}"
        );
    }

    #[test]
    fn focus_session_waypoint_skips_non_terminal_ide_sessions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");
        let mut meta = coord
            .claim_intent_and_symbols_with_branch(
                "antigravity-43f748a3-584",
                Some("Edit inside IDE"),
                &[],
                Some("main"),
            )
            .expect("claim");
        meta.waypoint = Some(SessionWaypoint {
            harness: "antigravity".to_string(),
            conversation_id: Some("43f748a3-5844-48d2-a8b7-85d35b19d0d5".to_string()),
            term_program: Some("iTerm.app".to_string()),
            term_session_id: Some("w0t0p0:12345678".to_string()),
            tty: None,
            shell_pid: Some(47687),
            ide_pid: Some(47654),
        });

        let report = focus_session_waypoint(&meta).expect("focus report");
        assert!(!report.focused);
        assert_eq!(report.mechanism, "waypoint-info");
    }

    #[test]
    fn process_classifiers_distinguish_cli_harnesses_from_ide_hosts() {
        assert!(is_shell_or_cli_harness_process("-zsh"));
        assert!(is_shell_or_cli_harness_process("/bin/bash"));
        assert!(is_shell_or_cli_harness_process("/Users/user/.local/bin/agy"));
        assert!(is_shell_or_cli_harness_process("/usr/local/bin/claude"));
        assert!(is_shell_or_cli_harness_process("codex-aarch64-apple-darwin"));
        assert!(is_shell_or_cli_harness_process("pwsh.exe"));
        assert!(!is_shell_or_cli_harness_process("/usr/bin/login"));
        assert!(!is_shell_or_cli_harness_process(
            "/Applications/iTerm.app/Contents/MacOS/iTerm2"
        ));
        assert!(!is_shell_or_cli_harness_process(
            "/Applications/Ghostty.app/Contents/MacOS/ghostty"
        ));
        assert!(!is_shell_or_cli_harness_process(
            "/Applications/Antigravity IDE.app/Contents/MacOS/Electron"
        ));

        assert!(is_ide_process_name(
            "/Applications/Antigravity IDE.app/Contents/MacOS/Electron"
        ));
        assert!(is_ide_process_name(
            "/Applications/Cursor.app/Contents/MacOS/Cursor"
        ));
        assert!(is_ide_process_name("/usr/share/code/code"));
        assert!(!is_ide_process_name("/Users/user/code/project/target/debug/tes"));
        assert!(!is_ide_process_name("/Users/user/.local/bin/agy"));
        assert!(!is_ide_process_name("/usr/local/bin/codex"));
    }

    #[test]
    fn switch_editor_and_terminal_branch_preserves_unhooked_in_flight_agent_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");

        // Agent claims directory `video` on `feat/product-video` while `.git/HEAD` is `main`.
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-video",
                Some("Create product video"),
                &["video".to_string()],
                Some("feat/product-video"),
            )
            .expect("claim feat/product-video");

        // Agent creates new files in `repo_root/video/...` before `handle_post_tool_hook` has run.
        let pkg_path = tmp.path().join("video/package.json");
        let root_path = tmp.path().join("video/src/Root.tsx");
        fs::create_dir_all(root_path.parent().expect("parent")).expect("mkdir");
        fs::write(&pkg_path, "{\"name\":\"video\"}\n").expect("write package.json");
        fs::write(&root_path, "export const RemotionRoot = () => null;\n").expect("write Root.tsx");

        // User switches terminal session and editor working tree to `feat/product-video`.
        coord
            .switch_branch("term-user", "feat/product-video")
            .expect("switch terminal branch");
        let sb_dir = coord.branch_sandbox_dir("feat/product-video");
        coord
            .switch_editor_branch("feat/product-video")
            .expect("switch editor branch to feat/product-video");

        assert_eq!(
            fs::read_to_string(&pkg_path).expect("read repo_root package.json"),
            "{\"name\":\"video\"}\n"
        );
        assert_eq!(
            fs::read_to_string(&root_path).expect("read repo_root Root.tsx"),
            "export const RemotionRoot = () => null;\n"
        );
        assert_eq!(
            fs::read_to_string(sb_dir.join("video/package.json"))
                .expect("read sandbox package.json"),
            "{\"name\":\"video\"}\n"
        );
        assert_eq!(
            fs::read_to_string(sb_dir.join("video/src/Root.tsx")).expect("read sandbox Root.tsx"),
            "export const RemotionRoot = () => null;\n"
        );

        // Agent creates another file while editor is on `feat/product-video`, and user switches editor to `main` and back.
        let scene_path = tmp.path().join("video/src/scenes/Scene1Problem.tsx");
        fs::create_dir_all(scene_path.parent().expect("parent")).expect("mkdir scenes");
        fs::write(&scene_path, "export const Scene1Problem = () => null;\n")
            .expect("write Scene1Problem.tsx");

        coord
            .switch_editor_branch("main")
            .expect("switch editor branch to main");
        assert!(
            !scene_path.exists(),
            "main editor working tree must not show feat/product-video files"
        );
        assert_eq!(
            fs::read_to_string(sb_dir.join("video/src/scenes/Scene1Problem.tsx"))
                .expect("sandbox must preserve Scene1Problem.tsx when switching editor to main"),
            "export const Scene1Problem = () => null;\n"
        );

        coord
            .switch_editor_branch("feat/product-video")
            .expect("switch editor branch back to feat/product-video");
        assert_eq!(
            fs::read_to_string(&scene_path)
                .expect("Scene1Problem.tsx must be restored when switching editor back"),
            "export const Scene1Problem = () => null;\n"
        );

        // Commit `agent-video` onto `feat/product-video`, advance `main`, merge `main` into
        // `feat/product-video`, and then merge `feat/product-video` back into `main`.
        coord
            .merge_agents(
                &["agent-video".to_string()],
                Some("feat/product-video"),
                None,
                false,
            )
            .expect("commit feat/product-video");

        coord
            .claim_intent_and_symbols_with_branch(
                "agent-main",
                Some("Update README on main"),
                &["README.md".to_string()],
                Some("main"),
            )
            .expect("claim README.md on main");
        fs::write(tmp.path().join("README.md"), "# Main Updated\n").expect("write README.md");
        handle_post_tool_hook(&coord, "agent-main", "README.md").expect("post-tool README.md");
        coord
            .merge_agents(
                &["agent-main".to_string()],
                Some("main"),
                None,
                false,
            )
            .expect("commit main");

        coord
            .merge_agents(
                &["main".to_string()],
                Some("feat/product-video"),
                None,
                false,
            )
            .expect("merge main into feat/product-video");

        coord
            .merge_agents(
                &["feat/product-video".to_string()],
                Some("main"),
                None,
                false,
            )
            .expect("merge feat/product-video into main");
        coord
            .switch_editor_branch("main")
            .expect("switch editor to main after merge");

        assert_eq!(
            fs::read_to_string(&pkg_path).expect("video/package.json must exist on main after merge"),
            "{\"name\":\"video\"}\n"
        );
        assert_eq!(
            fs::read_to_string(&scene_path)
                .expect("Scene1Problem.tsx must exist on main after merge"),
            "export const Scene1Problem = () => null;\n"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("README.md")).expect("README.md on main"),
            "# Main Updated\n"
        );
    }

    #[test]
    fn peer_non_head_commit_preserves_watcher_observed_in_flight_edit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app_vue = tmp.path().join("src/App.vue");
        fs::create_dir_all(app_vue.parent().expect("parent")).expect("mkdir src");
        fs::write(
            &app_vue,
            "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\n</script>\n<template>\n  <HelloWorld />\n</template>\n",
        )
        .expect("write initial App.vue");

        let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-init",
                Some("Initial App.vue"),
                &["src/App.vue".to_string()],
                Some("main"),
            )
            .expect("claim main");
        handle_post_tool_hook(&coord, "agent-init", "src/App.vue").expect("post-tool init");
        coord
            .merge_agents(&["agent-init".to_string()], Some("main"), None, false)
            .expect("commit main");

        coord
            .claim_intent_and_symbols_with_branch(
                "agent-pomodoro",
                Some("Build Pomodoro Focus Timer dashboard in src/App.vue"),
                &["src/App.vue".to_string()],
                Some("feat/pomodoro-timer"),
            )
            .expect("claim pomodoro");
        handle_pre_tool_hook(&coord, "agent-pomodoro", "src/App.vue").expect("pre-tool pomodoro");

        coord
            .claim_intent_and_symbols_with_branch(
                "agent-habit",
                Some("Build Daily Habit & Streak Tracker in src/App.vue"),
                &["src/App.vue".to_string()],
                Some("feat/habit-tracker"),
            )
            .expect("claim habit");
        handle_pre_tool_hook(&coord, "agent-habit", "src/App.vue").expect("pre-tool habit");

        let pomodoro_code = "<script setup lang=\"ts\">\nconst mode = 'pomodoro'\n</script>\n<template>\n  <div>Pomodoro Focus Timer</div>\n</template>\n";
        fs::write(&app_vue, pomodoro_code).expect("write pomodoro");
        handle_post_tool_hook(&coord, "agent-pomodoro", "src/App.vue").expect("post-tool pomodoro");
        assert_eq!(
            fs::read_to_string(&app_vue).expect("read app_vue after first post-tool"),
            "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\n</script>\n<template>\n  <HelloWorld />\n</template>\n",
            "post-tool must restore HEAD baseline to repo_root while a cross-branch peer is still awaiting its first edit"
        );

        // Simulate `tes exec` restoring `main` to `repo_root/src/App.vue`, followed by `agent-habit`
        // writing its Habit Tracker on disk and the background watcher recording it in `observed_blobs`
        // right before `agent-pomodoro` runs `tes commit`!
        let habit_code = "<script setup lang=\"ts\">\nconst tracker = 'habit'\n</script>\n<template>\n  <div>Daily Habit & Streak Tracker</div>\n</template>\n";
        fs::write(&app_vue, habit_code).expect("write habit");
        let habit_blob = coord
            .git()
            .write_blob(habit_code.as_bytes())
            .expect("write habit blob");
        coord
            .oplog()
            .record_tx(
                "watcher",
                "observe intermediate blob for src/App.vue",
                true,
                |view| {
                    view.observed_blobs
                        .entry("src/App.vue".to_string())
                        .or_default()
                        .push(habit_blob);
                    Ok(())
                },
            )
            .expect("record watcher observed_blob");

        // Now `agent-pomodoro` commits onto `feat/pomodoro-timer` before `agent-habit` runs `post-tool`.
        coord
            .merge_agents(
                &["agent-pomodoro".to_string()],
                Some("feat/pomodoro-timer"),
                None,
                false,
            )
            .expect("commit feat/pomodoro-timer");

        // `agent-habit` now runs `tes hook post-tool` and commits onto `feat/habit-tracker`.
        handle_post_tool_hook(&coord, "agent-habit", "src/App.vue").expect("post-tool habit");
        let report = coord
            .merge_agents(
                &["agent-habit".to_string()],
                Some("feat/habit-tracker"),
                None,
                false,
            )
            .expect("commit feat/habit-tracker");
        let committed_tree = coord
            .git()
            .read_commit(&report.merged_commit_id)
            .expect("read habit commit")
            .tree_id;
        let entry = coord
            .git()
            .read_tree_entry_at_path(&committed_tree, "src/App.vue")
            .expect("read entry")
            .expect("App.vue exists");
        let committed_bytes = coord.git().read_blob(&entry.blob_id).expect("read blob");
        assert_eq!(String::from_utf8_lossy(&committed_bytes), habit_code);
    }

    #[test]
    fn branching_off_feature_branch_via_from_or_switch_stages_parent_baseline_and_rejects_invalid_base() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let app_vue = tmp.path().join("src/App.vue");
        fs::create_dir_all(app_vue.parent().expect("parent")).expect("mkdir src");
        let main_vue = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\n</script>\n<template>\n  <HelloWorld />\n</template>\n";
        fs::write(&app_vue, main_vue).expect("write initial App.vue");

        let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-init",
                Some("Initial App.vue"),
                &["src/App.vue".to_string()],
                Some("main"),
            )
            .expect("claim main");
        handle_post_tool_hook(&coord, "agent-init", "src/App.vue").expect("post-tool init");
        coord
            .merge_agents(&["agent-init".to_string()], Some("main"), None, false)
            .expect("commit main");

        let habit_vue = "<script setup lang=\"ts\">\nconst habits = ['Read', 'Run']\n</script>\n<template>\n  <section id=\"habits\">Daily Habits</section>\n</template>\n";
        coord
            .claim_intent_and_symbols_with_branch(
                "agent-habit",
                Some("Create habit tracker on feat/habit-tracker"),
                &["src/App.vue".to_string()],
                Some("feat/habit-tracker"),
            )
            .expect("claim feat/habit-tracker");
        fs::write(&app_vue, habit_vue).expect("write habit_vue");
        handle_post_tool_hook(&coord, "agent-habit", "src/App.vue").expect("post-tool habit");
        let habit_commit = coord
            .merge_agents(
                &["agent-habit".to_string()],
                Some("feat/habit-tracker"),
                None,
                false,
            )
            .expect("commit feat/habit-tracker");

        assert_eq!(
            fs::read_to_string(&app_vue).expect("read main App.vue"),
            main_vue,
            "repo_root must remain on main after feat/habit-tracker commits"
        );

        assert!(
            coord
                .switch_branch_with_base(
                    "agent-sec",
                    "feat/evil",
                    Some("--upload-pack=evil")
                )
                .is_err()
        );
        assert!(
            coord
                .switch_branch_with_base("agent-sec", "feat/evil", Some("../evil"))
                .is_err()
        );
        assert!(
            coord
                .switch_branch_with_base(
                    "agent-sec",
                    "feat/missing",
                    Some("feat/does-not-exist")
                )
                .is_err()
        );
        assert!(
            coord
                .claim_intent_and_symbols_with_branch_and_base(
                    "agent-sec",
                    Some("Invalid base"),
                    &["src/App.vue".to_string()],
                    Some("feat/missing"),
                    Some("feat/does-not-exist"),
                )
                .is_err()
        );

        let analytics_meta = coord
            .claim_intent_and_symbols_with_branch_and_base(
                "agent-analytics",
                Some("Add analytics chart onto feat/habit-tracker"),
                &["src/App.vue".to_string()],
                Some("feat/habit-analytics"),
                Some("feat/habit-tracker"),
            )
            .expect("claim feat/habit-analytics from feat/habit-tracker");
        assert_eq!(
            analytics_meta.base_commit, habit_commit.merged_commit_id,
            "--from feat/habit-tracker must fork feat/habit-analytics from feat/habit-tracker's commit"
        );
        assert_eq!(
            fs::read_to_string(&app_vue).expect("read staged App.vue"),
            habit_vue,
            "claiming src/App.vue on feat/habit-analytics must stage feat/habit-tracker's App.vue in repo_root"
        );

        coord
            .switch_branch("agent-two-step", "feat/habit-tracker")
            .expect("switch to feat/habit-tracker");
        let export_meta = coord
            .switch_branch("agent-two-step", "feat/habit-export")
            .expect("switch to new child branch feat/habit-export");
        assert_eq!(
            export_meta.base_commit, habit_commit.merged_commit_id,
            "switching to a new branch from a session on feat/habit-tracker must fork from feat/habit-tracker"
        );
    }
}
