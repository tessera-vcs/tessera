//! Command-line interface and built-in MCP server for Tessera (`tes`).
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand};
use agent::{
    detect_session_id, handle_post_tool_hook, handle_pre_tool_hook, run_auto_watch_daemon,
    CommitOptions, Coordinator, ResolveStrategy,
};
use serde_json::{json, Value};

pub mod mcp;

/// Returns the ANSI color styling configuration for Clap help output.
fn cli_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::BrightBlack.on_default())
        .valid(AnsiColor::Green.on_default().effects(Effects::BOLD))
        .invalid(AnsiColor::Red.on_default().effects(Effects::BOLD))
        .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
}

/// Returns `true` when ANSI color output is enabled (`NO_COLOR` is not set).
fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// Formats `s` in bold ANSI text when color is enabled.
fn c_bold(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in dim ANSI text when color is enabled.
fn c_dim(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold green ANSI text when color is enabled.
fn c_green(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;32m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold cyan ANSI text when color is enabled.
fn c_cyan(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;36m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold yellow ANSI text when color is enabled.
fn c_yellow(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;33m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold red ANSI text when color is enabled.
fn c_red(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;31m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats `s` in bold orange ANSI text when color is enabled.
fn c_orange(s: impl std::fmt::Display) -> String {
    if use_color() {
        format!("\x1b[1;38;5;208m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Formats a single-letter file status badge (`A`, `M`, `D`) with ANSI color.
fn format_status_badge(st: &str, staged: bool) -> String {
    match st {
        "A" => c_green("A"),
        "D" => c_red("D"),
        "M" if staged => c_green("M"),
        "M" => c_yellow("M"),
        other => c_cyan(other),
    }
}

/// Percent-encodes a filesystem path string for safe inclusion as a URI query parameter value.
fn percent_encode_uri_param(raw: &str) -> String {
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

/// Ensures the background `TesseraURLHandler.app` (`tessera://` URL scheme with `LSUIElement = true`)
/// is compiled and registered with macOS LaunchServices so OSC 8 links focus sessions directly
/// without activating any unrelated IDE window.
#[cfg(target_os = "macos")]
fn ensure_macos_url_handler_installed() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };

        let app_dir = home.join("Applications").join("TesseraURLHandler.app");
        let contents_dir = app_dir.join("Contents");
        let macos_dir = contents_dir.join("MacOS");
        let stamp_file = contents_dir.join("version.txt");
        let bin_path = macos_dir.join("TesseraURLHandler");
        const HANDLER_VERSION: &str = "0.1.0-v2";

        if bin_path.is_file()
            && std::fs::read_to_string(&stamp_file)
                .ok()
                .is_some_and(|v| v.trim() == HANDLER_VERSION)
        {
            return;
        }

        if std::fs::create_dir_all(&macos_dir).is_err() {
            return;
        }

        let objc_source = r#"#import <Cocoa/Cocoa.h>
#import <CoreServices/CoreServices.h>

@interface AppDelegate : NSObject <NSApplicationDelegate>
@end

@implementation AppDelegate
- (void)application:(NSApplication *)application openURLs:(NSArray<NSURL *> *)urls {
    for (NSURL *url in urls) {
        NSURLComponents *comp = [NSURLComponents componentsWithURL:url resolvingAgainstBaseURL:NO];
        NSString *session = nil;
        NSString *repo = nil;

        for (NSURLQueryItem *item in comp.queryItems) {
            if ([item.name isEqualToString:@"session"]) {
                session = item.value;
            } else if ([item.name isEqualToString:@"repo"]) {
                repo = item.value;
            }
        }

        if (session.length > 0) {
            NSString *home = NSHomeDirectory();
            NSString *tesBin = [home stringByAppendingPathComponent:@".cargo/bin/tes"];

            if (![[NSFileManager defaultManager] isExecutableFileAtPath:tesBin]) {
                tesBin = [home stringByAppendingPathComponent:@".local/bin/tes"];
            }

            NSTask *task = [[NSTask alloc] init];
            task.executableURL = [NSURL fileURLWithPath:tesBin];

            if (repo.length > 0) {
                task.arguments = @[@"-C", repo, @"agent", @"focus", session];
            } else {
                task.arguments = @[@"agent", @"focus", session];
            }

            [task launchAndReturnError:nil];
            [task waitUntilExit];
        }
    }

    [NSApp terminate:nil];
}
@end

int main(int argc, const char * argv[]) {
    if (argc > 1 && strcmp(argv[1], "--register") == 0) {
        LSSetDefaultHandlerForURLScheme(CFSTR("tessera"), CFSTR("dev.getlunaris.tessera.urlhandler"));
        return 0;
    }

    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        AppDelegate *delegate = [[AppDelegate alloc] init];
        [app setDelegate:delegate];
        [app run];
    }

    return 0;
}
"#;

        let src_path = contents_dir.join("handler.m");

        if std::fs::write(&src_path, objc_source).is_err() {
            return;
        }

        let compiled = std::process::Command::new("clang")
            .args([
                "-O2",
                "-framework",
                "Cocoa",
                "-framework",
                "CoreServices",
                "-Wno-deprecated-declarations",
            ])
            .arg(&src_path)
            .arg("-o")
            .arg(&bin_path)
            .output()
            .is_ok_and(|o| o.status.success());

        let _ = std::fs::remove_file(&src_path);

        if !compiled {
            return;
        }

        let info_plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>TesseraURLHandler</string>
    <key>CFBundleIdentifier</key>
    <string>dev.getlunaris.tessera.urlhandler</string>
    <key>CFBundleName</key>
    <string>TesseraURLHandler</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>2</string>
    <key>LSUIElement</key>
    <true/>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>CFBundleURLName</key>
            <string>Tessera Session URL</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>tessera</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
"#;

        let _ = std::fs::write(contents_dir.join("Info.plist"), info_plist);
        let _ = std::fs::write(&stamp_file, HANDLER_VERSION);

        let _ = std::process::Command::new("/usr/bin/codesign")
            .args(["-s", "-", "-f", "--deep"])
            .arg(&app_dir)
            .output();

        let lsreg = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

        let _ = std::process::Command::new(lsreg)
            .args(["-f", "-R", "-trusted"])
            .arg(&app_dir)
            .output();

        let _ = std::process::Command::new(&bin_path)
            .arg("--register")
            .output();
    });
}

/// Wraps `styled_text` in an OSC 8 terminal hyperlink targeting `tessera://focus` for `session_id`
/// when standard output is an interactive terminal with color enabled.
fn format_session_link(session_id: &str, repo_root: &Path, styled_text: String) -> String {
    if !use_color() || !io::stdout().is_terminal() {
        return styled_text;
    }

    #[cfg(target_os = "macos")]
    ensure_macos_url_handler_installed();

    let encoded_repo = percent_encode_uri_param(&repo_root.to_string_lossy());

    format!(
        "\x1b]8;;tessera://focus?session={session_id}&repo={encoded_repo}\x1b\\{styled_text}\x1b]8;;\x1b\\"
    )
}

/// Quotes `s` as a POSIX single-quoted string (`'...'`), escaping embedded single quotes as `'\''`.
fn posix_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quotes `s` as a Fish single-quoted string (`'...'`), escaping `\` and `'`.
fn fish_single_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// Quotes `s` as a `tcsh`/`csh` single-quoted string (`'...'`), escaping `'`, `!`, and stripping control characters.
fn tcsh_quote(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !matches!(c, '\n' | '\r' | '\0'))
        .collect();
    let escaped = cleaned.replace('\'', "'\\''").replace('!', "\\!");
    format!("'{escaped}'")
}

/// Quotes `s` as an Elvish single-quoted string (`'...'`), escaping `'` by doubling (`''`).
fn elvish_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Queries `git config core.pager` for `repo_root` when no environment or Tessera pager is set.
fn query_git_core_pager(repo_root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["config", "--get", "core.pager"])
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let val = String::from_utf8_lossy(&out.stdout).trim().to_string();

    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// Resolves the pager command string following Git's precedence order:
/// `TES_PAGER` -> `cfg.pager` -> `GIT_PAGER` -> `git config core.pager` -> `PAGER` -> `less -FRX`.
fn resolve_pager_command(coord: Option<&Coordinator>) -> String {
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

/// Writes `text` directly to `stdout` or pipes it through the configured pager when `stdout` is
/// an interactive terminal and paging is active (`paginate || default_paged`).
fn emit_with_pager(
    text: &str,
    default_paged: bool,
    no_pager: bool,
    paginate: bool,
    json: bool,
    coord: Option<&Coordinator>,
) {
    if text.is_empty() || json || no_pager || !(paginate || default_paged) || !io::stdout().is_terminal() {
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

    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", pager_cmd]);
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", pager_cmd]);
        c
    };

    cmd.env("LESS", less_env)
        .env("LV", lv_env)
        .stdin(Stdio::piped());

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

/// Top-level command-line parser for the `tes` binary.
#[derive(Debug, Parser)]
#[command(
    name = "tes",
    version,
    author,
    styles = cli_styles(),
    about = "Multi-agent coordination layer on top of Git for concurrent AI agents and terminals"
)]
struct Cli {
    /// Emit machine-readable JSON output for AI harnesses.
    #[arg(long, global = true)]
    json: bool,

    /// Do not pipe output into a pager.
    #[arg(short = 'P', long, global = true)]
    no_pager: bool,

    /// Pipe output into a pager if standard output is a terminal.
    #[arg(long, global = true, overrides_with = "no_pager")]
    paginate: bool,

    /// Optional repository path (defaults to current working directory).
    #[arg(short = 'C', long, global = true)]
    repo: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

/// Top-level `tes` subcommands.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize Tessera (`.git/tessera`) and install repo-local AI harness hooks & MCP discovery files.
    Init {
        /// Optional target directory (defaults to current directory).
        path: Option<PathBuf>,
        /// Optional initial/target Git branch name (defaults to active Git branch or `init.defaultBranch`).
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// AI harness integration(s) to install: `antigravity`, `claude`, `cursor`, `windsurf`, `kiro`, `opencode`, `codex`, `vscode`, or `all` (auto-detected if omitted).
        #[arg(long)]
        harness: Option<String>,
    },

    /// Snapshot and display the current session's status, branch, staged/unstaged changes, and touched symbols.
    Status {
        /// Give the output in the short-format (`-s` / `--short` / `--porcelain`).
        #[arg(short = 's', long, visible_alias = "porcelain")]
        short: bool,
        /// Show the branch and session header even in short-format (`-b` / `--branch`).
        #[arg(short = 'b', long)]
        branch: bool,
        /// Optional explicit agent/session ID (auto-detected if omitted).
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Show line-by-line unified diffs for unstaged, staged/session, branch, or revision range (`<revA>..<revB>`) changes.
    Diff {
        /// Optional file paths, revisions, or revision range (`<revA>..<revB>` / `<revA>...<revB>`).
        targets: Vec<String>,
        /// Show only staged/session changes recorded in Tessera (`--staged` / `--cached`).
        #[arg(long, visible_alias = "cached")]
        staged: bool,
        /// Show combined changes across all active sessions on the branch (rather than only this session's changes).
        #[arg(long)]
        all: bool,
        /// Show a diffstat summary of changed files and line counts.
        #[arg(long)]
        stat: bool,
        /// Show only the aggregate line count summary (files changed, insertions, deletions).
        #[arg(long)]
        shortstat: bool,
        /// Show machine-friendly per-file added/deleted line counts.
        #[arg(long)]
        numstat: bool,
        /// Show only names of changed files.
        #[arg(long)]
        name_only: bool,
        /// Show names and status letters (`A`, `M`, `D`) of changed files.
        #[arg(long)]
        name_status: bool,
        /// Generate diffs with `<LINES>` lines of context instead of the default 3.
        #[arg(short = 'U', long = "unified", value_name = "LINES")]
        unified: Option<usize>,
        /// Ignore all whitespace when comparing lines.
        #[arg(short = 'w', long = "ignore-all-space")]
        ignore_all_space: bool,
        /// Ignore changes in amount of whitespace.
        #[arg(long = "ignore-space-change")]
        ignore_space_change: bool,
        /// Compare a specific branch against the trunk branch (`main`).
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
        /// Explicit file or directory paths after `--`.
        #[arg(last = true)]
        pathspecs: Vec<String>,
    },

    /// Display the live Symbol Collision Radar across all active agents.
    Radar,

    /// Declare or update the current session's intent, target branch, and claimed AST symbols/paths.
    Claim {
        /// Natural-language intent or invariant for this session.
        #[arg(long)]
        intent: Option<String>,
        /// Optional target Git branch for this session (e.g., `feat/locale`).
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(long)]
        agent: Option<String>,
        /// Target file paths or `path::symbol` identifiers.
        symbols: Vec<String>,
    },

    /// Create a named checkpoint in the lock-free Operation Log for instant rollback.
    Checkpoint {
        /// Checkpoint label.
        label: String,
        /// Optional explicit agent/session ID.
        #[arg(long)]
        agent: Option<String>,
    },

    /// Manage active agent/terminal sessions and isolated Copy-on-Write sandboxes (`spawn`, `list`, `focus`, `exec`, `clear`).
    #[command(visible_alias = "session")]
    Agent {
        #[command(subcommand)]
        sub: AgentCommands,
    },

    /// Commit the current session's changes onto its active Tessera branch (`refs/heads/<branch>`).
    Commit {
        /// Git commit message.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Target Git branch name (defaults to the session's active Tessera branch).
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Amend the tip commit of the target branch instead of creating a new child commit.
        #[arg(long)]
        amend: bool,
        /// Use the selected commit message without launching an editor (default when amending without `-m`).
        #[arg(long)]
        no_edit: bool,
        /// Speculative verification command to execute before advancing the branch (uses configured `verify_command` if passed without a value).
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        verify: Option<String>,
        /// Append a `Co-authored-by` trailer (defaults to configured `co_author_trailer` when passed without a value).
        #[arg(long, value_name = "NAME <EMAIL>", num_args = 0..=1, default_missing_value = "")]
        co_author: Vec<String>,
        /// Suppress `Co-authored-by` trailers even if `co_author = true` in repository config.
        #[arg(long, conflicts_with = "co_author")]
        no_co_author: bool,
        /// Override the commit author identity (`"Name <email>"`).
        #[arg(long, value_name = "NAME <EMAIL>")]
        author: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Push the current session's active Tessera branch to a remote (`origin` by default), auto-committing pending session changes if needed.
    Push {
        /// Git remote name (defaults to `origin`).
        #[arg(default_value = "origin")]
        remote: String,
        /// Branch to push (defaults to the current session's active Tessera branch).
        branch: Option<String>,
        /// Set upstream tracking reference (`-u` / `--set-upstream`, enabled by default).
        #[arg(short = 'u', long = "set-upstream")]
        set_upstream: bool,
        /// Optional commit message if uncommitted session changes need to be committed before pushing.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Force push (`--force-with-lease`).
        #[arg(short = 'f', long)]
        force: bool,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Fetch from a remote (`origin` by default), fast-forward or 3-way AST-merge the branch, and auto-rebase active sessions.
    Pull {
        /// Git remote name (defaults to `origin`).
        #[arg(default_value = "origin")]
        remote: String,
        /// Branch to pull (defaults to the current session's active Tessera branch).
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Stash and restore uncommitted session changes without affecting concurrent peer sessions (`push`, `pop`, `apply`, `list`, `drop`, `clear`).
    Stash {
        #[command(subcommand)]
        sub: Option<StashCommands>,
        /// Optional stash message when running bare `tes stash -m "<msg>"`.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Optional branch override when running bare `tes stash -b <branch>`.
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Show Git commit history enriched with branch refs, patches, stats, and path filters.
    Log {
        /// Optional branch, revision range (`HEAD~2..HEAD`), or file paths to inspect.
        targets: Vec<String>,
        /// Optional branch flag (`-b <branch>`).
        #[arg(short = 'b', long = "branch")]
        branch_flag: Option<String>,
        /// Maximum number of commits to show (`-n <N>` / `--max-count <N>`).
        #[arg(short = 'n', long, visible_alias = "max-count", default_value_t = 20)]
        limit: usize,
        /// Format each commit on a single concise line.
        #[arg(long)]
        oneline: bool,
        /// Generate patch (unified diff) for each commit (`-p` / `-u` / `--patch`).
        #[arg(short = 'p', short_alias = 'u', long)]
        patch: bool,
        /// Show diffstat of modified files for each commit.
        #[arg(long)]
        stat: bool,
        /// Show only names of changed files for each commit.
        #[arg(long)]
        name_only: bool,
        /// Show names and status letters (`A`, `M`, `D`) of changed files for each commit.
        #[arg(long)]
        name_status: bool,
        /// Draw a text-based graphical representation of the commit history.
        #[arg(long)]
        graph: bool,
        /// Show commits across all branches and refs.
        #[arg(long)]
        all: bool,
        /// Filter commits by author name or email pattern.
        #[arg(long)]
        author: Option<String>,
        /// Filter commits whose commit message matches the given pattern.
        #[arg(long)]
        grep: Option<String>,
        /// Filter commits that introduce or remove a string (`-S <STRING>`).
        #[arg(short = 'S', long = "pickaxe", value_name = "STRING")]
        pickaxe: Option<String>,
        /// Show commits more recent than a specific date (`--since` / `--after`).
        #[arg(long, visible_alias = "after")]
        since: Option<String>,
        /// Show commits older than a specific date (`--until` / `--before`).
        #[arg(long, visible_alias = "before")]
        until: Option<String>,
        /// Output the chosen commits in reverse chronological order.
        #[arg(long)]
        reverse: bool,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
        /// Explicit file or directory paths after `--` (e.g., `tes log -p -n 4 -- docs/`).
        #[arg(last = true)]
        paths: Vec<String>,
    },

    /// Apply the changes introduced by an existing commit onto the active (or specified) branch via 3-way AST merge.
    CherryPick {
        /// Commit SHA-1, branch name, or revision (`HEAD~1`, checkpoint) to cherry-pick.
        commit: String,
        /// Optional target branch name (defaults to the current session's active Tessera branch).
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Semantically merge one or more agent sessions into the target branch using Tree-sitter AST merge.
    Merge {
        /// Agent session IDs to merge (merges all active agents if omitted).
        agents: Vec<String>,
        /// Target Git branch name (defaults to currently checked-out Git branch or configured `trunk_branch`).
        #[arg(long)]
        into: Option<String>,
        /// Custom Git commit message (defaults to a clean message synthesized from agent task intents).
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Speculative verification command to execute in a COW sandbox before advancing the branch (uses configured `verify_command` if passed without a value).
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        verify: Option<String>,
        /// Keep merged agent sessions open instead of auto-closing them after a clean merge.
        #[arg(long)]
        keep_open: bool,
        /// Append a `Co-authored-by` trailer (defaults to configured `co_author_trailer` when passed without a value).
        #[arg(long, value_name = "NAME <EMAIL>", num_args = 0..=1, default_missing_value = "")]
        co_author: Vec<String>,
        /// Suppress `Co-authored-by` trailers even if `co_author = true` in repository config.
        #[arg(long, conflicts_with = "co_author")]
        no_co_author: bool,
        /// Override the commit author identity (`"Name <email>"`).
        #[arg(long, value_name = "NAME <EMAIL>")]
        author: Option<String>,
    },

    /// Non-blocking auto-rebase of all active agent sessions onto the latest trunk commit.
    Sync,

    /// List all active first-class algebraic conflicts with AST context and competing agent intents.
    Conflicts,

    /// Resolve a first-class conflict at `<path>`.
    Resolve {
        /// Repository-relative path of the conflicted file.
        path: String,
        /// Resolution strategy: `ours`, `theirs`, `ast-union`, `working`, or `content`.
        #[arg(long, default_value = "working")]
        strategy: String,
        /// Explicit resolved content when `--strategy content` is selected.
        #[arg(long)]
        content: Option<String>,
    },

    /// Inspect the lock-free Operation Log history.
    Oplog {
        /// Maximum number of operations to return.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },

    /// Roll back repository state and working tree to the previous operation, `--op <prefix>`, or `--checkpoint <label>`.
    Undo {
        /// Optional target operation ID prefix.
        #[arg(long)]
        op: Option<String>,
        /// Optional target checkpoint label created via `tes checkpoint <label>`.
        #[arg(long)]
        checkpoint: Option<String>,
    },

    /// Restore a branch or specific file paths to a Git commit, ref (`HEAD~1`, branch, tag), or named checkpoint.
    #[command(alias = "reset", alias = "discard")]
    Restore {
        /// Commit, ref (`HEAD~1`, `0bbe31f`, checkpoint), and/or file paths to restore.
        targets: Vec<String>,
        /// Optional source commit, ref, or checkpoint when restoring specific file paths (`--source <rev> <paths...>`).
        #[arg(short = 's', long)]
        source: Option<String>,
        /// Optional target branch name (defaults to the current session's active Tessera branch).
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Compact ephemeral working-copy micro-snapshots in `.git/tessera/oplog/`.
    Gc,

    /// Continuously watch the working tree and auto-reconcile edits for hookless editors/harnesses.
    Watch {
        /// Optional explicit agent/session ID (auto-detected if omitted).
        #[arg(short = 'a', long)]
        agent: Option<String>,
        /// Polling interval in milliseconds.
        #[arg(long, default_value_t = 250)]
        interval_ms: u64,
        /// Run a single watch scan tick and exit (useful for scripts and tests).
        #[arg(long)]
        once: bool,
        /// Internal background micro-watcher spawned automatically on `tes claim`.
        #[arg(long, hide = true)]
        auto_daemon: bool,
    },

    /// Internal lifecycle hook entrypoint invoked automatically by Claude Code, Antigravity, and Git hooks.
    Hook {
        /// Hook event kind: `prompt`, `pre-tool`, `post-tool`, or `git-sync`.
        event: String,
        /// Optional explicit session ID.
        #[arg(short = 'a', long, visible_alias = "agent")]
        session: Option<String>,
        /// Optional file path for `pre-tool` / `post-tool`.
        #[arg(long)]
        path: Option<String>,
        /// Optional prompt text for `prompt`.
        #[arg(long)]
        prompt: Option<String>,
    },

    /// List, switch, delete (`-d` / `-D`), or rename (`-m`) Tessera branches.
    #[command(alias = "branches")]
    Branch {
        /// Optional branch name to switch to (or second argument when renaming with `-m <old> <new>`).
        branch: Option<String>,
        /// List branches (`-l` / `--list`).
        #[arg(short = 'l', long)]
        list: bool,
        /// Show commit SHA and subject line for each branch in list mode (`-v` / `--verbose`).
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Delete a branch after verifying it is merged into trunk (`-d <branch>`).
        #[arg(short = 'd', long = "delete")]
        delete: Option<String>,
        /// Force-delete a branch even if it has unmerged commits or uncommitted changes (`-D <branch>`).
        #[arg(short = 'D', long = "force-delete")]
        force_delete: Option<String>,
        /// Rename a branch (`-m <new>` for current branch, or `-m <old> <new>`).
        #[arg(short = 'm', long = "move")]
        rename: Option<String>,
        /// Switch the primary editor working tree (`repo_root` and `.git/HEAD`) to `<branch>`.
        #[arg(short = 'e', long)]
        editor: bool,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Switch or bind the current terminal/session to a specific branch without altering `.git/HEAD` for other sessions.
    #[command(alias = "checkout")]
    Switch {
        /// Target branch name (created from HEAD if it does not exist yet; omit to display the current branch).
        branch: Option<String>,
        /// Create or switch to a branch (`-b <branch>` or `-c <branch>`).
        #[arg(short = 'b', short_alias = 'c', long = "create")]
        create: Option<String>,
        /// Switch the primary editor working tree (`repo_root` and `.git/HEAD`) to `<branch>` while keeping terminal sandboxes isolated.
        #[arg(short = 'e', long)]
        editor: bool,
        /// Optional explicit agent/session ID (auto-detected from terminal/harness if omitted).
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Execute tests or a dev server on a specific branch in a zero-copy Copy-on-Write sandbox (`node_modules`/`target` cloned, isolated `PORT` & `DATABASE_URL`).
    Exec {
        /// Optional target branch name (defaults to the current terminal/session's selected branch).
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
        /// Command and arguments to execute.
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },

    /// Emit or install shell integration (`--zsh`, `--bash`, `--fish`, `--sh`, `--tcsh`, `--nu`, `--elvish`, `--pwsh`, `--install`) for per-terminal branch prompts.
    Env {
        /// Emit the Zsh shell integration plugin (`eval "$(tes env --zsh)"`).
        #[arg(long)]
        zsh: bool,
        /// Emit the Bash shell integration plugin (`eval "$(tes env --bash)"` for `.bashrc`).
        #[arg(long)]
        bash: bool,
        /// Emit the Fish shell integration plugin (`tes env --fish | source`).
        #[arg(long)]
        fish: bool,
        /// Emit POSIX `sh` / `dash` / `ksh` / `mksh` / `oksh` / `yash` shell integration (`eval "$(tes env --sh)"`).
        #[arg(long)]
        sh: bool,
        /// Emit `tcsh` / `csh` shell integration (`eval "`tes env --tcsh`"`).
        #[arg(long, visible_alias = "csh")]
        tcsh: bool,
        /// Emit Nushell (`nu`) integration (`tes env --nu | save -f ~/.cache/tessera/init.nu; source ~/.cache/tessera/init.nu`).
        #[arg(long, visible_alias = "nushell")]
        nu: bool,
        /// Emit Elvish (`elvish`) integration (`eval (e:tes env --elvish | slurp)`).
        #[arg(long)]
        elvish: bool,
        /// Emit PowerShell (`pwsh`) integration (`Invoke-Expression (& tes env --pwsh)`).
        #[arg(long)]
        pwsh: bool,
        /// Automatically install the shell integration into your shell profile (`~/.zshrc`, `~/.bashrc`, `config.fish`).
        #[arg(long)]
        install: bool,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// View or modify repository configuration (`.git/tessera/config.toml` / `tessera.toml`), including custom dependency/build cache folders, ports, and env templates.
    Config {
        #[command(subcommand)]
        sub: Option<ConfigCommands>,
        /// Shorthand to set `verify_command` directly (`tes config --verify-cmd "<cmd>"`).
        #[arg(long = "verify-cmd")]
        verify_cmd: Option<String>,
    },

    /// Start the built-in Model Context Protocol (MCP) JSON-RPC 2.0 server over stdio.
    Mcp,

    /// Internal dynamic tab-completion engine invoked by Zsh, Bash, and Fish shell hooks.
    #[command(hide = true)]
    Complete {
        /// Target shell (`zsh`, `bash`, `fish`, or `plain`).
        #[arg(long, default_value = "plain")]
        shell: String,
        /// Zero-based index of the word currently being completed.
        #[arg(long)]
        cword: Option<usize>,
        /// Command line words.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },

    /// Transparent passthrough to `git <subcommand>` with automatic post-command Tessera state synchronization.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Subcommands for `tes stash`.
#[derive(Debug, Subcommand)]
enum StashCommands {
    /// Save uncommitted session changes to `stash@{0}` and restore clean branch files.
    #[command(alias = "save")]
    Push {
        /// Optional stash message.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Optional target branch override.
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Optional file paths to stash (or message when invoked as `tes stash save <msg>`).
        paths: Vec<String>,
    },
    /// Apply `stash@{index}` via 3-way AST merge and remove it from the stash stack.
    Pop {
        /// Stash index (`0` or `stash@{0}`; defaults to `0`).
        index: Option<String>,
    },
    /// Apply `stash@{index}` via 3-way AST merge without removing it from the stash stack.
    Apply {
        /// Stash index (`0` or `stash@{0}`; defaults to `0`).
        index: Option<String>,
    },
    /// List all stashed change sets.
    List,
    /// Remove a single entry (`stash@{index}`) from the stash stack.
    Drop {
        /// Stash index (`0` or `stash@{0}`; defaults to `0`).
        index: Option<String>,
    },
    /// Remove all entries from the stash stack.
    Clear,
}

/// Subcommands for `tes config`.
#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Display the effective repository configuration (`cow_cache_dirs`, `extra_bin_dirs`, `base_port`, `custom_env`, etc.).
    Show,
    /// Add one or more custom dependency or build cache folders to Copy-on-Write clone and preserve across branches.
    AddCacheDir {
        /// Directory names (e.g., `Pods`, `DerivedData`, `.stack-work`, `bazel-bin`).
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// Add one or more relative binary directories to automatically prepend to `PATH` inside `tes exec`.
    AddBinDir {
        /// Relative binary paths (e.g., `.tox/bin`, `scripts/bin`).
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// Add one or more monorepo package glob patterns.
    AddPackageGlob {
        /// Glob patterns (e.g., `modules/*`, `libs/*`).
        #[arg(required = true)]
        globs: Vec<String>,
    },
    /// Set a custom environment variable template for `tes exec` (supports `{slot}`, `{port}`, `{branch}`, `{agent_id}` placeholders).
    SetEnv {
        /// Environment variable name (e.g., `REDIS_URL`).
        key: String,
        /// Template value (e.g., `redis://localhost:6379/{slot}`).
        value: String,
    },
    /// Set a scalar configuration property (`trunk_branch`, `base_port`, `port_stride`, `verify_command`, `watch_interval_ms`, `watch_timeout_secs`, `co_author`, `co_author_trailer`, `auto_isolate_dirty_claims`, `pager`).
    Set {
        /// Configuration key.
        key: String,
        /// New value.
        value: String,
    },
    /// Trust the repository-tracked `tessera.toml` file so its execution-sensitive settings (`verify_command`, `extra_bin_dirs`, `custom_env`, `pager`) are honored.
    Trust,
    /// Revoke local trust for the repository-tracked `tessera.toml` file.
    Untrust,
}

/// Subcommands for `tes agent` (`tes session`).
#[derive(Debug, Subcommand)]
enum AgentCommands {
    /// Spawn an isolated Copy-on-Write workspace at `.tessera/workspaces/<name>`.
    Spawn {
        /// Agent identifier.
        name: String,
        /// Optional initial task intent.
        #[arg(long)]
        intent: Option<String>,
        /// Optional directory scope prefixes to materialize.
        #[arg(long = "scope")]
        scopes: Vec<String>,
    },
    /// List all active agent sessions, slots, Change-IDs, and intents.
    List,
    /// Focus the terminal tab, multiplexer pane, or IDE window for an active session (`tes session focus <id>`).
    #[command(visible_alias = "goto")]
    Focus {
        /// Session ID, conversation ID, or unique prefix.
        name: String,
    },
    /// Execute a command inside an agent's workspace with isolated `PORT`, `TES_DB_NAME`, and `TMPDIR`.
    Exec {
        /// Agent identifier.
        name: String,
        /// Command and arguments to run.
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Close an agent session (or `--all` sessions) and remove its isolated COW workspace.
    Close {
        /// Agent identifier to close (omit when passing `--all`).
        name: Option<String>,
        /// Close all active sessions.
        #[arg(long)]
        all: bool,
    },
    /// Clear idle or finished agent sessions (`--force` clears sessions with uncommitted changes too).
    #[command(visible_alias = "prune")]
    Clear {
        /// Also clear sessions that have uncommitted changes.
        #[arg(short = 'f', long)]
        force: bool,
    },
}

/// Parses a stash reference (`0` or `stash@{0}`) into its zero-based stack index.
///
/// # Errors
/// Returns [`core::CoreError::InvalidArgument`] if `raw` cannot be parsed as a non-negative integer.
fn parse_stash_index(raw: Option<&str>) -> core::CoreResult<usize> {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(0);
    };

    let inner = s
        .strip_prefix("stash@{")
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or(s);

    inner.parse::<usize>().map_err(|_| {
        core::CoreError::InvalidArgument(format!(
            "invalid stash index '{s}' (expected 0 or stash@{{0}})"
        ))
    })
}

/// Entrypoint for the `tes` CLI and built-in MCP server.
fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cwd = match &cli.repo {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };

    match cli.command {
        Commands::Init {
            path,
            branch,
            harness,
        } => {
            let target = path.unwrap_or_else(|| cwd.clone());
            let (_, report) = Coordinator::init_repo_with_options(
                &target,
                branch.as_deref(),
                harness.as_deref(),
            )?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{} Initialized Tessera in {} (branch: {}, head: {}, op: {})",
                    c_green("✓"),
                    c_bold(&report.repo_root),
                    c_green(&report.trunk_branch),
                    c_yellow(report.head_commit.short()),
                    c_yellow(report.root_op_id.short())
                );
                println!(
                    "  {} {}",
                    c_dim("Installed harness integrations:"),
                    c_cyan(report.harness_files_installed.join(", "))
                );
            }
        }
        Commands::Status {
            short,
            branch,
            agent,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            coord.reconcile_in_flight_sessions();
            let _ = coord.watch_tick(&session_id);
            let meta = coord.snapshot_agent(&session_id, true)?;
            let radar = coord.radar()?;
            let cfg = coord.oplog().load_config()?;
            let current = coord.oplog().load_current_reconciled()?;
            let default_branch = coord
                .git()
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());
            let active_branch = meta
                .target_branch
                .as_deref()
                .unwrap_or(&default_branch)
                .to_string();

            let base_commit = coord.git().read_commit(&meta.base_commit)?;
            let base_map = coord.git().read_tree_to_map(&base_commit.tree_id)?;
            let head_map = coord.git().read_tree_to_map(&meta.head_tree)?;

            // Collect files modified on the same branch by peer sessions so agents
            // can distinguish their own changes from concurrent peer changes.
            let mut peer_changes: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for (peer_id, peer_meta) in &current.view.agents {
                if peer_id == &meta.agent_id {
                    continue;
                }

                if peer_meta.effective_branch(&default_branch) != active_branch {
                    continue;
                }

                for (p, peer_blob) in &peer_meta.own_blobs {
                    if workspace::is_ignored_or_secret_path(
                        coord.git().repo_root(),
                        coord.git().git_dir(),
                        p,
                        base_map.contains_key(p),
                    ) {
                        continue;
                    }

                    if base_map.get(p).map(|e| &e.blob_id) != Some(peer_blob) {
                        peer_changes.entry(p.clone()).or_default().push(peer_id.clone());
                    }
                }

                for p in &peer_meta.deleted_paths {
                    if workspace::is_ignored_or_secret_path(
                        coord.git().repo_root(),
                        coord.git().git_dir(),
                        p,
                        base_map.contains_key(p),
                    ) {
                        continue;
                    }

                    peer_changes.entry(p.clone()).or_default().push(peer_id.clone());
                }
            }

            let mut staged_files: std::collections::BTreeMap<String, &'static str> =
                std::collections::BTreeMap::new();
            for (path, blob_id) in &meta.own_blobs {
                if workspace::is_ignored_or_secret_path(
                    coord.git().repo_root(),
                    coord.git().git_dir(),
                    path,
                    base_map.contains_key(path),
                ) {
                    continue;
                }

                let status = match base_map.get(path) {
                    None => "A",
                    Some(entry) if entry.blob_id != *blob_id => "M",
                    _ => continue,
                };
                staged_files.insert(path.clone(), status);
            }

            for path in &meta.deleted_paths {
                if base_map.contains_key(path) {
                    staged_files.insert(path.clone(), "D");
                }
            }

            let mut unstaged_files: std::collections::BTreeMap<String, &'static str> =
                std::collections::BTreeMap::new();
            for (path, entry) in &head_map {
                if staged_files.contains_key(path) {
                    continue;
                }

                match base_map.get(path) {
                    None => {
                        unstaged_files.insert(path.clone(), "A");
                    }
                    Some(base_entry)
                        if base_entry.blob_id != entry.blob_id
                            || base_entry.executable != entry.executable =>
                    {
                        unstaged_files.insert(path.clone(), "M");
                    }
                    _ => {}
                }
            }

            for path in base_map.keys() {
                if !head_map.contains_key(path) && !staged_files.contains_key(path) {
                    unstaged_files.insert(path.clone(), "D");
                }
            }

            let mut branch_files: std::collections::BTreeMap<String, &'static str> =
                std::collections::BTreeMap::new();
            if active_branch != cfg.trunk_branch {
                if let (Ok(Some(br_cid)), Ok(Some(tr_cid))) = (
                    coord.git().resolve_ref(&format!("refs/heads/{active_branch}")),
                    coord
                        .git()
                        .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch)),
                ) {
                    if br_cid != tr_cid {
                        if let (Ok(br_c), Ok(tr_c)) = (
                            coord.git().read_commit(&br_cid),
                            coord.git().read_commit(&tr_cid),
                        ) {
                            if let (Ok(br_m), Ok(tr_m)) = (
                                coord.git().read_tree_to_map(&br_c.tree_id),
                                coord.git().read_tree_to_map(&tr_c.tree_id),
                            ) {
                                for (p, e) in &br_m {
                                    match tr_m.get(p) {
                                        None => {
                                            branch_files.insert(p.clone(), "A");
                                        }
                                        Some(te) if te.blob_id != e.blob_id => {
                                            branch_files.insert(p.clone(), "M");
                                        }
                                        _ => {}
                                    }
                                }

                                for p in tr_m.keys() {
                                    if !br_m.contains_key(p) {
                                        branch_files.insert(p.clone(), "D");
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let unattributed_files: std::collections::BTreeSet<String> = if active_branch
                == default_branch
            {
                coord
                    .unattributed_worktree_changes()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| !staged_files.contains_key(p) && !unstaged_files.contains_key(p))
                    .collect()
            } else {
                std::collections::BTreeSet::new()
            };

            let session_conflicts: Vec<&core::ConflictRecord> = radar
                .active_conflicts
                .iter()
                .filter(|c| {
                    c.agent_intents.contains_key(&meta.agent_id)
                        || meta.owns_path(&c.path)
                        || meta.read_blobs.contains_key(&c.path)
                })
                .collect();
            let peer_conflict_count = radar.active_conflicts.len() - session_conflicts.len();

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "session": meta,
                        "active_branch": active_branch,
                        "staged_changes": staged_files,
                        "unstaged_changes": unstaged_files,
                        "peer_changes": peer_changes,
                        "unattributed_changes": unattributed_files,
                        "branch_changes": branch_files,
                        "active_conflicts": radar.active_conflicts,
                        "session_conflicts": session_conflicts,
                        "collisions": radar.collisions,
                    }))?
                );
            } else if short {
                let mut out_buf = String::new();

                if branch {
                    let repo_root = coord.git().repo_root();
                    let linked_id =
                        format_session_link(&meta.agent_id, repo_root, c_dim(&meta.agent_id));

                    let _ = writeln!(
                        out_buf,
                        "## {} {}{}{}",
                        c_green(&active_branch),
                        c_dim("[session: "),
                        linked_id,
                        c_dim("]")
                    );
                }

                let mut all_paths: std::collections::BTreeSet<&String> =
                    std::collections::BTreeSet::new();
                for p in staged_files.keys().chain(unstaged_files.keys()) {
                    all_paths.insert(p);
                }

                for p in all_paths {
                    let x = staged_files.get(p).copied().unwrap_or(" ");
                    let y = unstaged_files.get(p).copied().unwrap_or(" ");
                    let x_col = if x == " " { " ".to_string() } else { c_green(x) };
                    let y_col = if y == " " { " ".to_string() } else { c_yellow(y) };
                    let _ = writeln!(out_buf, "{x_col}{y_col} {p}");
                }

                emit_with_pager(
                    &out_buf,
                    false,
                    cli.no_pager,
                    cli.paginate,
                    cli.json,
                    Some(&coord),
                );
            } else {
                let mut out_buf = String::new();
                let repo_root = coord.git().repo_root();

                let _ = writeln!(
                    out_buf,
                    "{} {} (slot {}, change: {}, commit: {})",
                    c_bold("Session:"),
                    format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
                    c_yellow(format!("#{}", meta.slot)),
                    c_yellow(meta.change_id.short()),
                    c_yellow(meta.head_commit.short())
                );

                let _ = writeln!(out_buf, "{}  {}", c_bold("Branch:"), c_green(&active_branch));
                let _ = writeln!(out_buf, "{}  {}", c_bold("Intent:"), meta.intent);

                if !staged_files.is_empty() {
                    let _ = writeln!(out_buf, "\n{}", c_green("Staged / Session Changes:"));

                    for (p, st) in &staged_files {
                        let _ = writeln!(
                            out_buf,
                            "  {}  {}",
                            format_status_badge(st, true),
                            c_green(p)
                        );
                    }
                }

                if !unstaged_files.is_empty() {
                    let _ = writeln!(out_buf, "\n{}", c_yellow("Unstaged Changes:"));

                    for (p, st) in &unstaged_files {
                        let _ = writeln!(
                            out_buf,
                            "  {}  {}",
                            format_status_badge(st, false),
                            c_yellow(p)
                        );
                    }
                }

                if !peer_changes.is_empty() {
                    let _ = writeln!(
                        out_buf,
                        "\n{} '{}':",
                        c_cyan("Peer Session Changes on"),
                        c_green(&active_branch)
                    );

                    for (p, peers) in &peer_changes {
                        let badge = if repo_root.join(p).exists() { "M" } else { "D" };

                        let linked_peers: Vec<String> = peers
                            .iter()
                            .map(|peer_id| format_session_link(peer_id, repo_root, c_dim(peer_id)))
                            .collect();

                        let _ = writeln!(
                            out_buf,
                            "  {}  {} {}{}{}",
                            format_status_badge(badge, false),
                            c_cyan(p),
                            c_dim("("),
                            linked_peers.join(&c_dim(", ")),
                            c_dim(")")
                        );
                    }
                }

                if !unattributed_files.is_empty() {
                    let _ = writeln!(
                        out_buf,
                        "\n{}",
                        c_yellow("Unattributed Working-Tree Changes (not committed by this session):")
                    );
                    for p in &unattributed_files {
                        let badge = if coord.git().repo_root().join(p).exists() { "M" } else { "D" };
                        let _ = writeln!(
                            out_buf,
                            "  {}  {}",
                            format_status_badge(badge, false),
                            c_yellow(p)
                        );
                    }
                }

                if !branch_files.is_empty() {
                    let _ = writeln!(
                        out_buf,
                        "\n{} '{}' (ahead of '{}'):",
                        c_cyan("Committed on"),
                        c_green(&active_branch),
                        c_dim(&cfg.trunk_branch)
                    );
                    for (p, st) in &branch_files {
                        let _ = writeln!(
                            out_buf,
                            "  {}  {}",
                            format_status_badge(st, true),
                            c_cyan(p)
                        );
                    }
                }

                if staged_files.is_empty()
                    && unstaged_files.is_empty()
                    && branch_files.is_empty()
                    && unattributed_files.is_empty()
                {
                    let _ = writeln!(
                        out_buf,
                        "{} {}",
                        c_bold("Changes:"),
                        c_green("✓ working tree clean")
                    );
                }

                if !meta.claimed_symbols.is_empty() {
                    let _ = writeln!(
                        out_buf,
                        "{} {}",
                        c_bold("Symbols:"),
                        c_cyan(
                            meta.claimed_symbols
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    );
                }

                let conflict_str = if session_conflicts.is_empty() {
                    c_green("0")
                } else {
                    c_red(session_conflicts.len())
                };
                if peer_conflict_count > 0 {
                    let _ = writeln!(
                        out_buf,
                        "{} {} {}",
                        c_bold("Conflicts:"),
                        conflict_str,
                        c_dim(format!(
                            "({peer_conflict_count} in peer sessions, not blocking this session; see `tes conflicts`)"
                        ))
                    );
                } else {
                    let _ = writeln!(out_buf, "{} {}", c_bold("Conflicts:"), conflict_str);
                }
                emit_with_pager(
                    &out_buf,
                    false,
                    cli.no_pager,
                    cli.paginate,
                    cli.json,
                    Some(&coord),
                );
            }
        }
        Commands::Diff {
            targets,
            staged,
            all,
            stat,
            shortstat,
            numstat,
            name_only,
            name_status,
            unified,
            ignore_all_space,
            ignore_space_change,
            branch,
            agent,
            pathspecs,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let cfg = coord.oplog().load_config()?;
            let session_id = detect_session_id(agent.as_deref());
            let _ = coord.watch_tick(&session_id);
            let meta = coord.snapshot_agent(&session_id, true)?;
            let default_branch = coord
                .git()
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());
            let active_branch = branch
                .as_deref()
                .or(meta.target_branch.as_deref())
                .unwrap_or(&default_branch)
                .to_string();

            let (range_revs, mut effective_paths) = match targets.first().map(String::as_str) {
                Some(p) if p.contains("..") => {
                    let (left_rev, right_rev) = if let Some((a, b)) = p.split_once("...") {
                        (a.trim(), b.trim())
                    } else if let Some((a, b)) = p.split_once("..") {
                        (a.trim(), b.trim())
                    } else {
                        ("", "")
                    };
                    let l_rev = if left_rev.is_empty() { "HEAD" } else { left_rev };
                    let r_rev = if right_rev.is_empty() { "HEAD" } else { right_rev };
                    (
                        Some((l_rev.to_string(), r_rev.to_string())),
                        targets[1..].to_vec(),
                    )
                }
                Some(first)
                    if targets.len() >= 2
                        && !coord.git().repo_root().join(first).exists()
                        && coord.git().resolve_revision(first).is_ok()
                        && !coord.git().repo_root().join(&targets[1]).exists()
                        && coord.git().resolve_revision(&targets[1]).is_ok() =>
                {
                    (
                        Some((first.to_string(), targets[1].clone())),
                        targets[2..].to_vec(),
                    )
                }
                _ => (None, targets),
            };
            effective_paths.extend(pathspecs);

            let (left_tree, right_tree) = if let Some((l_rev, r_rev)) = range_revs {
                let l_cid = coord.git().resolve_revision(&l_rev)?;
                let r_cid = coord.git().resolve_revision(&r_rev)?;
                (
                    coord.git().read_commit(&l_cid)?.tree_id,
                    coord.git().read_commit(&r_cid)?.tree_id,
                )
            } else if all {
                let base_tree = coord.git().read_commit(&meta.base_commit)?.tree_id;
                let mut combined_map = coord.git().read_tree_to_map(&base_tree)?;
                let current = coord.oplog().load_current_reconciled()?;

                for a in current.view.agents.values() {
                    if a.effective_branch(&default_branch) == active_branch {
                        let a_head = coord.git().read_tree_to_map(&a.head_tree).unwrap_or_default();
                        a.apply_own_changes(&mut combined_map, &a_head);
                    }
                }

                // If a file was edited by multiple sessions, read the reconciled disk version.
                let ws_dir = if active_branch == default_branch {
                    coord.git().repo_root().to_path_buf()
                } else {
                    let sb = coord.branch_sandbox_dir(&active_branch);
                    if sb.is_dir() {
                        sb
                    } else {
                        coord.git().repo_root().to_path_buf()
                    }
                };

                for (p, entry) in &mut combined_map {
                    let disk_file = ws_dir.join(p);
                    if disk_file.is_file() {
                        if let Ok(bytes) = std::fs::read(&disk_file) {
                            if let Ok(disk_bid) = coord.git().write_blob(&bytes) {
                                entry.blob_id = disk_bid;
                            }
                        }
                    }
                }

                let combined_tree = coord.git().write_tree_from_map(&combined_map)?;
                (base_tree, combined_tree)
            } else if branch.is_some()
                || (meta.head_tree == coord.git().read_commit(&meta.base_commit)?.tree_id
                    && active_branch != cfg.trunk_branch)
            {
                let tr_cid = coord
                    .git()
                    .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))?
                    .unwrap_or(meta.base_commit);
                let br_cid = coord
                    .git()
                    .resolve_ref(&format!("refs/heads/{active_branch}"))?
                    .unwrap_or(meta.head_commit);
                (
                    coord.git().read_commit(&tr_cid)?.tree_id,
                    coord.git().read_commit(&br_cid)?.tree_id,
                )
            } else if staged && meta.has_own_changes() {
                let base_tree = coord.git().read_commit(&meta.base_commit)?.tree_id;
                let mut staged_map = coord.git().read_tree_to_map(&base_tree)?;
                let head_map = coord.git().read_tree_to_map(&meta.head_tree).unwrap_or_default();
                meta.apply_own_changes(&mut staged_map, &head_map);
                let staged_tree = coord.git().write_tree_from_map(&staged_map)?;
                (base_tree, staged_tree)
            } else {
                let base_tree = coord.git().read_commit(&meta.base_commit)?.tree_id;
                (base_tree, meta.head_tree)
            };

            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(coord.git().repo_root())
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .arg("--git-dir")
                .arg(coord.git().git_dir())
                .arg("diff")
                .arg(if use_color() && !cli.json {
                    "--color=always"
                } else {
                    "--color=never"
                });

            if stat {
                cmd.arg("--stat");
            }

            if shortstat {
                cmd.arg("--shortstat");
            }

            if numstat {
                cmd.arg("--numstat");
            }

            if name_only {
                cmd.arg("--name-only");
            }

            if name_status {
                cmd.arg("--name-status");
            }

            if let Some(lines) = unified {
                cmd.arg(format!("-U{lines}"));
            }

            if ignore_all_space {
                cmd.arg("-w");
            }

            if ignore_space_change {
                cmd.arg("--ignore-space-change");
            }

            cmd.arg(left_tree.as_str()).arg(right_tree.as_str());

            if !effective_paths.is_empty() {
                cmd.arg("--").args(&effective_paths);
            }

            let out = cmd.output()?;
            let diff_text = String::from_utf8_lossy(&out.stdout);
            emit_with_pager(
                &diff_text,
                true,
                cli.no_pager,
                cli.paginate,
                cli.json,
                Some(&coord),
            );
        }
        Commands::Radar => {
            let coord = Coordinator::open(&cwd)?;
            let report = coord.radar()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let default_branch = coord
                    .git()
                    .current_branch_name()
                    .unwrap_or_else(|_| "main".to_string());
                let mut out_buf = String::new();
                let _ = writeln!(
                    out_buf,
                    "{} ({}):",
                    c_bold("Active Agent Sessions"),
                    c_cyan(report.active_agents.len())
                );
                let repo_root = coord.git().repo_root();

                for a in &report.active_agents {
                    let br = a.target_branch.as_deref().unwrap_or(&default_branch);

                    let _ = writeln!(
                        out_buf,
                        "  {} {} [branch: {}, slot {}, change {}]: \"{}\"",
                        c_orange("•"),
                        format_session_link(&a.agent_id, repo_root, c_cyan(&a.agent_id)),
                        c_green(br),
                        c_yellow(format!("#{}", a.slot)),
                        c_yellow(a.change_id.short()),
                        a.intent
                    );
                }

                if !report.collisions.is_empty() {
                    let _ = writeln!(
                        out_buf,
                        "\n{} ({}):",
                        c_yellow("Live Overlaps"),
                        c_red(report.collisions.len())
                    );

                    for col in &report.collisions {
                        let pairs: Vec<String> = col
                            .competing_agents
                            .iter()
                            .map(|(k, v)| format!("{} (\"{v}\")", format_session_link(k, repo_root, c_cyan(k))))
                            .collect();

                        let _ = writeln!(
                            out_buf,
                            "  {} {} -> {}",
                            c_red("!"),
                            c_bold(&col.target),
                            pairs.join(" vs ")
                        );
                    }
                }
                emit_with_pager(
                    &out_buf,
                    false,
                    cli.no_pager,
                    cli.paginate,
                    cli.json,
                    Some(&coord),
                );
            }
        }
        Commands::Claim {
            intent,
            branch,
            agent,
            symbols,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            let meta = coord.claim_intent_and_symbols_with_branch(
                &session_id,
                intent.as_deref(),
                &symbols,
                branch.as_deref(),
            )?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            } else {
                let repo_root = coord.git().repo_root();

                println!(
                    "{} Updated session '{}': intent=\"{}\"",
                    c_green("✓"),
                    format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
                    c_bold(&meta.intent)
                );
            }
        }
        Commands::Checkpoint { label, agent } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            let op_id = coord.create_checkpoint(&session_id, &label)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "session_id": session_id,
                        "label": label,
                        "op_id": op_id,
                    }))?
                );
            } else {
                println!(
                    "{} Created checkpoint '{}' at operation {}",
                    c_green("✓"),
                    c_cyan(&label),
                    c_yellow(op_id.short())
                );
            }
        }
        Commands::Agent { sub } => {
            let coord = Coordinator::open(&cwd)?;
            match sub {
                AgentCommands::Spawn {
                    name,
                    intent,
                    scopes,
                } => {
                    let meta = coord.spawn_cow_agent(&name, intent.as_deref(), scopes)?;

                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&meta)?);
                    } else {
                        let repo_root = coord.git().repo_root();

                        println!(
                            "{} Spawned COW workspace for '{}' at {}",
                            c_green("✓"),
                            format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
                            c_bold(meta.workspace_path.as_deref().unwrap_or("."))
                        );
                    }
                }
                AgentCommands::List => {
                    let report = coord.radar()?;

                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report.active_agents)?);
                    } else {
                        let repo_root = coord.git().repo_root();

                        for a in &report.active_agents {
                            println!(
                                "{}\tslot={}\tchange={}\tcommit={}\tintent=\"{}\"",
                                format_session_link(&a.agent_id, repo_root, c_cyan(&a.agent_id)),
                                c_yellow(a.slot),
                                c_yellow(a.change_id.short()),
                                c_yellow(a.head_commit.short()),
                                a.intent
                            );
                        }
                    }
                }
                AgentCommands::Focus { name } => {
                    let mut report = coord.focus_session(&name)?;

                    #[cfg(target_os = "macos")]
                    if !report.focused && std::env::var_os("TES_FROM_IDE_EXTENSION").is_none() {
                        if let Some(wp) = &report.waypoint {
                            if wp.tty.is_none() && (wp.ide_pid.is_some() || wp.conversation_id.is_some()) {
                                let scheme = if wp.harness == "antigravity" || report.session_id.starts_with("antigravity-") {
                                    "antigravity-ide"
                                } else if wp.harness == "cursor" || report.session_id.starts_with("cursor-") {
                                    "cursor"
                                } else if wp.harness == "windsurf" || report.session_id.starts_with("windsurf-") {
                                    "windsurf"
                                } else {
                                    "vscode"
                                };

                                let uri = format!("{scheme}://Lunaris.tessera/focus?session={}", report.session_id);

                                if std::process::Command::new("open")
                                    .arg(&uri)
                                    .output()
                                    .is_ok_and(|o| o.status.success())
                                {
                                    report.focused = true;
                                    report.mechanism = format!("{scheme}-uri");
                                    report.detail = format!(
                                        "Focused IDE session via {scheme}:// for {}",
                                        report.session_id
                                    );
                                }
                            }
                        }
                    }

                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        let badge = if report.focused {
                            c_green("✓")
                        } else {
                            c_cyan("ℹ")
                        };

                        println!(
                            "{} {} [harness: {}, mechanism: {}]",
                            badge,
                            report.detail,
                            c_cyan(&report.harness),
                            c_yellow(&report.mechanism)
                        );
                    }
                }
                AgentCommands::Exec { name, cmd } => {
                    let code = coord.exec_in_agent(&name, &cmd)?;
                    if code != 0 {
                        std::process::exit(code);
                    }
                }
                AgentCommands::Close { name, all } => {
                    if all {
                        let removed = coord.clear_agents(true)?;
                        if cli.json {
                            println!("{}", serde_json::to_string_pretty(&removed)?);
                        } else {
                            println!(
                                "{} Closed {} session(s).",
                                c_green("✓"),
                                c_cyan(removed.len())
                            );
                        }
                    } else if let Some(target_name) = name {
                        let closed = coord.close_agent(&target_name)?;
                        if cli.json {
                            println!("{}", serde_json::to_string_pretty(&closed)?);
                        } else {
                            println!(
                                "{} Closed agent session '{}'.",
                                c_green("✓"),
                                c_cyan(&closed.agent_id)
                            );
                        }
                    } else {
                        let sid = detect_session_id(None);
                        let closed = coord.close_agent(&sid)?;
                        if cli.json {
                            println!("{}", serde_json::to_string_pretty(&closed)?);
                        } else {
                            println!(
                                "{} Closed current session '{}'.",
                                c_green("✓"),
                                c_cyan(&closed.agent_id)
                            );
                        }
                    }
                }
                AgentCommands::Clear { force } => {
                    let removed = coord.clear_agents(force)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&removed)?);
                    } else {
                        println!(
                            "{} Cleared {} session(s).",
                            c_green("✓"),
                            c_cyan(removed.len())
                        );
                    }
                }
            }
        }
        Commands::Commit {
            message,
            branch,
            amend,
            no_edit,
            verify,
            co_author,
            no_co_author,
            author,
            agent,
        } => {
            let _ = no_edit;
            let coord = Coordinator::open(&cwd)?;
            let cfg = coord.oplog().load_config()?;
            let eff_verify = match verify.as_deref() {
                Some("") => cfg.verify_command.clone(),
                Some(v) => Some(v.to_string()),
                None => None,
            };
            let session_id = detect_session_id(agent.as_deref());
            let _ = coord.watch_tick(&session_id);
            let commit_options = CommitOptions {
                message,
                author,
                co_author: if no_co_author {
                    Some(false)
                } else if !co_author.is_empty() {
                    Some(true)
                } else {
                    None
                },
                co_author_trailers: co_author,
                amend,
            };
            let report = coord.merge_agents_with_options(
                std::slice::from_ref(&session_id),
                branch.as_deref(),
                eff_verify.as_deref(),
                true,
                &commit_options,
            )?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let badge = if report.conflicts.is_empty() && report.verification_passed {
                    c_green("✓")
                } else {
                    c_red("!")
                };

                if report.created_commit {
                    println!(
                        "{} Committed onto {} -> commit {} (conflicts={})",
                        badge,
                        c_green(&report.target_branch),
                        c_yellow(report.merged_commit_id.short()),
                        if report.conflicts.is_empty() {
                            c_green("0")
                        } else {
                            c_red(report.conflicts.len())
                        }
                    );
                } else {
                    println!(
                        "Nothing to commit on {} (working tree matches commit {})",
                        c_green(&report.target_branch),
                        c_yellow(report.merged_commit_id.short())
                    );
                }
            }
        }
        Commands::Push {
            remote,
            branch,
            set_upstream: _,
            message,
            force,
            agent,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let cfg = coord.oplog().load_config()?;
            let session_id = detect_session_id(agent.as_deref());
            let _ = coord.watch_tick(&session_id);
            let meta = coord.snapshot_agent(&session_id, true)?;
            let default_branch = coord
                .git()
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());
            let active_branch = branch
                .clone()
                .or_else(|| meta.target_branch.clone())
                .unwrap_or(default_branch);
            core::validate_ref_name(&remote)?;
            core::validate_ref_name(&active_branch)?;

            // Auto-commit pending session changes onto `refs/heads/<active_branch>` before pushing.
            if meta.head_commit != meta.base_commit
                || !meta.own_blobs.is_empty()
                || message.is_some()
            {
                let report = coord.merge_agents_with_message(
                    std::slice::from_ref(&session_id),
                    Some(&active_branch),
                    None,
                    true,
                    message.as_deref(),
                )?;
                if !cli.json {
                    println!(
                        "{} Committed session changes onto {} -> commit {}",
                        c_green("✓"),
                        c_green(&report.target_branch),
                        c_yellow(report.merged_commit_id.short())
                    );
                }
            }

            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(coord.git().repo_root())
                .arg("push")
                .arg("-u");
            if force {
                cmd.arg("--force-with-lease");
            }
            cmd.arg("--end-of-options")
                .arg(&remote)
                .arg(format!("refs/heads/{active_branch}:refs/heads/{active_branch}"));

            let status = cmd.status()?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }

            if !cli.json {
                println!(
                    "{} Pushed branch {} to {}",
                    c_green("✓"),
                    c_green(&active_branch),
                    c_cyan(&remote)
                );
            }
        }
        Commands::Merge {
            mut agents,
            into,
            message,
            verify,
            keep_open,
            co_author,
            no_co_author,
            author,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let cfg = coord.oplog().load_config()?;
            let eff_verify = match verify.as_deref() {
                Some("") => cfg.verify_command.clone(),
                Some(v) => Some(v.to_string()),
                None => None,
            };
            let mut normalized_trailers = Vec::new();
            for raw in co_author {
                let trimmed = raw.trim();
                if !trimmed.is_empty() && !trimmed.contains('<') && !trimmed.contains('@') {
                    agents.push(trimmed.to_string());
                    normalized_trailers.push(String::new());
                } else {
                    normalized_trailers.push(raw);
                }
            }
            let commit_options = CommitOptions {
                message,
                author,
                co_author: if no_co_author {
                    Some(false)
                } else if !normalized_trailers.is_empty() {
                    Some(true)
                } else {
                    None
                },
                co_author_trailers: normalized_trailers,
                amend: false,
            };
            let report = coord.merge_agents_with_options(
                &agents,
                into.as_deref(),
                eff_verify.as_deref(),
                keep_open,
                &commit_options,
            )?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let badge = if report.conflicts.is_empty() && report.verification_passed {
                    c_green("✓")
                } else {
                    c_red("!")
                };
                if report.created_commit {
                    println!(
                        "{} Merged into {} -> commit {} (verified={}, conflicts={}, skipped_subtrees={}, renames={}, migrations_resequenced={})",
                        badge,
                        c_green(&report.target_branch),
                        c_yellow(report.merged_commit_id.short()),
                        if report.verification_passed { c_green("true") } else { c_red("false") },
                        if report.conflicts.is_empty() { c_green("0") } else { c_red(report.conflicts.len()) },
                        report.skipped_subtrees,
                        report.renames_applied.len(),
                        report.migrations_resequenced.len()
                    );
                } else {
                    println!(
                        "Nothing to merge into {} (already up to date at commit {})",
                        c_green(&report.target_branch),
                        c_yellow(report.merged_commit_id.short())
                    );
                }
            }
        }
        Commands::Sync => {
            let coord = Coordinator::open(&cwd)?;
            let rebased = coord.sync_all_agents()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&rebased)?);
            } else {
                println!(
                    "{} Synced and auto-rebased {} active session(s).",
                    c_green("✓"),
                    c_cyan(rebased.len())
                );
            }
        }
        Commands::Conflicts => {
            let coord = Coordinator::open(&cwd)?;
            let radar = coord.radar()?;
            if cli.json {
                let mut enriched = Vec::new();
                for c in &radar.active_conflicts {
                    let mut val = serde_json::to_value(c)
                        .unwrap_or_else(|_| json!({}));
                    if let Some(obj) = val.as_object_mut() {
                        obj.insert(
                            "ast_union_preview".to_string(),
                            json!(coord.compute_ast_union_preview(c)),
                        );
                    }
                    enriched.push(val);
                }
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else if radar.active_conflicts.is_empty() {
                println!("{} No active conflicts.", c_green("✓"));
            } else {
                let mut out_buf = String::new();
                let repo_root = coord.git().repo_root();

                for c in &radar.active_conflicts {
                    let sessions_suffix = if c.agent_intents.is_empty() {
                        String::new()
                    } else {
                        let linked_agents: Vec<String> = c
                            .agent_intents
                            .keys()
                            .map(|id| format_session_link(id, repo_root, c_cyan(id)))
                            .collect();

                        format!(" [sessions: {}]", linked_agents.join(", "))
                    };

                    let _ = writeln!(
                        out_buf,
                        "{} Conflict [{}] at {} (kind: {:?}, symbol: {:?}){}",
                        c_red("!"),
                        c_yellow(c.id.short()),
                        c_bold(&c.path),
                        c.kind,
                        c.symbol,
                        sessions_suffix
                    );
                }

                emit_with_pager(
                    &out_buf,
                    false,
                    cli.no_pager,
                    cli.paginate,
                    cli.json,
                    Some(&coord),
                );
            }
        }
        Commands::Resolve {
            path,
            strategy,
            content,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let strat = match strategy.as_str() {
                "ours" => ResolveStrategy::Ours,
                "theirs" => ResolveStrategy::Theirs,
                "ast-union" => ResolveStrategy::AstUnion,
                "content" => ResolveStrategy::ExplicitContent(content.unwrap_or_default()),
                _ => ResolveStrategy::WorkingFile,
            };
            let blob_id = coord.resolve_conflict(&path, strat)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "resolved_path": path,
                        "blob_id": blob_id,
                    }))?
                );
            } else {
                println!(
                    "{} Resolved conflict at {} (blob {})",
                    c_green("✓"),
                    c_bold(&path),
                    c_yellow(blob_id.short())
                );
            }
        }
        Commands::Oplog { limit } => {
            let coord = Coordinator::open(&cwd)?;
            let ops = coord.oplog().list_history(limit)?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&ops)?);
            } else {
                let mut out_buf = String::new();
                let repo_root = coord.git().repo_root();

                for op in &ops {
                    let kind_label = if op.is_ephemeral {
                        c_dim("ephemeral")
                    } else {
                        c_green("milestone")
                    };

                    let _ = writeln!(
                        out_buf,
                        "{} [{}] ({}): {}",
                        c_yellow(op.id.short()),
                        format_session_link(&op.agent_id, repo_root, c_cyan(&op.agent_id)),
                        kind_label,
                        op.description
                    );
                }

                emit_with_pager(
                    &out_buf,
                    true,
                    cli.no_pager,
                    cli.paginate,
                    cli.json,
                    Some(&coord),
                );
            }
        }
        Commands::Undo { op, checkpoint } => {
            let coord = Coordinator::open(&cwd)?;
            let target = op.or(checkpoint);
            let undo_op = coord.undo(target.as_deref())?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&undo_op)?);
            } else {
                println!(
                    "{} Restored state via operation {}",
                    c_green("✓"),
                    c_yellow(undo_op.id.short())
                );
            }
        }
        Commands::Restore {
            targets,
            source,
            branch,
            agent,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            let cfg = coord.oplog().load_config()?;
            let current = coord.oplog().load_current_reconciled()?;
            let default_branch = coord
                .git()
                .current_branch_name()
                .unwrap_or_else(|_| cfg.trunk_branch.clone());
            let eff_branch = branch
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .or_else(|| {
                    current
                        .view
                        .agents
                        .get(&session_id)
                        .and_then(|a| a.target_branch.clone())
                })
                .unwrap_or(default_branch);

            let (resolved_source, resolved_paths): (Option<String>, Vec<String>) =
                if let Some(src) = source {
                    let paths = if targets.is_empty() {
                        vec![".".to_string()]
                    } else {
                        targets
                    };
                    (Some(src), paths)
                } else if targets.is_empty() {
                    (None, vec![".".to_string()])
                } else if targets.len() == 1 {
                    let arg0 = &targets[0];
                    let path_on_disk = coord.git().repo_root().join(arg0).exists();
                    let is_rev = coord
                        .resolve_checkpoint_or_revision(arg0, &eff_branch)
                        .ok()
                        .flatten()
                        .is_some();
                    if is_rev && !path_on_disk {
                        (Some(arg0.clone()), Vec::new())
                    } else if path_on_disk {
                        (None, vec![arg0.clone()])
                    } else if is_rev {
                        (Some(arg0.clone()), Vec::new())
                    } else {
                        (None, vec![arg0.clone()])
                    }
                } else {
                    let arg0 = &targets[0];
                    let path_on_disk = coord.git().repo_root().join(arg0).exists();
                    let is_rev = coord
                        .resolve_checkpoint_or_revision(arg0, &eff_branch)
                        .ok()
                        .flatten()
                        .is_some();
                    if is_rev && !path_on_disk {
                        (Some(arg0.clone()), targets[1..].to_vec())
                    } else {
                        (None, targets)
                    }
                };

            let report = coord.restore(
                &session_id,
                branch.as_deref(),
                resolved_source.as_deref(),
                &resolved_paths,
            )?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.branch_reset {
                println!(
                    "{} Restored branch '{}' to commit {}",
                    c_green("✓"),
                    c_green(&report.branch),
                    c_yellow(report.source_commit_id.short())
                );
            } else {
                println!(
                    "{} Restored {} file(s) on branch '{}' from commit {}",
                    c_green("✓"),
                    c_cyan(report.restored_paths.len()),
                    c_green(&report.branch),
                    c_yellow(report.source_commit_id.short())
                );
            }
        }
        Commands::Gc => {
            let coord = Coordinator::open(&cwd)?;
            let removed = coord.oplog().gc_compact()?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "compacted_operations": removed }))?
                );
            } else {
                println!(
                    "{} Compacted {} ephemeral micro-snapshot operation(s).",
                    c_green("✓"),
                    c_cyan(removed)
                );
            }
        }
        Commands::Watch {
            agent,
            interval_ms,
            once,
            auto_daemon,
        } => {
            let coord = Coordinator::open(&cwd)?;
            if auto_daemon {
                run_auto_watch_daemon(&coord)?;
                return Ok(());
            }

            let session_id = detect_session_id(agent.as_deref());
            loop {
                let outcomes = coord.watch_tick(&session_id)?;
                if cli.json && (once || !outcomes.is_empty()) {
                    println!("{}", serde_json::to_string_pretty(&outcomes)?);
                } else {
                    for out in &outcomes {
                        if let Some(notice) = &out.context_notice {
                            println!("{}", c_yellow(notice));
                        } else {
                            println!(
                                "{} Snapshotted '{}' for session '{}'",
                                c_cyan("[Tessera Watch]:"),
                                c_bold(&out.path),
                                c_cyan(&out.session_id)
                            );
                        }
                    }
                }

                if once {
                    break;
                }

                std::thread::sleep(std::time::Duration::from_millis(interval_ms.max(50)));
            }
        }
        Commands::Hook {
            event,
            session,
            path,
            prompt,
        } => {
            handle_cli_hook(&cwd, &event, session, path, prompt, cli.json)?;
        }
        Commands::Branch {
            branch,
            list,
            verbose,
            delete,
            force_delete,
            rename,
            editor,
            agent,
        } => {
            let _ = list;
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());

            if let Some(del_br) = delete.as_deref().or(force_delete.as_deref()) {
                let force = force_delete.is_some();
                let report = coord.delete_branch(&session_id, del_br, force)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!(
                        "{} Deleted branch '{}' (was {})",
                        c_green("✓"),
                        c_green(&report.deleted_branch),
                        c_yellow(
                            report
                                .previous_commit
                                .as_ref()
                                .map_or("uncommitted", |c| c.short())
                        )
                    );

                    if !report.detached_sessions.is_empty() {
                        println!(
                            "  {} {}",
                            c_dim("Moved sessions to trunk:"),
                            c_cyan(report.detached_sessions.join(", "))
                        );
                    }
                }
            } else if let Some(rename_arg) = rename {
                let (old_name, new_name) = match branch.as_deref() {
                    Some(second) => (rename_arg, second.to_string()),
                    None => {
                        let cfg = coord.oplog().load_config()?;
                        let current = coord.oplog().load_current_reconciled()?;
                        let editor_br = coord
                            .git()
                            .current_branch_name()
                            .unwrap_or_else(|_| cfg.trunk_branch.clone());
                        let active_br = current
                            .view
                            .agents
                            .get(&session_id)
                            .and_then(|a| a.target_branch.clone())
                            .unwrap_or(editor_br);
                        (active_br, rename_arg)
                    }
                };
                let report = coord.rename_branch(&session_id, &old_name, &new_name)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!(
                        "{} Renamed branch '{}' -> '{}'",
                        c_green("✓"),
                        c_yellow(&report.old_branch),
                        c_green(&report.new_branch)
                    );
                }
            } else if let Some(target_br) = branch.filter(|_| !list) {
                if editor {
                    let switched = coord.switch_editor_branch(&target_br)?;
                    let should_bind_caller = session_id != "editor"
                        && (agent.is_some()
                            || std::env::var_os("TES_SESSION_ID").is_some()
                            || coord
                                .oplog()
                                .load_current_reconciled()
                                .ok()
                                .is_some_and(|op| op.view.agents.contains_key(&session_id)));

                    if should_bind_caller {
                        let _ = coord.switch_branch(&session_id, &switched);
                    }

                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "editor_branch": switched,
                            }))?
                        );
                    } else {
                        println!(
                            "{} Switched working tree to branch '{}'",
                            c_green("✓"),
                            c_green(&switched)
                        );
                    }
                } else {
                    let meta = coord.switch_branch(&session_id, &target_br)?;

                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&meta)?);
                    } else {
                        let repo_root = coord.git().repo_root();

                        println!(
                            "{} Session '{}' switched to branch '{}' (slot {}, base: {})",
                            c_green("✓"),
                            format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
                            c_green(meta.target_branch.as_deref().unwrap_or(&target_br)),
                            c_yellow(format!("#{}", meta.slot)),
                            c_yellow(meta.base_commit.short())
                        );
                    }
                }
            } else {
                let cfg = coord.oplog().load_config()?;
                let current = coord.oplog().load_current_reconciled()?;
                let editor_branch = coord
                    .git()
                    .current_branch_name()
                    .unwrap_or_else(|_| cfg.trunk_branch.clone());
                let active_branch = current
                    .view
                    .agents
                    .get(&session_id)
                    .and_then(|a| a.target_branch.clone())
                    .unwrap_or_else(|| editor_branch.clone());
                let mut all_branches: std::collections::BTreeSet<String> =
                    current.view.branches.keys().cloned().collect();

                for git_br in coord.git().list_local_branches() {
                    all_branches.insert(git_br);
                }

                all_branches.insert(editor_branch.clone());
                all_branches.insert(active_branch.clone());

                for a in current.view.agents.values() {
                    if let Some(tb) = &a.target_branch {
                        all_branches.insert(tb.clone());
                    }
                }

                if cli.json {
                    let main_files = coord
                        .git()
                        .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))
                        .ok()
                        .flatten()
                        .and_then(|cid| coord.git().read_commit(&cid).ok())
                        .and_then(|c| coord.git().read_tree_to_map(&c.tree_id).ok())
                        .unwrap_or_default();

                    let mut branch_details = Vec::new();
                    for br in &all_branches {
                        let cid_opt = coord
                            .git()
                            .resolve_ref(&format!("refs/heads/{br}"))
                            .ok()
                            .flatten()
                            .or_else(|| current.view.branches.get(br).cloned());
                        let commit_str = cid_opt
                            .as_ref()
                            .map(|c| c.as_str().to_string())
                            .unwrap_or_default();
                        let short_commit = cid_opt
                            .as_ref()
                            .map(|c| c.short().to_string())
                            .unwrap_or_default();

                        let mut modified_files = std::collections::BTreeSet::new();
                        if let Some(cid) = &cid_opt {
                            if let Ok(c) = coord.git().read_commit(cid) {
                                if let Ok(br_files) = coord.git().read_tree_to_map(&c.tree_id) {
                                    for (p, entry) in &br_files {
                                        if main_files.get(p).map(|e| &e.blob_id)
                                            != Some(&entry.blob_id)
                                        {
                                            modified_files.insert(p.clone());
                                        }
                                    }

                                    for p in main_files.keys() {
                                        if !br_files.contains_key(p) {
                                            modified_files.insert(p.clone());
                                        }
                                    }
                                }
                            }
                        }

                        let mut sessions = Vec::new();
                        let mut port = cfg.base_port;
                        for (aid, a) in &current.view.agents {
                            let a_br = a.target_branch.as_deref().unwrap_or(&editor_branch);
                            if a_br == br {
                                sessions.push(aid.clone());
                                port = cfg.base_port + a.slot.saturating_mul(cfg.port_stride);
                                for p in a.own_blobs.keys() {
                                    modified_files.insert(p.clone());
                                }
                            }
                        }

                        let ws_dir = coord.branch_sandbox_dir(br);
                        branch_details.push(json!({
                            "name": br,
                            "commit": commit_str,
                            "short_commit": short_commit,
                            "is_editor_branch": *br == editor_branch,
                            "workspace_path": ws_dir.display().to_string(),
                            "port": port,
                            "sessions": sessions,
                            "modified_files": modified_files,
                        }));
                    }

                    let commit_graph = coord.git().list_commit_graph(60);
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "session_id": session_id,
                            "trunk_branch": cfg.trunk_branch,
                            "editor_branch": editor_branch,
                            "active_branch": active_branch,
                            "branches": all_branches,
                            "branch_details": branch_details,
                            "commit_graph": commit_graph,
                        }))?
                    );
                } else {
                    let mut out = String::new();
                    let repo_root = coord.git().repo_root();
                    let linked_session =
                        format_session_link(&session_id, repo_root, c_dim(&session_id));

                    for br in &all_branches {
                        if verbose {
                            let cid_opt = coord
                                .git()
                                .resolve_ref(&format!("refs/heads/{br}"))
                                .ok()
                                .flatten()
                                .or_else(|| current.view.branches.get(br).cloned());
                            let short_sha = cid_opt
                                .as_ref()
                                .map(|c| c.short().to_string())
                                .unwrap_or_default();
                            let subject = cid_opt
                                .as_ref()
                                .and_then(|c| coord.git().read_commit(c).ok())
                                .and_then(|cm| cm.message.lines().next().map(ToString::to_string))
                                .unwrap_or_default();

                            if *br == active_branch {
                                let _ = writeln!(
                                    out,
                                    "{} {:<20} {} {} {}{}{}",
                                    c_green("*"),
                                    c_green(br),
                                    c_yellow(&short_sha),
                                    subject,
                                    c_dim("(current session: "),
                                    linked_session,
                                    c_dim(")")
                                );
                            } else {
                                let _ = writeln!(
                                    out,
                                    "  {:<20} {} {}",
                                    br,
                                    c_yellow(&short_sha),
                                    subject
                                );
                            }
                        } else if *br == active_branch {
                            let _ = writeln!(
                                out,
                                "{} {} {}{}{}",
                                c_green("*"),
                                c_green(br),
                                c_dim("(current session: "),
                                linked_session,
                                c_dim(")")
                            );
                        } else {
                            let _ = writeln!(out, "  {br}");
                        }
                    }

                    emit_with_pager(
                        &out,
                        true,
                        cli.no_pager,
                        cli.paginate,
                        cli.json,
                        Some(&coord),
                    );
                }
            }
        }
        Commands::Switch {
            branch,
            create,
            editor,
            agent,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            if let Some(target_br) = create.or(branch) {
                if editor {
                    let switched = coord.switch_editor_branch(&target_br)?;
                    let should_bind_caller = session_id != "editor"
                        && (agent.is_some()
                            || std::env::var_os("TES_SESSION_ID").is_some()
                            || coord
                                .oplog()
                                .load_current_reconciled()
                                .ok()
                                .is_some_and(|op| op.view.agents.contains_key(&session_id)));

                    if should_bind_caller {
                        let _ = coord.switch_branch(&session_id, &switched);
                    }

                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "editor_branch": switched,
                            }))?
                        );
                    } else {
                        println!(
                            "{} Switched working tree to branch '{}'",
                            c_green("✓"),
                            c_green(&switched)
                        );
                    }
                } else {
                    let meta = coord.switch_branch(&session_id, &target_br)?;

                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&meta)?);
                    } else {
                        let repo_root = coord.git().repo_root();

                        println!(
                            "{} Session '{}' switched to branch '{}' (slot {}, base: {})",
                            c_green("✓"),
                            format_session_link(&meta.agent_id, repo_root, c_cyan(&meta.agent_id)),
                            c_green(meta.target_branch.as_deref().unwrap_or(&target_br)),
                            c_yellow(format!("#{}", meta.slot)),
                            c_yellow(meta.base_commit.short())
                        );
                    }
                }
            } else {
                let cfg = coord.oplog().load_config()?;
                let current = coord.oplog().load_current_reconciled()?;
                let editor_branch = coord
                    .git()
                    .current_branch_name()
                    .unwrap_or_else(|_| cfg.trunk_branch.clone());
                let active_branch = current
                    .view
                    .agents
                    .get(&session_id)
                    .and_then(|a| a.target_branch.clone())
                    .unwrap_or_else(|| editor_branch.clone());
                let mut all_branches: std::collections::BTreeSet<String> =
                    current.view.branches.keys().cloned().collect();

                for git_br in coord.git().list_local_branches() {
                    all_branches.insert(git_br);
                }

                all_branches.insert(editor_branch.clone());
                all_branches.insert(active_branch.clone());

                for a in current.view.agents.values() {
                    if let Some(tb) = &a.target_branch {
                        all_branches.insert(tb.clone());
                    }
                }

                if cli.json {
                    let main_files = coord
                        .git()
                        .resolve_ref(&format!("refs/heads/{}", cfg.trunk_branch))
                        .ok()
                        .flatten()
                        .and_then(|cid| coord.git().read_commit(&cid).ok())
                        .and_then(|c| coord.git().read_tree_to_map(&c.tree_id).ok())
                        .unwrap_or_default();

                    let mut branch_details = Vec::new();
                    for br in &all_branches {
                        let cid_opt = coord
                            .git()
                            .resolve_ref(&format!("refs/heads/{br}"))
                            .ok()
                            .flatten()
                            .or_else(|| current.view.branches.get(br).cloned());
                        let commit_str = cid_opt
                            .as_ref()
                            .map(|c| c.as_str().to_string())
                            .unwrap_or_default();
                        let short_commit = cid_opt
                            .as_ref()
                            .map(|c| c.short().to_string())
                            .unwrap_or_default();

                        let mut modified_files = std::collections::BTreeSet::new();
                        if let Some(cid) = &cid_opt {
                            if let Ok(c) = coord.git().read_commit(cid) {
                                if let Ok(br_files) = coord.git().read_tree_to_map(&c.tree_id) {
                                    for (p, entry) in &br_files {
                                        if main_files.get(p).map(|e| &e.blob_id)
                                            != Some(&entry.blob_id)
                                        {
                                            modified_files.insert(p.clone());
                                        }
                                    }

                                    for p in main_files.keys() {
                                        if !br_files.contains_key(p) {
                                            modified_files.insert(p.clone());
                                        }
                                    }
                                }
                            }
                        }

                        let mut sessions = Vec::new();
                        let mut port = cfg.base_port;
                        for (aid, a) in &current.view.agents {
                            let a_br = a.target_branch.as_deref().unwrap_or(&editor_branch);
                            if a_br == br {
                                sessions.push(aid.clone());
                                port = cfg.base_port + a.slot.saturating_mul(cfg.port_stride);
                                for p in a.own_blobs.keys() {
                                    modified_files.insert(p.clone());
                                }
                            }
                        }

                        let ws_dir = coord.branch_sandbox_dir(br);
                        branch_details.push(json!({
                            "name": br,
                            "commit": commit_str,
                            "short_commit": short_commit,
                            "is_editor_branch": *br == editor_branch,
                            "workspace_path": ws_dir.display().to_string(),
                            "port": port,
                            "sessions": sessions,
                            "modified_files": modified_files,
                        }));
                    }

                    let commit_graph = coord.git().list_commit_graph(60);
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "session_id": session_id,
                            "trunk_branch": cfg.trunk_branch,
                            "editor_branch": editor_branch,
                            "active_branch": active_branch,
                            "branches": all_branches,
                            "branch_details": branch_details,
                            "commit_graph": commit_graph,
                        }))?
                    );
                } else {
                    let mut out = String::new();
                    let repo_root = coord.git().repo_root();
                    let linked_session =
                        format_session_link(&session_id, repo_root, c_dim(&session_id));

                    for br in &all_branches {
                        if *br == active_branch {
                            let _ = writeln!(
                                out,
                                "{} {} {}{}{}",
                                c_green("*"),
                                c_green(br),
                                c_dim("(current session: "),
                                linked_session,
                                c_dim(")")
                            );
                        } else {
                            let _ = writeln!(out, "  {br}");
                        }
                    }

                    emit_with_pager(
                        &out,
                        true,
                        cli.no_pager,
                        cli.paginate,
                        cli.json,
                        Some(&coord),
                    );
                }
            }
        }
        Commands::Exec { branch, agent, cmd } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            let code = coord.exec_on_branch(&session_id, branch.as_deref(), &cmd)?;
            std::process::exit(code);
        }
        Commands::Env {
            zsh,
            bash,
            fish,
            sh,
            tcsh,
            nu,
            elvish,
            pwsh,
            install,
            agent,
        } => {
            if install {
                let mut updated = Vec::new();
                if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
                    let home_path = std::path::PathBuf::from(home);
                    let targets = [
                        (
                            home_path.join(".zshrc"),
                            "eval \"$(tes env --zsh 2>/dev/null)\"",
                            !cfg!(windows) || home_path.join(".zshrc").exists(),
                        ),
                        (
                            home_path.join(".bashrc"),
                            "eval \"$(tes env --bash 2>/dev/null)\"",
                            true,
                        ),
                        (
                            home_path.join(".bash_profile"),
                            "eval \"$(tes env --bash 2>/dev/null)\"",
                            home_path.join(".bash_profile").exists(),
                        ),
                        (
                            home_path.join(".profile"),
                            "eval \"$(tes env --sh 2>/dev/null)\"",
                            home_path.join(".profile").exists(),
                        ),
                        (
                            home_path.join(".kshrc"),
                            "eval \"$(tes env --sh 2>/dev/null)\"",
                            home_path.join(".kshrc").exists(),
                        ),
                        (
                            home_path.join(".mkshrc"),
                            "eval \"$(tes env --sh 2>/dev/null)\"",
                            home_path.join(".mkshrc").exists(),
                        ),
                        (
                            home_path.join(".tcshrc"),
                            "eval \"`tes env --tcsh`\"",
                            home_path.join(".tcshrc").exists(),
                        ),
                        (
                            home_path.join(".cshrc"),
                            "eval \"`tes env --tcsh`\"",
                            home_path.join(".cshrc").exists(),
                        ),
                        (
                            home_path.join(".config/fish/config.fish"),
                            "command -sq tes; and tes env --fish 2>/dev/null | source",
                            home_path.join(".config/fish").exists(),
                        ),
                        (
                            home_path.join("Documents/PowerShell/Microsoft.PowerShell_profile.ps1"),
                            "Invoke-Expression (& tes env --pwsh)",
                            home_path.join("Documents/PowerShell").exists(),
                        ),
                        (
                            home_path.join(".config/powershell/Microsoft.PowerShell_profile.ps1"),
                            "Invoke-Expression (& tes env --pwsh)",
                            home_path.join(".config/powershell").exists(),
                        ),
                    ];

                    for (rc_path, snippet, should_write) in targets {
                        if !should_write {
                            continue;
                        }

                        let existing = std::fs::read_to_string(&rc_path).unwrap_or_default();
                        if !existing.contains("tes env") {
                            let mut next = existing;
                            if !next.is_empty() && !next.ends_with('\n') {
                                next.push('\n');
                            }
                            next.push_str("\n# Tessera terminal branch prompt\n");
                            next.push_str(snippet);
                            next.push('\n');

                            if std::fs::write(&rc_path, next).is_ok() {
                                updated.push(rc_path.display().to_string());
                            }
                        } else {
                            updated.push(format!("{} (already configured)", rc_path.display()));
                        }
                    }
                }

                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "configured_profiles": updated }))?
                    );
                } else {
                    println!(
                        "Configured shell prompt integration in: {}",
                        updated.join(", ")
                    );
                }
            } else if zsh {
                let zsh_plugin = r#"# Tessera branch prompt & tab-completion integration for Zsh (Powerlevel10k, Oh My Zsh, Starship, standard Zsh)
export TES_SESSION_ID="term-$$"
setopt PROMPT_SUBST

tes_prompt_info() {
  if [ -n "$TES_BRANCH" ]; then
    echo "%F{cyan}(tes:${TES_BRANCH})%f "
  fi
}

# Powerlevel10k native segment support
prompt_tes() {
  if [[ -n "$TES_BRANCH" ]]; then
    p10k segment -f cyan -t "(tes:$TES_BRANCH)"
  fi
}

_tes_preexec() {
  if [[ "$1" == tes\ * || "$1" == "tes" || "$1" == git\ * || "$1" == "git" || "$1" == cd\ * || "$1" == "cd" ]]; then
    return
  fi
  local phys_pwd="${PWD:A}"
  if [[ -n "$TES_WORKSPACE" && -d "$TES_WORKSPACE" && -n "$TES_REPO_ROOT" && "$TES_WORKSPACE" != "$TES_REPO_ROOT" && -d "$TES_REPO_ROOT/.git/tessera" && "$phys_pwd" == "$TES_REPO_ROOT"* ]]; then
    _TES_PREV_PWD="$PWD"
    local rel_sub="${phys_pwd#$TES_REPO_ROOT}"
    if [[ -d "${TES_WORKSPACE}${rel_sub}" ]]; then
      builtin cd -q "${TES_WORKSPACE}${rel_sub}"
    else
      builtin cd -q "$TES_WORKSPACE"
    fi
  fi
}

_tes_precmd() {
  export TES_SESSION_ID="term-$$"
  if [[ -n "$_TES_PREV_PWD" ]]; then
    if [[ -d "$_TES_PREV_PWD" ]]; then
      builtin cd -q "$_TES_PREV_PWD"
    fi
    unset _TES_PREV_PWD
  elif [[ -n "$TES_WORKSPACE" && "$PWD" == "$TES_WORKSPACE"* && -f "$PWD/.tessera/repo_root" && ! -L "$PWD/.tessera/repo_root" ]]; then
    local root_dir
    root_dir="$(<"$PWD/.tessera/repo_root")"
    if [[ -n "$root_dir" && -d "$root_dir/.git/tessera" ]]; then
      builtin cd -q "$root_dir"
    fi
  fi
  local env_out
  if command -v tes >/dev/null 2>&1 && env_out="$(command tes env 2>/dev/null)" && [[ -n "$env_out" ]]; then
    eval "$env_out"
  else
    unset TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT
  fi
}

_tes_ensure_prompt() {
  if (( ! $+functions[p10k] )); then
    if [[ "$PROMPT" != *"tes_prompt_info"* && "$RPROMPT" != *"tes_prompt_info"* ]]; then
      PROMPT='$(tes_prompt_info)'"$PROMPT"
    fi
  fi
}

tes() {
  export TES_SESSION_ID="term-$$"
  command tes "$@"
  local ret=$?
  if [[ "$1" != "env" ]]; then
    _tes_precmd
  fi
  return $ret
}

preexec_functions=(_tes_preexec ${preexec_functions:#_tes_preexec})
precmd_functions=(_tes_precmd ${precmd_functions:#_tes_precmd})
precmd_functions=(${precmd_functions:#_tes_ensure_prompt} _tes_ensure_prompt)

if (( $+functions[p10k] )); then
  if (( ! ${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[(I)tes]} )); then
    local vcs_idx=${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[(I)vcs]}
    if (( vcs_idx > 0 )); then
      POWERLEVEL9K_LEFT_PROMPT_ELEMENTS=(
        "${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[@]:0:$vcs_idx}"
        tes
        "${POWERLEVEL9K_LEFT_PROMPT_ELEMENTS[@]:$vcs_idx}"
      )
    else
      POWERLEVEL9K_LEFT_PROMPT_ELEMENTS+=(tes)
    fi
    p10k reload
  fi
else
  _tes_ensure_prompt
fi

_tes_completion() {
  local -a completions
  local IFS=$'\n'
  local cword=$(( CURRENT - 1 ))
  completions=($(command tes complete --shell zsh --cword "$cword" -- "${words[@]}" 2>/dev/null))
  if (( ${#completions[@]} > 0 )); then
    _describe -t tes 'tes' completions
  else
    _files
  fi
}
if (( $+functions[compdef] )); then
  compdef _tes_completion tes
fi

_tes_precmd
"#;
                print!("{zsh_plugin}");
            } else if bash {
                let bash_plugin = r#"# Tessera branch prompt & tab-completion integration for Bash
export TES_SESSION_ID="term-$$"

tes_prompt_info() {
  if [ -n "$TES_BRANCH" ]; then
    printf "\001\033[36m\002(tes:%s)\001\033[0m\002 " "$TES_BRANCH"
  fi
}

_tes_preexec() {
  if [ -n "${COMP_LINE:-}" ]; then
    return
  fi
  case "$1" in
    tes\ *|tes|git\ *|git|cd\ *|cd|_tes_*|tes_prompt_info*) return ;;
  esac
  if [ -n "$TES_WORKSPACE" ] && [ -d "$TES_WORKSPACE" ] && [ -n "$TES_REPO_ROOT" ] && [ "$TES_WORKSPACE" != "$TES_REPO_ROOT" ] && [ -d "$TES_REPO_ROOT/.git/tessera" ]; then
    local phys_pwd
    phys_pwd="$(builtin pwd -P 2>/dev/null || printf '%s' "$PWD")"
    case "$phys_pwd" in
      "$TES_REPO_ROOT"*)
        _TES_PREV_PWD="$PWD"
        local rel_sub="${phys_pwd#$TES_REPO_ROOT}"
        if [ -d "${TES_WORKSPACE}${rel_sub}" ]; then
          builtin cd "${TES_WORKSPACE}${rel_sub}"
        else
          builtin cd "$TES_WORKSPACE"
        fi
        ;;
    esac
  fi
}

_tes_prompt_command() {
  export TES_SESSION_ID="term-$$"
  if [ -n "$_TES_PREV_PWD" ]; then
    if [ -d "$_TES_PREV_PWD" ]; then
      builtin cd "$_TES_PREV_PWD"
    fi
    unset _TES_PREV_PWD
  elif [ -n "$TES_WORKSPACE" ] && [[ "$PWD" == "$TES_WORKSPACE"* ]] && [ -f "$PWD/.tessera/repo_root" ] && [ ! -L "$PWD/.tessera/repo_root" ]; then
    local root_dir
    root_dir="$(cat "$PWD/.tessera/repo_root" 2>/dev/null)"
    if [ -n "$root_dir" ] && [ -d "$root_dir/.git/tessera" ]; then
      builtin cd "$root_dir"
    fi
  fi
  local env_out
  if command -v tes >/dev/null 2>&1 && env_out="$(command tes env 2>/dev/null)" && [ -n "$env_out" ]; then
    eval "$env_out"
  else
    unset TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT
  fi
  if [[ "$PS1" != *"tes_prompt_info"* ]]; then
    PS1='$(tes_prompt_info)'"$PS1"
  fi
}

tes() {
  export TES_SESSION_ID="term-$$"
  command tes "$@"
  local ret=$?
  if [ "$1" != "env" ]; then
    _tes_prompt_command
  fi
  return $ret
}

if [[ ";${PROMPT_COMMAND:-};" != *";_tes_prompt_command;"* ]]; then
  PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND;}_tes_prompt_command"
fi
trap '_tes_preexec "$BASH_COMMAND"' DEBUG

_tes_bash_completion() {
  local IFS=$'\n'
  COMPREPLY=($(command tes complete --shell bash --cword "$COMP_CWORD" -- "${COMP_WORDS[@]}" 2>/dev/null))
}
complete -o default -F _tes_bash_completion tes

_tes_prompt_command
"#;
                print!("{bash_plugin}");
            } else if fish {
                let fish_plugin = r#"# Tessera branch prompt & tab-completion integration for Fish
set -gx TES_SESSION_ID "term-$fish_pid"
set -gx TES_SHELL "fish"

function _tes_preexec --on-event fish_preexec
    if string match -qr '^(tes|git|cd)(\s|$)' -- "$argv[1]"
        return
    end
    set -l phys_pwd (pwd -P 2>/dev/null; or echo "$PWD")
    if set -q TES_WORKSPACE; and test -d "$TES_WORKSPACE"; and set -q TES_REPO_ROOT; and test "$TES_WORKSPACE" != "$TES_REPO_ROOT"; and test -d "$TES_REPO_ROOT/.git/tessera"; and string match -q "$TES_REPO_ROOT*" -- "$phys_pwd"
        set -g _TES_PREV_PWD "$PWD"
        set -l rel_sub (string replace -- "$TES_REPO_ROOT" "" "$phys_pwd")
        if test -d "$TES_WORKSPACE$rel_sub"
            builtin cd "$TES_WORKSPACE$rel_sub"
        else
            builtin cd "$TES_WORKSPACE"
        end
    end
end

function _tes_sync_env --on-event fish_prompt --on-event fish_postexec
    set -gx TES_SESSION_ID "term-$fish_pid"
    set -gx TES_SHELL "fish"
    if set -q _TES_PREV_PWD
        if test -d "$_TES_PREV_PWD"
            builtin cd "$_TES_PREV_PWD"
        end
        set -e _TES_PREV_PWD
    else if set -q TES_WORKSPACE; and string match -q "$TES_WORKSPACE*" -- "$PWD"; and test -f "$PWD/.tessera/repo_root"; and not test -L "$PWD/.tessera/repo_root"
        set -l root_dir (cat "$PWD/.tessera/repo_root" 2>/dev/null)
        if test -n "$root_dir"; and test -d "$root_dir/.git/tessera"
            builtin cd "$root_dir"
        end
    end
    if command -sq tes
        set -l env_out (command tes env 2>/dev/null)
        if test -n "$env_out"
            printf "%s\n" $env_out | source
            return
        end
    end
    set -e TES_BRANCH TES_PORT TES_WORKSPACE TES_REPO_ROOT
end

function tes --wraps tes
    set -gx TES_SESSION_ID "term-$fish_pid"
    command tes $argv
    set -l ret $status
    if test "$argv[1]" != "env"
        _tes_sync_env
    end
    return $ret
end

if not functions -q _tes_orig_fish_prompt
    if functions -q fish_prompt
        functions -c fish_prompt _tes_orig_fish_prompt
        function fish_prompt
            if set -q TES_BRANCH
                set_color cyan
                echo -n "(tes:$TES_BRANCH) "
                set_color normal
            end
            _tes_orig_fish_prompt
        end
    end
end

function __tes_complete
    set -l cmd (commandline -opc)
    set -l cur (commandline -ct)
    set -l cword (count $cmd)
    set -a cmd $cur
    command tes complete --shell fish --cword $cword -- $cmd 2>/dev/null
end
complete -c tes -f -a "(__tes_complete)"

_tes_sync_env
"#;
                print!("{fish_plugin}");
            } else if sh {
                let sh_plugin = r#"# Tessera branch prompt integration for POSIX sh / dash / ksh / mksh / oksh / yash
export TES_SESSION_ID="term-$$"

_tes_sh_sync() {
  export TES_SESSION_ID="term-$$"
  if [ -n "${_TES_PREV_PWD:-}" ]; then
    if [ -d "$_TES_PREV_PWD" ]; then
      cd "$_TES_PREV_PWD" || true
    fi
    unset _TES_PREV_PWD
  elif [ -n "${TES_WORKSPACE:-}" ] && [ -f "$PWD/.tessera/repo_root" ] && [ ! -L "$PWD/.tessera/repo_root" ]; then
    case "$PWD" in
      "$TES_WORKSPACE"*)
        _tes_root_dir="$(cat "$PWD/.tessera/repo_root" 2>/dev/null)"
        if [ -n "$_tes_root_dir" ] && [ -d "$_tes_root_dir/.git/tessera" ]; then
          cd "$_tes_root_dir" || true
        fi
        unset _tes_root_dir
        ;;
    esac
  fi
  if command -v tes >/dev/null 2>&1; then
    _tes_out="$(command tes env 2>/dev/null)"
    if [ -n "$_tes_out" ]; then
      eval "$_tes_out"
      TES_PROMPT="(tes:${TES_BRANCH}) "
      export TES_PROMPT
      return
    fi
  fi
  unset TES_BRANCH TES_PORT TES_PROMPT TES_WORKSPACE TES_REPO_ROOT
}

tes() {
  export TES_SESSION_ID="term-$$"
  command tes "$@"
  _tes_ret=$?
  if [ "${1:-}" != "env" ]; then
    _tes_sh_sync
  fi
  return $_tes_ret
}

cd() {
  command cd "$@"
  _tes_ret=$?
  _tes_sh_sync
  return $_tes_ret
}

case "${PS1:-}" in
  *TES_PROMPT*) ;;
  *) PS1='${TES_PROMPT:-}'"${PS1:-$ }" ;;
esac

_tes_sh_sync
"#;
                print!("{sh_plugin}");
            } else if tcsh {
                let tcsh_plugin = r#"setenv TES_SESSION_ID "term-$$";
setenv TES_SHELL "tcsh";
alias _tes_sync 'eval "`\tes env`"';
alias tes '\tes \!* ; eval "`\tes env`"';
alias precmd 'eval "`\tes env`"';
eval "`\tes env`";
"#;
                print!("{tcsh_plugin}");
            } else if nu {
                let nu_plugin = r#"# Tessera branch prompt integration for Nushell (nu)
export-env {
    $env.TES_SESSION_ID = ("term-" + ($nu.pid | into string))
    $env.TES_SHELL = "nu"
    let info = (^tes env --json | complete)
    if $info.exit_code == 0 {
        let data = ($info.stdout | from json)
        $env.TES_BRANCH = $data.branch
        $env.TES_PORT = ($data.port | into string)
        $env.TES_REPO_ROOT = $data.repo_root
        $env.TES_WORKSPACE = ($data.workspace | default $data.repo_root)
        $env.TES_PROMPT = ("(tes:" + $data.branch + ") ")
    }
}

export def --env --wrapped tes [...rest: string] {
    $env.TES_SESSION_ID = ("term-" + ($nu.pid | into string))
    ^tes ...$rest
    let info = (^tes env --json | complete)
    if $info.exit_code == 0 {
        let data = ($info.stdout | from json)
        $env.TES_BRANCH = $data.branch
        $env.TES_PORT = ($data.port | into string)
        $env.TES_REPO_ROOT = $data.repo_root
        $env.TES_WORKSPACE = ($data.workspace | default $data.repo_root)
        $env.TES_PROMPT = ("(tes:" + $data.branch + ") ")
    }
}
"#;
                print!("{nu_plugin}");
            } else if elvish {
                let elvish_plugin = r#"# Tessera branch prompt integration for Elvish
set E:TES_SESSION_ID = "term-"$pid
set E:TES_SHELL = "elvish"

fn _tes_sync {
    set E:TES_SESSION_ID = "term-"$pid
    try {
        var data = (e:tes env --json | from-json)
        set E:TES_BRANCH = $data[branch]
        set E:TES_PORT = (to-string $data[port])
        set E:TES_REPO_ROOT = $data[repo_root]
        set E:TES_WORKSPACE = $data[workspace]
        set E:TES_PROMPT = "(tes:"$data[branch]") "
    } catch _ {
        unset-env TES_BRANCH
        unset-env TES_PORT
        unset-env TES_WORKSPACE
        unset-env TES_REPO_ROOT
        unset-env TES_PROMPT
    }
}

fn tes {|@a|
    set E:TES_SESSION_ID = "term-"$pid
    e:tes $@a
    _tes_sync
}

_tes_sync
"#;
                print!("{elvish_plugin}");
            } else if pwsh {
                let pwsh_plugin = r#"# Tessera branch prompt integration for PowerShell (pwsh)
$env:TES_SESSION_ID = "term-$PID"

function global:_TesSyncEnv {
    $env:TES_SESSION_ID = "term-$PID"
    $tesCmd = Get-Command tes -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($tesCmd) {
        $info = (& $tesCmd.Source env --json 2>$null | ConvertFrom-Json -ErrorAction SilentlyContinue)
        if ($info -and $info.branch) {
            $env:TES_BRANCH = $info.branch
            $env:TES_PORT = "$($info.port)"
            $env:TES_REPO_ROOT = $info.repo_root
            if ($info.workspace) {
                $env:TES_WORKSPACE = $info.workspace
            } else {
                Remove-Item Env:\TES_WORKSPACE -ErrorAction SilentlyContinue
            }
            return
        }
    }
    Remove-Item Env:\TES_BRANCH -ErrorAction SilentlyContinue
    Remove-Item Env:\TES_PORT -ErrorAction SilentlyContinue
    Remove-Item Env:\TES_WORKSPACE -ErrorAction SilentlyContinue
    Remove-Item Env:\TES_REPO_ROOT -ErrorAction SilentlyContinue
}

function global:tes {
    $env:TES_SESSION_ID = "term-$PID"
    $tesCmd = Get-Command tes -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($tesCmd) {
        & $tesCmd.Source @args
    }
    _TesSyncEnv
}

Register-ArgumentCompleter -Native -CommandName tes -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $tokens = $commandAst.CommandElements | ForEach-Object { $_.Extent.Text }
    $sub = if ($tokens.Count -ge 2) { $tokens[1] } else { "" }
    $tesCmd = Get-Command tes -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($tesCmd) {
        & $tesCmd.Source completions --current "$wordToComplete" --prev "$sub" --words ($tokens -join " ") 2>$null | ForEach-Object {
            [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
        }
    }
}

if (-not (Test-Path Function:\_TesOrigPrompt)) {
    Copy-Item Function:\prompt Function:\_TesOrigPrompt
    function global:prompt {
        _TesSyncEnv
        if ($env:TES_BRANCH) {
            Write-Host "(tes:$($env:TES_BRANCH)) " -NoNewline -ForegroundColor Cyan
        }
        _TesOrigPrompt
    }
}
_TesSyncEnv
"#;
                print!("{pwsh_plugin}");
            } else if let Ok(coord) = Coordinator::open(&cwd) {
                let session_id = detect_session_id(agent.as_deref());
                let cfg = coord.oplog().load_config()?;
                let default_branch = coord
                    .git()
                    .current_branch_name()
                    .unwrap_or_else(|_| cfg.trunk_branch.clone());
                let current = coord.oplog().load_current_reconciled()?;
                let ensured = if current.view.agents.contains_key(&session_id) {
                    None
                } else {
                    coord.ensure_session(&session_id, None).ok()
                };
                let (branch_name, slot) = match ensured
                    .as_ref()
                    .or_else(|| current.view.agents.get(&session_id))
                {
                    Some(a) => (
                        a.target_branch
                            .clone()
                            .unwrap_or_else(|| default_branch.clone()),
                        a.slot,
                    ),
                    None => (default_branch.clone(), current.view.next_agent_slot.max(1)),
                };
                let stride = if cfg.port_stride == 0 {
                    100
                } else {
                    cfg.port_stride
                };
                let port = cfg.base_port.saturating_add(slot.saturating_mul(stride));
                let port_str = port.to_string();
                let port_isolation = cfg.is_port_isolation_enabled();
                let shim_bin_dir = coord.sandboxes_root().join("bin");

                if port_isolation {
                    workspace::ensure_polyglot_port_shims(&shim_bin_dir);
                }

                let shim_bin_str = shim_bin_dir.display().to_string();
                let port_vars = workspace::polyglot_port_env_vars(port);
                let repo_root_str = coord.git().repo_root().display().to_string();

                let ws_path_opt = if branch_name == default_branch {
                    Some(repo_root_str.clone())
                } else {
                    let sb_dir = coord.branch_sandbox_dir(&branch_name);
                    let stamp_file = PathBuf::from(format!("{}.tree", sb_dir.display()));

                    let expected_cid = coord
                        .git()
                        .resolve_ref(&format!("refs/heads/{branch_name}"))
                        .ok()
                        .flatten()
                        .or_else(|| current.view.branches.get(&branch_name).copied());

                    let stamp_fresh = sb_dir.is_dir()
                        && expected_cid.is_some_and(|cid| {
                            std::fs::read_to_string(&stamp_file)
                                .ok()
                                .is_some_and(|s| s.trim().starts_with(cid.as_str()))
                        });

                    let has_active_branch_edits = current.view.agents.values().any(|a| {
                        a.target_branch.as_deref().unwrap_or(&default_branch) == branch_name
                            && (!a.own_blobs.is_empty() || !a.claimed_symbols.is_empty())
                    });

                    if stamp_fresh && !has_active_branch_edits {
                        Some(sb_dir.display().to_string())
                    } else {
                        coord
                            .prepare_branch_sandbox(&session_id, Some(&branch_name))
                            .ok()
                            .map(|(_, dir, _)| dir.display().to_string())
                            .or_else(|| sb_dir.is_dir().then(|| sb_dir.display().to_string()))
                    }
                };

                let is_fish = std::env::var("FISH_VERSION").is_ok()
                    || std::env::var("TES_SHELL").is_ok_and(|s| s == "fish");
                let is_tcsh = std::env::var("TES_SHELL").is_ok_and(|s| s == "tcsh" || s == "csh");
                let is_elvish = std::env::var("TES_SHELL").is_ok_and(|s| s == "elvish");

                if cli.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "session_id": session_id,
                            "branch": branch_name,
                            "slot": slot,
                            "port": port,
                            "port_isolation": port_isolation,
                            "workspace": ws_path_opt,
                            "repo_root": repo_root_str,
                            "shim_bin": port_isolation.then_some(shim_bin_str),
                        }))?
                    );
                } else if is_fish {
                    println!("set -gx TES_BRANCH {};", fish_single_quote(&branch_name));
                    println!("set -gx TES_PORT {};", fish_single_quote(&port_str));

                    if port_isolation {
                        println!("set -gx TES_PORT_ISOLATION '1';");

                        for (k, v) in &port_vars {
                            println!("set -gx {k} {};", fish_single_quote(v));
                        }

                        println!(
                            "set -gx TES_SHIM_BIN {};",
                            fish_single_quote(&shim_bin_str)
                        );
                        println!(
                            "if not contains -- {} $PATH; set -gx PATH {} $PATH; end;",
                            fish_single_quote(&shim_bin_str),
                            fish_single_quote(&shim_bin_str)
                        );
                    } else {
                        let non_tes_port_keys: Vec<&str> = workspace::POLYGLOT_PORT_ENV_KEYS
                            .iter()
                            .copied()
                            .filter(|k| *k != "TES_PORT")
                            .collect();
                        println!(
                            "set -e TES_PORT_ISOLATION TES_SHIM_BIN {};",
                            non_tes_port_keys.join(" ")
                        );
                    }

                    println!(
                        "set -gx TES_REPO_ROOT {};",
                        fish_single_quote(&repo_root_str)
                    );

                    if let Some(ws) = ws_path_opt {
                        println!("set -gx TES_WORKSPACE {};", fish_single_quote(&ws));
                    } else {
                        println!("set -e TES_WORKSPACE;");
                    }
                } else if is_tcsh {
                    let prompt_str = format!("(tes:{branch_name}) ");
                    println!("setenv TES_BRANCH {};", tcsh_quote(&branch_name));
                    println!("setenv TES_PORT {};", tcsh_quote(&port_str));

                    if port_isolation {
                        println!("setenv TES_PORT_ISOLATION '1';");

                        for (k, v) in &port_vars {
                            println!("setenv {k} {};", tcsh_quote(v));
                        }

                        println!("setenv TES_SHIM_BIN {};", tcsh_quote(&shim_bin_str));
                    } else {
                        let mut unset_cmds =
                            String::from("unsetenv TES_PORT_ISOLATION; unsetenv TES_SHIM_BIN;");

                        for k in workspace::POLYGLOT_PORT_ENV_KEYS {
                            if *k != "TES_PORT" {
                                let _ = write!(unset_cmds, " unsetenv {k};");
                            }
                        }

                        println!("{unset_cmds}");
                    }

                    println!("setenv TES_REPO_ROOT {};", tcsh_quote(&repo_root_str));
                    println!("setenv TES_PROMPT {};", tcsh_quote(&prompt_str));

                    if let Some(ws) = ws_path_opt {
                        println!("setenv TES_WORKSPACE {};", tcsh_quote(&ws));
                    } else {
                        println!("unsetenv TES_WORKSPACE;");
                    }
                } else if is_elvish {
                    let prompt_str = format!("(tes:{branch_name}) ");
                    println!("set E:TES_BRANCH = {}", elvish_single_quote(&branch_name));
                    println!("set E:TES_PORT = {}", elvish_single_quote(&port_str));

                    if port_isolation {
                        println!("set E:TES_PORT_ISOLATION = '1'");

                        for (k, v) in &port_vars {
                            println!("set E:{k} = {}", elvish_single_quote(v));
                        }

                        println!(
                            "set E:TES_SHIM_BIN = {}",
                            elvish_single_quote(&shim_bin_str)
                        );
                    } else {
                        let mut unset_cmds =
                            String::from("unset-env TES_PORT_ISOLATION; unset-env TES_SHIM_BIN");

                        for k in workspace::POLYGLOT_PORT_ENV_KEYS {
                            if *k != "TES_PORT" {
                                let _ = write!(unset_cmds, "; unset-env {k}");
                            }
                        }

                        println!("{unset_cmds}");
                    }

                    println!(
                        "set E:TES_REPO_ROOT = {}",
                        elvish_single_quote(&repo_root_str)
                    );
                    println!("set E:TES_PROMPT = {}", elvish_single_quote(&prompt_str));

                    if let Some(ws) = ws_path_opt {
                        println!("set E:TES_WORKSPACE = {}", elvish_single_quote(&ws));
                    } else {
                        println!("unset-env TES_WORKSPACE");
                    }
                } else {
                    println!("export TES_BRANCH={}", posix_single_quote(&branch_name));
                    println!("export TES_PORT={}", posix_single_quote(&port_str));

                    if port_isolation {
                        println!("export TES_PORT_ISOLATION='1'");

                        for (k, v) in &port_vars {
                            println!("export {k}={}", posix_single_quote(v));
                        }

                        println!(
                            "export TES_SHIM_BIN={}",
                            posix_single_quote(&shim_bin_str)
                        );
                        println!(
                            r#"case ":${{PATH:-}}:" in *:"$TES_SHIM_BIN":*) ;; *) export PATH="$TES_SHIM_BIN${{PATH:+:$PATH}}" ;; esac"#
                        );
                    } else {
                        let non_tes_port_keys: Vec<&str> = workspace::POLYGLOT_PORT_ENV_KEYS
                            .iter()
                            .copied()
                            .filter(|k| *k != "TES_PORT")
                            .collect();
                        println!(
                            "unset TES_PORT_ISOLATION TES_SHIM_BIN {}",
                            non_tes_port_keys.join(" ")
                        );
                    }

                    println!(
                        "export TES_REPO_ROOT={}",
                        posix_single_quote(&repo_root_str)
                    );

                    if let Some(ws) = ws_path_opt {
                        println!("export TES_WORKSPACE={}", posix_single_quote(&ws));
                    } else {
                        println!("unset TES_WORKSPACE");
                    }

                    println!(
                        r#"if [ -n "${{ZSH_VERSION:-}}" ]; then
  eval '
  _tes_preexec() {{
    if [[ "$1" == tes\ * || "$1" == "tes" || "$1" == git\ * || "$1" == "git" || "$1" == cd\ * || "$1" == "cd" ]]; then return; fi
    local phys_pwd="${{PWD:A}}"
    if [[ -n "$TES_WORKSPACE" && -d "$TES_WORKSPACE" && -n "$TES_REPO_ROOT" && "$TES_WORKSPACE" != "$TES_REPO_ROOT" && -d "$TES_REPO_ROOT/.git/tessera" && "$phys_pwd" == "$TES_REPO_ROOT"* ]]; then
      _TES_PREV_PWD="$PWD"
      local rel_sub="${{phys_pwd#$TES_REPO_ROOT}}"
      if [[ -d "${{TES_WORKSPACE}}${{rel_sub}}" ]]; then builtin cd -q "${{TES_WORKSPACE}}${{rel_sub}}"; else builtin cd -q "$TES_WORKSPACE"; fi
    fi
  }}
  _tes_precmd() {{
    export TES_SESSION_ID="term-$$"
    if [[ -n "$_TES_PREV_PWD" ]]; then
      if [[ -d "$_TES_PREV_PWD" ]]; then builtin cd -q "$_TES_PREV_PWD"; fi
      unset _TES_PREV_PWD
    elif [[ -n "$TES_WORKSPACE" && "$PWD" == "$TES_WORKSPACE"* && -f "$PWD/.tessera/repo_root" && ! -L "$PWD/.tessera/repo_root" ]]; then
      local root_dir="$(<"$PWD/.tessera/repo_root")"
      if [[ -n "$root_dir" && -d "$root_dir/.git/tessera" ]]; then builtin cd -q "$root_dir"; fi
    fi
    local env_out
    if command -v tes >/dev/null 2>&1 && env_out="$(command tes env 2>/dev/null)" && [[ -n "$env_out" ]]; then
      eval "$env_out"
    else
      unset TES_BRANCH TES_PORT PORT TES_PORT_ISOLATION TES_SHIM_BIN TES_WORKSPACE TES_REPO_ROOT
    fi
  }}
  if (( ! ${{preexec_functions[(I)_tes_preexec]}} )); then
    preexec_functions=(_tes_preexec "${{preexec_functions[@]}}")
  fi
  '
fi"#
                    );
                }
            } else if !cli.json {
                let is_fish = std::env::var("FISH_VERSION").is_ok()
                    || std::env::var("TES_SHELL").is_ok_and(|s| s == "fish");
                let is_tcsh = std::env::var("TES_SHELL").is_ok_and(|s| s == "tcsh" || s == "csh");
                let is_elvish = std::env::var("TES_SHELL").is_ok_and(|s| s == "elvish");
                let port_keys = workspace::POLYGLOT_PORT_ENV_KEYS.join(" ");

                if is_fish {
                    println!(
                        "set -e TES_BRANCH TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_SHIM_BIN _TES_PREV_PWD {port_keys};"
                    );
                } else if is_tcsh {
                    let mut unset_cmds = String::from(
                        "unsetenv TES_BRANCH; unsetenv TES_WORKSPACE; unsetenv TES_REPO_ROOT; unsetenv TES_PORT_ISOLATION; unsetenv TES_SHIM_BIN; unsetenv TES_PROMPT; unsetenv _TES_PREV_PWD;",
                    );

                    for k in workspace::POLYGLOT_PORT_ENV_KEYS {
                        let _ = write!(unset_cmds, " unsetenv {k};");
                    }

                    println!("{unset_cmds}");
                } else if is_elvish {
                    let mut unset_cmds = String::from(
                        "unset-env TES_BRANCH; unset-env TES_WORKSPACE; unset-env TES_REPO_ROOT; unset-env TES_PORT_ISOLATION; unset-env TES_SHIM_BIN; unset-env TES_PROMPT",
                    );

                    for k in workspace::POLYGLOT_PORT_ENV_KEYS {
                        let _ = write!(unset_cmds, "; unset-env {k}");
                    }

                    println!("{unset_cmds}");
                } else {
                    println!(
                        "unset TES_BRANCH TES_WORKSPACE TES_REPO_ROOT TES_PORT_ISOLATION TES_SHIM_BIN _TES_PREV_PWD {port_keys}"
                    );
                }
            }
        }
        Commands::Config { sub, verify_cmd } => {
            let coord = Coordinator::open(&cwd)?;
            let mut cfg = coord.oplog().load_config()?;

            if let Some(vcmd) = verify_cmd {
                cfg.verify_command = if vcmd.trim().is_empty() {
                    None
                } else {
                    Some(vcmd)
                };
                coord.oplog().save_config(&cfg)?;
            }

            match sub.unwrap_or(ConfigCommands::Show) {
                ConfigCommands::Show => {}
                ConfigCommands::Trust => {
                    let digest = coord.oplog().trust_repo_config()?;
                    cfg = coord.oplog().load_config()?;

                    if !cli.json {
                        println!(
                            "{} Trusted repository tessera.toml (blake3: {})",
                            c_green("✓"),
                            c_yellow(&digest[..12.min(digest.len())])
                        );
                    }
                }
                ConfigCommands::Untrust => {
                    coord.oplog().untrust_repo_config()?;
                    cfg = coord.oplog().load_config()?;

                    if !cli.json {
                        println!(
                            "{} Revoked trust for repository tessera.toml execution settings",
                            c_green("✓")
                        );
                    }
                }
                ConfigCommands::AddCacheDir { dirs } => {
                    for d in dirs {
                        let clean = d.trim().trim_matches('/').to_string();

                        if !clean.is_empty() && !cfg.cow_cache_dirs.contains(&clean) {
                            cfg.cow_cache_dirs.push(clean);
                        }
                    }

                    coord.oplog().save_config(&cfg)?;
                }
                ConfigCommands::AddBinDir { dirs } => {
                    for d in dirs {
                        let clean = d.trim().trim_matches('/').to_string();

                        if !clean.is_empty() && !cfg.extra_bin_dirs.contains(&clean) {
                            cfg.extra_bin_dirs.push(clean);
                        }
                    }

                    coord.oplog().save_config(&cfg)?;
                }
                ConfigCommands::AddPackageGlob { globs } => {
                    for g in globs {
                        let clean = g.trim().to_string();

                        if !clean.is_empty() && !cfg.package_globs.contains(&clean) {
                            cfg.package_globs.push(clean);
                        }
                    }

                    coord.oplog().save_config(&cfg)?;
                }
                ConfigCommands::SetEnv { key, value } => {
                    if workspace::is_forbidden_custom_env_key(&key) {
                        anyhow::bail!(
                            "environment variable '{}' is restricted and cannot be set via custom_env",
                            key.trim()
                        );
                    }
                    cfg.custom_env.insert(key.trim().to_string(), value);
                    coord.oplog().save_config(&cfg)?;
                }
                ConfigCommands::Set { key, value } => {
                    match key.trim() {
                        "trunk_branch" => cfg.trunk_branch = value,
                        "base_port" => cfg.base_port = value.parse()?,
                        "port_stride" => cfg.port_stride = value.parse()?,
                        "port_isolation" | "isolate_ports" => {
                            cfg.port_isolation = match value.trim().to_ascii_lowercase().as_str() {
                                "true" | "1" | "yes" | "on" => true,
                                "false" | "0" | "no" | "off" => false,
                                other => anyhow::bail!(
                                    "invalid value '{other}' for port_isolation (expected true or false)"
                                ),
                            };
                        }
                        "verify_command" => {
                            cfg.verify_command = if value.trim().is_empty() {
                                None
                            } else {
                                Some(value)
                            };
                        }
                        "watch_interval_ms" => cfg.watch_interval_ms = value.parse()?,
                        "watch_timeout_secs" => cfg.watch_timeout_secs = value.parse()?,
                        "co_author" => {
                            let v = value.trim();

                            match v.to_ascii_lowercase().as_str() {
                                "true" | "1" | "yes" | "on" => cfg.co_author = true,
                                "false" | "0" | "no" | "off" | "" => cfg.co_author = false,
                                _ => {
                                    cfg.co_author = true;
                                    cfg.co_author_trailer = v.to_string();
                                }
                            }
                        }
                        "co_author_trailer" => {
                            cfg.co_author_trailer = value.trim().to_string();
                        }
                        "auto_isolate_dirty_claims" => {
                            cfg.auto_isolate_dirty_claims = match value.trim().to_ascii_lowercase().as_str() {
                                "true" | "1" | "yes" | "on" => true,
                                "false" | "0" | "no" | "off" => false,
                                other => anyhow::bail!(
                                    "invalid value '{other}' for auto_isolate_dirty_claims (expected true or false)"
                                ),
                            };
                        }
                        "pager" => {
                            let trimmed = value.trim();
                            cfg.pager = if trimmed.is_empty() {
                                None
                            } else {
                                Some(trimmed.to_string())
                            };
                        }
                        other => anyhow::bail!("unknown config key '{other}'"),
                    }

                    coord.oplog().save_config(&cfg)?;
                }
            }

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&cfg)?);
            } else {
                println!(
                    "trunk_branch = {:?}\nbase_port = {}\nport_stride = {}\nport_isolation = {}\nwatch_interval_ms = {}\nwatch_timeout_secs = {}\nco_author = {}\nco_author_trailer = {:?}\npager = {:?}\nverify_command = {:?}\ncow_cache_dirs = {:?}\nextra_bin_dirs = {:?}\npackage_globs = {:?}\ncustom_env = {:?}",
                    cfg.trunk_branch,
                    cfg.base_port,
                    cfg.port_stride,
                    cfg.port_isolation,
                    cfg.watch_interval_ms,
                    cfg.watch_timeout_secs,
                    cfg.co_author,
                    cfg.co_author_trailer,
                    cfg.pager,
                    cfg.verify_command,
                    cfg.cow_cache_dirs,
                    cfg.extra_bin_dirs,
                    cfg.package_globs,
                    cfg.custom_env
                );
            }
        }
        Commands::Mcp => {
            mcp::run_mcp_stdio_server(&cwd)?;
        }
        Commands::Pull {
            remote,
            branch,
            agent,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            let report = coord.pull_branch(&session_id, &remote, branch.as_deref())?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{} Pulled '{}/{}' ({} -> {}, {} session(s) rebased, fast-forward: {})",
                    c_green("✓"),
                    c_cyan(&report.remote),
                    c_green(&report.branch),
                    c_yellow(report.old_commit.short()),
                    c_yellow(report.new_commit.short()),
                    c_cyan(report.rebased_sessions),
                    c_cyan(report.fast_forward)
                );
            }
        }
        Commands::Stash {
            sub,
            message,
            branch,
            agent,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            let action = sub.unwrap_or(StashCommands::Push {
                message: message.clone(),
                branch: branch.clone(),
                paths: Vec::new(),
            });

            match action {
                StashCommands::Push {
                    message: sub_msg,
                    branch: sub_br,
                    paths,
                } => {
                    let mut eff_msg = sub_msg.or(message);
                    let mut eff_paths = paths;
                    if eff_msg.is_none()
                        && eff_paths.len() == 1
                        && !coord.git().repo_root().join(&eff_paths[0]).exists()
                    {
                        eff_msg = eff_paths.pop();
                    }
                    let eff_br = sub_br.or(branch);
                    let entry = coord.stash_push_with_paths(
                        &session_id,
                        eff_msg.as_deref(),
                        eff_br.as_deref(),
                        &eff_paths,
                    )?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&entry)?);
                    } else {
                        println!(
                            "{} Saved working state on '{}' ({} file(s)): {}",
                            c_green("✓"),
                            c_green(&entry.branch),
                            c_cyan(entry.own_blobs.len()),
                            c_bold(&entry.message)
                        );
                    }
                }
                StashCommands::Pop { index } => {
                    let idx = parse_stash_index(index.as_deref())?;
                    let report = coord.stash_pop(&session_id, idx, false)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        println!(
                            "{} Popped stash@{{{}}} onto '{}' ({} file(s))",
                            c_green("✓"),
                            idx,
                            c_green(&report.applied_branch),
                            c_cyan(report.restored_paths.len())
                        );
                        if report.has_conflicts {
                            println!(
                                "  {}",
                                c_red("Conflicts recorded during 3-way AST application (stash entry preserved).")
                            );
                        }
                    }
                }
                StashCommands::Apply { index } => {
                    let idx = parse_stash_index(index.as_deref())?;
                    let report = coord.stash_pop(&session_id, idx, true)?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        println!(
                            "{} Applied stash@{{{}}} onto '{}' ({} file(s))",
                            c_green("✓"),
                            idx,
                            c_green(&report.applied_branch),
                            c_cyan(report.restored_paths.len())
                        );
                        if report.has_conflicts {
                            println!(
                                "  {}",
                                c_red("Conflicts recorded during 3-way AST application.")
                            );
                        }
                    }
                }
                StashCommands::List => {
                    let stashes = coord.stash_list()?;

                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&stashes)?);
                    } else if stashes.is_empty() {
                        println!("{}", c_dim("No stashed entries."));
                    } else {
                        let mut out = String::new();
                        let repo_root = coord.git().repo_root();

                        for (idx, s) in stashes.iter().enumerate() {
                            let _ = writeln!(
                                out,
                                "{}: on {} ({}, {} file(s)): {}",
                                c_yellow(format!("stash@{{{idx}}}")),
                                c_green(&s.branch),
                                format_session_link(&s.session_id, repo_root, c_cyan(&s.session_id)),
                                s.own_blobs.len(),
                                s.message
                            );
                        }

                        emit_with_pager(
                            &out,
                            true,
                            cli.no_pager,
                            cli.paginate,
                            cli.json,
                            Some(&coord),
                        );
                    }
                }
                StashCommands::Drop { index } => {
                    let idx = parse_stash_index(index.as_deref())?;
                    let dropped = coord.stash_drop(&session_id, Some(idx))?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({ "dropped": dropped }))?
                        );
                    } else {
                        println!(
                            "{} Dropped stash@{{{}}} ({} entry removed)",
                            c_green("✓"),
                            idx,
                            dropped
                        );
                    }
                }
                StashCommands::Clear => {
                    let dropped = coord.stash_drop(&session_id, None)?;
                    if cli.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({ "dropped": dropped }))?
                        );
                    } else {
                        println!(
                            "{} Cleared {} stash entry/entries.",
                            c_green("✓"),
                            c_cyan(dropped)
                        );
                    }
                }
            }
        }
        Commands::Log {
            targets,
            branch_flag,
            limit,
            oneline,
            patch,
            stat,
            name_only,
            name_status,
            graph,
            all,
            author,
            grep,
            pickaxe,
            since,
            until,
            reverse,
            agent,
            paths,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let mut rev_arg: Option<String> = branch_flag;
            let mut path_args: Vec<String> = Vec::new();

            for t in targets {
                if rev_arg.is_none()
                    && (t.contains("..") || coord.git().resolve_revision(&t).is_ok())
                {
                    rev_arg = Some(t);
                } else {
                    path_args.push(t);
                }
            }

            path_args.extend(paths);

            let eff_branch = rev_arg.or_else(|| {
                if all {
                    return None;
                }
                let sid = detect_session_id(agent.as_deref());
                coord
                    .oplog()
                    .load_current_reconciled()
                    .ok()
                    .and_then(|op| op.view.agents.get(&sid).and_then(|a| a.target_branch.clone()))
            });

            let use_git_log = patch
                || stat
                || name_only
                || name_status
                || graph
                || all
                || author.is_some()
                || grep.is_some()
                || pickaxe.is_some()
                || since.is_some()
                || until.is_some()
                || reverse
                || !path_args.is_empty()
                || eff_branch.as_deref().is_some_and(|r| r.contains(".."));

            if !use_git_log {
                let entries = coord.commit_log(eff_branch.as_deref(), limit)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&entries)?);
                } else {
                    let mut out = String::new();
                    for entry in &entries {
                        let refs_badge = if entry.refs.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", c_green(entry.refs.join(", ")))
                        };

                        if oneline {
                            let _ = writeln!(
                                out,
                                "{}{} {}",
                                c_yellow(&entry.short_sha),
                                refs_badge,
                                entry.subject
                            );
                        } else {
                            let _ = writeln!(
                                out,
                                "{} {}{}",
                                c_yellow("commit"),
                                c_yellow(&entry.sha),
                                refs_badge
                            );
                            let _ =
                                writeln!(out, "{} {}", c_bold("Author:"), c_cyan(&entry.author));
                            let _ = writeln!(out, "    {}\n", entry.subject);
                        }
                    }
                    emit_with_pager(
                        &out,
                        true,
                        cli.no_pager,
                        cli.paginate,
                        cli.json,
                        Some(&coord),
                    );
                }
            } else if cli.json {
                let mut cmd = std::process::Command::new("git");
                cmd.current_dir(coord.git().repo_root())
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .arg("--git-dir")
                    .arg(coord.git().git_dir())
                    .arg("log")
                    .arg("--topo-order")
                    .arg(format!("-n{}", limit.max(1)))
                    .arg("--format=%H\t%h\t%P\t%an\t%ct\t%D\t%s");

                if all {
                    cmd.arg("--all");
                }

                if let Some(a) = &author {
                    cmd.arg(format!("--author={a}"));
                }

                if let Some(g) = &grep {
                    cmd.arg(format!("--grep={g}"));
                }

                if let Some(s) = &pickaxe {
                    cmd.arg(format!("-S{s}"));
                }

                if let Some(s) = &since {
                    cmd.arg(format!("--since={s}"));
                }

                if let Some(u) = &until {
                    cmd.arg(format!("--until={u}"));
                }

                if reverse {
                    cmd.arg("--reverse");
                }

                cmd.arg("--end-of-options");

                if let Some(b) = eff_branch.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    cmd.arg(b);
                } else if !all {
                    cmd.arg("HEAD");
                }

                if !path_args.is_empty() {
                    cmd.arg("--").args(&path_args);
                }

                let out = cmd.output()?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    anyhow::bail!("git log failed: {stderr}");
                }

                let mut entries = Vec::new();
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let parts: Vec<&str> = line.splitn(7, '\t').collect();
                    if parts.len() == 7 {
                        let sha = parts[0].trim().to_string();
                        let short_sha = parts[1].trim().to_string();
                        let parents: Vec<String> = parts[2]
                            .split_whitespace()
                            .filter(|s| !s.is_empty())
                            .map(ToString::to_string)
                            .collect();
                        let entry_author = parts[3].trim().to_string();
                        let timestamp = parts[4].trim().parse::<i64>().unwrap_or(0);
                        let refs: Vec<String> = parts[5]
                            .split(',')
                            .map(str::trim)
                            .filter(|r| !r.is_empty() && !r.starts_with("tessera/"))
                            .map(ToString::to_string)
                            .collect();
                        let subject = parts[6].trim().to_string();

                        entries.push(json!({
                            "sha": sha,
                            "short_sha": short_sha,
                            "parents": parents,
                            "author": entry_author,
                            "timestamp": timestamp,
                            "subject": subject,
                            "refs": refs,
                        }));
                    }
                }
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                let mut cmd = std::process::Command::new("git");
                cmd.current_dir(coord.git().repo_root())
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .arg("--git-dir")
                    .arg(coord.git().git_dir())
                    .arg("log")
                    .arg(if use_color() {
                        "--color=always"
                    } else {
                        "--color=never"
                    })
                    .arg("--decorate=short")
                    .arg(format!("-n{}", limit.max(1)));

                if oneline {
                    cmd.arg("--oneline");
                }

                if patch {
                    cmd.arg("-p");
                }

                if stat {
                    cmd.arg("--stat");
                }

                if name_only {
                    cmd.arg("--name-only");
                }

                if name_status {
                    cmd.arg("--name-status");
                }

                if graph {
                    cmd.arg("--graph");
                }

                if all {
                    cmd.arg("--all");
                }

                if let Some(a) = &author {
                    cmd.arg(format!("--author={a}"));
                }

                if let Some(g) = &grep {
                    cmd.arg(format!("--grep={g}"));
                }

                if let Some(s) = &pickaxe {
                    cmd.arg(format!("-S{s}"));
                }

                if let Some(s) = &since {
                    cmd.arg(format!("--since={s}"));
                }

                if let Some(u) = &until {
                    cmd.arg(format!("--until={u}"));
                }

                if reverse {
                    cmd.arg("--reverse");
                }

                cmd.arg("--end-of-options");

                if let Some(b) = eff_branch.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    cmd.arg(b);
                } else if !all {
                    cmd.arg("HEAD");
                }

                if !path_args.is_empty() {
                    cmd.arg("--").args(&path_args);
                }

                let out = cmd.output()?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    anyhow::bail!("git log failed: {stderr}");
                }

                let log_text = String::from_utf8_lossy(&out.stdout);
                emit_with_pager(
                    &log_text,
                    true,
                    cli.no_pager,
                    cli.paginate,
                    cli.json,
                    Some(&coord),
                );
            }
        }
        Commands::CherryPick {
            commit,
            branch,
            agent,
        } => {
            let coord = Coordinator::open(&cwd)?;
            let session_id = detect_session_id(agent.as_deref());
            let report = coord.cherry_pick(&session_id, &commit, branch.as_deref())?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "{} Cherry-picked {} onto '{}' -> commit {} ({} file(s) touched)",
                    c_green("✓"),
                    c_yellow(report.source_commit.short()),
                    c_green(&report.target_branch),
                    c_yellow(report.new_commit_id.short()),
                    c_cyan(report.touched_paths.len())
                );
                if !report.conflicts.is_empty() {
                    println!(
                        "  {} {} conflict(s) recorded.",
                        c_red("!"),
                        report.conflicts.len()
                    );
                }
            }
        }
        Commands::Complete {
            shell,
            cword,
            words,
        } => {
            handle_dynamic_completions(&cwd, &shell, cword, &words);
        }
        Commands::External(args) => {
            let Some(subcmd) = args.first() else {
                anyhow::bail!("missing subcommand");
            };
            let coord_opt = Coordinator::open(&cwd).ok();
            let repo_dir = coord_opt
                .as_ref()
                .map(|c| c.git().repo_root().to_path_buf())
                .unwrap_or_else(|| cwd.clone());

            let status = std::process::Command::new("git")
                .current_dir(&repo_dir)
                .args(&args)
                .status()
                .map_err(|e| anyhow::anyhow!("failed to execute 'git {subcmd}': {e}"))?;

            if let Some(coord) = &coord_opt {
                let _ = coord.sync_all_agents();
            }

            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }

    Ok(())
}

/// Generates shell-specific tab-completion candidates for `tes` subcommands, flags, branches, revisions, and active sessions.
fn handle_dynamic_completions(
    cwd: &Path,
    shell: &str,
    cword_opt: Option<usize>,
    raw_words: &[String],
) {
    let stripped: Vec<String> = if raw_words.first().map(String::as_str) == Some("--") {
        raw_words[1..].to_vec()
    } else {
        raw_words.to_vec()
    };

    let words: Vec<String> = if stripped
        .first()
        .is_none_or(|w| w != "tes" && !w.ends_with("/tes"))
    {
        let mut v = vec!["tes".to_string()];
        v.extend(stripped);
        v
    } else {
        stripped
    };
    let cword = cword_opt.unwrap_or_else(|| words.len().saturating_sub(1));

    let cur = words.get(cword).map(String::as_str).unwrap_or("");
    let prev = if cword > 0 {
        words.get(cword - 1).map(String::as_str).unwrap_or("")
    } else {
        ""
    };

    let subcmd = words
        .iter()
        .enumerate()
        .skip(1)
        .take_while(|(idx, _)| *idx < cword)
        .find_map(|(_, w)| {
            if !w.starts_with('-') {
                Some(w.as_str())
            } else {
                None
            }
        });

    let coord_opt = Coordinator::open(cwd).ok();
    let collect_branches = || -> Vec<(String, String)> {
        let mut set = std::collections::BTreeSet::new();
        if let Some(coord) = &coord_opt {
            for b in coord.git().list_local_branches() {
                set.insert(b);
            }

            if let Ok(current) = coord.oplog().load_current_reconciled() {
                for b in current.view.branches.keys() {
                    set.insert(b.clone());
                }

                for a in current.view.agents.values() {
                    if let Some(tb) = &a.target_branch {
                        set.insert(tb.clone());
                    }
                }
            }
        }
        set.into_iter()
            .map(|b| (b, "branch".to_string()))
            .collect()
    };

    let collect_revisions = || -> Vec<(String, String)> {
        let mut items = collect_branches();
        items.push(("HEAD".to_string(), "current branch tip".to_string()));
        items.push(("HEAD~1".to_string(), "1 commit ago".to_string()));
        items.push(("HEAD~2".to_string(), "2 commits ago".to_string()));
        if let Some(coord) = &coord_opt {
            if let Ok(current) = coord.oplog().load_current_reconciled() {
                for a in current.view.agents.values() {
                    for (cp_name, op_id) in &a.checkpoints {
                        items.push((
                            cp_name.clone(),
                            format!("checkpoint ({})", op_id.short()),
                        ));
                    }
                }
            }

            for (sha, subj) in coord.git().list_recent_commits(25) {
                items.push((sha, format!("commit: {subj}")));
            }
        }
        items
    };

    let collect_agents = || -> Vec<(String, String)> {
        let mut items = Vec::new();
        if let Some(coord) = &coord_opt {
            if let Ok(current) = coord.oplog().load_current_reconciled() {
                for (id, a) in &current.view.agents {
                    let br = a.target_branch.as_deref().unwrap_or("main");
                    items.push((id.clone(), format!("session on {br} ({})", a.intent)));
                }
            }
        }
        items
    };

    let collect_checkpoints_and_ops = || -> Vec<(String, String)> {
        let mut items = Vec::new();
        if let Some(coord) = &coord_opt {
            if let Ok(current) = coord.oplog().load_current_reconciled() {
                for a in current.view.agents.values() {
                    for (cp_name, op_id) in &a.checkpoints {
                        items.push((
                            cp_name.clone(),
                            format!("checkpoint ({})", op_id.short()),
                        ));
                    }
                }
            }

            if let Ok(ops) = coord.oplog().list_history(15) {
                for op in ops {
                    items.push((op.id.short().to_string(), op.description));
                }
            }
        }
        items
    };

    let mut candidates: Vec<(String, String)> = Vec::new();

    match prev {
        "-b" | "--branch" | "--into" | "-d" | "--delete" | "-D" | "--force-delete" | "-m"
        | "--move" => {
            candidates = collect_branches();
        }
        "-s" | "--source" => {
            candidates = collect_revisions();
        }
        "--strategy" => {
            candidates = vec![
                (
                    "ast-union".to_string(),
                    "Combine both sides' AST declarations".to_string(),
                ),
                ("ours".to_string(), "Keep the first side".to_string()),
                ("theirs".to_string(), "Take the second side".to_string()),
                (
                    "working".to_string(),
                    "Use the current file content on disk".to_string(),
                ),
            ];
        }
        "--harness" => {
            for h in [
                "all",
                "antigravity",
                "claude",
                "cursor",
                "windsurf",
                "kiro",
                "opencode",
                "codex",
                "vscode",
            ] {
                candidates.push((h.to_string(), "AI harness integration".to_string()));
            }
        }
        "--agent" | "--session" => {
            candidates = collect_agents();
        }
        "--checkpoint" | "--op" => {
            candidates = collect_checkpoints_and_ops();
        }
        _ => match subcmd {
            None => {
                candidates = vec![
                    (
                        "init".to_string(),
                        "Initialize Tessera (.git/tessera) and AI harness hooks".to_string(),
                    ),
                    (
                        "status".to_string(),
                        "Show session status, branch, and staged/unstaged/peer changes".to_string(),
                    ),
                    (
                        "diff".to_string(),
                        "Show line-by-line unified diffs".to_string(),
                    ),
                    (
                        "switch".to_string(),
                        "Switch or bind current terminal/session to a branch".to_string(),
                    ),
                    (
                        "branch".to_string(),
                        "List, switch, delete (-d/-D), or rename (-m) branches".to_string(),
                    ),
                    (
                        "commit".to_string(),
                        "Commit current session changes onto its active branch".to_string(),
                    ),
                    (
                        "push".to_string(),
                        "Push branch to remote (auto-commits pending changes)".to_string(),
                    ),
                    (
                        "pull".to_string(),
                        "Fetch from remote and 3-way AST-merge into active branch".to_string(),
                    ),
                    (
                        "stash".to_string(),
                        "Stash and restore session-isolated uncommitted changes".to_string(),
                    ),
                    (
                        "log".to_string(),
                        "Show commit history for the current or specified branch".to_string(),
                    ),
                    (
                        "cherry-pick".to_string(),
                        "Apply a commit's delta onto the active branch via 3-way AST merge".to_string(),
                    ),
                    (
                        "restore".to_string(),
                        "Restore branch or files to a commit, ref, or checkpoint".to_string(),
                    ),
                    (
                        "claim".to_string(),
                        "Declare session intent, target branch, and claimed files".to_string(),
                    ),
                    (
                        "exec".to_string(),
                        "Execute command in isolated Copy-on-Write branch sandbox".to_string(),
                    ),
                    (
                        "merge".to_string(),
                        "Semantically AST-merge sessions or branches".to_string(),
                    ),
                    (
                        "radar".to_string(),
                        "Display live Symbol Collision Radar across active agents".to_string(),
                    ),
                    (
                        "conflicts".to_string(),
                        "List active first-class algebraic conflicts".to_string(),
                    ),
                    (
                        "resolve".to_string(),
                        "Resolve a conflict using ast-union, ours, theirs, or working".to_string(),
                    ),
                    (
                        "sync".to_string(),
                        "Auto-rebase active sessions onto latest trunk commit".to_string(),
                    ),
                    (
                        "checkpoint".to_string(),
                        "Save a named checkpoint in the Operation Log".to_string(),
                    ),
                    (
                        "undo".to_string(),
                        "Roll back repository state to previous operation or checkpoint".to_string(),
                    ),
                    (
                        "oplog".to_string(),
                        "Inspect lock-free Operation Log history".to_string(),
                    ),
                    (
                        "agent".to_string(),
                        "Manage isolated Copy-on-Write agent sandboxes".to_string(),
                    ),
                    (
                        "config".to_string(),
                        "View or modify repository configuration".to_string(),
                    ),
                    (
                        "env".to_string(),
                        "Emit or install shell prompt and tab-completion integration".to_string(),
                    ),
                    (
                        "watch".to_string(),
                        "Continuously watch and auto-reconcile working tree edits".to_string(),
                    ),
                    (
                        "gc".to_string(),
                        "Compact ephemeral snapshots in Operation Log".to_string(),
                    ),
                    (
                        "mcp".to_string(),
                        "Start Model Context Protocol JSON-RPC server".to_string(),
                    ),
                ];
            }
            Some("status") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        (
                            "-s".to_string(),
                            "Give output in short-format".to_string(),
                        ),
                        (
                            "--short".to_string(),
                            "Give output in short-format".to_string(),
                        ),
                        (
                            "--porcelain".to_string(),
                            "Give output in machine-readable short-format".to_string(),
                        ),
                        (
                            "-b".to_string(),
                            "Show branch and session header in short-format".to_string(),
                        ),
                        (
                            "--branch".to_string(),
                            "Show branch and session header in short-format".to_string(),
                        ),
                    ];
                }
            }
            Some("switch" | "checkout") => {
                candidates = collect_branches();
            }
            Some("branch" | "branches") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-l".to_string(), "List branches".to_string()),
                        ("--list".to_string(), "List branches".to_string()),
                        (
                            "-v".to_string(),
                            "Show commit SHA and subject in list mode".to_string(),
                        ),
                        (
                            "--verbose".to_string(),
                            "Show commit SHA and subject in list mode".to_string(),
                        ),
                        ("-d".to_string(), "Delete merged branch".to_string()),
                        ("-D".to_string(), "Force-delete branch".to_string()),
                        ("-m".to_string(), "Rename branch".to_string()),
                        ("-e".to_string(), "Switch editor working tree".to_string()),
                    ];
                } else {
                    candidates = collect_branches();
                }
            }
            Some("stash") => {
                if cword == 2 {
                    candidates = vec![
                        (
                            "push".to_string(),
                            "Stash current session's uncommitted changes".to_string(),
                        ),
                        (
                            "pop".to_string(),
                            "Apply stash via 3-way AST merge and remove it".to_string(),
                        ),
                        (
                            "apply".to_string(),
                            "Apply stash via 3-way AST merge and keep it".to_string(),
                        ),
                        ("list".to_string(), "List stashed entries".to_string()),
                        ("drop".to_string(), "Remove a single stash entry".to_string()),
                        ("clear".to_string(), "Remove all stash entries".to_string()),
                    ];
                }
            }
            Some("log") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-n".to_string(), "Number of commits to show".to_string()),
                        (
                            "--max-count".to_string(),
                            "Number of commits to show".to_string(),
                        ),
                        ("-b".to_string(), "Branch to inspect".to_string()),
                        ("--oneline".to_string(), "Compact one-line output".to_string()),
                        (
                            "-p".to_string(),
                            "Generate patch (unified diff) for each commit".to_string(),
                        ),
                        (
                            "--patch".to_string(),
                            "Generate patch (unified diff) for each commit".to_string(),
                        ),
                        (
                            "--stat".to_string(),
                            "Show diffstat of modified files for each commit".to_string(),
                        ),
                        (
                            "--name-only".to_string(),
                            "Show only names of changed files".to_string(),
                        ),
                        (
                            "--name-status".to_string(),
                            "Show names and status of changed files".to_string(),
                        ),
                        (
                            "--graph".to_string(),
                            "Draw text-based commit graph".to_string(),
                        ),
                        (
                            "--all".to_string(),
                            "Show commits across all branches and refs".to_string(),
                        ),
                        (
                            "--author".to_string(),
                            "Filter commits by author pattern".to_string(),
                        ),
                        (
                            "--grep".to_string(),
                            "Filter commits by message pattern".to_string(),
                        ),
                        (
                            "-S".to_string(),
                            "Filter commits introducing or removing a string".to_string(),
                        ),
                        (
                            "--since".to_string(),
                            "Show commits more recent than date".to_string(),
                        ),
                        (
                            "--until".to_string(),
                            "Show commits older than date".to_string(),
                        ),
                        (
                            "--reverse".to_string(),
                            "Output chosen commits in reverse order".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_branches();
                }
            }
            Some("cherry-pick" | "restore" | "reset" | "discard") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-b".to_string(), "Target branch name".to_string()),
                        ("--branch".to_string(), "Target branch name".to_string()),
                        (
                            "-s".to_string(),
                            "Source commit, ref, or checkpoint".to_string(),
                        ),
                        (
                            "--source".to_string(),
                            "Source commit, ref, or checkpoint".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_revisions();
                }
            }
            Some("push" | "pull") => {
                if cword == 2 && !cur.starts_with('-') {
                    candidates.push(("origin".to_string(), "default Git remote".to_string()));
                    candidates.extend(collect_branches());
                } else {
                    candidates = collect_branches();
                }
            }
            Some("commit" | "ci") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("-m".to_string(), "Commit message".to_string()),
                        ("--message".to_string(), "Commit message".to_string()),
                        ("-b".to_string(), "Target branch name".to_string()),
                        ("--branch".to_string(), "Target branch name".to_string()),
                        (
                            "--amend".to_string(),
                            "Amend the tip commit of the target branch".to_string(),
                        ),
                        (
                            "--no-edit".to_string(),
                            "Reuse previous commit message when amending".to_string(),
                        ),
                        (
                            "--co-author".to_string(),
                            "Append Co-authored-by trailer (optional Name <email>)".to_string(),
                        ),
                        (
                            "--no-co-author".to_string(),
                            "Omit Co-authored-by trailer for this commit".to_string(),
                        ),
                        (
                            "--author".to_string(),
                            "Override commit author (Name <email>)".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_branches();
                }
            }
            Some("merge") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        ("--into".to_string(), "Target branch to merge into".to_string()),
                        ("-m".to_string(), "Commit message".to_string()),
                        ("--verify".to_string(), "Verification command".to_string()),
                        (
                            "--keep-open".to_string(),
                            "Keep merged sessions open".to_string(),
                        ),
                        (
                            "--co-author".to_string(),
                            "Append Co-authored-by trailer (optional Name <email>)".to_string(),
                        ),
                        (
                            "--no-co-author".to_string(),
                            "Omit Co-authored-by trailer for this merge commit".to_string(),
                        ),
                        (
                            "--author".to_string(),
                            "Override merge commit author (Name <email>)".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_branches();
                    candidates.extend(collect_agents());
                }
            }
            Some("diff") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        (
                            "--staged".to_string(),
                            "Show only staged/session changes".to_string(),
                        ),
                        (
                            "--cached".to_string(),
                            "Show only staged/session changes".to_string(),
                        ),
                        (
                            "--all".to_string(),
                            "Include concurrent peer session changes on the branch".to_string(),
                        ),
                        (
                            "--stat".to_string(),
                            "Show diffstat summary of changed files".to_string(),
                        ),
                        (
                            "--shortstat".to_string(),
                            "Show aggregate line count summary".to_string(),
                        ),
                        (
                            "--numstat".to_string(),
                            "Show machine-friendly per-file line counts".to_string(),
                        ),
                        (
                            "--name-only".to_string(),
                            "Show only names of changed files".to_string(),
                        ),
                        (
                            "--name-status".to_string(),
                            "Show names and status of changed files".to_string(),
                        ),
                        (
                            "-U".to_string(),
                            "Number of context lines in unified diff".to_string(),
                        ),
                        (
                            "-w".to_string(),
                            "Ignore all whitespace when comparing lines".to_string(),
                        ),
                        (
                            "--ignore-space-change".to_string(),
                            "Ignore changes in amount of whitespace".to_string(),
                        ),
                        ("-b".to_string(), "Compare specific branch".to_string()),
                        ("--branch".to_string(), "Compare specific branch".to_string()),
                    ];
                } else {
                    candidates = collect_branches();
                }
            }
            Some("resolve") => {
                if cur.starts_with('-') {
                    candidates = vec![(
                        "--strategy".to_string(),
                        "Resolution strategy (ast-union, ours, theirs, working)".to_string(),
                    )];
                } else if let Some(coord) = &coord_opt {
                    if let Ok(current) = coord.oplog().load_current_reconciled() {
                        for path in current.view.conflicts.keys() {
                            candidates.push((path.clone(), "conflicted file".to_string()));
                        }
                    }
                }
            }
            Some("undo") => {
                if cur.starts_with('-') {
                    candidates = vec![
                        (
                            "--checkpoint".to_string(),
                            "Restore to named checkpoint".to_string(),
                        ),
                        (
                            "--op".to_string(),
                            "Restore to operation ID prefix".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_checkpoints_and_ops();
                }
            }
            Some("agent" | "session" | "sessions") => {
                if cword == 2 {
                    candidates = vec![
                        ("list".to_string(), "List active agent sessions".to_string()),
                        ("spawn".to_string(), "Spawn COW agent workspace".to_string()),
                        ("exec".to_string(), "Execute command in agent workspace".to_string()),
                        ("close".to_string(), "Close active agent session".to_string()),
                        (
                            "focus".to_string(),
                            "Focus terminal or IDE harness for an active session".to_string(),
                        ),
                    ];
                } else {
                    candidates = collect_agents();
                }
            }
            Some("config") => {
                if cword == 2 {
                    candidates = vec![
                        ("show".to_string(), "Display effective configuration".to_string()),
                        (
                            "add-cache-dir".to_string(),
                            "Add custom dependency/build cache directory".to_string(),
                        ),
                        (
                            "add-bin-dir".to_string(),
                            "Add relative binary directory to PATH".to_string(),
                        ),
                        (
                            "add-package-glob".to_string(),
                            "Add monorepo package glob".to_string(),
                        ),
                        (
                            "set-env".to_string(),
                            "Set custom environment variable template".to_string(),
                        ),
                        ("set".to_string(), "Set configuration property".to_string()),
                        (
                            "trust".to_string(),
                            "Trust repository tessera.toml execution settings".to_string(),
                        ),
                        (
                            "untrust".to_string(),
                            "Revoke trust for repository tessera.toml execution settings".to_string(),
                        ),
                    ];
                } else if words.get(2).map(String::as_str) == Some("set") && cword == 3 {
                    for k in [
                        "trunk_branch",
                        "base_port",
                        "port_stride",
                        "verify_command",
                        "watch_interval_ms",
                        "watch_timeout_secs",
                        "co_author",
                        "co_author_trailer",
                        "pager",
                    ] {
                        candidates.push((k.to_string(), "config key".to_string()));
                    }
                }
            }
            _ => {}
        },
    }

    for (val, desc) in candidates {
        if !cur.is_empty() && !val.starts_with(cur) {
            continue;
        }

        match shell {
            "zsh" => {
                let escaped_val = val.replace(':', "\\:");
                let clean_desc = desc.replace('\n', " ");
                println!("{escaped_val}:{clean_desc}");
            }
            "fish" => {
                let clean_desc = desc.replace(['\n', '\t'], " ");
                println!("{val}\t{clean_desc}");
            }
            _ => {
                println!("{val}");
            }
        }
    }
}

/// Dispatches AI harness lifecycle hooks (`prompt`, `pre-tool`, `post-tool`, `git-sync`) to the coordinator.
///
/// # Errors
/// Returns an error if session registration, pre/post-tool reconciliation, or JSON serialization fails.
fn handle_cli_hook(
    cwd: &Path,
    event: &str,
    session_flag: Option<String>,
    path_flag: Option<String>,
    prompt_flag: Option<String>,
    json_out: bool,
) -> anyhow::Result<()> {
    let Ok(coord) = Coordinator::open(cwd) else {
        return Ok(());
    };

    let stdin_json = read_nonblocking_stdin_json();
    let payload_session = stdin_json
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let session_id = detect_session_id(session_flag.or(payload_session).as_deref());

    match event {
        "prompt" => {
            let extracted_prompt = prompt_flag.or_else(|| {
                stdin_json
                    .as_ref()
                    .and_then(|v| v.get("prompt"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            });
            let meta = coord.ensure_session(&session_id, extracted_prompt.as_deref())?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&meta)?);
            }
        }
        "pre-tool" => {
            let paths = if let Some(p) = path_flag {
                vec![p]
            } else {
                extract_hook_file_paths(&stdin_json)
            };
            for target_path in paths {
                let outcome = handle_pre_tool_hook(&coord, &session_id, &target_path)?;
                if json_out {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else if let Some(warn) = outcome.radar_warning {
                    println!("{warn}");
                }
            }
        }
        "post-tool" => {
            let paths = if let Some(p) = path_flag {
                vec![p]
            } else {
                extract_hook_file_paths(&stdin_json)
            };
            for target_path in paths {
                let outcome = handle_post_tool_hook(&coord, &session_id, &target_path)?;
                if json_out {
                    println!("{}", serde_json::to_string_pretty(&outcome)?);
                } else if let Some(notice) = outcome.context_notice {
                    println!("{notice}");
                }
            }
        }
        "git-sync" => {
            let _ = coord.sync_all_agents();
        }
        _ => {}
    }

    Ok(())
}

/// Reads a JSON hook payload (bounded at 10 MiB) from non-interactive stdin when invoked by an AI harness hook runner.
fn read_nonblocking_stdin_json() -> Option<Value> {
    if io::stdin().is_terminal() {
        return None;
    }

    let mut buf = String::new();
    let mut limited = io::stdin().take(10 * 1024 * 1024);
    if limited.read_to_string(&mut buf).is_ok() && !buf.trim().is_empty() {
        serde_json::from_str(buf.trim()).ok()
    } else {
        None
    }
}

/// Extracts target file path(s) from a JSON hook payload across Claude Code, Codex (`apply_patch`), Cursor, and Antigravity schemas.
fn extract_hook_file_paths(stdin_json: &Option<Value>) -> Vec<String> {
    let Some(v) = stdin_json.as_ref() else {
        return Vec::new();
    };

    if let Some(tool_input) = v.get("tool_input") {
        for key in ["file_path", "path", "TargetFile", "AbsolutePath"] {
            if let Some(s) = tool_input.get(key).and_then(Value::as_str) {
                if !s.trim().is_empty() {
                    return vec![s.trim().to_string()];
                }
            }
        }

        if let Some(patch_str) = tool_input.as_str() {
            let paths = extract_apply_patch_paths(patch_str);
            if !paths.is_empty() {
                return paths;
            }
        }

        for key in ["patch", "input", "diff"] {
            if let Some(patch_str) = tool_input.get(key).and_then(Value::as_str) {
                let paths = extract_apply_patch_paths(patch_str);
                if !paths.is_empty() {
                    return paths;
                }
            }
        }
    }

    for key in ["file_path", "path"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return vec![s.trim().to_string()];
            }
        }
    }

    if let Some(patch_str) = v.get("patch").and_then(Value::as_str) {
        let paths = extract_apply_patch_paths(patch_str);
        if !paths.is_empty() {
            return paths;
        }
    }

    Vec::new()
}

/// Extracts modified/added file paths from an OpenAI Codex `apply_patch` body (`*** Update File: <path>` / `*** Add File: <path>`).
fn extract_apply_patch_paths(patch: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in patch.lines() {
        let trimmed = line.trim();
        for prefix in [
            "*** Update File:",
            "*** Add File:",
            "*** Delete File:",
            "*** Move to:",
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let path = rest.trim();
                if !path.is_empty() && !paths.iter().any(|p| p == path) {
                    paths.push(path.to_string());
                }
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_commit_and_merge_parse_co_author_and_author_flags() {
        let bare = Cli::try_parse_from(["tes", "commit", "-m", "feat: bare", "--co-author"]).unwrap();
        match bare.command {
            Commands::Commit {
                co_author,
                no_co_author,
                author,
                amend,
                no_edit,
                ..
            } => {
                assert_eq!(co_author, vec![String::new()]);
                assert!(!no_co_author);
                assert!(author.is_none());
                assert!(!amend);
                assert!(!no_edit);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let custom = Cli::try_parse_from([
            "tes",
            "commit",
            "--amend",
            "--no-edit",
            "-m",
            "feat: custom",
            "--co-author",
            "Alice <alice@example.com>",
            "--author",
            "Bob <bob@example.com>",
        ])
        .unwrap();
        match custom.command {
            Commands::Commit {
                co_author,
                no_co_author,
                author,
                amend,
                no_edit,
                ..
            } => {
                assert_eq!(co_author, vec!["Alice <alice@example.com>".to_string()]);
                assert!(!no_co_author);
                assert_eq!(author.as_deref(), Some("Bob <bob@example.com>"));
                assert!(amend);
                assert!(no_edit);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let disabled =
            Cli::try_parse_from(["tes", "commit", "-m", "feat: none", "--no-co-author"]).unwrap();
        match disabled.command {
            Commands::Commit {
                co_author,
                no_co_author,
                ..
            } => {
                assert!(co_author.is_empty());
                assert!(no_co_author);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let conflict = Cli::try_parse_from([
            "tes",
            "commit",
            "-m",
            "feat: conflict",
            "--co-author",
            "--no-co-author",
        ]);
        assert!(conflict.is_err());

        let merge_cmd = Cli::try_parse_from([
            "tes",
            "-P",
            "merge",
            "feat-a",
            "--into",
            "main",
            "-m",
            "feat: merge",
            "--co-author=Pair <pair@example.com>",
            "--author",
            "Lead <lead@example.com>",
        ])
        .unwrap();
        assert!(merge_cmd.no_pager);
        match merge_cmd.command {
            Commands::Merge {
                agents,
                co_author,
                no_co_author,
                author,
                ..
            } => {
                assert_eq!(agents, vec!["feat-a".to_string()]);
                assert_eq!(co_author, vec!["Pair <pair@example.com>".to_string()]);
                assert!(!no_co_author);
                assert_eq!(author.as_deref(), Some("Lead <lead@example.com>"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_git_familiar_flags_parse_for_log_diff_status_and_branch() {
        let log_cmd =
            Cli::try_parse_from(["tes", "log", "-p", "-n", "4", "--stat", "--", "docs/"]).unwrap();
        match log_cmd.command {
            Commands::Log {
                patch,
                stat,
                limit,
                paths,
                ..
            } => {
                assert!(patch);
                assert!(stat);
                assert_eq!(limit, 4);
                assert_eq!(paths, vec!["docs/".to_string()]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let diff_cmd = Cli::try_parse_from([
            "tes",
            "diff",
            "--cached",
            "--stat",
            "--name-only",
            "-U5",
            "-w",
            "HEAD~2..HEAD",
            "--",
            "crates/cli/src/main.rs",
        ])
        .unwrap();
        match diff_cmd.command {
            Commands::Diff {
                staged,
                stat,
                name_only,
                unified,
                ignore_all_space,
                targets,
                pathspecs,
                ..
            } => {
                assert!(staged);
                assert!(stat);
                assert!(name_only);
                assert_eq!(unified, Some(5));
                assert!(ignore_all_space);
                assert_eq!(targets, vec!["HEAD~2..HEAD".to_string()]);
                assert_eq!(pathspecs, vec!["crates/cli/src/main.rs".to_string()]);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let status_cmd = Cli::try_parse_from(["tes", "status", "-sb"]).unwrap();
        match status_cmd.command {
            Commands::Status { short, branch, .. } => {
                assert!(short);
                assert!(branch);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let branch_cmd = Cli::try_parse_from(["tes", "branch", "-lv"]).unwrap();
        match branch_cmd.command {
            Commands::Branch { list, verbose, .. } => {
                assert!(list);
                assert!(verbose);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_session_focus_and_pager_flags_parse() {
        let focus = Cli::try_parse_from(["tes", "--paginate", "session", "focus", "antigravity-6c7a"]).unwrap();
        assert!(focus.paginate);
        assert!(!focus.no_pager);
        match focus.command {
            Commands::Agent {
                sub: AgentCommands::Focus { name },
            } => {
                assert_eq!(name, "antigravity-6c7a");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        assert!(!resolve_pager_command(None).is_empty());
    }

    #[test]
    fn extract_hook_file_paths_supports_standard_and_codex_apply_patch_payloads() {
        let claude_payload: Option<Value> = Some(serde_json::json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": "src/lib.rs" }
        }));
        assert_eq!(
            extract_hook_file_paths(&claude_payload),
            vec!["src/lib.rs".to_string()]
        );

        let codex_patch_payload: Option<Value> = Some(serde_json::json!({
            "tool_name": "apply_patch",
            "tool_input": {
                "patch": "*** Begin Patch\n*** Update File: crates/core/src/lib.rs\n@@\n-old\n+new\n*** Add File: crates/core/src/codex.rs\n+pub fn ok() {}\n*** End Patch"
            }
        }));
        assert_eq!(
            extract_hook_file_paths(&codex_patch_payload),
            vec![
                "crates/core/src/lib.rs".to_string(),
                "crates/core/src/codex.rs".to_string()
            ]
        );
    }
}

