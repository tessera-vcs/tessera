//! Adversarial state-synchronization invariants driven through the real `tes` binary.
//!
//! Every scenario asserts the same contract: after a lifecycle operation, `tes status` and raw
//! `git status --porcelain --untracked-files=all` agree on every added, modified, mode-changed and
//! deleted path, and once no uncommitted changes remain, `git status` and `git diff HEAD` are empty.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Environment variables that influence session detection or Git plumbing and must never leak
/// from the developer's shell (or an AI harness running the test suite) into the scenarios.
const ISOLATED_ENV: &[&str] = &[
    "TES_SESSION_ID",
    "TES_BRANCH",
    "TES_WORKSPACE",
    "TES_PORT",
    "TES_PORT_ISOLATION",
    "PORT",
    "TES_SHIM_BIN",
    "ANTIGRAVITY_CONVERSATION_ID",
    "ANTIGRAVITY_AGENT",
    "CLAUDE_SESSION_ID",
    "CLAUDECODE",
    "CURSOR_TRACE_ID",
    "CURSOR_SESSION_ID",
    "WINDSURF_SESSION_ID",
    "CODEIUM_SESSION_ID",
    "KIRO_SESSION_ID",
    "OPENCODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_SESSION_ID",
    "OPENAI_CODEX_SESSION_ID",
    "CODEX_SANDBOX",
    "CODEX_CI",
    "COPILOT_SESSION_ID",
    "TERM_SESSION_ID",
    "ITERM_SESSION_ID",
    "TMUX_PANE",
    "ZELLIJ_PANE_ID",
    "WEZTERM_PANE",
    "KITTY_WINDOW_ID",
    "GHOSTTY_SURFACE_ID",
    "ALACRITTY_WINDOW_ID",
    "KONSOLE_DBUS_SESSION",
    "GNOME_TERMINAL_SCREEN",
    "TILIX_ID",
    "WT_SESSION",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
];

/// A throwaway repository initialized by `tes init` with a clean baseline commit.
struct Repo {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    /// Creates `files` (`path`, `content`, `executable`), runs `tes init`, and asserts the
    /// resulting repository starts with an empty `git status`.
    fn new(files: &[(&str, &str, bool)]) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let repo = Self { _tmp: tmp, root };

        for (path, content, executable) in files {
            repo.write(path, content);
            repo.chmod(path, *executable);
        }

        let init = repo.run_tes(None, &["init", "--json"]);
        assert!(
            init.status.success(),
            "tes init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        repo.assert_git_clean("after tes init");
        repo
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.path(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&p, content).expect("write file");
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).expect("read file")
    }

    fn rm(&self, rel: &str) {
        fs::remove_file(self.path(rel)).expect("rm file");
    }

    fn chmod(&self, rel: &str, executable: bool) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if executable { 0o755 } else { 0o644 };
            fs::set_permissions(self.path(rel), fs::Permissions::from_mode(mode)).expect("chmod");
        }

        #[cfg(not(unix))]
        {
            let _ = (rel, executable);
        }
    }

    fn is_executable(&self, rel: &str) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(self.path(rel))
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }

        #[cfg(not(unix))]
        {
            let _ = rel;
            false
        }
    }

    /// Runs the `tes` binary under test. A `session` starting with `term-` runs as that human
    /// terminal session (via `TES_SESSION_ID`); any other `session` selects an AI session via
    /// `ANTIGRAVITY_CONVERSATION_ID` (resolving to `antigravity-<session>`); `None` runs without
    /// any harness identity.
    fn run_tes(&self, session: Option<&str>, args: &[&str]) -> Output {
        let bin = Path::new(env!("CARGO_BIN_EXE_tes"));
        let path_env = std::env::var("PATH").unwrap_or_default();
        let bin_dir = bin
            .parent()
            .map(|d| d.display().to_string())
            .unwrap_or_default();

        let mut cmd = Command::new(bin);
        cmd.current_dir(&self.root)
            .args(args)
            .env("PATH", format!("{bin_dir}:{path_env}"))
            .env("GIT_AUTHOR_NAME", "Test Author")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test Author")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("TES_NO_PAGER", "1");
        for key in ISOLATED_ENV {
            cmd.env_remove(key);
        }

        match session {
            Some(s) if s.starts_with("term-") => {
                cmd.env("TES_SESSION_ID", s);
            }
            Some(s) => {
                cmd.env("ANTIGRAVITY_CONVERSATION_ID", s);
            }
            None => {}
        }

        cmd.output().expect("spawn tes")
    }

    /// Runs `tes` as `session` and asserts success, returning stdout.
    fn tes(&self, session: &str, args: &[&str]) -> String {
        let out = self.run_tes(Some(session), args);
        assert!(
            out.status.success(),
            "`tes {}` as {session} failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn hook(&self, session: &str, rel: &str) {
        self.tes(session, &["hook", "post-tool", "--path", rel]);
    }

    /// Runs raw `git` the way a user's shell would: the `tes` binary under test is first on `PATH`
    /// so the Git hooks installed by `tes init` invoke it, and session identity never leaks in.
    fn git(&self, args: &[&str]) -> String {
        let bin = Path::new(env!("CARGO_BIN_EXE_tes"));
        let path_env = std::env::var("PATH").unwrap_or_default();
        let bin_dir = bin
            .parent()
            .map(|d| d.display().to_string())
            .unwrap_or_default();

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.root)
            .args(args)
            .env("PATH", format!("{bin_dir}:{path_env}"))
            .env("GIT_AUTHOR_NAME", "Test Author")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test Author")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        for key in ISOLATED_ENV {
            cmd.env_remove(key);
        }

        let out = cmd.output().expect("spawn git");
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Returns raw `git status` paths (one per changed, deleted, mode-changed or untracked file).
    fn git_status_paths(&self) -> BTreeSet<String> {
        self.git(&["status", "--porcelain", "--untracked-files=all"])
            .lines()
            .filter(|l| l.len() > 3)
            .map(|l| l[3..].trim().to_string())
            .collect()
    }

    fn assert_git_clean(&self, ctx: &str) {
        let status = self.git(&["status", "--porcelain", "--untracked-files=all"]);
        assert!(
            status.is_empty(),
            "expected clean `git status` {ctx}, got:\n{status}"
        );
        let diff = self.git(&["diff", "HEAD"]);
        assert!(
            diff.is_empty(),
            "expected empty `git diff HEAD` {ctx}, got:\n{diff}"
        );
    }

    /// Returns `path -> (mode, blob)` for the committed `HEAD` tree.
    fn head_tree(&self) -> BTreeMap<String, (String, String)> {
        self.tree_of("HEAD")
    }

    fn tree_of(&self, rev: &str) -> BTreeMap<String, (String, String)> {
        self.git(&["ls-tree", "-r", rev])
            .lines()
            .filter_map(|l| {
                let (meta, path) = l.split_once('\t')?;
                let mut parts = meta.split_whitespace();
                let mode = parts.next()?.to_string();
                let _kind = parts.next()?;
                let blob = parts.next()?.to_string();
                Some((path.to_string(), (mode, blob)))
            })
            .collect()
    }

    fn head_content(&self, rel: &str) -> String {
        self.git(&["show", &format!("HEAD:{rel}")])
    }

    /// Returns the paths `session` would commit: the union of staged and unstaged paths reported by
    /// `tes status --json`.
    fn tes_session_paths(&self, session: &str) -> BTreeSet<String> {
        let v = self.status_json(session);
        let mut out = BTreeSet::new();
        for key in ["staged_changes", "unstaged_changes"] {
            if let Some(obj) = v.get(key).and_then(serde_json::Value::as_object) {
                out.extend(obj.keys().cloned());
            }
        }
        out
    }

    /// Returns every working-tree path `tes status --json` reports for `session`: its own staged and
    /// unstaged changes, peer session changes, and unattributed changes nobody will commit yet. This
    /// set must always equal [`Self::git_status_paths`].
    fn tes_status_paths(&self, session: &str) -> BTreeSet<String> {
        let v = self.status_json(session);
        let mut out = self.tes_session_paths(session);
        for key in ["peer_changes", "unattributed_changes"] {
            match v.get(key) {
                Some(serde_json::Value::Object(obj)) => out.extend(obj.keys().cloned()),
                Some(serde_json::Value::Array(items)) => {
                    out.extend(
                        items
                            .iter()
                            .filter_map(|i| i.as_str().map(ToString::to_string)),
                    );
                }
                _ => {}
            }
        }
        out
    }

    fn status_json(&self, session: &str) -> serde_json::Value {
        serde_json::from_str(&self.tes(session, &["status", "--json"])).expect("status json")
    }

    fn sandbox_dir(&self, session: &str) -> PathBuf {
        let _ = self.tes(session, &["exec", "-b", "main", "--", "git", "--version"]);
        let canonical = fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        let coord = agent::Coordinator::open(&canonical).expect("open coord for sandbox_dir");
        let dir = coord.branch_sandbox_dir("main");
        if dir.is_dir() {
            dir
        } else {
            agent::Coordinator::open(&self.root)
                .expect("open coord fallback")
                .branch_sandbox_dir("main")
        }
    }
}

/// Bulk directory move (`mv src/old src/new`, 12 files) mixed with hooked edits, a hooked
/// directory-level post-tool, an unhooked creation and an unhooked deletion: status must match
/// `git status`, and the commit must contain exactly the moved tree with no resurrected sources.
#[test]
fn bulk_directory_move_with_mixed_hooked_and_unhooked_edits_commits_cleanly() {
    let mut files: Vec<(String, String)> = (0..12)
        .map(|i| {
            (
                format!("src/old/m{i:02}.rs"),
                format!("pub fn m{i}() -> u32 {{ {i} }}\n"),
            )
        })
        .collect();
    files.push(("keep.rs".into(), "pub fn keep() {}\n".into()));
    files.push(("notes.txt".into(), "notes\n".into()));
    let spec: Vec<(&str, &str, bool)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str(), false))
        .collect();
    let repo = Repo::new(&spec);

    repo.tes(
        "mover",
        &["claim", "--intent", "Move old module tree to new"],
    );
    fs::rename(repo.path("src/old"), repo.path("src/new")).expect("mv dir");
    repo.hook("mover", "src/old");
    repo.hook("mover", "src/new/m00.rs");
    repo.write("keep.rs", "pub fn keep() -> bool { true }\n");
    repo.hook("mover", "keep.rs");
    repo.write("added.rs", "pub fn added() {}\n");
    repo.rm("notes.txt");

    let git_paths = repo.git_status_paths();
    let tes_paths = repo.tes_status_paths("mover");
    assert_eq!(
        tes_paths, git_paths,
        "tes status must agree with git status"
    );

    repo.tes("mover", &["commit", "-m", "refactor: move module tree"]);

    let after_agent = repo.head_tree();
    for i in 0..12 {
        assert!(
            !after_agent.contains_key(&format!("src/old/m{i:02}.rs")),
            "src/old/m{i:02}.rs resurrected"
        );
        assert!(
            after_agent.contains_key(&format!("src/new/m{i:02}.rs")),
            "moved src/new/m{i:02}.rs not committed"
        );
    }
    assert_eq!(repo.tes_status_paths("mover"), repo.git_status_paths());

    repo.tes(
        "term-human",
        &["commit", "-m", "chore: commit remaining shell edits"],
    );

    let head = repo.head_tree();
    for i in 0..12 {
        assert!(
            !head.contains_key(&format!("src/old/m{i:02}.rs")),
            "src/old/m{i:02}.rs resurrected"
        );
        assert!(
            head.contains_key(&format!("src/new/m{i:02}.rs")),
            "src/new/m{i:02}.rs missing"
        );
        assert!(!repo.path(&format!("src/old/m{i:02}.rs")).exists());
    }
    assert!(
        !repo.path("src/old").exists(),
        "empty source directory left behind"
    );
    assert!(head.contains_key("added.rs"));
    assert!(!head.contains_key("notes.txt"));
    assert_eq!(
        repo.head_content("keep.rs"),
        "pub fn keep() -> bool { true }\n"
    );
    repo.assert_git_clean("after committing a bulk directory move");
}

/// Session A deletes a file plus a 10-file directory while Session B edits a different file on
/// the same branch. In every commit order (and in a combined `tes merge`), neither session may
/// resurrect A's deletions in `repo_root`, `HEAD`, or the index.
#[test]
fn peer_deletions_survive_concurrent_peer_edits_in_every_commit_order() {
    for order in ["a-first", "b-first", "combined"] {
        let mut files: Vec<(String, String)> = (0..10)
            .map(|i| (format!("gone/g{i}.txt"), format!("g{i}\n")))
            .collect();
        files.push(("doomed.rs".into(), "pub fn doomed() {}\n".into()));
        files.push(("other.rs".into(), "pub fn other() {}\n".into()));
        let spec: Vec<(&str, &str, bool)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str(), false))
            .collect();
        let repo = Repo::new(&spec);

        repo.tes("deleter", &["claim", "--intent", "Remove dead code"]);
        repo.tes(
            "editor1",
            &["claim", "--intent", "Improve other", "other.rs"],
        );

        repo.rm("doomed.rs");
        fs::remove_dir_all(repo.path("gone")).expect("rm -r gone");
        repo.hook("deleter", "doomed.rs");
        repo.hook("deleter", "gone");

        repo.write("other.rs", "pub fn other() -> u8 { 7 }\n");
        repo.hook("editor1", "other.rs");

        assert_eq!(
            repo.tes_status_paths("deleter")
                .union(&repo.tes_status_paths("editor1"))
                .cloned()
                .collect::<BTreeSet<_>>(),
            repo.git_status_paths(),
            "[{order}] session changes must partition git status"
        );

        match order {
            "a-first" => {
                repo.tes("deleter", &["commit", "-m", "chore: remove dead code"]);
                assert!(!repo.head_tree().contains_key("doomed.rs"));
                assert_eq!(repo.head_content("other.rs"), "pub fn other() {}\n");
                assert_eq!(
                    repo.git_status_paths(),
                    BTreeSet::from(["other.rs".to_string()]),
                    "[{order}] only B's edit may remain uncommitted"
                );
                repo.tes("editor1", &["commit", "-m", "feat: improve other"]);
            }
            "b-first" => {
                repo.tes("editor1", &["commit", "-m", "feat: improve other"]);
                assert!(repo.head_tree().contains_key("doomed.rs"));
                assert!(
                    !repo.path("doomed.rs").exists(),
                    "[{order}] B's commit resurrected doomed.rs on disk"
                );
                assert!(
                    !repo.path("gone/g0.txt").exists(),
                    "[{order}] B's commit resurrected gone/ on disk"
                );
                let remaining = repo.git_status_paths();
                assert!(
                    remaining.contains("doomed.rs") && remaining.len() == 11,
                    "[{order}] {remaining:?}"
                );
                repo.tes("deleter", &["commit", "-m", "chore: remove dead code"]);
            }
            _ => {
                repo.tes("deleter", &["merge", "-m", "chore: combined"]);
            }
        }

        let head = repo.head_tree();
        assert!(
            !head.contains_key("doomed.rs"),
            "[{order}] doomed.rs resurrected in HEAD"
        );
        assert!(
            (0..10).all(|i| !head.contains_key(&format!("gone/g{i}.txt"))),
            "[{order}] gone/ resurrected"
        );
        assert!(
            !repo.path("doomed.rs").exists(),
            "[{order}] doomed.rs resurrected on disk"
        );
        assert_eq!(
            repo.head_content("other.rs"),
            "pub fn other() -> u8 { 7 }\n"
        );
        repo.assert_git_clean(&format!("[{order}] after both sessions committed"));
    }
}

/// Unhooked `chmod +x` / `chmod -x` must show up in `tes status`, be committed with the right Git
/// modes (`100755` / `100644`), and leave the index and working tree in parity.
#[test]
#[cfg(unix)]
fn unhooked_chmod_changes_are_tracked_and_committed_with_exact_modes() {
    let repo = Repo::new(&[
        ("tool.sh", "#!/bin/sh\necho tool\n", false),
        ("run.sh", "#!/bin/sh\necho run\n", true),
        ("lib.rs", "pub fn lib() {}\n", false),
    ]);
    assert_eq!(repo.head_tree()["run.sh"].0, "100755");

    repo.tes("chmodder", &["claim", "--intent", "Fix script permissions"]);
    repo.chmod("tool.sh", true);
    repo.chmod("run.sh", false);

    assert_eq!(repo.tes_status_paths("chmodder"), repo.git_status_paths());
    assert_eq!(
        repo.git_status_paths(),
        BTreeSet::from(["run.sh".to_string(), "tool.sh".to_string()])
    );

    repo.tes("chmodder", &["commit", "-m", "chore: fix script modes"]);
    let head = repo.head_tree();
    assert_eq!(head["tool.sh"].0, "100755");
    assert_eq!(head["run.sh"].0, "100644");
    assert!(repo.is_executable("tool.sh"));
    assert!(!repo.is_executable("run.sh"));
    repo.assert_git_clean("after committing mode changes");
}

/// A stale `branches/main` sandbox (old file contents and a stray file) must never shadow or
/// revert edits, deletions, renames and mode changes made in `repo_root` on the checked-out branch.
#[test]
fn stale_head_branch_sandbox_never_shadows_repo_root() {
    let repo = Repo::new(&[
        ("a.rs", "pub fn a() -> u8 { 1 }\n", false),
        ("c.rs", "pub fn c() {}\n", false),
        ("d.rs", "pub fn d() {}\n", false),
        ("e.sh", "#!/bin/sh\n", false),
        ("r1.rs", "pub fn r() {}\n", false),
    ]);

    repo.tes(
        "worker",
        &[
            "claim",
            "--intent",
            "Head branch edits",
            "a.rs",
            "c.rs",
            "d.rs",
            "e.sh",
            "r1.rs",
            "r2.rs",
        ],
    );
    let sandbox = repo.sandbox_dir("worker");
    assert!(
        sandbox.join("a.rs").is_file(),
        "sandbox not materialized at {}",
        sandbox.display()
    );
    fs::write(sandbox.join("a.rs"), "pub fn a() -> u8 { 999 } // STALE\n").expect("stale a.rs");
    fs::write(sandbox.join("ghost.rs"), "pub fn ghost() {}\n").expect("ghost");

    repo.write("c.rs", "pub fn c() -> i32 { 3 }\n");
    repo.hook("worker", "c.rs");
    repo.rm("d.rs");
    repo.chmod("e.sh", true);
    fs::rename(repo.path("r1.rs"), repo.path("r2.rs")).expect("rename");

    assert_eq!(repo.tes_session_paths("worker"), repo.git_status_paths());
    repo.tes("worker", &["commit", "-m", "feat: head branch edits"]);

    let head = repo.head_tree();
    assert_eq!(
        repo.head_content("a.rs"),
        "pub fn a() -> u8 { 1 }\n",
        "stale sandbox reverted a.rs"
    );
    assert!(
        !head.contains_key("ghost.rs"),
        "stray sandbox file leaked into the commit"
    );
    assert_eq!(repo.head_content("c.rs"), "pub fn c() -> i32 { 3 }\n");
    assert!(!head.contains_key("d.rs"));
    #[cfg(unix)]
    assert_eq!(head["e.sh"].0, "100755");
    assert!(!head.contains_key("r1.rs") && head.contains_key("r2.rs"));
    assert_eq!(repo.read("a.rs"), "pub fn a() -> u8 { 1 }\n");
    repo.assert_git_clean("after committing over a stale branches/main sandbox");

    assert!(
        !sandbox.join("d.rs").exists(),
        "sandbox still holds a file deleted on main"
    );
    assert!(
        !sandbox.join("r1.rs").exists(),
        "sandbox still holds a renamed-away file"
    );
    assert_eq!(
        fs::read_to_string(sandbox.join("a.rs")).expect("sandbox a.rs"),
        "pub fn a() -> u8 { 1 }\n",
        "sandbox mirror not refreshed from the committed tree"
    );
}

/// `target_branch: None` and `target_branch: Some(<checked-out branch>)` must produce identical
/// status, identical commits and identical working-tree state for the same sequence of edits.
#[test]
fn target_branch_none_and_explicit_head_branch_behave_identically() {
    let files = [
        ("x.rs", "pub fn x() {}\n", false),
        ("y.rs", "pub fn y() {}\n", false),
        ("z.sh", "#!/bin/sh\n", false),
    ];
    let mut outcomes = Vec::new();

    for explicit in [false, true] {
        let repo = Repo::new(&files);
        if explicit {
            repo.tes(
                "pinned",
                &[
                    "claim",
                    "--intent",
                    "Same work",
                    "--branch",
                    "main",
                    "x.rs",
                    "y.rs",
                    "z.sh",
                    "w.rs",
                ],
            );
        } else {
            repo.tes(
                "pinned",
                &[
                    "claim",
                    "--intent",
                    "Same work",
                    "x.rs",
                    "y.rs",
                    "z.sh",
                    "w.rs",
                ],
            );
        }

        repo.write("x.rs", "pub fn x() -> u8 { 1 }\n");
        repo.hook("pinned", "x.rs");
        repo.rm("y.rs");
        repo.hook("pinned", "y.rs");
        repo.write("w.rs", "pub fn w() {}\n");
        repo.chmod("z.sh", true);

        let status = repo.status_json("pinned");
        let mut union = BTreeMap::new();
        for key in ["staged_changes", "unstaged_changes"] {
            if let Some(obj) = status.get(key).and_then(serde_json::Value::as_object) {
                for (k, v) in obj {
                    union.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
                }
            }
        }
        assert_eq!(status["active_branch"], "main");
        assert_eq!(
            union.keys().cloned().collect::<BTreeSet<_>>(),
            repo.git_status_paths()
        );

        repo.tes("pinned", &["commit", "-m", "feat: same work"]);
        repo.assert_git_clean(&format!("after commit (explicit branch = {explicit})"));
        let tree: BTreeMap<String, String> = repo
            .head_tree()
            .into_iter()
            .map(|(p, (mode, blob))| (p, format!("{mode} {blob}")))
            .collect();
        let branch = repo.git(&["branch", "--show-current"]).trim().to_string();
        outcomes.push((union, tree, branch));
    }

    assert_eq!(outcomes[0], outcomes[1], "None vs Some(head) diverged");
}

/// `git status --porcelain` and `git diff HEAD` must be empty after `undo`, `restore`, `stash
/// push`/`pop` round trips (including deletions and modes) and `sync`.
#[test]
fn git_index_parity_after_undo_restore_stash_and_sync() {
    let repo = Repo::new(&[
        ("app.rs", "pub fn app() -> u8 { 1 }\n", false),
        ("bin.sh", "#!/bin/sh\n", true),
        ("old.rs", "pub fn old() {}\n", false),
    ]);

    repo.tes("cycler", &["claim", "--intent", "Lifecycle"]);
    repo.write("app.rs", "pub fn app() -> u8 { 2 }\n");
    repo.hook("cycler", "app.rs");
    repo.tes("cycler", &["commit", "-m", "feat: app v2"]);
    repo.assert_git_clean("after commit");

    repo.tes("cycler", &["undo"]);
    assert_eq!(repo.read("app.rs"), "pub fn app() -> u8 { 1 }\n");
    repo.assert_git_clean("after undo");

    repo.write("app.rs", "pub fn app() -> u8 { 3 }\n");
    repo.rm("old.rs");
    repo.chmod("bin.sh", false);
    repo.write("scratch.rs", "pub fn scratch() {}\n");
    repo.rm("scratch.rs");
    repo.tes("cycler", &["restore", "."]);
    #[cfg(unix)]
    assert!(
        repo.is_executable("bin.sh"),
        "restore lost the executable bit"
    );
    repo.assert_git_clean("after restore .");

    repo.tes(
        "cycler",
        &[
            "claim",
            "--intent",
            "Lifecycle",
            "app.rs",
            "bin.sh",
            "old.rs",
        ],
    );
    repo.write("app.rs", "pub fn app() -> u8 { 4 }\n");
    repo.hook("cycler", "app.rs");
    repo.rm("old.rs");
    repo.hook("cycler", "old.rs");
    repo.chmod("bin.sh", false);
    let before_stash = repo.git_status_paths();
    assert_eq!(before_stash.len(), 3, "{before_stash:?}");

    repo.tes("cycler", &["stash", "push", "-m", "wip"]);
    assert!(
        repo.path("old.rs").is_file(),
        "stash push did not restore the deleted file"
    );
    assert!(
        repo.is_executable("bin.sh"),
        "stash push did not restore the mode"
    );
    repo.assert_git_clean("after stash push");

    repo.tes("cycler", &["stash", "pop"]);
    assert_eq!(
        repo.git_status_paths(),
        before_stash,
        "stash pop did not round-trip"
    );
    assert!(
        !repo.path("old.rs").exists(),
        "stash pop resurrected a stashed deletion"
    );
    assert!(
        !repo.is_executable("bin.sh"),
        "stash pop lost the stashed mode change"
    );
    assert_eq!(repo.tes_status_paths("cycler"), before_stash);

    repo.tes("cycler", &["commit", "-m", "feat: app v4"]);
    repo.assert_git_clean("after committing popped changes");
    assert!(!repo.head_tree().contains_key("old.rs"));
    assert_eq!(repo.head_tree()["bin.sh"].0, "100644");

    repo.tes("cycler", &["sync"]);
    repo.assert_git_clean("after sync");
}

/// Tracked files that match `.gitignore`, built-in secret patterns, or default build directories
/// must survive unrelated commits and have their edits captured.
#[test]
fn tracked_ignored_files_survive_commits_and_capture_edits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root_buf = tmp.path().to_path_buf();
    let root = root_buf.as_path();
    fs::write(root.join(".gitignore"), "dist/\n*.pem\n").expect("gitignore");
    fs::create_dir_all(root.join("dist")).expect("mkdir dist");
    fs::create_dir_all(root.join("certs")).expect("mkdir certs");
    fs::write(root.join("dist/bundle.js"), "console.log(1);\n").expect("bundle");
    fs::write(root.join("certs/test.pem"), "-----BEGIN TEST-----\n").expect("pem");
    fs::write(root.join("main.rs"), "fn main() {}\n").expect("main");

    let git = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.current_dir(root).args(args);
        for key in ISOLATED_ENV {
            cmd.env_remove(key);
        }
        let out = cmd
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-f", "."]);
    git(&["commit", "-q", "-m", "seed"]);

    let repo = Repo {
        _tmp: tmp,
        root: root_buf.clone(),
    };
    let init = repo.run_tes(None, &["init", "--json"]);
    assert!(init.status.success());
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "harness files", "--allow-empty"]);
    repo.assert_git_clean("after seeding tracked ignored files");

    repo.tes("builder", &["claim", "--intent", "Unrelated edit"]);
    repo.write("main.rs", "fn main() { println!(\"hi\"); }\n");
    repo.hook("builder", "main.rs");
    repo.tes("builder", &["commit", "-m", "feat: greet"]);

    let head = repo.head_tree();
    assert!(
        head.contains_key("dist/bundle.js"),
        "tracked dist/ file deleted by commit"
    );
    assert!(
        head.contains_key("certs/test.pem"),
        "tracked *.pem file deleted by commit"
    );
    assert!(
        repo.path("dist/bundle.js").is_file(),
        "tracked dist/ file deleted from disk"
    );
    repo.assert_git_clean("after unrelated commit");

    repo.write("dist/bundle.js", "console.log(2);\n");
    assert_eq!(repo.tes_status_paths("builder"), repo.git_status_paths());
    repo.tes("builder", &["commit", "-m", "build: refresh bundle"]);
    assert_eq!(repo.head_content("dist/bundle.js"), "console.log(2);\n");
    repo.assert_git_clean("after committing a tracked ignored file");
}

/// A user's in-progress edits (made before an agent started, and while it works) are never swept
/// into the agent's commit, and an agent's commit is not blocked by an unrelated peer conflict.
#[test]
fn user_work_in_progress_is_never_committed_by_an_agent() {
    let repo = Repo::new(&[
        ("README.md", "# Project\n", false),
        ("docs/guide.md", "# Guide\n", false),
        ("src/lib.rs", "pub fn lib() {}\n", false),
        (
            "src/shared.rs",
            "pub fn shared() -> u8 {\n    1\n}\n",
            false,
        ),
    ]);

    repo.write("README.md", "# Project\n\nUser draft.\n");

    repo.tes("worker", &["claim", "--intent", "Edit lib", "src/lib.rs"]);
    repo.write(
        "docs/guide.md",
        "# Guide\n\nUser edit while the agent works.\n",
    );
    repo.write("src/lib.rs", "pub fn lib() -> u8 { 1 }\n");
    repo.hook("worker", "src/lib.rs");

    repo.tes("left", &["claim", "--intent", "Return 2", "src/shared.rs"]);
    repo.tes("right", &["claim", "--intent", "Return 3", "src/shared.rs"]);
    repo.write("src/shared.rs", "pub fn shared() -> u8 {\n    2\n}\n");
    repo.hook("left", "src/shared.rs");
    repo.write("src/shared.rs", "pub fn shared() -> u8 {\n    3\n}\n");
    repo.hook("right", "src/shared.rs");

    let worker_paths = repo.tes_session_paths("worker");
    assert_eq!(
        worker_paths,
        BTreeSet::from(["src/lib.rs".to_string()]),
        "{worker_paths:?}"
    );

    let report: serde_json::Value =
        serde_json::from_str(&repo.tes("worker", &["--json", "commit", "-m", "feat: lib"]))
            .expect("commit json");
    assert!(
        report["conflicts"].as_array().is_some_and(Vec::is_empty),
        "worker commit reported unrelated conflicts: {report}"
    );

    assert_eq!(repo.head_content("README.md"), "# Project\n");
    assert_eq!(repo.head_content("docs/guide.md"), "# Guide\n");
    assert_eq!(
        repo.head_content("src/lib.rs"),
        "pub fn lib() -> u8 { 1 }\n"
    );
    assert_eq!(
        repo.read("README.md"),
        "# Project\n\nUser draft.\n",
        "user draft lost from disk"
    );
    let remaining = repo.git_status_paths();
    assert!(
        remaining.contains("README.md") && remaining.contains("docs/guide.md"),
        "{remaining:?}"
    );
    assert!(!remaining.contains("src/lib.rs"), "{remaining:?}");
}

/// Claiming a file that already holds someone else's uncommitted changes isolates the fresh agent
/// session on its own branch instead of mixing its commit into that work on `main`.
#[test]
fn claiming_dirty_files_isolates_new_session_on_its_own_branch() {
    let repo = Repo::new(&[
        ("README.md", "# Project\n", false),
        ("src/lib.rs", "pub fn lib() {}\n", false),
    ]);
    let main_before = repo.git(&["rev-parse", "main"]);

    repo.write("README.md", "# Project\n\nUser draft.\n");
    let claim: serde_json::Value = serde_json::from_str(&repo.tes(
        "isolated",
        &["--json", "claim", "--intent", "Polish readme", "README.md"],
    ))
    .expect("claim json");
    let branch = claim["target_branch"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        branch.starts_with("agent/"),
        "expected auto-isolated branch, got {claim}"
    );

    repo.tes("plain", &["claim", "--intent", "Edit lib", "src/lib.rs"]);
    let plain = repo.status_json("plain");
    assert_eq!(
        plain["active_branch"], "main",
        "non-overlapping claims must stay on main"
    );

    assert_eq!(repo.git(&["rev-parse", "main"]), main_before);
    assert_eq!(repo.read("README.md"), "# Project\n\nUser draft.\n");
}

/// `tes commit` with nothing to commit must not write an empty commit (matching `git commit`),
/// while `--amend` with an unchanged tree still rewords the tip and a real edit still commits.
#[test]
fn commit_with_nothing_to_commit_writes_no_empty_commit() {
    let repo = Repo::new(&[("app.rs", "pub fn app() {}\n", false)]);
    let head_before = repo.git(&["rev-parse", "HEAD"]);

    repo.tes("idle", &["claim", "--intent", "Nothing yet"]);
    let out = repo.tes("idle", &["commit", "-m", "chore: empty"]);
    assert!(
        out.contains("Nothing to commit"),
        "unexpected output: {out}"
    );
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"]),
        head_before,
        "empty commit was written"
    );

    let report: serde_json::Value =
        serde_json::from_str(&repo.tes("idle", &["--json", "commit", "-m", "chore: empty again"]))
            .expect("commit json");
    assert_eq!(report["created_commit"], false, "{report}");
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), head_before);
    repo.assert_git_clean("after a no-op commit");

    repo.tes(
        "idle",
        &["commit", "--amend", "-m", "chore: reworded initial commit"],
    );
    assert_ne!(
        repo.git(&["rev-parse", "HEAD"]),
        head_before,
        "--amend must still reword the tip"
    );
    assert_eq!(
        repo.git(&["log", "-1", "--format=%s"]).trim(),
        "chore: reworded initial commit"
    );
    let amended = repo.git(&["rev-parse", "HEAD"]);
    repo.assert_git_clean("after a reword-only amend");

    repo.write("app.rs", "pub fn app() -> u8 { 1 }\n");
    repo.hook("idle", "app.rs");
    repo.tes("idle", &["commit", "-m", "feat: app returns 1"]);
    assert_eq!(repo.git(&["rev-parse", "HEAD~1"]), amended);
    assert_eq!(repo.head_content("app.rs"), "pub fn app() -> u8 { 1 }\n");
    repo.assert_git_clean("after a real commit");
}

/// A stale same-branch peer session that edited a file (new or existing, in the same function or a
/// different function) and then went idle must never cause subsequent sequential `post-tool` hooks
/// by an active session to revert the active session's earlier edits.
#[test]
fn stale_peer_never_reverts_subsequent_hooked_edits_on_new_or_existing_file() {
    let initial = concat!(
        "pub fn a() -> u8 {\n",
        "    1\n",
        "}\n\n",
        "pub fn b() -> u8 {\n",
        "    let first = 1;\n",
        "    let pad1 = 0;\n",
        "    let pad2 = 0;\n",
        "    let pad3 = 0;\n",
        "    let pad4 = 0;\n",
        "    let last = 1;\n",
        "    first + last + pad1 + pad2 + pad3 + pad4\n",
        "}\n",
    );

    for is_existing in [false, true] {
        for ghost_same_fn in [false, true] {
            for live_adopt_hook in [false, true] {
                let seed: &[(&str, &str, bool)] = if is_existing {
                    &[("seed.txt", "seed\n", false), ("f.rs", initial, false)]
                } else {
                    &[("seed.txt", "seed\n", false)]
                };
                let repo = Repo::new(seed);
                if !is_existing {
                    repo.write("f.rs", initial);
                }

                repo.tes(
                    "ghost",
                    &["claim", "--branch", "main", "--intent", "ghost work", "f.rs"],
                );
                let mut text = initial.replace("pub fn a() -> u8 {\n    1", "pub fn a() -> u8 {\n    10");
                if ghost_same_fn {
                    text = text.replace("let pad2 = 0;", "let pad2 = 7;");
                }
                repo.write("f.rs", &text);
                repo.hook("ghost", "f.rs");

                repo.tes(
                    "live",
                    &["claim", "--branch", "main", "--intent", "live work", "f.rs"],
                );
                if live_adopt_hook {
                    repo.hook("live", "f.rs");
                }

                let after_first = repo.read("f.rs").replace("let first = 1;", "let first = 20;");
                repo.write("f.rs", &after_first);
                repo.hook("live", "f.rs");

                let after_second = repo.read("f.rs").replace("let last = 1;", "let last = 30;");
                repo.write("f.rs", &after_second);
                repo.hook("live", "f.rs");

                let final_disk = repo.read("f.rs");
                let ctx = format!(
                    "is_existing={is_existing} ghost_same_fn={ghost_same_fn} live_adopt_hook={live_adopt_hook}\n{final_disk}"
                );
                assert!(final_disk.contains("    10\n"), "lost ghost edit: {ctx}");
                assert!(
                    final_disk.contains("let first = 20;"),
                    "stale peer reverted first hooked edit: {ctx}"
                );
                assert!(
                    final_disk.contains("let last = 30;"),
                    "lost second hooked edit: {ctx}"
                );
                if ghost_same_fn {
                    assert!(
                        final_disk.contains("let pad2 = 7;"),
                        "lost ghost same-fn edit: {ctx}"
                    );
                }
                assert!(!final_disk.contains("<<<<<<<"), "conflict markers: {ctx}");
            }
        }
    }
}

/// An off-HEAD branch session (`tes claim --branch feat/off-head`) executing deletions, moves,
/// `chmod +x`, and `tes exec -b` inside its branch sandbox must keep `repo_root` untouched on
/// `main` and preserve all deletions, moves, and executable modes when committed and merged.
#[test]
fn off_head_branch_session_deletions_moves_chmod_and_exec_merge_cleanly() {
    let repo = Repo::new(&[
        ("keep.txt", "keep\n", false),
        ("obsolete.txt", "remove me\n", false),
        ("legacy/mod.rs", "pub fn legacy() -> u8 { 1 }\n", false),
        ("scripts/deploy.sh", "#!/bin/sh\necho v1\n", false),
    ]);

    repo.tes(
        "offhead",
        &[
            "claim",
            "--branch",
            "feat/off-head",
            "--intent",
            "Off-head sandbox refactor",
            "obsolete.txt",
            "legacy",
            "modern",
            "scripts/deploy.sh",
        ],
    );

    repo.tes(
        "offhead",
        &[
            "exec",
            "-b",
            "feat/off-head",
            "--",
            "sh",
            "-c",
            "rm obsolete.txt && mkdir -p modern && mv legacy/mod.rs modern/mod.rs && rmdir legacy && printf '#!/bin/sh\\necho v2\\n' > scripts/deploy.sh && chmod +x scripts/deploy.sh",
        ],
    );

    assert!(
        repo.path("obsolete.txt").is_file(),
        "off-HEAD sandbox exec must not delete files from repo_root on main"
    );
    assert!(
        repo.path("legacy/mod.rs").is_file(),
        "off-HEAD sandbox exec must not move files in repo_root on main"
    );
    repo.assert_git_clean("repo_root on main while feat/off-head has sandbox edits");

    repo.tes(
        "offhead",
        &["commit", "-b", "feat/off-head", "-m", "feat: off-head refactor"],
    );
    repo.assert_git_clean("after committing feat/off-head");

    repo.tes(
        "offhead",
        &[
            "merge",
            "feat/off-head",
            "--into",
            "main",
            "-m",
            "merge: feat/off-head into main",
        ],
    );
    repo.assert_git_clean("after merging feat/off-head into main");

    let head = repo.head_tree();
    assert!(!head.contains_key("obsolete.txt"), "{head:?}");
    assert!(!head.contains_key("legacy/mod.rs"), "{head:?}");
    assert!(head.contains_key("modern/mod.rs"), "{head:?}");
    #[cfg(unix)]
    {
        assert_eq!(head["scripts/deploy.sh"].0, "100755", "{head:?}");
        assert!(repo.is_executable("scripts/deploy.sh"));
    }
}

/// `switch --root`, `cherry-pick`, `pull`, `resolve`, and `stash apply` must maintain `.git/index`
/// and `git status --porcelain` parity, including executable file permissions.
#[test]
fn switch_cherry_pick_pull_resolve_and_stash_apply_preserve_git_status_parity() {
    let repo = Repo::new(&[
        ("run.sh", "#!/bin/sh\necho v1\n", true),
        ("notes.txt", "line 1\n", false),
    ]);

    repo.tes(
        "picker",
        &["claim", "--branch", "feat/pick", "--intent", "Add picked.txt", "picked.txt"],
    );
    repo.tes(
        "picker",
        &[
            "exec",
            "-b",
            "feat/pick",
            "--",
            "sh",
            "-c",
            "printf 'picked content\\n' > picked.txt",
        ],
    );
    repo.tes(
        "picker",
        &["commit", "-b", "feat/pick", "-m", "feat: add picked.txt"],
    );
    let pick_sha = repo.git(&["rev-parse", "feat/pick"]);

    repo.tes("picker", &["switch", "--editor", "feat/pick"]);
    repo.assert_git_clean("after switch --editor to feat/pick");
    assert!(repo.path("picked.txt").is_file());

    repo.tes("picker", &["switch", "--editor", "main"]);
    repo.assert_git_clean("after switch --editor back to main");
    assert!(!repo.path("picked.txt").exists());

    repo.tes("picker", &["cherry-pick", &pick_sha]);
    repo.assert_git_clean("after cherry-pick onto main");
    assert_eq!(repo.read("picked.txt"), "picked content\n");

    let remote_tmp = tempfile::tempdir().expect("remote tempdir");
    let bare_path = remote_tmp.path().join("origin.git");
    let clone_path = remote_tmp.path().join("clone");
    repo.git(&[
        "init",
        "--bare",
        "-b",
        "main",
        "-q",
        bare_path.to_str().expect("bare path"),
    ]);
    repo.git(&["remote", "add", "origin", bare_path.to_str().expect("bare path")]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    repo.git(&[
        "clone",
        "-q",
        "-b",
        "main",
        bare_path.to_str().expect("bare path"),
        clone_path.to_str().expect("clone path"),
    ]);
    fs::write(clone_path.join("upstream.txt"), "from origin\n").expect("write upstream.txt");
    let run_clone_git = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.current_dir(&clone_path).args(args);
        for key in ISOLATED_ENV {
            cmd.env_remove(key);
        }
        let out = cmd
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("clone git");
        assert!(out.status.success(), "{:?}", String::from_utf8_lossy(&out.stderr));
    };
    run_clone_git(&["add", "upstream.txt"]);
    run_clone_git(&["commit", "-q", "-m", "feat: upstream file"]);
    run_clone_git(&["push", "-q", "origin", "main"]);

    repo.tes("picker", &["pull", "origin", "main"]);
    repo.assert_git_clean("after tes pull origin main");
    assert_eq!(repo.read("upstream.txt"), "from origin\n");

    repo.write("notes.txt", "line 1\nstashed line\n");
    repo.tes("picker", &["stash", "push", "-m", "stash notes"]);
    repo.assert_git_clean("after stash push");
    repo.tes("picker", &["stash", "apply"]);
    assert_eq!(
        repo.tes_status_paths("picker"),
        repo.git_status_paths(),
        "stash apply must keep tes status and git status in sync"
    );
    let stashes: serde_json::Value =
        serde_json::from_str(&repo.tes("picker", &["--json", "stash", "list"]))
            .expect("stash list json");
    assert_eq!(
        stashes.as_array().map(Vec::len),
        Some(1),
        "stash apply must retain stash entry"
    );
    repo.tes("picker", &["commit", "-m", "docs: apply stashed notes"]);
    repo.assert_git_clean("after committing applied stash");

    repo.tes(
        "left",
        &["claim", "--branch", "main", "--intent", "Left run.sh", "run.sh"],
    );
    repo.tes(
        "right",
        &["claim", "--branch", "main", "--intent", "Right run.sh", "run.sh"],
    );
    repo.write("run.sh", "#!/bin/sh\necho left\n");
    repo.hook("left", "run.sh");
    repo.write("run.sh", "#!/bin/sh\necho right\n");
    repo.hook("right", "run.sh");

    repo.tes("right", &["resolve", "run.sh", "--strategy", "theirs"]);
    #[cfg(unix)]
    assert!(
        repo.is_executable("run.sh"),
        "tes resolve must preserve executable permissions on run.sh"
    );
    repo.tes("right", &["commit", "-m", "fix: resolve run.sh"]);
    repo.assert_git_clean("after resolving and committing run.sh");
    #[cfg(unix)]
    assert_eq!(repo.head_tree()["run.sh"].0, "100755");
}

/// Untracked files matched by a nested `<subdir>/.gitignore` (such as `docs/.gitignore` ignoring
/// `.vercel`) must be excluded from `snapshot_working_dir` and `tes status`, matching `git status`.
#[test]
fn nested_gitignore_files_excluded_from_status_and_snapshots() {
    let repo = Repo::new(&[
        ("docs/.gitignore", ".vercel\nbuild-out/\n", false),
        ("docs/index.md", "# Docs\n", false),
    ]);

    repo.write("docs/.vercel/project.json", "{\"projectId\":\"prj_123\"}\n");
    repo.write("docs/.vercel/README.txt", "Vercel internal\n");
    repo.write("docs/build-out/bundle.js", "console.log(1);\n");

    repo.assert_git_clean("git status ignores nested .gitignore matches");
    assert!(
        repo.tes_status_paths("viewer").is_empty(),
        "tes status must ignore nested .gitignore matches: {:?}",
        repo.status_json("viewer")
    );
}

/// Concurrent terminals (`tes env`) and `tes exec` must allocate distinct, low reused slots
/// (`3100`, `3200`, ...), keep polyglot port isolation off by default, and when opted in via
/// `tes config set port_isolation true`, enforce polyglot port isolation across Node, Python, PHP,
/// Go, Rust, Ruby, Java, and .NET environment variables and CLI wrapper shims.
#[test]
fn slot_reuse_and_polyglot_port_isolation_across_shells_and_dev_servers() {
    let repo = Repo::new(&[("README.md", "# Polyglot Port Test\n", false)]);

    let env1_default_json: serde_json::Value =
        serde_json::from_str(&repo.tes("term-1001", &["--json", "env"])).expect("env1 default json");
    let env2_default_json: serde_json::Value =
        serde_json::from_str(&repo.tes("term-1002", &["--json", "env"])).expect("env2 default json");

    assert_eq!(env1_default_json["slot"], 1, "{env1_default_json}");
    assert_eq!(env1_default_json["port"], 3100, "{env1_default_json}");
    assert_eq!(
        env1_default_json["port_isolation"], false,
        "port_isolation must be false by default: {env1_default_json}"
    );
    assert!(
        env1_default_json["shim_bin"].is_null(),
        "shim_bin must be null when port_isolation is false: {env1_default_json}"
    );
    assert_eq!(env2_default_json["slot"], 2, "{env2_default_json}");
    assert_eq!(env2_default_json["port"], 3200, "{env2_default_json}");

    let default_shell = repo.tes("term-1001", &["env"]);
    assert!(
        default_shell.contains("export TES_PORT='3100'"),
        "TES_PORT must still be exported as informational slot port:\n{default_shell}"
    );
    assert!(
        !default_shell.contains("export PORT="),
        "PORT must not be exported when port_isolation is off:\n{default_shell}"
    );
    assert!(
        !default_shell.contains("export VITE_PORT="),
        "VITE_PORT must not be exported when port_isolation is off:\n{default_shell}"
    );
    assert!(
        !default_shell.contains("export TES_PORT_ISOLATION="),
        "TES_PORT_ISOLATION must not be exported when port_isolation is off:\n{default_shell}"
    );
    assert!(
        default_shell.contains("unset TES_PORT_ISOLATION TES_SHIM_BIN PORT"),
        "default tes env must unset port overrides:\n{default_shell}"
    );

    repo.tes("term-1001", &["config", "set", "port_isolation", "true"]);

    let env1_json: serde_json::Value =
        serde_json::from_str(&repo.tes("term-1001", &["--json", "env"])).expect("env1 json");
    assert_eq!(env1_json["port_isolation"], true, "{env1_json}");

    let env1_shell = repo.tes("term-1001", &["env"]);

    for expected in [
        "export TES_PORT_ISOLATION='1'",
        "export PORT='3100'",
        "export TES_PORT='3100'",
        "export BUN_PORT='3100'",
        "export SERVER_PORT='3100'",
        "export FLASK_RUN_PORT='3100'",
        "export UVICORN_PORT='3100'",
        "export DJANGO_RUNSERVER_PORT='3100'",
        "export ROCKET_PORT='3100'",
        "export NITRO_PORT='3100'",
        "export VITE_PORT='3100'",
        "export SB_PORT='3106'",
        "export STORYBOOK_PORT='3106'",
        "export TES_DOCS_PORT='3107'",
        "export TES_STUDIO_PORT='3108'",
        "export RCT_METRO_PORT='3109'",
        "export ASPNETCORE_HTTP_PORTS='3100'",
        "export ASPNETCORE_URLS='http://localhost:3100'",
        "export QUARKUS_HTTP_PORT='3100'",
        "export KTOR_DEPLOYMENT_PORT='3100'",
        "export PHX_PORT='3100'",
    ] {
        assert!(
            env1_shell.contains(expected),
            "missing {expected} in:\n{env1_shell}"
        );
    }

    #[cfg(unix)]
    {
        let fake_bin = repo.root.join("fake-bin");
        fs::create_dir_all(&fake_bin).expect("create fake-bin");

        for tool in [
            "node", "bun", "deno", "npx", "python3", "uv", "mkdocs", "gunicorn", "php", "ruby",
            "bundle", "hugo", "caddy", "dotnet", "mvn", "gradle", "sbt", "flutter",
        ] {
            let rel = format!("fake-bin/{tool}");
            repo.write(&rel, "#!/bin/sh\nprintf '%s' \"$*\"\n");
            repo.chmod(&rel, true);
        }

        let shim_dir = PathBuf::from(env1_json["shim_bin"].as_str().expect("shim_bin"));
        let orig_path = std::env::var("PATH").unwrap_or_default();
        let combined_path = format!("{}:{}:{orig_path}", shim_dir.display(), fake_bin.display());

        let mut env_cmd = Command::new(Path::new(env!("CARGO_BIN_EXE_tes")));
        env_cmd
            .current_dir(&repo.root)
            .env("TES_SESSION_ID", "term-1001")
            .env("PATH", &combined_path)
            .args(["env"]);
        let out = env_cmd.output().expect("tes env with fake-bin");
        assert!(out.status.success());

        let out_disabled = Command::new(shim_dir.join("node"))
            .current_dir(&repo.root)
            .env("PATH", &combined_path)
            .env("PORT", "3100")
            .env("TES_PORT", "3100")
            .env_remove("TES_PORT_ISOLATION")
            .args(["node_modules/.bin/vite"])
            .output()
            .expect("run shimmed binary without TES_PORT_ISOLATION");
        assert!(out_disabled.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out_disabled.stdout),
            "node_modules/.bin/vite",
            "shim must pass arguments untouched when TES_PORT_ISOLATION is not enabled"
        );

        let run_shimmed = |port: &str, prog: &str, args: &[&str]| -> String {
            let out = Command::new(shim_dir.join(prog))
                .current_dir(&repo.root)
                .env("PATH", &combined_path)
                .env("PORT", port)
                .env("TES_PORT", port)
                .env("TES_PORT_ISOLATION", "1")
                .args(args)
                .output()
                .expect("run shimmed binary");
            assert!(out.status.success());
            String::from_utf8_lossy(&out.stdout).to_string()
        };

        assert_eq!(
            run_shimmed("3100", "node", &["node_modules/.bin/vitepress", "dev", "docs"]),
            "node_modules/.bin/vitepress dev docs --port 3100"
        );
        assert_eq!(
            run_shimmed("3200", "node", &["node_modules/.bin/vite"]),
            "node_modules/.bin/vite --port 3200"
        );
        assert_eq!(
            run_shimmed("3100", "node", &["node_modules/next/dist/bin/next", "dev"]),
            "node_modules/next/dist/bin/next dev -p 3100"
        );
        assert_eq!(
            run_shimmed("3100", "node", &["node_modules/nuxi/bin/nuxi.mjs", "dev"]),
            "node_modules/nuxi/bin/nuxi.mjs dev --port 3100"
        );
        assert_eq!(
            run_shimmed("3100", "node", &["node_modules/@remix-run/dev/dist/cli.js", "vite:dev"]),
            "node_modules/@remix-run/dev/dist/cli.js vite:dev --port 3100"
        );
        assert_eq!(
            run_shimmed("3100", "node", &["node_modules/storybook/index.js", "dev"]),
            "node_modules/storybook/index.js dev -p 3106"
        );
        assert_eq!(
            run_shimmed("3100", "node", &["node_modules/prisma/build/index.js", "studio"]),
            "node_modules/prisma/build/index.js studio --port 3108"
        );
        assert_eq!(
            run_shimmed("3100", "bun", &["x", "astro", "dev"]),
            "x astro dev --port 3100"
        );
        assert_eq!(
            run_shimmed("3100", "npx", &["vite"]),
            "vite --port 3100"
        );
        assert_eq!(
            run_shimmed("3100", "deno", &["serve", "main.ts"]),
            "serve main.ts --port 3100"
        );
        assert_eq!(
            run_shimmed("3100", "python3", &["manage.py", "runserver"]),
            "manage.py runserver 3100"
        );
        assert_eq!(
            run_shimmed("3100", "python3", &["-m", "http.server"]),
            "-m http.server 3100"
        );
        assert_eq!(
            run_shimmed("3100", "uv", &["run", "uvicorn", "app:app"]),
            "run uvicorn app:app --port 3100"
        );
        assert_eq!(
            run_shimmed("3100", "uv", &["run", "python", "manage.py", "runserver"]),
            "run python manage.py runserver 3100"
        );
        assert_eq!(
            run_shimmed("3100", "mkdocs", &["serve"]),
            "serve --dev-addr 127.0.0.1:3100"
        );
        assert_eq!(
            run_shimmed("3100", "gunicorn", &["wsgi:app"]),
            "wsgi:app --bind 127.0.0.1:3100"
        );
        assert_eq!(
            run_shimmed("3100", "php", &["artisan", "serve"]),
            "artisan serve --port=3100"
        );
        assert_eq!(
            run_shimmed("3100", "php", &["artisan", "octane:start"]),
            "artisan octane:start --port=3100"
        );
        assert_eq!(
            run_shimmed("3100", "php", &["-S", "127.0.0.1:8000", "-t", "public"]),
            "-S 127.0.0.1:3100 -t public"
        );
        assert_eq!(
            run_shimmed("3100", "ruby", &["-run", "-e", "httpd", "."]),
            "-run -e httpd . -p 3100"
        );
        assert_eq!(
            run_shimmed("3100", "bundle", &["exec", "rails", "s"]),
            "exec rails s -p 3100"
        );
        assert_eq!(
            run_shimmed("3100", "hugo", &["server"]),
            "server -p 3100"
        );
        assert_eq!(
            run_shimmed("3100", "caddy", &["file-server"]),
            "file-server --listen :3100"
        );
        assert_eq!(
            run_shimmed("3100", "dotnet", &["run"]),
            "run --urls http://localhost:3100"
        );
        assert_eq!(
            run_shimmed("3100", "mvn", &["spring-boot:run"]),
            "spring-boot:run -Dspring-boot.run.arguments=--server.port=3100 -Dserver.port=3100"
        );
        assert_eq!(
            run_shimmed("3100", "gradle", &["bootRun"]),
            "bootRun -Dserver.port=3100 -Dquarkus.http.port=3100"
        );
        assert_eq!(
            run_shimmed("3100", "sbt", &["run"]),
            "-Dhttp.port=3100 run"
        );
        assert_eq!(
            run_shimmed("3100", "flutter", &["run", "-d", "web-server"]),
            "run -d web-server --web-port=3109"
        );

        let lease_dir = shim_dir.join(".leases").join("3100");
        fs::create_dir_all(&lease_dir).expect("create lease 3100");
        fs::write(lease_dir.join("pid"), format!("{}\n", std::process::id()))
            .expect("write live pid");
        assert_eq!(
            run_shimmed("3100", "node", &["node_modules/.bin/vite"]),
            "node_modules/.bin/vite --port 3101",
            "second concurrent server in the same session must step to 3101 when 3100 is leased"
        );
        let _ = fs::remove_file(lease_dir.join("pid"));
    }
}
