//! Terminal output styling, OSC 8 hyperlinks, safe paging, and pathspec normalization helpers.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use agent::Coordinator;

/// Shared invocation context passed to CLI command handlers.
#[derive(Debug, Clone)]
pub struct CliContext {
    /// Effective working directory (either `-C <repo>` or `std::env::current_dir()`).
    pub cwd: PathBuf,
    /// Emit structured JSON output instead of human-formatted text.
    pub json: bool,
    /// Disable automatic paging (`-P` / `--no-pager`).
    pub no_pager: bool,
    /// Force paging when stdout is an interactive terminal (`--paginate`).
    pub paginate: bool,
}

/// Returns `true` when ANSI color output is enabled (`stdout` is a TTY and `NO_COLOR` is unset).
#[must_use]
pub fn use_color() -> bool {
    io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// Formats `s` in bold ANSI text when color is enabled.
pub fn c_bold(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in dim ANSI text when color is enabled.
pub fn c_dim(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold green ANSI text when color is enabled.
pub fn c_green(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;32m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold cyan ANSI text when color is enabled.
pub fn c_cyan(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;36m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold yellow ANSI text when color is enabled.
pub fn c_yellow(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;33m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold red ANSI text when color is enabled.
pub fn c_red(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;31m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold orange ANSI text when color is enabled.
pub fn c_orange(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;38;5;208m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats a single-letter file status badge (`A`, `M`, `D`) with ANSI color.
pub fn format_status_badge(st: &str, staged: bool) -> String {
    match st {
        "A" => c_green("A"),
        "D" => c_red("D"),
        "M" if staged => c_green("M"),
        "M" => c_yellow("M"),
        other => c_cyan(other),
    }
}

/// Percent-encodes a string for safe inclusion as a URI query parameter value.
#[must_use]
pub fn percent_encode_uri_param(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());

    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }

    out
}

/// Wraps `styled_text` in an OSC 8 terminal hyperlink targeting `tessera://focus` for `session_id`
/// when standard output is an interactive terminal with color enabled.
///
/// Never spawns external compilers or LaunchServices registrations during read-only CLI commands.
#[must_use]
pub fn format_session_link(session_id: &str, repo_root: &Path, styled_text: String) -> String {
    if !use_color() {
        return styled_text;
    }

    let encoded_session = percent_encode_uri_param(session_id);
    let encoded_repo = percent_encode_uri_param(&repo_root.to_string_lossy());

    format!(
        "\x1b]8;;tessera://focus?session={encoded_session}&repo={encoded_repo}\x1b\\{styled_text}\x1b]8;;\x1b\\"
    )
}

/// Returns `true` when `TES_NO_PAGER` explicitly disables paging (`1`, `true`, `yes`, `on`).
#[must_use]
pub fn is_env_no_pager() -> bool {
    std::env::var("TES_NO_PAGER").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Queries `git config core.pager` for `repo_root` when no environment or Tessera pager is set.
fn query_git_core_pager(repo_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["config", "--get", "core.pager"])
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let val = String::from_utf8_lossy(&out.stdout).trim().to_string();

    if val.is_empty() { None } else { Some(val) }
}

/// Resolves the pager command string following Git's precedence order:
/// `TES_PAGER` -> `cfg.pager` -> `GIT_PAGER` -> `git config core.pager` -> `PAGER` -> `less -FRX`.
#[must_use]
pub fn resolve_pager_command(coord: Option<&Coordinator>) -> String {
    if let Ok(val) = std::env::var("TES_PAGER") {
        return val;
    }

    if let Some(cfg_pager) = coord
        .and_then(|c| c.oplog().load_config().ok())
        .and_then(|cfg| cfg.pager)
    {
        return cfg_pager;
    }

    if let Ok(val) = std::env::var("GIT_PAGER") {
        return val;
    }

    if let Some(git_pager) = coord.and_then(|c| query_git_core_pager(c.git().repo_root())) {
        return git_pager;
    }

    if let Ok(val) = std::env::var("PAGER") {
        return val;
    }

    "less -FRX".to_string()
}

/// Resolves an executable name or path against `PATH` (including `PATHEXT` extensions on Windows).
#[must_use]
pub fn find_executable_in_path(program: &str) -> Option<PathBuf> {
    let trimmed = program.trim();

    if trimmed.is_empty() {
        return None;
    }

    let as_path = Path::new(trimmed);

    if as_path.components().count() > 1 || as_path.is_absolute() {
        return as_path.is_file().then(|| as_path.to_path_buf());
    }

    let path_env = std::env::var_os("PATH")?;

    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(trimmed);

        if candidate.is_file() {
            return Some(candidate);
        }

        #[cfg(windows)]
        {
            let exts =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());

            for ext in exts.split(';').filter(|e| !e.is_empty()) {
                let with_ext = dir.join(format!("{trimmed}{ext}"));

                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }

    None
}

/// Splits a simple command string without shell metacharacters into program and argument tokens.
/// Returns `None` when shell syntax (`|`, `;`, `&`, `<`, `>`, `$`, `` ` ``, `(`, `)`) is present.
fn split_simple_command(raw: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }

            continue;
        }

        if in_double {
            match ch {
                '"' => in_double = false,
                '\\' => {
                    if let Some(&next) = chars.peek() {
                        if matches!(next, '"' | '\\' | '$' | '`') {
                            escaped = true;
                        } else {
                            current.push(ch);
                        }
                    } else {
                        current.push(ch);
                    }
                }
                '$' | '`' => return None,
                _ => current.push(ch),
            }

            continue;
        }

        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            '\\' => escaped = true,
            '|' | ';' | '&' | '<' | '>' | '$' | '`' | '(' | ')' => return None,
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if in_single || in_double || escaped {
        return None;
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

/// Extracts the leading executable word from a shell pipeline command so its presence on `PATH`
/// can be verified before delegating to `sh -c` / `cmd /C`.
fn extract_leading_command_token(raw: &str) -> Option<&str> {
    for tok in raw.split_whitespace() {
        if tok.contains('=') && !tok.starts_with('=') && !tok.starts_with('-') {
            continue;
        }

        let cleaned = tok.trim_matches(|c| c == '\'' || c == '"');

        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }

    None
}

/// RAII terminal and signal guard for interactive Unix pagers.
///
/// Saves `stdout`'s `termios` state and ignores `SIGINT` in the parent process while waiting for
/// the pager child so pressing `Ctrl-C` inside `less` never terminates `tes` early, orphans the
/// pager, or leaves the terminal stuck in raw / `-onlcr` mode.
#[cfg(unix)]
struct PagerTtyGuard {
    saved_termios: Option<libc::termios>,
    prev_sigint: Option<libc::sighandler_t>,
}

#[cfg(unix)]
impl PagerTtyGuard {
    /// Captures current `termios` attributes on `stdout` and ignores `SIGINT` in the parent,
    /// returning `(guard, restore_child_sigint)` where `restore_child_sigint` is `true` when the
    /// pager child should reset `SIGINT` back to `SIG_DFL` before `exec`.
    fn enter() -> (Self, bool) {
        let saved_termios = {
            let mut term = std::mem::MaybeUninit::<libc::termios>::zeroed();
            // SAFETY: `tcgetattr` initializes `term` on `STDOUT_FILENO` when it returns `0`.
            let rc = unsafe { libc::tcgetattr(libc::STDOUT_FILENO, term.as_mut_ptr()) };

            if rc == 0 {
                // SAFETY: `tcgetattr` succeeded, so `term` is fully initialized.
                Some(unsafe { term.assume_init() })
            } else {
                None
            }
        };

        // SAFETY: Setting `SIGINT` to `SIG_IGN` is a standard POSIX signal disposition update
        // without installing any custom handler function pointer.
        let prev = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let prev_sigint = (prev != libc::SIG_ERR).then_some(prev);
        let restore_child_sigint = prev != libc::SIG_IGN;

        (
            Self {
                saved_termios,
                prev_sigint,
            },
            restore_child_sigint,
        )
    }
}

#[cfg(unix)]
impl Drop for PagerTtyGuard {
    fn drop(&mut self) {
        if let Some(saved) = &self.saved_termios {
            // SAFETY: `saved` was captured by `tcgetattr` on `STDOUT_FILENO` and remains valid.
            unsafe {
                libc::tcsetattr(libc::STDOUT_FILENO, libc::TCSANOW, saved);
            }
        }

        if let Some(prev) = self.prev_sigint {
            // SAFETY: `prev` is the exact prior `SIGINT` disposition returned by `libc::signal`.
            unsafe {
                libc::signal(libc::SIGINT, prev);
            }
        }
    }
}

/// Writes `text` directly to `stdout` or pipes it safely through the configured pager when
/// `stdout` is an interactive terminal and paging is active (`paginate || default_paged`).
///
/// Always verifies the pager executable exists before spawning so output is never swallowed when
/// `less` or a configured pager is absent from `PATH`. On Unix, ignores `SIGINT` in the parent
/// while waiting on the pager child and restores `termios` on exit so `Ctrl-C` or an abnormal
/// pager exit never corrupts the caller's terminal state.
pub fn emit_paged_output(
    text: &str,
    default_paged: bool,
    no_pager: bool,
    paginate: bool,
    json: bool,
    coord: Option<&Coordinator>,
) {
    if text.is_empty()
        || json
        || no_pager
        || is_env_no_pager()
        || !(paginate || default_paged)
        || !io::stdout().is_terminal()
    {
        print!("{text}");
        return;
    }

    let pager_raw = resolve_pager_command(coord);
    let pager_cmd = pager_raw.trim();

    if pager_cmd.is_empty() || pager_cmd == "cat" || pager_cmd == "false" {
        print!("{text}");
        return;
    }

    let less_env = std::env::var("LESS").unwrap_or_else(|_| "FRX".to_string());
    let lv_env = std::env::var("LV").unwrap_or_else(|_| "-c".to_string());

    let mut cmd = if let Some(tokens) = split_simple_command(pager_cmd) {
        let Some(resolved_bin) = find_executable_in_path(&tokens[0]) else {
            print!("{text}");
            return;
        };

        let mut c = Command::new(resolved_bin);
        c.args(&tokens[1..]);
        c
    } else {
        let Some(first_cmd) = extract_leading_command_token(pager_cmd) else {
            print!("{text}");
            return;
        };

        if find_executable_in_path(first_cmd).is_none() {
            print!("{text}");
            return;
        }

        if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", pager_cmd]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", pager_cmd]);
            c
        }
    };

    cmd.env("LESS", less_env)
        .env("LV", lv_env)
        .stdin(Stdio::piped());

    #[cfg(unix)]
    let _tty_guard = {
        use std::os::unix::process::CommandExt;

        let (guard, restore_child_sigint) = PagerTtyGuard::enter();

        if restore_child_sigint {
            // SAFETY: `libc::signal` is async-signal-safe per POSIX.1-2008 and restores default
            // `SIGINT` handling for the pager child between `fork` and `exec`.
            unsafe {
                cmd.pre_exec(|| {
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                    Ok(())
                });
            }
        }

        guard
    };

    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }

            let _ = child.wait();
        }
        Err(_) => {
            print!("{text}");
        }
    }
}

/// Normalizes a relative [`Path`] lexically, collapsing `.` and `..` segments cleanly and
/// formatting separators as Git forward slashes.
fn normalize_rel_path(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();

    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|p| p != "..") {
                    parts.pop();
                } else {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(seg) => {
                parts.push(seg.to_string_lossy().into_owned());
            }
            Component::RootDir | Component::Prefix(_) => {}
        }
    }

    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Computes the subdirectory prefix of `invocation_cwd` inside `repo_root` (or `None` when at the
/// repository root or outside `repo_root`).
#[must_use]
pub fn cwd_prefix_in_repo(invocation_cwd: &Path, repo_root: &Path) -> Option<PathBuf> {
    let canon_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let canon_cwd =
        std::fs::canonicalize(invocation_cwd).unwrap_or_else(|_| invocation_cwd.to_path_buf());

    let rel = canon_cwd.strip_prefix(&canon_root).ok()?;

    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel.to_path_buf())
    }
}

/// Resolves user-supplied pathspecs relative to `invocation_cwd` into repository-relative
/// forward-slash paths suitable for passing to `git` with `.current_dir(repo_root)`.
#[must_use]
pub fn resolve_user_pathspecs(
    invocation_cwd: &Path,
    repo_root: &Path,
    raw_paths: &[String],
) -> Vec<String> {
    let canon_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let rel_prefix = cwd_prefix_in_repo(invocation_cwd, repo_root);

    raw_paths
        .iter()
        .map(|raw| {
            if raw.starts_with(":(") || raw.starts_with(":/") || raw.starts_with(":!") {
                return raw.clone();
            }

            let candidate = Path::new(raw);

            if candidate.is_absolute() {
                let canon_cand =
                    std::fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());

                if let Ok(stripped) = canon_cand.strip_prefix(&canon_root) {
                    return normalize_rel_path(stripped);
                }

                return raw.clone();
            }

            if let Some(prefix) = &rel_prefix {
                let joined = prefix.join(candidate);
                return normalize_rel_path(&joined);
            }

            normalize_rel_path(candidate)
        })
        .collect()
}

/// Returns `true` when `token` refers to an existing filesystem path relative to either
/// `invocation_cwd` or `repo_root`.
#[must_use]
pub fn is_existing_user_path(invocation_cwd: &Path, repo_root: &Path, token: &str) -> bool {
    let p = Path::new(token);

    if p.is_absolute() {
        return p.exists();
    }

    invocation_cwd.join(p).exists() || repo_root.join(p).exists()
}
