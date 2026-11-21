//! Command-line argument definitions and Clap parser structs for `tes`.

use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Args, Parser, Subcommand};

/// Returns the ANSI color styling configuration for Clap help output.
pub fn cli_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::BrightBlack.on_default())
        .valid(AnsiColor::Green.on_default().effects(Effects::BOLD))
        .invalid(AnsiColor::Red.on_default().effects(Effects::BOLD))
        .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
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
pub struct Cli {
    /// Emit machine-readable JSON output for AI harnesses.
    #[arg(long, global = true)]
    pub json: bool,

    /// Do not pipe output into a pager.
    #[arg(short = 'P', long, global = true)]
    pub no_pager: bool,

    /// Pipe output into a pager if standard output is a terminal.
    #[arg(long, global = true, overrides_with = "no_pager")]
    pub paginate: bool,

    /// Optional repository path (defaults to current working directory).
    #[arg(short = 'C', long, global = true)]
    pub repo: Option<PathBuf>,

    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Commands,
}

/// Top-level `tes` subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Initialize Tessera (`.git/tessera`) and install repo-local AI harness hooks & MCP discovery files.
    Init {
        /// Optional target directory (defaults to current directory or `-C <repo>`).
        path: Option<PathBuf>,
        /// Optional initial/target Git branch name (defaults to active Git branch or `init.defaultBranch`).
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// AI harness integration(s) to install: `antigravity`, `claude`, `cursor`, `windsurf`, `kiro`, `opencode`, `codex`, `vscode`, or `all` (auto-detected if omitted).
        #[arg(long)]
        harness: Option<String>,
        /// Record newly created harness rule/config files in `.git/info/exclude` so they stay local and untracked.
        #[arg(long)]
        local: bool,
    },

    /// Run repository health, Copy-on-Write, config trust, hook, sandbox, and conflict diagnostics.
    Doctor,

    /// Snapshot and display the current session's status, branch, staged/unstaged changes, and touched symbols.
    Status {
        /// Give the output in the short-format (`-s` / `--short`).
        #[arg(short = 's', long)]
        short: bool,
        /// Give the output in machine-readable porcelain format with zero ANSI color escapes.
        #[arg(long)]
        porcelain: bool,
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
        /// Compare a specific branch against its merge-base with the trunk branch.
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
        /// Optional parent branch or revision to fork `--branch` from (e.g., `feat/parent`).
        #[arg(long, visible_alias = "base")]
        from: Option<String>,
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

    /// Manage active agent/terminal sessions and isolated Copy-on-Write sandboxes (`spawn`, `list`, `focus`, `exec`, `close`, `clear`).
    #[command(visible_alias = "session")]
    Agent {
        /// Session management subcommand.
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
        /// Reuse the tip commit's message without editing when paired with `--amend`.
        #[arg(long)]
        no_edit: bool,
        /// Run speculative verification before advancing the branch (`--verify` uses configured `verify_command`; `--verify="<cmd>"` runs a custom command).
        #[arg(long, num_args = 0..=1, default_missing_value = "", require_equals = true)]
        verify: Option<String>,
        /// Explicit speculative verification command to execute before advancing the branch.
        #[arg(long = "verify-cmd", value_name = "CMD", conflicts_with = "verify")]
        verify_cmd: Option<String>,
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

    /// Push the current session's active Tessera branch to a remote, auto-committing pending session changes unless `--no-auto-commit` is passed.
    Push {
        /// Git remote name (or branch/refspec when a single non-remote argument is supplied).
        remote: Option<String>,
        /// Branch or refspec (`local:remote`) to push (defaults to the current session's active branch).
        branch: Option<String>,
        /// Set upstream tracking reference (`-u` / `--set-upstream`).
        #[arg(short = 'u', long = "set-upstream")]
        set_upstream: bool,
        /// Optional commit message if uncommitted session changes need to be committed before pushing.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Force push (`--force`).
        #[arg(short = 'f', long)]
        force: bool,
        /// Force push safely with lease check (`--force-with-lease`).
        #[arg(long = "force-with-lease", conflicts_with = "force")]
        force_with_lease: bool,
        /// Push all tags (`--tags`).
        #[arg(long)]
        tags: bool,
        /// Delete the specified branch or ref on the remote (`-d` / `--delete`).
        #[arg(short = 'd', long)]
        delete: bool,
        /// Perform a dry run without updating remote refs (`-n` / `--dry-run`).
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Do not auto-commit uncommitted session changes before pushing.
        #[arg(long)]
        no_auto_commit: bool,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Fetch from a remote, fast-forward or 3-way AST-merge the branch, and auto-rebase active sessions.
    Pull {
        /// Git remote name (or branch name when a single non-remote argument is supplied).
        remote: Option<String>,
        /// Branch to pull (defaults to the current session's active Tessera branch).
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// Stash and restore uncommitted session changes without affecting concurrent peer sessions (`push`, `pop`, `apply`, `list`, `drop`, `clear`).
    Stash {
        /// Stash subcommand.
        #[command(subcommand)]
        sub: Option<StashCommands>,
        /// Optional stash message when running bare `tes stash -m "<msg>"`.
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Optional branch override when running bare `tes stash -b <branch>`.
        #[arg(short = 'b', long)]
        branch: Option<String>,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long, global = true)]
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
        /// Agent session IDs or branch names to merge (merges all active agents if omitted).
        agents: Vec<String>,
        /// Target Git branch name (defaults to currently checked-out Git branch or configured `trunk_branch`).
        #[arg(long)]
        into: Option<String>,
        /// Custom Git commit message (defaults to a clean message synthesized from agent task intents).
        #[arg(short = 'm', long)]
        message: Option<String>,
        /// Run speculative verification in a COW sandbox before advancing the branch (`--verify` uses configured `verify_command`; `--verify="<cmd>"` runs a custom command).
        #[arg(long, num_args = 0..=1, default_missing_value = "", require_equals = true)]
        verify: Option<String>,
        /// Explicit speculative verification command to execute before advancing the branch.
        #[arg(long = "verify-cmd", value_name = "CMD", conflicts_with = "verify")]
        verify_cmd: Option<String>,
        /// Simulate the full merge and optional verification gate without mutating Git refs, working trees, or the Operation Log.
        #[arg(long)]
        dry_run: bool,
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
        #[arg(long, conflicts_with = "content_file")]
        content: Option<String>,
        /// Path to a file containing explicit resolved content when `--strategy content` is selected.
        #[arg(long = "content-file", value_name = "PATH", conflicts_with = "content")]
        content_file: Option<PathBuf>,
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
        #[arg(long, conflicts_with = "checkpoint")]
        op: Option<String>,
        /// Optional target checkpoint label created via `tes checkpoint <label>`.
        #[arg(long, conflicts_with = "op")]
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
        /// Hook lifecycle event subcommand.
        #[command(subcommand)]
        sub: HookCommands,
        /// Optional explicit session ID.
        #[arg(short = 'a', long, visible_alias = "agent", global = true)]
        session: Option<String>,
    },

    /// List, switch, delete (`-d` / `-D`), or rename (`-m`) Tessera branches.
    #[command(alias = "branches")]
    Branch {
        /// Optional branch name to switch to, filter pattern with `-l`, or second argument when renaming with `-m <old> <new>`.
        branch: Option<String>,
        /// List branches (`-l` / `--list`), optionally filtered by a glob pattern.
        #[arg(short = 'l', long, conflicts_with_all = ["delete", "force_delete", "rename", "show_current"])]
        list: bool,
        /// Print only the current active branch name (`--show-current`).
        #[arg(long = "show-current", conflicts_with_all = ["delete", "force_delete", "rename", "list", "remotes", "all"])]
        show_current: bool,
        /// List remote-tracking branches (`-r` / `--remotes`).
        #[arg(short = 'r', long, conflicts_with_all = ["delete", "force_delete", "rename", "all"])]
        remotes: bool,
        /// List both local and remote-tracking branches (`-a` / `--all`).
        #[arg(short = 'a', long, conflicts_with_all = ["delete", "force_delete", "rename", "remotes"])]
        all: bool,
        /// Show commit SHA and subject line for each branch in list mode (`-v` / `--verbose`).
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Delete a branch after verifying it is merged into trunk (`-d <branch>`).
        #[arg(short = 'd', long = "delete", conflicts_with_all = ["force_delete", "rename", "list", "show_current", "remotes", "all"])]
        delete: Option<String>,
        /// Force-delete a branch even if it has unmerged commits or uncommitted changes (`-D <branch>`).
        #[arg(short = 'D', long = "force-delete", conflicts_with_all = ["delete", "rename", "list", "show_current", "remotes", "all"])]
        force_delete: Option<String>,
        /// Rename a branch (`-m <new>` for current branch, or `-m <old> <new>`).
        #[arg(short = 'm', long = "move", conflicts_with_all = ["delete", "force_delete", "list", "show_current", "remotes", "all"])]
        rename: Option<String>,
        /// Switch the primary editor working tree (`repo_root` and `.git/HEAD`) to `<branch>`.
        #[arg(short = 'e', long)]
        editor: bool,
        /// Optional explicit agent/session ID.
        #[arg(long)]
        agent: Option<String>,
    },

    /// Switch or bind the current terminal/session to a specific branch without altering `.git/HEAD` for other sessions.
    #[command(alias = "checkout")]
    Switch {
        /// Target branch name (or start-point when paired with `-b`/`-c <new-branch>`).
        branch: Option<String>,
        /// Optional parent branch or start-point when creating a branch (`tes switch <new-branch> <start-point>`).
        start_point: Option<String>,
        /// Create or switch to a branch (`-b <branch>` or `-c <branch>`).
        #[arg(short = 'b', short_alias = 'c', long = "create")]
        create: Option<String>,
        /// Optional parent branch or revision to fork from (`--from <base>`).
        #[arg(long, visible_alias = "base")]
        from: Option<String>,
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
        /// Automatically install the shell integration into detected shell profiles.
        #[arg(long)]
        install: bool,
        /// Optional explicit agent/session ID.
        #[arg(short = 'a', long)]
        agent: Option<String>,
    },

    /// View or modify repository configuration (`.git/tessera/config.toml` / `tessera.toml`), including custom dependency/build cache folders, ports, and env templates.
    Config {
        /// Configuration subcommand.
        #[command(subcommand)]
        sub: Option<ConfigCommands>,
        /// Shorthand to set `verify_command` directly (`tes config --verify-cmd "<cmd>"`).
        #[arg(long = "verify-cmd")]
        verify_cmd: Option<String>,
    },

    /// Start the built-in Model Context Protocol (MCP) JSON-RPC 2.0 server over stdio.
    Mcp,

    /// Internal dynamic tab-completion engine invoked by shell hooks.
    #[command(hide = true)]
    Complete(CompleteArgs),

    /// Transparent passthrough to `git <subcommand>` with automatic post-command Tessera state synchronization.
    #[command(external_subcommand)]
    External(Vec<String>),
}

/// Arguments for the internal `tes complete` dynamic completion subcommand.
#[derive(Debug, Args)]
pub struct CompleteArgs {
    /// Target shell (`zsh`, `bash`, `fish`, or `plain`).
    #[arg(long, default_value = "plain")]
    pub shell: String,
    /// Zero-based index of the word currently being completed.
    #[arg(long)]
    pub cword: Option<usize>,
    /// Command line words.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub words: Vec<String>,
}

/// Subcommands for `tes hook`.
#[derive(Debug, Subcommand)]
pub enum HookCommands {
    /// Record an AI user prompt and ensure the session exists.
    Prompt {
        /// Optional prompt text for session intent tracking.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Run pre-tool collision radar before editing file(s).
    #[command(name = "pre-tool")]
    PreTool {
        /// Explicit file path(s) via `--path`.
        #[arg(long, num_args = 1..)]
        path: Vec<String>,
        /// Optional positional file path(s).
        paths: Vec<String>,
    },
    /// Reconcile file(s) on disk after editing via 3-way AST merge.
    #[command(name = "post-tool")]
    PostTool {
        /// Explicit file path(s) via `--path`.
        #[arg(long, num_args = 1..)]
        path: Vec<String>,
        /// Optional positional file path(s).
        paths: Vec<String>,
    },
    /// Synchronize active sessions after a native Git hook event.
    #[command(name = "git-sync")]
    GitSync,
    /// Catch-all for future or external hook event names (ignored silently).
    #[command(external_subcommand)]
    Other(Vec<String>),
}

/// Subcommands for `tes stash`.
#[derive(Debug, Subcommand)]
pub enum StashCommands {
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
pub enum ConfigCommands {
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
    /// Set a scalar configuration property (`trunk_branch`, `base_port`, `port_stride`, `port_isolation`, `verify_command`, `watch_interval_ms`, `watch_timeout_secs`, `co_author`, `co_author_trailer`, `auto_isolate_dirty_claims`, `pager`).
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
pub enum AgentCommands {
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
