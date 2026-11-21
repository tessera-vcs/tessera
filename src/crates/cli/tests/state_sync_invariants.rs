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
    assert_eq!(
        before_stash.len(),
        if cfg!(unix) { 3 } else { 2 },
        "{before_stash:?}"
    );

    repo.tes("cycler", &["stash", "push", "-m", "wip"]);
    assert!(
        repo.path("old.rs").is_file(),
        "stash push did not restore the deleted file"
    );
    #[cfg(unix)]
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
                    &[
                        "claim",
                        "--branch",
                        "main",
                        "--intent",
                        "ghost work",
                        "f.rs",
                    ],
                );
                let mut text =
                    initial.replace("pub fn a() -> u8 {\n    1", "pub fn a() -> u8 {\n    10");
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

                let after_first = repo
                    .read("f.rs")
                    .replace("let first = 1;", "let first = 20;");
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
        &[
            "commit",
            "-b",
            "feat/off-head",
            "-m",
            "feat: off-head refactor",
        ],
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
        &[
            "claim",
            "--branch",
            "feat/pick",
            "--intent",
            "Add picked.txt",
            "picked.txt",
        ],
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
    repo.git(&[
        "remote",
        "add",
        "origin",
        bare_path.to_str().expect("bare path"),
    ]);
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
        assert!(
            out.status.success(),
            "{:?}",
            String::from_utf8_lossy(&out.stderr)
        );
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
        &[
            "claim",
            "--branch",
            "main",
            "--intent",
            "Left run.sh",
            "run.sh",
        ],
    );
    repo.tes(
        "right",
        &[
            "claim",
            "--branch",
            "main",
            "--intent",
            "Right run.sh",
            "run.sh",
        ],
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
        serde_json::from_str(&repo.tes("term-1001", &["--json", "env"]))
            .expect("env1 default json");
    let env2_default_json: serde_json::Value =
        serde_json::from_str(&repo.tes("term-1002", &["--json", "env"]))
            .expect("env2 default json");

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
                .env("TES_SKIP_TCP_PROBE", "1")
                .args(args)
                .output()
                .expect("run shimmed binary");
            assert!(out.status.success());
            String::from_utf8_lossy(&out.stdout).to_string()
        };

        assert_eq!(
            run_shimmed(
                "3100",
                "node",
                &["node_modules/.bin/vitepress", "dev", "docs"]
            ),
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
            run_shimmed(
                "3100",
                "node",
                &["node_modules/@remix-run/dev/dist/cli.js", "vite:dev"]
            ),
            "node_modules/@remix-run/dev/dist/cli.js vite:dev --port 3100"
        );
        assert_eq!(
            run_shimmed("3100", "node", &["node_modules/storybook/index.js", "dev"]),
            "node_modules/storybook/index.js dev -p 3106"
        );
        assert_eq!(
            run_shimmed(
                "3100",
                "node",
                &["node_modules/prisma/build/index.js", "studio"]
            ),
            "node_modules/prisma/build/index.js studio --port 3108"
        );
        assert_eq!(
            run_shimmed("3100", "bun", &["x", "astro", "dev"]),
            "x astro dev --port 3100"
        );
        assert_eq!(run_shimmed("3100", "npx", &["vite"]), "vite --port 3100");
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
        assert_eq!(run_shimmed("3100", "hugo", &["server"]), "server -p 3100");
        assert_eq!(
            run_shimmed("3100", "caddy", &["file-server"]),
            "file-server --listen :3100"
        );
        assert_eq!(
            run_shimmed("3100", "dotnet", &["run"]),
            "run",
            "dotnet run relies on ASPNETCORE_URLS instead of injecting --urls so console apps never break"
        );
        assert_eq!(
            run_shimmed("3100", "mvn", &["spring-boot:run"]),
            "spring-boot:run -Dspring-boot.run.arguments=--server.port=3100 -Dserver.port=3100"
        );
        assert_eq!(
            run_shimmed("3100", "gradle", &["bootRun"]),
            "bootRun -Dserver.port=3100 -Dquarkus.http.port=3100"
        );
        assert_eq!(run_shimmed("3100", "sbt", &["run"]), "-Dhttp.port=3100 run");
        assert_eq!(
            run_shimmed("3100", "flutter", &["run", "-d", "web-server"]),
            "run -d web-server --web-port=3109"
        );

        let lease_dir = shim_dir.join(".leases").join("3100");
        fs::create_dir_all(&lease_dir).expect("create lease 3100");
        let mut peer_sleeper = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn peer lease sleeper");
        fs::write(lease_dir.join("pid"), format!("{}\n", peer_sleeper.id()))
            .expect("write distinct live peer pid");
        let stepped = run_shimmed("3100", "node", &["node_modules/.bin/vite"]);
        let _ = peer_sleeper.kill();
        let _ = peer_sleeper.wait();
        let _ = fs::remove_file(lease_dir.join("pid"));
        assert_eq!(
            stepped, "node_modules/.bin/vite --port 3101",
            "second concurrent server in the same session must step to 3101 when 3100 is leased by a distinct live peer PID"
        );
    }
}

#[test]
fn cross_branch_watcher_restored_baseline_preserves_both_sessions_and_multiline_zsh_paste_hops() {
    let base_app = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\n</script>\n\n<template>\n  <HelloWorld />\n</template>\n";
    let repo = Repo::new(&[("src/App.vue", base_app, false)]);

    repo.tes(
        "habit",
        &[
            "claim",
            "--branch",
            "feat/habit-tracker",
            "--intent",
            "Add HabitTracker component with daily habits and streak counter",
            "src/App.vue",
        ],
    );

    repo.tes(
        "pomodoro",
        &[
            "claim",
            "--branch",
            "feat/pomodoro-timer",
            "--intent",
            "Build PomodoroTimer component with focus and break countdown",
            "src/App.vue",
        ],
    );

    repo.tes("habit", &["hook", "pre-tool", "--path", "src/App.vue"]);
    repo.tes("pomodoro", &["hook", "pre-tool", "--path", "src/App.vue"]);

    let habit_app = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\nimport HabitTracker from './components/HabitTracker.vue'\n</script>\n\n<template>\n  <HelloWorld />\n  <HabitTracker />\n</template>\n";
    let pomodoro_app = "<script setup lang=\"ts\">\nimport HelloWorld from './components/HelloWorld.vue'\nimport PomodoroTimer from './components/PomodoroTimer.vue'\n</script>\n\n<template>\n  <HelloWorld />\n  <PomodoroTimer />\n</template>\n";

    {
        let coord = agent::Coordinator::open(&repo.root).expect("open coordinator");
        let base_bid = coord
            .git()
            .write_blob(base_app.as_bytes())
            .expect("write base blob");
        let habit_bid = coord
            .git()
            .write_blob(habit_app.as_bytes())
            .expect("write habit blob");
        let pomo_bid = coord
            .git()
            .write_blob(pomodoro_app.as_bytes())
            .expect("write pomodoro blob");

        coord
            .oplog()
            .record_tx(
                "watcher",
                "observe intermediate blobs for src/App.vue",
                true,
                |view| {
                    view.observed_blobs.insert(
                        "src/App.vue".to_string(),
                        vec![base_bid, habit_bid, pomo_bid],
                    );
                    Ok(())
                },
            )
            .expect("record observed blobs");
    }

    repo.write("src/App.vue", base_app);

    repo.hook("pomodoro", "src/App.vue");
    repo.hook("habit", "src/App.vue");

    repo.tes("pomodoro", &["commit", "-m", "feat: add pomodoro timer"]);
    repo.tes("habit", &["commit", "-m", "feat: add habit tracker"]);

    #[cfg(unix)]
    if Path::new("/bin/zsh").is_file() {
        let bin = Path::new(env!("CARGO_BIN_EXE_tes"));
        let bin_dir = bin.parent().expect("tes bin parent").display().to_string();
        let zsh_script = r#"
            eval "$("$TES_BIN" env --zsh)"
            _tes_preexec $'tes switch feat/habit-tracker\nnode -v'
            tes switch feat/habit-tracker >/dev/null
            echo "AFTER_MULTILINE_DEV=$PWD"
            _tes_precmd
            echo "AFTER_PRECMD=$PWD"
            _tes_preexec $'tes switch feat/pomodoro-timer\nagy'
            tes switch feat/pomodoro-timer >/dev/null
            echo "AFTER_MULTILINE_AGY=$PWD"
        "#;

        let out = Command::new("/bin/zsh")
            .args(["-c", zsh_script])
            .current_dir(&repo.root)
            .env("TES_BIN", bin)
            .env(
                "PATH",
                format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default()),
            )
            .output()
            .expect("run zsh multiline test");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "zsh multiline script failed: stdout={stdout} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("branches/feat-habit-tracker"),
            "multiline `tes switch feat/habit-tracker\\nnode -v` must hop into branch sandbox: {stdout}"
        );

        let canonical_root = fs::canonicalize(&repo.root).unwrap_or_else(|_| repo.root.clone());
        assert!(
            stdout.contains(&format!("AFTER_MULTILINE_AGY={}", repo.root.display()))
                || stdout.contains(&format!("AFTER_MULTILINE_AGY={}", canonical_root.display())),
            "multiline `tes switch feat/pomodoro-timer\\nagy` must keep PWD at repo_root: {stdout}"
        );
    }
}

/// Verifies `tes init --local` (plus `-C` relative path resolution), `tes doctor --json`,
/// `tes merge --dry-run`, `--verify` error handling, and non-zero exit codes on verification failures.
#[test]
fn doctor_init_local_and_merge_dry_run_e2e() {
    let parent_tmp = tempfile::tempdir().expect("parent tempdir");
    let rel_repo = parent_tmp.path().join("nested-local-repo");
    fs::create_dir_all(&rel_repo).expect("mkdir nested-local-repo");
    fs::write(rel_repo.join("lib.rs"), "pub fn base() -> i32 { 1 }\n").expect("write lib.rs");

    let tes_bin = Path::new(env!("CARGO_BIN_EXE_tes"));
    let init_local = Command::new(tes_bin)
        .args([
            "-C",
            parent_tmp.path().to_str().expect("parent path"),
            "init",
            "nested-local-repo",
            "--local",
            "--harness",
            "antigravity",
            "--json",
        ])
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("tes -C parent init nested-local-repo --local");
    assert!(
        init_local.status.success(),
        "tes init --local failed: {}",
        String::from_utf8_lossy(&init_local.stderr)
    );

    let exclude_content =
        fs::read_to_string(rel_repo.join(".git/info/exclude")).expect("read .git/info/exclude");
    assert!(
        exclude_content.contains(".agents/rules/tessera.md"),
        "expected harness file in .git/info/exclude, got:\n{exclude_content}"
    );

    let git_status_local = Command::new("git")
        .current_dir(&rel_repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["status", "--porcelain"])
        .output()
        .expect("git status local");
    assert!(
        git_status_local.status.success() && git_status_local.stdout.is_empty(),
        "expected clean git status after tes init --local, got: {}",
        String::from_utf8_lossy(&git_status_local.stdout)
    );

    let repo = Repo::new(&[("lib.rs", "pub fn base() -> i32 { 1 }\n", false)]);

    fs::write(
        repo.path("tessera.toml"),
        "verify_command = \"echo untrusted\"\n",
    )
    .expect("write untrusted tessera.toml");

    let doc_out = repo.tes("term-doc", &["doctor", "--json"]);
    let doc_json: serde_json::Value = serde_json::from_str(&doc_out).expect("doctor json");
    assert_eq!(doc_json["ok"], true, "doctor report: {doc_json}");
    let checks = doc_json["checks"].as_array().expect("checks array");
    assert_eq!(checks.len(), 7, "expected 7 doctor checks: {doc_json}");

    let cfg_check = checks
        .iter()
        .find(|c| c["id"] == "repo_config")
        .expect("repo_config check");
    assert_eq!(
        cfg_check["status"], "warn",
        "untrusted tessera.toml with verify_command should warn: {cfg_check}"
    );

    repo.tes(
        "dry-worker",
        &[
            "claim",
            "--branch",
            "feat/dry",
            "--intent",
            "Add dry fn",
            "lib.rs",
        ],
    );
    repo.tes(
        "dry-worker",
        &[
            "exec",
            "-b",
            "feat/dry",
            "--",
            "sh",
            "-c",
            "printf 'pub fn base() -> i32 { 1 }\\npub fn dry() -> i32 { 2 }\\n' > lib.rs",
        ],
    );
    repo.tes(
        "dry-worker",
        &["commit", "-b", "feat/dry", "-m", "feat: dry"],
    );

    let head_before = repo.git(&["rev-parse", "main"]);
    let oplog_before: serde_json::Value =
        serde_json::from_str(&repo.tes("term-doc", &["oplog", "--json"])).expect("oplog before");

    let dry_out = repo.tes(
        "term-doc",
        &["merge", "feat/dry", "--into", "main", "--dry-run", "--json"],
    );
    let dry_json: serde_json::Value = serde_json::from_str(&dry_out).expect("dry-run json");
    assert_eq!(dry_json["created_commit"], false, "{dry_json}");
    assert_eq!(dry_json["verification_passed"], true, "{dry_json}");
    assert!(
        dry_json["conflicts"].as_array().is_some_and(Vec::is_empty),
        "{dry_json}"
    );
    assert_eq!(
        repo.git(&["rev-parse", "main"]),
        head_before,
        "dry-run must not advance main"
    );
    assert_eq!(
        repo.read("lib.rs"),
        "pub fn base() -> i32 { 1 }\n",
        "dry-run must not modify working tree"
    );
    let oplog_after: serde_json::Value =
        serde_json::from_str(&repo.tes("term-doc", &["oplog", "--json"])).expect("oplog after");
    assert_eq!(
        oplog_before.as_array().map(Vec::len),
        oplog_after.as_array().map(Vec::len),
        "dry-run must not write OpLog entries"
    );

    let missing_verify = repo.run_tes(Some("term-doc"), &["merge", "--verify", "feat/dry"]);
    assert!(
        !missing_verify.status.success(),
        "bare --verify with no verify_command configured must fail explicitly"
    );
    let missing_stderr = String::from_utf8_lossy(&missing_verify.stderr);
    assert!(
        missing_stderr.contains("no verify_command is configured"),
        "expected missing verify_command error (not running 'feat/dry' as command!), got: {missing_stderr}"
    );

    let failed_verify = repo.run_tes(
        Some("term-doc"),
        &["merge", "feat/dry", "--into", "main", "--verify=exit 1"],
    );
    assert!(
        !failed_verify.status.success(),
        "failing --verify must exit non-zero"
    );
    let failed_stdout = String::from_utf8_lossy(&failed_verify.stdout);
    assert!(
        failed_stdout.contains("Verification failed; branch 'main' was not advanced"),
        "expected clear verification failure message, got: {failed_stdout}"
    );
    assert_eq!(
        repo.git(&["rev-parse", "main"]),
        head_before,
        "failed verification must not advance main"
    );
}

/// Verifies `tes push` (single-arg branch, auto-commit with deletions, `--json`, conflict blocking)
/// and divergent `tes pull` (conflict registration, non-zero exit code, caller new-file preservation).
#[test]
fn push_and_divergent_pull_conflicts_and_preservation_e2e() {
    let repo = Repo::new(&[
        ("shared.txt", "line 1\nline 2\n", false),
        ("obsolete.txt", "delete me\n", false),
    ]);

    let remote_tmp = tempfile::tempdir().expect("remote tempdir");
    let bare_path = remote_tmp.path().join("origin.git");
    let clone_path = remote_tmp.path().join("clone");
    let bare_str = bare_path.to_str().expect("bare path");
    let clone_str = clone_path.to_str().expect("clone path");

    repo.git(&["init", "--bare", "-b", "main", "-q", bare_str]);
    repo.git(&["remote", "add", "origin", bare_str]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);

    // Delete a tracked file and verify `tes push main --json` auto-commits the deletion and emits real JSON.
    repo.tes(
        "pusher",
        &["claim", "--intent", "Drop obsolete file", "obsolete.txt"],
    );
    repo.rm("obsolete.txt");
    repo.hook("pusher", "obsolete.txt");

    let push_json_raw = repo.tes("pusher", &["push", "main", "--json"]);
    let push_json: serde_json::Value = serde_json::from_str(&push_json_raw).expect("push json");
    assert_eq!(push_json["ok"], true, "{push_json}");
    assert_eq!(push_json["remote"], "origin", "{push_json}");
    assert_eq!(push_json["branch"], "main", "{push_json}");
    assert!(
        !repo.head_tree().contains_key("obsolete.txt"),
        "push auto-commit must include deletion tombstones"
    );

    // Clone origin and create a divergent upstream commit editing `shared.txt`.
    repo.git(&["clone", "-q", "-b", "main", bare_str, clone_str]);
    fs::write(clone_path.join("shared.txt"), "remote change\nline 2\n")
        .expect("write remote shared");

    let run_clone_git = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.current_dir(&clone_path).args(args);

        for key in ISOLATED_ENV {
            cmd.env_remove(key);
        }

        let out = cmd
            .env("GIT_AUTHOR_NAME", "Upstream")
            .env("GIT_AUTHOR_EMAIL", "up@example.com")
            .env("GIT_COMMITTER_NAME", "Upstream")
            .env("GIT_COMMITTER_EMAIL", "up@example.com")
            .output()
            .expect("clone git");
        assert!(
            out.status.success(),
            "clone git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_clone_git(&["add", "shared.txt"]);
    run_clone_git(&["commit", "-q", "-m", "feat: upstream edit shared.txt"]);
    run_clone_git(&["push", "-q", "origin", "main"]);

    // Make a conflicting local commit on `shared.txt`, plus keep an unrelated new file on disk.
    repo.write("shared.txt", "local change\nline 2\n");
    repo.hook("pusher", "shared.txt");
    repo.tes("pusher", &["commit", "-m", "feat: local edit shared.txt"]);
    repo.write("local_notes.txt", "keep this new file safe\n");

    let pull_out = repo.run_tes(Some("pusher"), &["pull", "main", "--json"]);
    assert!(
        !pull_out.status.success(),
        "divergent pull with conflict must exit non-zero"
    );
    let pull_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&pull_out.stdout)).expect("pull json");
    let conflicts = pull_json["conflicts"].as_array().expect("conflicts array");
    assert!(
        conflicts.iter().any(|c| c["path"] == "shared.txt"),
        "expected shared.txt conflict in PullReport: {pull_json}"
    );
    assert_eq!(
        repo.read("local_notes.txt"),
        "keep this new file safe\n",
        "divergent pull must preserve caller's unrelated new file"
    );

    // Resolve the pull conflict, then create a fresh auto-commit conflict and prove `tes push` aborts.
    repo.tes(
        "pusher",
        &[
            "resolve",
            "shared.txt",
            "--strategy",
            "content",
            "--content",
            "resolved line\nline 2\n",
        ],
    );
    repo.rm("local_notes.txt");
    repo.tes("pusher", &["commit", "-m", "fix: resolve pull conflict"]);

    repo.tes(
        "c1",
        &[
            "claim",
            "--branch",
            "main",
            "--intent",
            "Edit c1",
            "shared.txt",
        ],
    );
    repo.tes(
        "c2",
        &[
            "claim",
            "--branch",
            "main",
            "--intent",
            "Edit c2",
            "shared.txt",
        ],
    );
    repo.write("shared.txt", "c1 line\nline 2\n");
    repo.hook("c1", "shared.txt");
    repo.write("shared.txt", "c2 line\nline 2\n");
    repo.hook("c2", "shared.txt");

    let blocked_push = repo.run_tes(Some("c2"), &["push", "origin", "main", "--json"]);
    assert!(
        !blocked_push.status.success(),
        "tes push must abort with non-zero exit when auto-commit encounters conflicts"
    );
}

/// Verifies `tes diff` (`--staged` empty, `--json`, `HEAD~1`, `A...B`, subdirectory pathspecs),
/// piped `tes status --porcelain` (zero ANSI + `??`), `tes config` trust/untrust isolation,
/// `tes resolve` strict strategy + `--content-file`, and `tes stash` global `-a` & deleted paths.
#[test]
fn diff_status_porcelain_resolve_stash_and_config_trust_e2e() {
    let repo = Repo::new(&[
        ("sub/mod.rs", "pub fn v() -> i32 { 1 }\n", false),
        ("other.txt", "hello\n", false),
        ("dropme.txt", "bye\n", false),
    ]);

    // Unstaged edit only -> `tes diff --staged` must emit NOTHING instead of falling through.
    repo.write("other.txt", "unstaged edit\n");
    let empty_staged = repo.tes("term-diff", &["diff", "--staged"]);
    assert!(
        empty_staged.trim().is_empty(),
        "tes diff --staged must be empty when session has no staged changes, got:\n{empty_staged}"
    );

    // Untracked file + piped `tes status --porcelain` -> zero ANSI escapes and `??` marker.
    repo.write("untracked.txt", "brand new\n");
    let porcelain_out = repo.run_tes(Some("term-diff"), &["status", "--porcelain"]);
    assert!(porcelain_out.status.success());
    let porcelain_str = String::from_utf8_lossy(&porcelain_out.stdout);
    assert!(
        !porcelain_str.contains('\x1b'),
        "piped tes status --porcelain must never emit ANSI codes: {porcelain_str:?}"
    );
    assert!(
        porcelain_str.contains("?? untracked.txt"),
        "expected untracked file as `??` in porcelain output, got:\n{porcelain_str}"
    );
    repo.rm("untracked.txt");
    repo.tes("term-diff", &["restore", "other.txt"]);

    // Commit v2 on `main`, then create branch `feat/three-dot` at v2 and advance `main` to v3.
    repo.write("sub/mod.rs", "pub fn v() -> i32 { 2 }\n");
    repo.hook("term-diff", "sub/mod.rs");
    repo.tes("term-diff", &["commit", "-m", "feat: v2"]);

    repo.tes(
        "brancher",
        &[
            "claim",
            "--branch",
            "feat/three-dot",
            "--intent",
            "Branch edit",
            "sub/mod.rs",
        ],
    );
    repo.tes(
        "brancher",
        &[
            "exec",
            "-b",
            "feat/three-dot",
            "--",
            "sh",
            "-c",
            "printf 'pub fn v() -> i32 { 2 }\\npub fn feat() -> i32 { 9 }\\n' > sub/mod.rs",
        ],
    );
    repo.tes(
        "brancher",
        &["commit", "-b", "feat/three-dot", "-m", "feat: branch fn"],
    );

    repo.write("other.txt", "trunk v3\n");
    repo.hook("term-diff", "other.txt");
    repo.tes("term-diff", &["commit", "-m", "feat: trunk v3"]);

    // `tes diff main...feat/three-dot` must diff merge-base(main, feat/three-dot)..feat/three-dot
    // so it shows `feat()` without showing an inverted diff of `other.txt`!
    let three_dot = repo.tes("term-diff", &["diff", "main...feat/three-dot", "--json"]);
    let three_dot_json: serde_json::Value =
        serde_json::from_str(&three_dot).expect("three-dot diff json");
    let three_dot_diff = three_dot_json["diff"].as_str().unwrap_or_default();
    assert!(
        three_dot_diff.contains("pub fn feat()"),
        "expected feat() in three-dot diff: {three_dot_json}"
    );
    assert!(
        !three_dot_diff.contains("other.txt"),
        "three-dot merge-base diff must not include inverted trunk commit other.txt: {three_dot_json}"
    );

    // Single-revision diff (`tes diff HEAD~1`) + subdirectory `-C sub diff HEAD~1 mod.rs`.
    let single_rev = repo.tes("term-diff", &["diff", "HEAD~1", "--json"]);
    let single_json: serde_json::Value =
        serde_json::from_str(&single_rev).expect("single rev json");
    assert!(
        single_json["diff"]
            .as_str()
            .is_some_and(|d| d.contains("trunk v3")),
        "expected trunk v3 in HEAD~1 diff: {single_json}"
    );

    let sub_diff = Command::new(env!("CARGO_BIN_EXE_tes"))
        .current_dir(repo.path("sub"))
        .args(["diff", "HEAD~2..HEAD", "mod.rs"])
        .env("TES_SESSION_ID", "term-diff")
        .env("TES_NO_PAGER", "1")
        .output()
        .expect("subdir tes diff");
    assert!(sub_diff.status.success());
    assert!(
        String::from_utf8_lossy(&sub_diff.stdout).contains("pub fn v() -> i32 { 2 }"),
        "subdir diff should resolve mod.rs to sub/mod.rs: {}",
        String::from_utf8_lossy(&sub_diff.stdout)
    );

    // `tes config set` while `tessera.toml` is trusted, followed by `tes config untrust`
    // must NOT bake `tessera.toml`'s `verify_command` into `.git/tessera/config.toml`!
    repo.write("tessera.toml", "verify_command = \"echo from-repo-toml\"\n");
    repo.tes("term-diff", &["config", "trust"]);
    let trusted_show: serde_json::Value =
        serde_json::from_str(&repo.tes("term-diff", &["config", "show", "--json"]))
            .expect("trusted config");
    assert_eq!(trusted_show["verify_command"], "echo from-repo-toml");

    repo.tes("term-diff", &["config", "set", "base_port", "4100"]);
    repo.tes("term-diff", &["config", "untrust"]);
    let untrusted_show: serde_json::Value =
        serde_json::from_str(&repo.tes("term-diff", &["config", "show", "--json"]))
            .expect("untrusted config");
    assert_eq!(untrusted_show["base_port"], 4100);
    assert!(
        untrusted_show["verify_command"].is_null(),
        "untrust must revoke tessera.toml verify_command even after tes config set: {untrusted_show}"
    );

    // `tes resolve`: invalid strategy rejected, missing content rejected without truncation, `--content-file` works.
    repo.tes(
        "r1",
        &["claim", "--branch", "main", "--intent", "Left", "other.txt"],
    );
    repo.tes(
        "r2",
        &[
            "claim",
            "--branch",
            "main",
            "--intent",
            "Right",
            "other.txt",
        ],
    );
    repo.write("other.txt", "left version\n");
    repo.hook("r1", "other.txt");
    repo.write("other.txt", "right version\n");
    repo.hook("r2", "other.txt");

    let bad_strat = repo.run_tes(Some("r2"), &["resolve", "other.txt", "--strategy", "bogus"]);
    assert!(
        !bad_strat.status.success(),
        "unknown resolve strategy must fail"
    );

    let missing_content = repo.run_tes(
        Some("r2"),
        &["resolve", "other.txt", "--strategy", "content"],
    );
    assert!(
        !missing_content.status.success(),
        "--strategy content without --content or --content-file must fail"
    );
    assert!(
        !repo.read("other.txt").is_empty(),
        "failed resolve must never truncate file to 0 bytes"
    );

    let cf_path = repo.path("resolved_input.txt");
    fs::write(&cf_path, "resolved from file\n").expect("write content-file");
    repo.tes(
        "r2",
        &[
            "resolve",
            "other.txt",
            "--strategy",
            "content",
            "--content-file",
            cf_path.to_str().expect("cf path"),
        ],
    );
    assert_eq!(repo.read("other.txt"), "resolved from file\n");
    repo.rm("resolved_input.txt");
    repo.tes("r2", &["commit", "-m", "fix: resolve via content-file"]);
    repo.tes("r1", &["agent", "close", "antigravity-r1"]);

    // `tes stash push <deleted-file>` + `tes stash pop 0 -a <agent>`.
    repo.tes(
        "stasher",
        &[
            "claim",
            "--branch",
            "main",
            "--intent",
            "Delete dropme",
            "dropme.txt",
        ],
    );
    repo.rm("dropme.txt");
    repo.hook("stasher", "dropme.txt");
    let stash_out = repo.tes("stasher", &["stash", "push", "dropme.txt"]);
    assert!(
        stash_out.contains("1 file(s)"),
        "stash push of deleted file must treat dropme.txt as path and count deleted_paths: {stash_out}"
    );
    assert!(
        repo.path("dropme.txt").is_file(),
        "stash push must restore dropme.txt"
    );

    repo.tes(
        "term-other",
        &["stash", "pop", "0", "-a", "antigravity-stasher"],
    );
    assert!(
        !repo.path("dropme.txt").exists(),
        "tes stash pop 0 -a <agent> must reapply deleted path"
    );
}

/// Verifies `tes env` protects user `PORT` when `port_isolation=false` and outside repos,
/// strips `TES_SHIM_BIN` from `PATH` when leaving or disabling isolation, never mutates `OpLog`
/// on passive prompts, and emits fixed PowerShell, Nushell, and quote-aware Zsh scripts.
#[test]
fn env_port_preservation_passive_oplog_and_shell_parsing_e2e() {
    let repo = Repo::new(&[("lib.rs", "pub fn ok() {}\n", false)]);

    let oplog_before: serde_json::Value =
        serde_json::from_str(&repo.tes("term-check", &["oplog", "--json"])).expect("oplog before");
    let _ = repo.tes("term-passive-1", &["env"]);
    let _ = repo.tes("term-passive-2", &["env"]);
    let oplog_after: serde_json::Value =
        serde_json::from_str(&repo.tes("term-check", &["oplog", "--json"])).expect("oplog after");
    assert_eq!(
        oplog_before.as_array().map(Vec::len),
        oplog_after.as_array().map(Vec::len),
        "passive `tes env` prompts must not write OpLog operations"
    );

    let pwsh_script = repo.tes("term-check", &["env", "--pwsh"]);
    assert!(
        pwsh_script.contains("complete --shell plain") && !pwsh_script.contains("completions"),
        "pwsh hook must call `tes complete`, not `tes completions`: {pwsh_script}"
    );
    assert!(
        pwsh_script.contains("$exitCode = $LASTEXITCODE")
            && pwsh_script.contains("$global:LASTEXITCODE = $exitCode"),
        "pwsh hook must preserve $LASTEXITCODE across _TesSyncEnv: {pwsh_script}"
    );

    let nu_script = repo.tes("term-check", &["env", "--nu"]);
    assert!(
        nu_script.contains("is-not-empty") && nu_script.contains("hide-env -i TES_BRANCH"),
        "nu hook must guard empty stdout and hide TES_* outside repos: {nu_script}"
    );

    #[cfg(unix)]
    {
        let tes_bin = Path::new(env!("CARGO_BIN_EXE_tes"));
        let bin_dir = tes_bin.parent().expect("bin dir").display().to_string();
        let path_with_tes = format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default());
        let non_repo = tempfile::tempdir().expect("non-repo dir");

        // Verify user's PORT=8080 survives `eval "$(tes env)"` inside a repo with port_isolation=false
        // AND outside a repo, while Tessera-managed ports + TES_SHIM_BIN ARE cleaned up when transitioning.
        let sh_check = format!(
            concat!(
                "export PORT=8080\n",
                "eval \"$(tes env)\"\n",
                "printf 'IN_DEFAULT_PORT=%s\\n' \"$PORT\"\n",
                "cd '{}' && eval \"$(tes env)\"\n",
                "printf 'OUT_DEFAULT_PORT=%s\\n' \"$PORT\"\n",
                "cd '{}' && tes config set port_isolation true >/dev/null\n",
                "eval \"$(tes env)\"\n",
                "case \":$PATH:\" in *\":$TES_SHIM_BIN:\"*) HAS_SHIM=yes ;; *) HAS_SHIM=no ;; esac\n",
                "printf 'ISOLATED_PORT=%s|MANAGED=%s|HAS_SHIM=%s\\n' \"$PORT\" \"$TES_MANAGED_PORTS\" \"$HAS_SHIM\"\n",
                "OLD_SHIM=\"$TES_SHIM_BIN\"\n",
                "tes config set port_isolation false >/dev/null\n",
                "eval \"$(tes env)\"\n",
                "case \":$PATH:\" in *\":$OLD_SHIM:\"*) SHIM_REMOVED=no ;; *) SHIM_REMOVED=yes ;; esac\n",
                "printf 'AFTER_DISABLE_PORT=%s|SHIM_REMOVED=%s\\n' \"${{PORT:-unset}}\" \"$SHIM_REMOVED\"\n"
            ),
            non_repo.path().display(),
            repo.root.display()
        );
        let sh_out = Command::new("/bin/sh")
            .current_dir(&repo.root)
            .env("PATH", &path_with_tes)
            .env("TES_SESSION_ID", "term-port-check")
            .args(["-c", &sh_check])
            .output()
            .expect("run sh port preservation check");
        let sh_stdout = String::from_utf8_lossy(&sh_out.stdout);
        assert!(
            sh_out.status.success(),
            "sh check failed: stdout={sh_stdout}, stderr={}",
            String::from_utf8_lossy(&sh_out.stderr)
        );
        assert!(
            sh_stdout.contains("IN_DEFAULT_PORT=8080"),
            "user PORT=8080 must not be unset when port_isolation=false:\n{sh_stdout}"
        );
        assert!(
            sh_stdout.contains("OUT_DEFAULT_PORT=8080"),
            "user PORT=8080 must not be unset outside a Tessera repo:\n{sh_stdout}"
        );
        assert!(
            sh_stdout.contains("MANAGED=1|HAS_SHIM=yes"),
            "port_isolation=true must set TES_MANAGED_PORTS=1 and prepend TES_SHIM_BIN:\n{sh_stdout}"
        );
        assert!(
            sh_stdout.contains("AFTER_DISABLE_PORT=unset|SHIM_REMOVED=yes"),
            "disabling port_isolation must unset Tessera-managed PORT and strip TES_SHIM_BIN from PATH:\n{sh_stdout}"
        );

        // Verify Zsh quote-aware `_tes_preexec` does NOT hop for `git commit -m "a; b"` or `FOO=1 git status`.
        if Path::new("/bin/zsh").is_file() {
            repo.tes("term-zsh-q", &["switch", "feat/zsh-quote"]);
            let zsh_script = r#"
                export TES_SESSION_ID="term-zsh-q"
                eval "$(tes env --zsh)"
                _tes_preexec 'git commit -m "a; b"'
                echo "QUOTED_PWD=$PWD"
                _tes_preexec 'FOO=1 command git status'
                echo "ENV_PREFIX_PWD=$PWD"
            "#;
            let z_out = Command::new("/bin/zsh")
                .current_dir(&repo.root)
                .env("PATH", &path_with_tes)
                .args(["-c", zsh_script])
                .output()
                .expect("run zsh quote check");
            let z_stdout = String::from_utf8_lossy(&z_out.stdout);
            assert!(
                z_out.status.success(),
                "zsh quote check failed: stdout={z_stdout}, stderr={}",
                String::from_utf8_lossy(&z_out.stderr)
            );
            assert!(
                !z_stdout.contains("branches/feat-zsh-quote"),
                "quoted semicolon or env-var prefixed git must NOT hop into sandbox:\n{z_stdout}"
            );
        }
    }
}

/// Verifies binary file conflict detection, Unix symlink preservation, linked Git worktree
/// discovery & diagnostics, and `tes gc` + `tes undo` milestone chain integrity.
#[test]
fn binary_conflict_symlink_worktree_and_gc_undo_integrity_e2e() {
    let repo = Repo::new(&[("code.rs", "pub fn v() -> u8 { 1 }\n", false)]);

    // Seed a binary file and verify divergent binary edits record a `Binary` conflict.
    fs::write(
        repo.path("logo.bin"),
        [0x89u8, 0x50, 0x4E, 0x47, 0x00, 0x01, 0x02],
    )
    .expect("write bin");
    repo.hook("bin-seed", "logo.bin");
    repo.tes("bin-seed", &["commit", "-m", "feat: add binary asset"]);

    repo.tes(
        "bin-a",
        &[
            "claim",
            "--branch",
            "main",
            "--intent",
            "Update logo A",
            "logo.bin",
        ],
    );
    repo.tes(
        "bin-b",
        &[
            "claim",
            "--branch",
            "main",
            "--intent",
            "Update logo B",
            "logo.bin",
        ],
    );
    fs::write(
        repo.path("logo.bin"),
        [0x89u8, 0x50, 0x4E, 0x47, 0x00, 0xAA, 0xBB],
    )
    .expect("write bin A");
    repo.hook("bin-a", "logo.bin");
    fs::write(
        repo.path("logo.bin"),
        [0x89u8, 0x50, 0x4E, 0x47, 0x00, 0xCC, 0xDD],
    )
    .expect("write bin B");
    repo.hook("bin-b", "logo.bin");

    let conflicts_json: serde_json::Value =
        serde_json::from_str(&repo.tes("bin-b", &["conflicts", "--json"])).expect("conflicts json");
    let conflict_arr = conflicts_json.as_array().expect("conflicts array");
    assert!(
        conflict_arr
            .iter()
            .any(|c| c["path"] == "logo.bin" && c["kind"] == "Binary"),
        "expected Binary conflict for logo.bin: {conflicts_json}"
    );
    repo.tes("bin-b", &["resolve", "logo.bin", "--strategy", "ours"]);
    repo.tes("bin-b", &["commit", "-m", "fix: resolve binary conflict"]);
    repo.tes("bin-a", &["agent", "close", "--all"]);

    // Unix tracked symlink preservation across commits.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(Path::new("code.rs"), repo.path("link_to_code.rs"))
            .expect("create relative symlink");
        repo.tes("term-sym", &["commit", "-m", "feat: add tracked symlink"]);
        let head = repo.head_tree();
        assert_eq!(
            head.get("link_to_code.rs").map(|(m, _)| m.as_str()),
            Some("120000"),
            "tracked symlink must keep mode 120000 in Git tree: {head:?}"
        );
        assert!(
            repo.path("link_to_code.rs")
                .symlink_metadata()
                .expect("lstat symlink")
                .file_type()
                .is_symlink(),
            "symlink must remain a symlink on disk after commit"
        );
    }

    // Linked Git worktree discovery and `tes doctor --json`.
    let wt_tmp = tempfile::tempdir().expect("worktree tempdir");
    let wt_dir = wt_tmp.path().join("linked-wt");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feat/worktree",
        wt_dir.to_str().expect("wt dir"),
    ]);

    let wt_doctor_out = Command::new(env!("CARGO_BIN_EXE_tes"))
        .current_dir(&wt_dir)
        .args(["doctor", "--json"])
        .env("TES_SESSION_ID", "term-wt")
        .output()
        .expect("tes doctor in linked worktree");
    assert!(
        wt_doctor_out.status.success(),
        "tes doctor failed in linked worktree: {}",
        String::from_utf8_lossy(&wt_doctor_out.stderr)
    );
    let wt_doc_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&wt_doctor_out.stdout))
            .expect("wt doctor json");
    let wt_git_check = wt_doc_json["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["id"] == "git_repository")
        .expect("git_repository check");
    assert_eq!(wt_git_check["details"]["is_linked_worktree"], true);

    let wt_status = Command::new(env!("CARGO_BIN_EXE_tes"))
        .current_dir(&wt_dir)
        .args(["status", "--json"])
        .env("TES_SESSION_ID", "term-wt")
        .output()
        .expect("tes status in linked worktree");
    assert!(wt_status.status.success());

    // `tes gc` + `tes undo` chain integrity after multiple ephemeral edits and a milestone commit.
    repo.tes(
        "gc-agent",
        &["claim", "--intent", "Stepwise edits", "code.rs"],
    );

    for n in 2u8..=5u8 {
        repo.write("code.rs", &format!("pub fn v() -> u8 {{ {n} }}\n"));
        repo.hook("gc-agent", "code.rs");
    }

    repo.tes("gc-agent", &["commit", "-m", "feat: v5"]);
    assert_eq!(repo.read("code.rs"), "pub fn v() -> u8 { 5 }\n");

    let gc_out = repo.tes("gc-agent", &["gc", "--json"]);
    let gc_json: serde_json::Value = serde_json::from_str(&gc_out).expect("gc json");
    assert!(gc_json["compacted_operations"].is_u64(), "{gc_json}");
    assert_eq!(gc_json["dry_run"], false, "{gc_json}");
    assert!(gc_json["pruned_sandboxes"].is_array(), "{gc_json}");

    // Seed an orphaned sandbox + dead port lease under the exact sandboxes root used by `tes`,
    // confirm `tes doctor` suggests `tes gc`, confirm `tes gc --dry-run` preserves them, then
    // `tes gc` cleans them so `tes doctor` passes cleanly.
    let doc_probe_raw = repo.tes("gc-agent", &["doctor", "--json"]);
    let doc_probe: serde_json::Value =
        serde_json::from_str(&doc_probe_raw).expect("doctor probe json");
    let sandboxes_root = PathBuf::from(
        doc_probe["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .find(|c| c["id"] == "cow_strategy")
            .and_then(|c| c["details"]["probe_root"].as_str())
            .expect("cow_strategy probe_root"),
    );
    let orphan_dir = sandboxes_root.join("branches").join("stale-legacy-sandbox");
    fs::create_dir_all(&orphan_dir).expect("mkdir orphan_dir");
    fs::write(
        sandboxes_root
            .join("branches")
            .join("stale-legacy-sandbox.slot"),
        b"7",
    )
    .expect("write orphan slot");
    let dead_lease_dir = sandboxes_root.join("bin").join(".leases").join("3987");
    fs::create_dir_all(&dead_lease_dir).expect("mkdir dead lease");
    fs::write(dead_lease_dir.join("pid"), b"99999999").expect("write dead pid");

    let doc_warn_raw = repo.tes("gc-agent", &["doctor", "--json"]);
    let doc_warn: serde_json::Value =
        serde_json::from_str(&doc_warn_raw).expect("doctor warn json");
    let rt_check = doc_warn["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|c| c["id"] == "sandboxes_leases_conflicts")
        .expect("sandboxes_leases_conflicts check");
    assert_eq!(rt_check["status"], "warn", "{rt_check}");
    assert!(
        rt_check["summary"]
            .as_str()
            .unwrap_or("")
            .contains("run 'tes gc' to clean up"),
        "doctor warning should guide user to tes gc: {rt_check}"
    );

    let dry_out = repo.tes("gc-agent", &["gc", "--dry-run", "--json"]);
    let dry_json: serde_json::Value = serde_json::from_str(&dry_out).expect("gc dry-run json");
    assert_eq!(dry_json["dry_run"], true, "{dry_json}");
    assert_eq!(
        dry_json["pruned_sandboxes"],
        serde_json::json!(["stale-legacy-sandbox"]),
        "{dry_json}"
    );
    assert_eq!(
        dry_json["pruned_dead_leases"],
        serde_json::json!(["3987"]),
        "{dry_json}"
    );
    assert!(orphan_dir.is_dir(), "gc --dry-run must keep orphan_dir");
    assert!(dead_lease_dir.is_dir(), "gc --dry-run must keep dead_lease");

    let apply_out = repo.tes("gc-agent", &["gc", "--json"]);
    let apply_json: serde_json::Value = serde_json::from_str(&apply_out).expect("gc apply json");
    assert_eq!(
        apply_json["pruned_sandboxes"],
        serde_json::json!(["stale-legacy-sandbox"]),
        "{apply_json}"
    );
    assert_eq!(
        apply_json["pruned_dead_leases"],
        serde_json::json!(["3987"]),
        "{apply_json}"
    );
    assert!(!orphan_dir.exists(), "gc must delete orphan_dir");
    assert!(!dead_lease_dir.exists(), "gc must delete dead_lease");

    let doc_clean_raw = repo.tes("gc-agent", &["doctor", "--json"]);
    let doc_clean: serde_json::Value =
        serde_json::from_str(&doc_clean_raw).expect("doctor clean json");
    let rt_clean = doc_clean["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|c| c["id"] == "sandboxes_leases_conflicts")
        .expect("sandboxes_leases_conflicts check");
    assert_eq!(rt_clean["status"], "pass", "{rt_clean}");

    repo.tes("gc-agent", &["undo"]);
    assert_eq!(
        repo.read("code.rs"),
        "pub fn v() -> u8 { 1 }\n",
        "tes undo after tes gc must cleanly restore pre-commit milestone state"
    );
}

/// Verifies subdirectory-relative `tes restore .` / `tes restore <file>` / `tes stash push <file>`,
/// `tes push --tags` leaving WIP uncommitted, and MCP `push` failing with `"isError": true`.
#[test]
fn subdir_restore_stash_push_tags_and_mcp_push_error_e2e() {
    let repo = Repo::new(&[
        ("sub/a.rs", "pub fn a() -> i32 { 1 }\n", false),
        ("sub/b.rs", "pub fn b() -> i32 { 1 }\n", false),
        ("other/keep.rs", "pub fn keep() -> i32 { 1 }\n", false),
        ("root_keep.rs", "pub fn root() -> i32 { 1 }\n", false),
    ]);

    let tes_bin = Path::new(env!("CARGO_BIN_EXE_tes"));
    let run_in_subdir = |subdir: &str, session: &str, args: &[&str]| -> Output {
        let mut cmd = Command::new(tes_bin);
        cmd.current_dir(repo.path(subdir))
            .args(args)
            .env("TES_NO_PAGER", "1");

        for key in ISOLATED_ENV {
            cmd.env_remove(key);
        }

        cmd.env("TES_SESSION_ID", session);
        cmd.output().expect("spawn tes in subdir")
    };

    // Subdirectory `cd sub && tes restore .` must restore ONLY `sub/`, leaving siblings intact.
    repo.write("sub/a.rs", "pub fn a() -> i32 { 999 }\n");
    repo.write("other/keep.rs", "pub fn keep() -> i32 { 222 }\n");
    repo.write("root_keep.rs", "pub fn root() -> i32 { 333 }\n");

    let restore_dot = run_in_subdir("sub", "term-sub-restore", &["restore", "."]);
    assert!(
        restore_dot.status.success(),
        "cd sub && tes restore . failed: {}",
        String::from_utf8_lossy(&restore_dot.stderr)
    );
    assert_eq!(
        repo.read("sub/a.rs"),
        "pub fn a() -> i32 { 1 }\n",
        "sub/a.rs must be restored by `cd sub && tes restore .`"
    );
    assert_eq!(
        repo.read("other/keep.rs"),
        "pub fn keep() -> i32 { 222 }\n",
        "sibling directory other/keep.rs must NEVER be overwritten by `cd sub && tes restore .`"
    );
    assert_eq!(
        repo.read("root_keep.rs"),
        "pub fn root() -> i32 { 333 }\n",
        "root_keep.rs outside sub/ must NEVER be overwritten by `cd sub && tes restore .`"
    );

    // Subdirectory `cd sub && tes restore b.rs` resolves relative to `sub/`.
    repo.write("sub/b.rs", "pub fn b() -> i32 { 888 }\n");
    let restore_b = run_in_subdir("sub", "term-sub-restore", &["restore", "b.rs"]);
    assert!(
        restore_b.status.success(),
        "cd sub && tes restore b.rs failed: {}",
        String::from_utf8_lossy(&restore_b.stderr)
    );
    assert_eq!(repo.read("sub/b.rs"), "pub fn b() -> i32 { 1 }\n");
    assert_eq!(repo.read("other/keep.rs"), "pub fn keep() -> i32 { 222 }\n");

    // Subdirectory `cd sub && tes stash push a.rs` stashes ONLY `sub/a.rs`.
    repo.write("sub/a.rs", "pub fn a() -> i32 { 444 }\n");
    let stash_out = run_in_subdir("sub", "term-sub-stash", &["stash", "push", "a.rs"]);
    assert!(
        stash_out.status.success(),
        "cd sub && tes stash push a.rs failed: {}",
        String::from_utf8_lossy(&stash_out.stderr)
    );
    assert_eq!(
        repo.read("sub/a.rs"),
        "pub fn a() -> i32 { 1 }\n",
        "sub/a.rs must be stashed and restored to baseline"
    );
    assert_eq!(
        repo.read("other/keep.rs"),
        "pub fn keep() -> i32 { 222 }\n",
        "other/keep.rs must NOT be stashed when running `cd sub && tes stash push a.rs`"
    );

    let stash_list_raw = repo.tes("term-sub-stash", &["stash", "list", "--json"]);
    let stash_list: serde_json::Value =
        serde_json::from_str(&stash_list_raw).expect("parse stash list json");
    let first_stash = &stash_list[0];
    assert!(
        first_stash["own_blobs"].get("sub/a.rs").is_some(),
        "stash entry must record sub/a.rs: {first_stash}"
    );
    assert!(
        first_stash["own_blobs"].get("other/keep.rs").is_none(),
        "stash entry must not include unrelated sibling other/keep.rs: {first_stash}"
    );

    repo.tes("term-sub-stash", &["stash", "drop", "0"]);
    repo.tes(
        "term-sub-restore",
        &["restore", "other/keep.rs", "root_keep.rs"],
    );

    // `tes push --tags` must push tags without silently auto-committing WIP edits.
    let remote_tmp = tempfile::tempdir().expect("remote tempdir");
    let bare_init = Command::new("git")
        .current_dir(remote_tmp.path())
        .args(["init", "--bare"])
        .output()
        .expect("git init --bare");
    assert!(bare_init.status.success());

    repo.git(&[
        "remote",
        "add",
        "origin",
        remote_tmp.path().to_str().expect("remote path"),
    ]);
    repo.git(&["push", "-u", "origin", "HEAD"]);
    repo.git(&["tag", "v0.1.0"]);

    let head_before = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.write("root_keep.rs", "pub fn root() -> i32 { 777 }\n");
    repo.hook("term-tags", "root_keep.rs");

    repo.tes("term-tags", &["push", "--tags"]);

    let head_after = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(
        head_after, head_before,
        "tes push --tags must NOT silently auto-commit WIP edits"
    );
    assert_eq!(
        repo.read("root_keep.rs"),
        "pub fn root() -> i32 { 777 }\n",
        "WIP edit must remain uncommitted after tes push --tags"
    );

    let remote_tag = Command::new("git")
        .arg("--git-dir")
        .arg(remote_tmp.path())
        .args(["rev-parse", "refs/tags/v0.1.0"])
        .output()
        .expect("check remote tag");
    assert!(
        remote_tag.status.success(),
        "tes push --tags must push tag v0.1.0 to origin"
    );

    // MCP `push` failure must return `"isError": true`.
    let current_branch = repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let current_branch = current_branch.trim();
    let mcp_req = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{{\"name\":\"push\",\"arguments\":{{\"remote\":\"nonexistent-remote-xyz\",\"branch\":\"{current_branch}\",\"no_auto_commit\":true}}}}}}\n"
    );
    let mut mcp_cmd = Command::new(tes_bin);
    mcp_cmd
        .current_dir(&repo.root)
        .args(["mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());

    for key in ISOLATED_ENV {
        mcp_cmd.env_remove(key);
    }

    let mut mcp_child = mcp_cmd.spawn().expect("spawn tes mcp");

    {
        use std::io::Write;
        let stdin = mcp_child.stdin.as_mut().expect("mcp stdin");
        stdin.write_all(mcp_req.as_bytes()).expect("write mcp req");
    }

    let mcp_out = mcp_child.wait_with_output().expect("wait mcp");
    let mcp_stdout = String::from_utf8_lossy(&mcp_out.stdout);
    let resp: serde_json::Value = serde_json::from_str(mcp_stdout.trim())
        .unwrap_or_else(|e| panic!("invalid MCP JSON response ({e}): {mcp_stdout}"));
    assert_eq!(
        resp["result"]["isError"],
        serde_json::json!(true),
        "failed MCP push must return isError: true, got: {resp}"
    );
}

/// Verifies on a real controlling Unix PTY that `tes` ignores `SIGINT` while waiting on a pager
/// child and unconditionally restores `termios` (`OPOST`, `ONLCR`, `ICANON`, `ECHO`) even if the
/// pager leaves the terminal in raw / `-onlcr` mode on exit.
#[cfg(unix)]
#[test]
fn pager_ignores_sigint_and_restores_termios_on_pty() {
    use std::ffi::{CStr, CString};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    let repo = Repo::new(&[("lib.rs", "pub fn alpha() -> i32 { 1 }\n", false)]);
    let sync_tmp = tempfile::tempdir().expect("sync tempdir");
    let ready_flag = sync_tmp.path().join("pager_ready");
    let done_flag = sync_tmp.path().join("pager_done");
    let pager_script = sync_tmp.path().join("corrupt_pager.sh");

    fs::write(
        &pager_script,
        "#!/bin/sh\n\
trap '' INT\n\
cat >/dev/null\n\
stty -onlcr -echo -icanon <&1\n\
printf ok > \"$TES_TEST_PAGER_READY\"\n\
i=0\n\
while [ ! -f \"$TES_TEST_PAGER_DONE\" ] && [ \"$i\" -lt 250 ]; do\n\
  sleep 0.02\n\
  i=$((i + 1))\n\
done\n\
exit 0\n",
    )
    .expect("write pager script");
    fs::set_permissions(&pager_script, fs::Permissions::from_mode(0o755))
        .expect("chmod pager script");

    // SAFETY: Standard POSIX PTY allocation (`posix_openpt` / `grantpt` / `unlockpt` / `ptsname`).
    let (master_fd, slave_cstr) = unsafe {
        let mfd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(mfd >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(mfd), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(mfd), 0, "unlockpt failed");
        let name_ptr = libc::ptsname(mfd);
        assert!(!name_ptr.is_null(), "ptsname returned null");
        let name = CStr::from_ptr(name_ptr).to_owned();
        (mfd, name)
    };

    // Keep an open slave fd in the parent so we can inspect `termios` cleanly after the child exits.
    // SAFETY: `slave_cstr` is a NUL-terminated path returned by `ptsname`.
    let inspect_slave_fd =
        unsafe { libc::open(slave_cstr.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    assert!(inspect_slave_fd >= 0, "open inspect slave PTY failed");

    // Ensure baseline `OPOST | ONLCR` and `ICANON | ECHO` are enabled before spawning `tes`.
    // On macOS/BSD, exiting the session leader revokes open slave descriptors, so fall back to
    // `master_fd` if `inspect_slave_fd` returns `EIO` after the child exits.
    let read_termios = || -> libc::termios {
        let mut t = std::mem::MaybeUninit::<libc::termios>::zeroed();
        // SAFETY: `inspect_slave_fd` and `master_fd` are valid open PTY descriptors.
        let rc = unsafe {
            let r = libc::tcgetattr(inspect_slave_fd, t.as_mut_ptr());

            if r == 0 {
                r
            } else {
                libc::tcgetattr(master_fd, t.as_mut_ptr())
            }
        };
        assert_eq!(rc, 0, "tcgetattr failed on both slave and master PTY");
        // SAFETY: `tcgetattr` succeeded.
        unsafe { t.assume_init() }
    };

    let mut baseline = read_termios();
    baseline.c_oflag |= libc::OPOST | libc::ONLCR;
    baseline.c_lflag |= libc::ICANON | libc::ECHO;
    // SAFETY: `inspect_slave_fd` is a valid open PTY descriptor.
    assert_eq!(
        unsafe { libc::tcsetattr(inspect_slave_fd, libc::TCSANOW, &baseline) },
        0,
        "tcsetattr baseline failed"
    );

    let tes_bin = Path::new(env!("CARGO_BIN_EXE_tes"));
    let child_slave_cstr: CString = slave_cstr.clone();
    let mut cmd = Command::new(tes_bin);
    cmd.current_dir(&repo.root)
        .args(["--paginate", "doctor"])
        .env("TES_PAGER", &pager_script)
        .env("TES_TEST_PAGER_READY", &ready_flag)
        .env("TES_TEST_PAGER_DONE", &done_flag)
        .env("TERM", "xterm-256color")
        .env_remove("TES_NO_PAGER")
        .env_remove("GIT_PAGER")
        .env_remove("PAGER");

    for key in ISOLATED_ENV {
        cmd.env_remove(key);
    }

    // SAFETY: Only async-signal-safe POSIX calls (`setsid`, `open`, `dup2`, `close`) execute inside
    // `pre_exec`, giving `tes` and its pager child a real controlling terminal on `child_slave_cstr`.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }

            let sfd = libc::open(child_slave_cstr.as_ptr(), libc::O_RDWR);

            if sfd < 0 {
                return Err(std::io::Error::last_os_error());
            }

            libc::dup2(sfd, libc::STDIN_FILENO);
            libc::dup2(sfd, libc::STDOUT_FILENO);
            libc::dup2(sfd, libc::STDERR_FILENO);

            if sfd > libc::STDERR_FILENO {
                libc::close(sfd);
            }

            libc::close(master_fd);
            libc::close(inspect_slave_fd);
            Ok(())
        });
    }

    let mut child = cmd.spawn().expect("spawn paged tes doctor on PTY");
    let child_pid = child.id() as libc::pid_t;

    let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    while !ready_flag.exists() && std::time::Instant::now() < ready_deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    assert!(
        ready_flag.exists(),
        "timed out waiting for pager script to corrupt PTY state"
    );

    let during = read_termios();
    assert_eq!(
        during.c_oflag & libc::ONLCR,
        0,
        "test pager must have disabled ONLCR while running"
    );

    // Send SIGINT to the child's process group (simulating Ctrl-C on the controlling terminal).
    // SAFETY: `child_pid` is the live session/process-group leader spawned above.
    unsafe {
        libc::kill(-child_pid, libc::SIGINT);
    }

    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(
        child.try_wait().expect("try_wait after SIGINT").is_none(),
        "tes must ignore SIGINT while waiting for its pager child"
    );

    fs::write(&done_flag, b"done").expect("signal pager exit");
    let status = child.wait().expect("wait paged tes doctor");
    assert!(
        status.success(),
        "tes --paginate doctor should exit 0 after pager exits, got: {status}"
    );

    let after = read_termios();
    assert_ne!(
        after.c_oflag & libc::ONLCR,
        0,
        "ONLCR must be restored after pager exits"
    );
    assert_ne!(
        after.c_oflag & libc::OPOST,
        0,
        "OPOST must be restored after pager exits"
    );
    assert_ne!(
        after.c_lflag & libc::ICANON,
        0,
        "ICANON must be restored after pager exits"
    );
    assert_ne!(
        after.c_lflag & libc::ECHO,
        0,
        "ECHO must be restored after pager exits"
    );

    // SAFETY: Closing our owned PTY descriptors.
    unsafe {
        libc::close(inspect_slave_fd);
        libc::close(master_fd);
    }
}

#[test]
fn workspaces_root_env_override_isolates_sandboxes_and_rejects_relative_paths() {
    let repo = Repo::new(&[("lib.rs", "pub fn x() -> i32 { 1 }\n", false)]);
    let custom_base = tempfile::tempdir().expect("custom_base tempdir");

    let out = Command::new(env!("CARGO_BIN_EXE_tes"))
        .current_dir(&repo.root)
        .env("TES_SESSION_ID", "env-override-agent")
        .env("TES_WORKSPACES_ROOT", custom_base.path())
        .args(["doctor", "--json"])
        .output()
        .expect("run tes doctor with TES_WORKSPACES_ROOT");
    assert!(
        out.status.success(),
        "tes doctor failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor json with override");
    let probe_root = PathBuf::from(
        doc["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .find(|c| c["id"] == "cow_strategy")
            .and_then(|c| c["details"]["probe_root"].as_str())
            .expect("probe_root"),
    );

    assert!(
        probe_root.starts_with(custom_base.path())
            || probe_root
                .canonicalize()
                .ok()
                .zip(custom_base.path().canonicalize().ok())
                .is_some_and(|(a, b)| a.starts_with(b)),
        "TES_WORKSPACES_ROOT must place sandboxes inside custom base: {}",
        probe_root.display()
    );
    assert!(
        custom_base.path().join("CACHEDIR.TAG").is_file(),
        "custom workspaces root must receive CACHEDIR.TAG"
    );
    assert!(
        probe_root.join(".repo_root").is_file(),
        "sandbox slug must write .repo_root marker"
    );

    // Relative TES_WORKSPACES_ROOT must be ignored safely.
    let rel_out = Command::new(env!("CARGO_BIN_EXE_tes"))
        .current_dir(&repo.root)
        .env("TES_SESSION_ID", "env-override-agent")
        .env("TES_WORKSPACES_ROOT", "relative/unsafe/path")
        .args(["doctor", "--json"])
        .output()
        .expect("run tes doctor with relative TES_WORKSPACES_ROOT");
    assert!(rel_out.status.success());

    let rel_doc: serde_json::Value =
        serde_json::from_slice(&rel_out.stdout).expect("doctor json with relative override");
    let rel_probe = rel_doc["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|c| c["id"] == "cow_strategy")
        .and_then(|c| c["details"]["probe_root"].as_str())
        .expect("rel_probe");
    assert!(
        !rel_probe.contains("relative/unsafe/path"),
        "relative TES_WORKSPACES_ROOT must be ignored: {rel_probe}"
    );
}

#[test]
fn gc_all_sweeps_deleted_repo_and_doctor_reports_merged_verify_and_usage() {
    let repo = Repo::new(&[("lib.rs", "pub fn x() -> i32 { 1 }\n", false)]);
    let custom_base = tempfile::tempdir().expect("custom_base tempdir");
    let existing_other_repo = tempfile::tempdir().expect("existing_other_repo tempdir");
    fs::create_dir_all(existing_other_repo.path().join(".git")).expect("mkdir .git");

    let dead_slug = custom_base
        .path()
        .join("deleted-proj-0123456789abcdef0123456789abcdef");
    fs::create_dir_all(dead_slug.join("branches").join("main")).expect("mkdir dead slug");
    fs::write(
        dead_slug.join(".repo_root"),
        custom_base
            .path()
            .join("nonexistent-repo")
            .display()
            .to_string(),
    )
    .expect("write dead .repo_root");
    fs::write(
        dead_slug.join("branches").join("main").join("big.bin"),
        vec![b'D'; 65_536],
    )
    .expect("write dead big.bin");

    let live_slug = custom_base
        .path()
        .join("live-proj-fedcba9876543210fedcba9876543210");
    fs::create_dir_all(live_slug.join("branches").join("main")).expect("mkdir live slug");
    fs::write(
        live_slug.join(".repo_root"),
        existing_other_repo.path().display().to_string(),
    )
    .expect("write live .repo_root");

    let run_tes = |args: &[&str]| -> serde_json::Value {
        let out = Command::new(env!("CARGO_BIN_EXE_tes"))
            .current_dir(&repo.root)
            .env("TES_SESSION_ID", "gc-all-agent")
            .env("TES_WORKSPACES_ROOT", custom_base.path())
            .env("TES_NO_PAGER", "1")
            .args(args)
            .output()
            .expect("spawn tes");
        assert!(
            out.status.success(),
            "tes {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("parse json output")
    };

    // Create a merged feature branch and materialize its sandbox via `tes exec`.
    let _ = Command::new(env!("CARGO_BIN_EXE_tes"))
        .current_dir(&repo.root)
        .env("TES_SESSION_ID", "gc-all-agent")
        .env("TES_WORKSPACES_ROOT", custom_base.path())
        .args(["switch", "-b", "feat/already-merged"])
        .output()
        .expect("switch -b");
    let exec_out = Command::new(env!("CARGO_BIN_EXE_tes"))
        .current_dir(&repo.root)
        .env("TES_SESSION_ID", "gc-all-agent")
        .env("TES_WORKSPACES_ROOT", custom_base.path())
        .args([
            "exec",
            "-b",
            "feat/already-merged",
            "--",
            env!("CARGO_BIN_EXE_tes"),
            "--version",
        ])
        .output()
        .expect("exec to materialize sandbox");
    assert!(
        exec_out.status.success(),
        "tes exec failed: {}",
        String::from_utf8_lossy(&exec_out.stderr)
    );
    let _ = Command::new(env!("CARGO_BIN_EXE_tes"))
        .current_dir(&repo.root)
        .env("TES_SESSION_ID", "gc-all-agent")
        .env("TES_WORKSPACES_ROOT", custom_base.path())
        .args(["switch", "main"])
        .output()
        .expect("switch back to main");

    let doc_probe = run_tes(&["doctor", "--json"]);
    let probe_root = PathBuf::from(
        doc_probe["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .find(|c| c["id"] == "cow_strategy")
            .and_then(|c| c["details"]["probe_root"].as_str())
            .expect("probe_root"),
    );

    // Seed a crashed verify sandbox and an orphaned in-repo agent workspace.
    let dead_verify = probe_root.join("verify-sandbox-99999999-42-1");
    fs::create_dir_all(&dead_verify).expect("mkdir dead_verify");
    let orphan_ws = repo
        .root
        .join(".tessera")
        .join("workspaces")
        .join("stale-agent");
    fs::create_dir_all(&orphan_ws).expect("mkdir orphan_ws");

    let merged_safe = agent::Coordinator::sanitize_branch_name("feat/already-merged");

    let doc_warn = run_tes(&["doctor", "--json"]);
    let rt_check = doc_warn["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["id"] == "sandboxes_leases_conflicts")
        .expect("sandboxes_leases_conflicts");
    assert_eq!(rt_check["status"], "warn", "{rt_check}");
    assert_eq!(
        rt_check["details"]["merged_sandboxes"],
        serde_json::json!([merged_safe.clone()]),
        "{rt_check}"
    );
    assert_eq!(
        rt_check["details"]["crashed_verify_sandboxes"],
        serde_json::json!(["verify-sandbox-99999999-42-1"]),
        "{rt_check}"
    );
    assert_eq!(
        rt_check["details"]["orphaned_agent_workspaces"],
        serde_json::json!(["stale-agent"]),
        "{rt_check}"
    );
    assert_eq!(
        rt_check["details"]["stale_global_repos"],
        serde_json::json!(["deleted-proj-0123456789abcdef0123456789abcdef"]),
        "{rt_check}"
    );
    assert!(
        rt_check["details"]["sandbox_disk_usage"]["file_count"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        "doctor must report sandbox file_count: {rt_check}"
    );

    // Dry-run `--all` previews all deletions without removing anything.
    let dry = run_tes(&["gc", "--dry-run", "--all", "--json"]);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(
        dry["pruned_merged_sandboxes"],
        serde_json::json!([merged_safe]),
        "{dry}"
    );
    assert_eq!(
        dry["pruned_global_repos"],
        serde_json::json!(["deleted-proj-0123456789abcdef0123456789abcdef"]),
        "{dry}"
    );
    assert!(
        dry["reclaimed_bytes"].as_u64().unwrap_or(0) >= 65_536,
        "{dry}"
    );
    assert!(dead_slug.is_dir(), "dry-run must preserve dead_slug");
    assert!(dead_verify.is_dir(), "dry-run must preserve dead_verify");

    // Apply `--all` sweeps dead repo, merged sandbox, crashed verify sandbox, and orphaned ws, while keeping live_slug.
    let applied = run_tes(&["gc", "--all", "--json"]);
    assert_eq!(
        applied["pruned_global_repos"],
        serde_json::json!(["deleted-proj-0123456789abcdef0123456789abcdef"]),
        "{applied}"
    );
    assert!(!dead_slug.exists(), "dead_slug must be deleted");
    assert!(live_slug.is_dir(), "live_slug must be preserved");
    assert!(!dead_verify.exists(), "dead_verify must be deleted");
    assert!(!orphan_ws.exists(), "orphan_ws must be deleted");

    let doc_clean = run_tes(&["doctor", "--json"]);
    let rt_clean = doc_clean["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["id"] == "sandboxes_leases_conflicts")
        .expect("sandboxes_leases_conflicts");
    assert_eq!(rt_clean["status"], "pass", "{rt_clean}");
}
