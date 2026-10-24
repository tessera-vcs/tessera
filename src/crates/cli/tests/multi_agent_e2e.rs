//! End-to-end integration tests for multi-agent coordination, 3-way AST reconciliation,
//! Copy-on-Write branch sandboxes, MCP JSON-RPC stdio transport, and multi-shell hooks.

use std::fs;
use std::process::Command;
use agent::{handle_post_tool_hook, handle_pre_tool_hook, Coordinator, ResolveStrategy};
use core::ConflictKind;

/// Verifies that `init_repo_with_options` installs either scoped or universal AI harness hooks and Git hooks.
#[test]
fn init_installs_all_harness_discovery_and_hook_files() {
    let tmp_default = tempfile::tempdir().expect("tempdir default");
    let (_, default_report) =
        Coordinator::init_repo_with_options(tmp_default.path(), None, Some("antigravity"))
            .expect("init antigravity");
    for expected in [
        ".agents/rules/tessera.md",
        ".mcp.json",
        "AGENTS.md",
        ".git/hooks/post-commit",
        ".git/hooks/post-merge",
        ".git/hooks/post-checkout",
    ] {
        assert!(tmp_default.path().join(expected).is_file());
        assert!(default_report
            .harness_files_installed
            .contains(&expected.to_string()));
    }
    assert!(!tmp_default.path().join(".claude/settings.json").exists());
    assert!(!tmp_default.path().join("CLAUDE.md").exists());
    assert!(!tmp_default.path().join(".cursor/mcp.json").exists());

    let tmp = tempfile::tempdir().expect("tempdir all");
    let (_, report) =
        Coordinator::init_repo_with_options(tmp.path(), None, Some("all")).expect("init all");

    for expected in [
        ".agents/rules/tessera.md",
        ".claude/settings.json",
        ".mcp.json",
        ".cursor/mcp.json",
        ".cursor/rules/tessera.mdc",
        "AGENTS.md",
        "CLAUDE.md",
        ".git/hooks/post-commit",
        ".git/hooks/post-merge",
        ".git/hooks/post-checkout",
    ] {
        assert!(
            tmp.path().join(expected).is_file(),
            "missing bootstrap file: {expected}"
        );
        assert!(report.harness_files_installed.contains(&expected.to_string()));
    }
}

/// Verifies that concurrent in-place edits in a shared working directory are 3-way AST-reconciled with inferred symbol intent.
#[test]
fn zero_command_autopilot_reconciles_concurrent_edits_in_shared_root_and_infers_intent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let api_dir = tmp.path().join("packages/api/src");
    fs::create_dir_all(&api_dir).expect("mkdir");
    let lib_path = api_dir.join("lib.rs");

    let base_code = r#"use std::fmt;

pub fn health_check() -> &'static str {
    "ok"
}
"#;
    fs::write(&lib_path, base_code).expect("write base");

    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init repo");

    let pre1 = handle_pre_tool_hook(&coord, "antigravity-1", "packages/api/src/lib.rs")
        .expect("pre-tool 1");
    assert!(pre1.radar_warning.is_none());

    let pre2 =
        handle_pre_tool_hook(&coord, "claude-2", "packages/api/src/lib.rs").expect("pre-tool 2");
    assert!(
        pre2.radar_warning.is_some(),
        "second session should receive live radar warning that antigravity-1 is active on the file"
    );

    let agy_code = r#"use std::fmt;
use std::sync::Arc;

pub fn health_check() -> &'static str {
    "ok"
}

pub fn verify_jwt(token: &str) -> bool {
    !token.is_empty()
}
"#;
    fs::write(&lib_path, agy_code).expect("write agy edit");
    let post1 = handle_post_tool_hook(&coord, "antigravity-1", "packages/api/src/lib.rs")
        .expect("post-tool 1");
    assert!(!post1.auto_merged_concurrent_edit);
    assert!(
        post1.session_intent.contains("fn verify_jwt"),
        "expected AST auto-inferred intent, got: {}",
        post1.session_intent
    );

    // `claude-2`, working from the original `base_code` in its context window,
    // overwrites `lib.rs` with its own edit (`use std::path::PathBuf;` and `fn rate_limit`).
    let claude_code = r#"use std::fmt;
use std::path::PathBuf;

pub fn health_check() -> &'static str {
    "ok"
}

pub fn rate_limit(requests: u32) -> bool {
    requests < 100
}
"#;
    fs::write(&lib_path, claude_code).expect("write claude edit");
    let post2 =
        handle_post_tool_hook(&coord, "claude-2", "packages/api/src/lib.rs").expect("post-tool 2");

    assert!(post2.auto_merged_concurrent_edit);
    assert!(!post2.has_conflict);
    assert!(post2.context_notice.is_some());

    let reconciled_disk = fs::read_to_string(&lib_path).expect("read reconciled file");
    assert!(reconciled_disk.contains("use std::sync::Arc;"));
    assert!(reconciled_disk.contains("use std::path::PathBuf;"));
    assert!(reconciled_disk.contains("pub fn verify_jwt"));
    assert!(reconciled_disk.contains("pub fn rate_limit"));

    let radar = coord.radar().expect("radar");
    assert!(
        !radar.collisions.iter().any(|c| c.target.contains("::")),
        "expected 0 AST symbol collisions when agents add distinct functions, got: {:?}",
        radar.collisions
    );

    let pre3 =
        handle_pre_tool_hook(&coord, "cursor-3", "packages/api/src/lib.rs").expect("pre-tool 3");
    assert!(pre3.radar_warning.is_some());
    let cursor_code = r#"use std::fmt;

pub fn health_check() -> &'static str {
    "ok"
}

pub fn audit_log(event: &str) -> usize {
    event.len()
}
"#;
    fs::write(&lib_path, cursor_code).expect("write cursor edit");
    let post3 =
        handle_post_tool_hook(&coord, "cursor-3", "packages/api/src/lib.rs").expect("post-tool 3");
    assert!(post3.auto_merged_concurrent_edit);
    assert!(!post3.has_conflict);

    let final_disk = fs::read_to_string(&lib_path).expect("read final disk");
    assert!(final_disk.contains("pub fn verify_jwt"));
    assert!(final_disk.contains("pub fn rate_limit"));
    assert!(final_disk.contains("pub fn audit_log"));

    let merge_rep = coord
        .merge_agents(&[], Some("main"), None, false)
        .expect("merge shared-root sessions");
    assert!(merge_rep.conflicts.is_empty());
    assert_eq!(merge_rep.closed_agents.len(), 3);
}

/// Verifies rename propagation, SQL migration resequencing, speculative verification gates, and operation-log undo.
#[test]
fn multi_agent_cow_merge_handles_renames_migrations_verification_gate_and_conflict_resolution() {
    let tmp = tempfile::tempdir().expect("tempdir");

    fs::create_dir_all(tmp.path().join("packages/core/src")).expect("mkdir core");
    fs::create_dir_all(tmp.path().join("db/migrations")).expect("mkdir migrations");

    fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[package]
name = "monorepo-root"
version = "0.1.0"

[dependencies]
serde = "1.0"
"#,
    )
    .expect("write Cargo.toml");

    fs::write(
        tmp.path().join("packages/core/src/helper.rs"),
        r#"pub fn sanitize_input(raw: &str) -> String {
    raw.trim().to_string()
}

pub fn compute_checksum(data: &str) -> usize {
    data.len() * 17
}
"#,
    )
    .expect("write helper.rs");

    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");

    let a1 = coord
        .spawn_cow_agent(
            "agent-alpha",
            Some("Move helper.rs to shared and add users migration"),
            Vec::new(),
        )
        .expect("spawn alpha");
    let a2 = coord
        .spawn_cow_agent(
            "agent-beta",
            Some("Add validate_email to helper.rs and add billing migration"),
            Vec::new(),
        )
        .expect("spawn beta");

    let ws1 = std::path::PathBuf::from(a1.workspace_path.expect("ws1"));
    let ws2 = std::path::PathBuf::from(a2.workspace_path.expect("ws2"));

    // Agent Alpha moves `packages/core/src/helper.rs` -> `packages/shared/src/helper.rs`,
    // adds `tokio` to `Cargo.toml`, and creates `db/migrations/0001_users.sql`.
    fs::create_dir_all(ws1.join("packages/shared/src")).expect("mkdir shared");
    let helper_content =
        fs::read_to_string(ws1.join("packages/core/src/helper.rs")).expect("read helper");
    fs::remove_file(ws1.join("packages/core/src/helper.rs")).expect("rm old helper");
    fs::write(ws1.join("packages/shared/src/helper.rs"), helper_content).expect("write moved");
    fs::write(
        ws1.join("Cargo.toml"),
        r#"[package]
name = "monorepo-root"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = "1.40"
"#,
    )
    .expect("write ws1 Cargo.toml");
    fs::write(
        ws1.join("db/migrations/0001_users.sql"),
        "CREATE TABLE users (id INT);\n",
    )
    .expect("write m1");

    // Agent Beta edits `packages/core/src/helper.rs` at its old path, adds `axum` to `Cargo.toml`,
    // and creates colliding migration `db/migrations/0001_billing.sql`.
    fs::write(
        ws2.join("packages/core/src/helper.rs"),
        r#"pub fn sanitize_input(raw: &str) -> String {
    raw.trim().to_string()
}

pub fn compute_checksum(data: &str) -> usize {
    data.len() * 17
}

pub fn validate_email(email: &str) -> bool {
    email.contains('@')
}
"#,
    )
    .expect("write ws2 helper");
    fs::write(
        ws2.join("Cargo.toml"),
        r#"[package]
name = "monorepo-root"
version = "0.1.0"

[dependencies]
serde = "1.0"
axum = "0.7"
"#,
    )
    .expect("write ws2 Cargo.toml");
    fs::write(
        ws2.join("db/migrations/0001_billing.sql"),
        "CREATE TABLE billing (id INT);\n",
    )
    .expect("write m2");

    let failed_merge = coord
        .merge_agents(
            &["agent-alpha".to_string(), "agent-beta".to_string()],
            Some("main"),
            Some("exit 1"),
            false,
        )
        .expect("merge with failing verify");
    assert!(!failed_merge.verification_passed);
    assert_eq!(failed_merge.conflicts.len(), 1);
    assert_eq!(failed_merge.conflicts[0].kind, ConflictKind::SemanticBuild);
    assert!(failed_merge.closed_agents.is_empty());

    coord
        .resolve_conflict(
            "<speculative-verify>",
            ResolveStrategy::ExplicitContent(String::new()),
        )
        .expect("resolve build conflict");

    let ok_merge = coord
        .merge_agents(
            &["agent-alpha".to_string(), "agent-beta".to_string()],
            Some("main"),
            Some("test -f packages/shared/src/helper.rs"),
            false,
        )
        .expect("merge with passing verify");

    assert!(ok_merge.verification_passed);
    assert!(ok_merge.conflicts.is_empty());
    assert_eq!(ok_merge.renames_applied.len(), 1);
    assert_eq!(ok_merge.migrations_resequenced.len(), 1);
    assert_eq!(
        ok_merge.migrations_resequenced[0].resequenced_path,
        "db/migrations/0002_billing.sql"
    );
    assert_eq!(
        ok_merge.closed_agents,
        vec!["agent-alpha".to_string(), "agent-beta".to_string()]
    );
    assert!(!ws1.exists(), "closed COW workspace ws1 should be removed");
    assert!(!ws2.exists(), "closed COW workspace ws2 should be removed");

    let merged_helper =
        fs::read_to_string(tmp.path().join("packages/shared/src/helper.rs")).expect("read shared");
    assert!(merged_helper.contains("pub fn validate_email"));
    assert!(
        !tmp.path().join("packages/core/src/helper.rs").exists(),
        "stale moved file packages/core/src/helper.rs must be removed on materialize"
    );

    let merged_cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).expect("read Cargo.toml");
    assert!(merged_cargo.contains("tokio = \"1.40\""));
    assert!(merged_cargo.contains("axum = \"0.7\""));
    assert!(tmp.path().join("db/migrations/0001_users.sql").is_file());
    assert!(tmp.path().join("db/migrations/0002_billing.sql").is_file());

    let cached_diff = Command::new("git")
        .current_dir(tmp.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["diff", "--cached", "--name-only"])
        .output()
        .expect("git diff --cached");
    assert!(
        cached_diff.stdout.is_empty(),
        "expected empty staged diff after sync_git_index, got: {}",
        String::from_utf8_lossy(&cached_diff.stdout)
    );

    let _ = coord.oplog().gc_compact().expect("gc");
    let undone = coord.undo(None).expect("undo");
    assert!(undone.description.contains("undo to operation"));
    let cargo_after_undo =
        fs::read_to_string(tmp.path().join("Cargo.toml")).expect("read Cargo.toml after undo");
    assert!(
        !cargo_after_undo.contains("tokio = \"1.40\""),
        "undo should restore Cargo.toml on disk to pre-merge trunk state"
    );
    assert!(
        tmp.path().join("packages/core/src/helper.rs").is_file(),
        "undo should restore original packages/core/src/helper.rs on disk"
    );
    assert!(
        !tmp.path().join("packages/shared/src/helper.rs").exists(),
        "undo should remove post-merge packages/shared/src/helper.rs from disk"
    );
}

/// Verifies background watcher ticks, `ResolveStrategy::AstUnion` conflict resolution, and named checkpoint rollback.
#[test]
fn watch_tick_and_ast_union_conflict_resolution_and_agent_close() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_file = tmp.path().join("service.rs");
    let base_src = r#"pub fn process(req: &str) -> Vec<String> {
    let mut log = Vec::new();
    log.push(req.to_string());
    log
}
"#;
    fs::write(&src_file, base_src).expect("write base");

    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");
    coord
        .create_checkpoint("cursor-1", "baseline-service")
        .expect("create named checkpoint");

    handle_pre_tool_hook(&coord, "cursor-1", "service.rs").expect("pre 1");
    handle_pre_tool_hook(&coord, "codex-2", "service.rs").expect("pre 2");

    let left_src = r#"pub fn process(req: &str) -> Vec<String> {
    let mut log = Vec::new();
    log.push("audit:start".to_string());
    log.push(req.to_string());
    log
}
"#;
    fs::write(&src_file, left_src).expect("write left");
    let tick1 = coord.watch_tick("cursor-1").expect("watch_tick 1");
    assert_eq!(tick1.len(), 1);
    assert_eq!(tick1[0].path, "service.rs");

    let right_src = r#"pub fn process(req: &str) -> Vec<String> {
    let mut log = Vec::new();
    log.push("metrics:increment".to_string());
    log.push(req.to_string());
    log
}
"#;
    fs::write(&src_file, right_src).expect("write right");
    let tick2 = coord.watch_tick("codex-2").expect("watch_tick 2");
    assert_eq!(tick2.len(), 1);
    assert!(tick2[0].has_conflict);

    coord
        .resolve_conflict("service.rs", ResolveStrategy::AstUnion)
        .expect("resolve with ast-union");
    let resolved_disk = fs::read_to_string(&src_file).expect("read resolved");
    assert!(resolved_disk.contains("audit:start"));
    assert!(resolved_disk.contains("metrics:increment"));

    let closed = coord.close_agent("cursor-1").expect("close cursor-1");
    assert_eq!(closed.agent_id, "cursor-1");
    let radar = coord.radar().expect("radar");
    assert_eq!(radar.active_agents.len(), 1);
    assert_eq!(radar.active_agents[0].agent_id, "codex-2");

    coord
        .undo(Some("baseline-service"))
        .expect("undo to named checkpoint");
    let restored_disk = fs::read_to_string(&src_file).expect("read restored service.rs");
    assert_eq!(restored_disk, base_src);
}

/// Verifies `tes mcp` stdio JSON-RPC 2.0 protocol execution and `git fsck --full` integrity after commits.
#[test]
fn cli_binary_mcp_stdio_and_git_fsck_validation() {
    use std::io::Write;
    use std::process::Stdio;

    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("lib.rs"), "pub fn base() -> i32 { 1 }\n").expect("write lib.rs");

    let cli_bin = env!("CARGO_BIN_EXE_tes");

    let init_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["init", "--json"])
        .output()
        .expect("run tes init");
    assert!(init_out.status.success());

    let mut child = Command::new(cli_bin)
        .current_dir(tmp.path())
        .arg("mcp")
        .env("ANTIGRAVITY_CONVERSATION_ID", "agy-mcp-test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn tes mcp");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
        )
        .expect("write init");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#
        )
        .expect("write tools/list");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"claim","arguments":{{"intent":"Enforce strict auth on /v2","symbols":["lib.rs::fn base"]}}}}}}"#
        )
        .expect("write claim");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"radar","arguments":{{}}}}}}"#
        )
        .expect("write radar");
    }

    let mcp_out = child.wait_with_output().expect("wait tes mcp");
    assert!(mcp_out.status.success());
    let stdout_str = String::from_utf8_lossy(&mcp_out.stdout);
    let lines: Vec<&str> = stdout_str.lines().collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[1].contains("\"name\":\"radar\""));
    assert!(lines[1].contains("\"name\":\"merge\""));
    assert!(lines[1].contains("\"name\":\"undo\""));
    assert!(!lines[1].contains("tes_"));
    assert!(lines[2].contains("Enforce strict auth on /v2"));
    assert!(lines[3].contains("antigravity-agy-mcp-test"));

    fs::write(tmp.path().join("lib.rs"), "pub fn base() -> i32 { 42 }\n").expect("edit lib.rs");
    let merge_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args([
            "merge",
            "antigravity-agy-mcp-test",
            "--into",
            "feature/auth",
            "-m",
            "Enforce strict auth on /v2",
        ])
        .env("GIT_AUTHOR_NAME", "Jane Doe")
        .env("GIT_AUTHOR_EMAIL", "jane@example.com")
        .output()
        .expect("run tes merge");
    assert!(merge_out.status.success());

    let branch_out = Command::new("git")
        .current_dir(tmp.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["branch", "--show-current"])
        .output()
        .expect("git branch --show-current");
    assert_eq!(
        String::from_utf8_lossy(&branch_out.stdout).trim(),
        "feature/auth"
    );

    let status_out = Command::new("git")
        .current_dir(tmp.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["status", "--porcelain"])
        .output()
        .expect("run git status");
    assert!(
        status_out.status.success() && status_out.stdout.is_empty(),
        "expected clean git status after tes merge, got: {}",
        String::from_utf8_lossy(&status_out.stdout)
    );

    let log_out = Command::new("git")
        .current_dir(tmp.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["log", "-n", "1", "--format=%an <%ae>%n%B"])
        .output()
        .expect("run git log");
    assert!(log_out.status.success());
    let log_str = String::from_utf8_lossy(&log_out.stdout);
    assert!(
        log_str.contains("Jane Doe <jane@example.com>"),
        "expected git author Jane Doe <jane@example.com>, got: {log_str}"
    );
    assert!(
        log_str.contains("Enforce strict auth on /v2"),
        "expected clean commit message, got: {log_str}"
    );
    assert!(
        !log_str.contains("Tessera-Agent:")
            && !log_str.contains("Tessera Merge Engine")
            && !log_str.contains("Co-authored-by:"),
        "commit log must not contain trailers when co_author=false by default: {log_str}"
    );

    // Enable `co_author = true` and verify subsequent commits include `Co-authored-by: Tessera <noreply@tessera.getlunaris.dev>`.
    let cfg_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["config", "set", "co_author", "true"])
        .output()
        .expect("tes config set co_author true");
    assert!(cfg_out.status.success());

    fs::write(tmp.path().join("lib.rs"), "pub fn base() -> i32 { 99 }\n").expect("edit lib.rs 2");
    let hook_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .env("ANTIGRAVITY_CONVERSATION_ID", "agy-mcp-test")
        .args(["hook", "post-tool", "--path", "lib.rs"])
        .output()
        .expect("tes hook post-tool");
    assert!(hook_out.status.success());

    let commit_co_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .env("ANTIGRAVITY_CONVERSATION_ID", "agy-mcp-test")
        .args(["commit", "-m", "feat: update base to 99"])
        .env("GIT_AUTHOR_NAME", "Jane Doe")
        .env("GIT_AUTHOR_EMAIL", "jane@example.com")
        .output()
        .expect("tes commit with co_author=true");
    assert!(commit_co_out.status.success());

    let log_co_out = Command::new("git")
        .current_dir(tmp.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["log", "-n", "1", "--format=%an <%ae>%n%B"])
        .output()
        .expect("run git log after co_author=true");
    let log_co_str = String::from_utf8_lossy(&log_co_out.stdout);
    assert!(
        log_co_str.contains("Jane Doe <jane@example.com>")
            && log_co_str.contains("Co-authored-by: Tessera <noreply@tessera.getlunaris.dev>"),
        "expected Co-authored-by: Tessera <noreply@tessera.getlunaris.dev> when co_author=true, got: {log_co_str}"
    );

    let fsck_out = Command::new("git")
        .current_dir(tmp.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["fsck", "--full"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck_out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck_out.stderr)
    );
}

/// Verifies that an early commit by one peer session preserves the claimed baseline of an in-flight peer session.
#[test]
fn peer_session_merging_early_does_not_wipe_in_flight_claimed_session_baseline() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_dir = tmp.path().join("src");
    fs::create_dir_all(&src_dir).expect("mkdir src");
    let app_path = src_dir.join("app.ts");

    let base_ts = r#"export function formatCurrency(amount: number, currency: string): string {
    return `${currency} ${amount.toFixed(2)}`;
}
"#;
    fs::write(&app_path, base_ts).expect("write base app.ts");

    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init repo");

    coord
        .claim_intent_and_symbols(
            "antigravity-tab-1",
            Some("Add locale parameter to formatCurrency"),
            &["src/app.ts".to_string()],
        )
        .expect("tab 1 claim");
    coord
        .claim_intent_and_symbols(
            "antigravity-tab-2",
            Some("Add decimals parameter to formatCurrency"),
            &["src/app.ts".to_string()],
        )
        .expect("tab 2 claim");

    let tab2_ts = r#"export function formatCurrency(amount: number, currency: string, decimals: number = 2): string {
    return `${currency} ${amount.toFixed(2)}`;
}
"#;
    fs::write(&app_path, tab2_ts).expect("write tab 2 edit");
    let post_tab2 =
        handle_post_tool_hook(&coord, "antigravity-tab-2", "src/app.ts").expect("post-tool tab 2");
    assert!(!post_tab2.has_conflict);

    let merge_tab2 = coord
        .merge_agents_with_message(
            &[],
            Some("main"),
            None,
            false,
            Some("feat(formatCurrency): add decimals parameter"),
        )
        .expect("merge tab 2");
    assert!(merge_tab2.conflicts.is_empty());
    assert_eq!(
        merge_tab2.closed_agents,
        vec!["antigravity-tab-2".to_string()],
        "only tab 2 (which committed edits) should be closed; in-flight tab 1 must remain active"
    );

    // Tab 1 now finishes generating its edit from the original 2-parameter baseline (`base_ts`),
    // overwriting `src/app.ts` on disk with `locale: string = "en-US"`, then running `post-tool` and `merge`.
    let tab1_ts = r#"export function formatCurrency(amount: number, currency: string, locale: string = "en-US"): string {
    return `${currency} ${amount.toFixed(2)}`;
}
"#;
    fs::write(&app_path, tab1_ts).expect("write tab 1 edit");
    let post_tab1 =
        handle_post_tool_hook(&coord, "antigravity-tab-1", "src/app.ts").expect("post-tool tab 1");
    assert!(
        post_tab1.auto_merged_concurrent_edit,
        "tab 1 must reconcile its baseline against tab 2's already-merged commit on main"
    );
    assert!(!post_tab1.has_conflict);

    let merge_tab1 = coord
        .merge_agents_with_message(
            &[],
            Some("main"),
            None,
            false,
            Some("feat(formatCurrency): add locale parameter"),
        )
        .expect("merge tab 1");
    assert!(merge_tab1.conflicts.is_empty());
    assert_eq!(
        merge_tab1.closed_agents,
        vec!["antigravity-tab-1".to_string()]
    );

    let final_ts = fs::read_to_string(&app_path).expect("read final app.ts");
    assert!(
        final_ts.contains("decimals: number = 2") && final_ts.contains("locale: string = \"en-US\""),
        "expected both decimals and locale parameters in merged app.ts, got:\n{final_ts}"
    );
}

/// Verifies per-branch isolation when concurrent sessions edit the same file on separate virtual branches.
#[test]
fn concurrent_sessions_on_different_branches_and_uncoordinated_writes_merge_cleanly() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_dir = tmp.path().join("src");
    fs::create_dir_all(&src_dir).expect("mkdir src");
    let app_path = src_dir.join("app.ts");

    let base_ts = r#"export function formatCurrency(amount: number, currency: string): string {
    try {
        return new Intl.NumberFormat("en-US", { style: "currency", currency }).format(amount);
    } catch {
        return `${currency} ${amount.toFixed(2)}`;
    }
}
"#;
    fs::write(&app_path, base_ts).expect("write base app.ts");

    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init repo");

    coord
        .claim_intent_and_symbols_with_branch(
            "antigravity-tab-1",
            Some("Add optional locale parameter to formatCurrency"),
            &["src/app.ts".to_string()],
            Some("feat/locale"),
        )
        .expect("claim tab 1 on feat/locale");

    coord
        .claim_intent_and_symbols_with_branch(
            "antigravity-tab-2",
            Some("Add optional decimals parameter to formatCurrency"),
            &["src/app.ts".to_string()],
            Some("feat/decimals"),
        )
        .expect("claim tab 2 on feat/decimals");

    // Simulate Tab 1 writing `tab1_ts` to disk, observed by the background watcher before Tab 1 runs `post-tool`,
    // and Tab 2 immediately overwriting `src/app.ts` on disk with `tab2_ts`.
    let tab1_ts = r#"export function formatCurrency(amount: number, currency: string, locale: string = "en-US"): string {
    try {
        return new Intl.NumberFormat(locale, { style: "currency", currency }).format(amount);
    } catch {
        return `${currency} ${amount.toFixed(2)}`;
    }
}
"#;
    let tab1_blob = coord
        .git()
        .write_blob(tab1_ts.as_bytes())
        .expect("write tab 1 blob");
    coord
        .oplog()
        .record_tx("watcher", "observe tab 1 write".to_string(), true, |view| {
            view.observed_blobs
                .entry("src/app.ts".to_string())
                .or_default()
                .push(tab1_blob);
            Ok(())
        })
        .expect("record observed tab 1 blob");

    let tab2_ts = r#"export function formatCurrency(amount: number, currency: string, decimals: number = 2): string {
    try {
        return new Intl.NumberFormat("en-US", { style: "currency", currency }).format(amount);
    } catch {
        return `${currency} ${amount.toFixed(decimals)}`;
    }
}
"#;
    fs::write(&app_path, tab2_ts).expect("tab 2 overwrites disk before tab 1 runs post-tool");

    let post1 =
        handle_post_tool_hook(&coord, "antigravity-tab-1", "src/app.ts").expect("post-tool 1");
    assert!(post1.auto_merged_concurrent_edit);
    assert!(!post1.has_conflict);

    let post2 =
        handle_post_tool_hook(&coord, "antigravity-tab-2", "src/app.ts").expect("post-tool 2");
    assert!(!post2.has_conflict);

    let m1 = coord
        .merge_agents_with_message(
            &["antigravity-tab-1".to_string()],
            None,
            None,
            false,
            Some("feat(locale): add locale parameter"),
        )
        .expect("merge tab 1 into feat/locale");
    assert_eq!(m1.target_branch, "feat/locale");

    let m2 = coord
        .merge_agents_with_message(
            &["antigravity-tab-2".to_string()],
            None,
            None,
            false,
            Some("feat(decimals): add decimals parameter"),
        )
        .expect("merge tab 2 into feat/decimals");
    assert_eq!(m2.target_branch, "feat/decimals");

    let locale_tree = coord
        .git()
        .read_tree_to_map(&m1.merged_tree_id)
        .expect("read feat/locale tree");
    let locale_str = String::from_utf8(
        coord
            .git()
            .read_blob(&locale_tree["src/app.ts"].blob_id)
            .expect("read locale blob"),
    )
    .expect("utf8");
    assert!(locale_str.contains("locale: string = \"en-US\""));
    assert!(!locale_str.contains("decimals: number = 2"));

    let decimals_tree = coord
        .git()
        .read_tree_to_map(&m2.merged_tree_id)
        .expect("read feat/decimals tree");
    let decimals_str = String::from_utf8(
        coord
            .git()
            .read_blob(&decimals_tree["src/app.ts"].blob_id)
            .expect("read decimals blob"),
    )
    .expect("utf8");
    assert!(decimals_str.contains("decimals: number = 2"));
    assert!(!decimals_str.contains("locale: string = \"en-US\""));

    let main_before_merge = fs::read_to_string(&app_path).expect("read main app.ts before branch merge");
    assert_eq!(main_before_merge, base_ts);
    let git_status_main = Command::new("git")
        .current_dir(tmp.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["status", "--porcelain"])
        .output()
        .expect("git status on main");
    assert!(
        git_status_main.status.success() && git_status_main.stdout.is_empty(),
        "expected clean git status on main after non-HEAD branch commits, got: {}",
        String::from_utf8_lossy(&git_status_main.stdout)
    );

    let nm_dir = tmp.path().join("node_modules").join("dummy-pkg");
    fs::create_dir_all(&nm_dir).expect("mkdir node_modules");
    fs::write(nm_dir.join("index.js"), "module.exports = 42;\n").expect("write dummy pkg");

    let sw1 = coord
        .switch_branch("term-1", "feat/locale")
        .expect("switch term-1 to feat/locale");
    let sw2 = coord
        .switch_branch("term-2", "feat/decimals")
        .expect("switch term-2 to feat/decimals");
    assert_ne!(sw1.slot, sw2.slot);

    let rc1 = coord
        .exec_on_branch(
            "term-1",
            None,
            &[
                "sh".to_string(),
                "-c".to_string(),
                "grep -q 'locale: string' src/app.ts && ! grep -q 'decimals: number' src/app.ts && test -f node_modules/dummy-pkg/index.js && test \"$TES_BRANCH\" = 'feat/locale'"
                    .to_string(),
            ],
        )
        .expect("exec on feat/locale");
    assert_eq!(rc1, 0);

    let rc2 = coord
        .exec_on_branch(
            "term-2",
            None,
            &[
                "sh".to_string(),
                "-c".to_string(),
                "grep -q 'decimals: number' src/app.ts && ! grep -q 'locale: string' src/app.ts && test -f node_modules/dummy-pkg/index.js && test \"$TES_BRANCH\" = 'feat/decimals'"
                    .to_string(),
            ],
        )
        .expect("exec on feat/decimals");
    assert_eq!(rc2, 0);

    let m_main = coord
        .merge_agents_with_message(
            &["feat/locale".to_string(), "feat/decimals".to_string()],
            Some("main"),
            None,
            false,
            Some("feat: merge locale and decimals branches"),
        )
        .expect("merge both branches into main");
    assert!(m_main.conflicts.is_empty());

    let main_ts = fs::read_to_string(&app_path).expect("read main app.ts");
    assert!(main_ts.contains("locale: string = \"en-US\""));
    assert!(main_ts.contains("decimals: number = 2"));
    assert!(main_ts.contains("Intl.NumberFormat(locale"));
    assert!(main_ts.contains("amount.toFixed(decimals)"));
}

/// Verifies file and branch restoration (`tes restore`), dynamic tab completions (`tes complete`), and editor branch switching.
#[test]
fn restore_and_dynamic_tab_completion_e2e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app_path = tmp.path().join("app.ts");
    let v1 = "export const version = 1;\n";
    let v2 = "export const version = 2;\n";
    let v3_dirty = "export const version = 999;\n";

    fs::write(&app_path, v1).expect("write v1");
    let cli_bin = env!("CARGO_BIN_EXE_tes");

    let init_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .arg("init")
        .output()
        .expect("tes init");
    assert!(init_out.status.success());

    let coord = Coordinator::open(tmp.path()).expect("open coord");
    let c1 = coord
        .git()
        .resolve_head_commit()
        .expect("resolve head")
        .expect("head c1");

    coord
        .create_checkpoint("term-1", "cp-v1")
        .expect("create checkpoint cp-v1");
    coord
        .claim_intent_and_symbols_with_branch(
            "agent-v2",
            Some("Bump version to 2"),
            &["app.ts".to_string()],
            Some("main"),
        )
        .expect("claim v2");
    fs::write(&app_path, v2).expect("write v2");
    handle_post_tool_hook(&coord, "agent-v2", "app.ts").expect("post-tool v2");
    let m_v2 = coord
        .merge_agents_with_message(&["agent-v2".to_string()], Some("main"), None, false, Some("feat: v2"))
        .expect("merge v2");
    assert_ne!(m_v2.merged_commit_id, c1);
    assert_eq!(fs::read_to_string(&app_path).expect("read v2"), v2);

    coord
        .switch_branch("term-demo", "feature/demo")
        .expect("switch feature/demo");

    fs::write(&app_path, v3_dirty).expect("write dirty v3");
    let restore_file_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["restore", "app.ts"])
        .output()
        .expect("tes restore app.ts");
    assert!(restore_file_out.status.success());
    assert_eq!(
        fs::read_to_string(&app_path).expect("read after file restore"),
        v2
    );

    let c1_short = &c1.as_str()[..7];
    let restore_src_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["restore", "--source", c1_short, "app.ts"])
        .output()
        .expect("tes restore --source c1 app.ts");
    assert!(restore_src_out.status.success());
    assert_eq!(
        fs::read_to_string(&app_path).expect("read after --source restore"),
        v1
    );

    let _ = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["restore", "app.ts"])
        .output()
        .expect("restore app.ts back to HEAD");
    assert_eq!(fs::read_to_string(&app_path).expect("read v2"), v2);

    let restore_branch_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["restore", "HEAD~1"])
        .output()
        .expect("tes restore HEAD~1");
    assert!(restore_branch_out.status.success());
    assert_eq!(
        fs::read_to_string(&app_path).expect("read after HEAD~1 restore"),
        v1
    );
    assert_eq!(
        coord
            .git()
            .resolve_head_commit()
            .expect("resolve head")
            .expect("head after restore"),
        c1
    );

    let comp_sub = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["complete", "sw"])
        .output()
        .expect("tes complete sw");
    assert!(comp_sub.status.success());
    let sub_out = String::from_utf8_lossy(&comp_sub.stdout);
    assert!(sub_out.lines().any(|l| l == "switch"));

    let comp_branches = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["complete", "switch", ""])
        .output()
        .expect("tes complete switch ''");
    assert!(comp_branches.status.success());
    let branches_out = String::from_utf8_lossy(&comp_branches.stdout);
    assert!(branches_out.lines().any(|l| l == "main"));
    assert!(branches_out.lines().any(|l| l == "feature/demo"));

    let comp_restore = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["complete", "restore", ""])
        .output()
        .expect("tes complete restore ''");
    assert!(comp_restore.status.success());
    let restore_out = String::from_utf8_lossy(&comp_restore.stdout);
    assert!(restore_out.lines().any(|l| l == "cp-v1"));
    assert!(restore_out.lines().any(|l| l == "HEAD~1"));

    // `feature/demo` was created at `v2` before `main` was restored to `v1` (`HEAD~1`).
    let sw_editor = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["switch", "--editor", "feature/demo"])
        .output()
        .expect("tes switch --editor feature/demo");
    assert!(sw_editor.status.success());
    assert_eq!(
        fs::read_to_string(&app_path).expect("read app.ts on feature/demo"),
        v2
    );

    let br_json_out = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["--json", "branch"])
        .output()
        .expect("tes --json branch");
    assert!(br_json_out.status.success());
    let br_json_str = String::from_utf8_lossy(&br_json_out.stdout);
    assert!(br_json_str.contains("\"editor_branch\": \"feature/demo\""));
    assert!(br_json_str.contains("\"branch_details\""));
    assert!(br_json_str.contains("\"commit_graph\""));

    let sw_back = Command::new(cli_bin)
        .current_dir(tmp.path())
        .args(["switch", "--editor", "main"])
        .output()
        .expect("tes switch --editor main");
    assert!(sw_back.status.success());
    assert_eq!(
        fs::read_to_string(&app_path).expect("read app.ts back on main"),
        v1
    );
}

/// Verifies that two branch-isolated agents editing shared HTML/JS/CSS files in place do not leak changes across branches.
#[test]
fn two_branch_agents_editing_shared_files_in_place_stay_strictly_isolated() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_dir = tmp.path().join("src");
    fs::create_dir_all(&src_dir).expect("mkdir src");

    let index_path = tmp.path().join("index.html");
    let main_js_path = src_dir.join("main.js");
    let style_css_path = src_dir.join("style.css");

    let base_index = "<!doctype html>\n<html><body><div id=\"app\"></div><script type=\"module\" src=\"/src/main.js\"></script></body></html>\n";
    let base_main_js = r#"import './style.css'

document.querySelector('#app').innerHTML = `
  <div>
    <h1>Vite App</h1>
    <div class="card">
      <button id="counter" type="button">count is 0</button>
    </div>
  </div>
`
"#;
    let base_style_css = ":root {\n  color: #213547;\n  background-color: #ffffff;\n}\n.card {\n  padding: 2em;\n}\n";

    fs::write(&index_path, base_index).expect("write base index.html");
    fs::write(&main_js_path, base_main_js).expect("write base main.js");
    fs::write(&style_css_path, base_style_css).expect("write base style.css");

    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init repo");

    coord
        .claim_intent_and_symbols_with_branch(
            "antigravity-theme",
            Some("Add light/dark theme toggle"),
            &[
                "index.html".to_string(),
                "src/main.js".to_string(),
                "src/style.css".to_string(),
            ],
            Some("feature/theme"),
        )
        .expect("claim feature/theme");

    coord
        .claim_intent_and_symbols_with_branch(
            "antigravity-counter",
            Some("Add decrement and reset counter buttons"),
            &["src/main.js".to_string(), "src/style.css".to_string()],
            Some("feature/counter-controls"),
        )
        .expect("claim feature/counter-controls");

    let theme_index = "<!doctype html>\n<html data-theme=\"dark\"><body><div id=\"app\"></div><script type=\"module\" src=\"/src/main.js\"></script></body></html>\n";
    let theme_main_js = r#"import './style.css'

document.querySelector('#app').innerHTML = `
  <div>
    <button id="theme-toggle" type="button">Toggle Theme</button>
    <h1>Vite App</h1>
    <div class="card">
      <button id="counter" type="button">count is 0</button>
    </div>
  </div>
`
"#;
    let theme_style_css = ":root {\n  color: #213547;\n  background-color: #ffffff;\n}\n.card {\n  padding: 2em;\n}\n.theme-toggle {\n  border: 1px solid #646cff;\n}\n";

    fs::write(&index_path, theme_index).expect("write theme index.html");
    handle_post_tool_hook(&coord, "antigravity-theme", "index.html").expect("post-tool index.html");
    fs::write(&main_js_path, theme_main_js).expect("write theme main.js");
    handle_post_tool_hook(&coord, "antigravity-theme", "src/main.js").expect("post-tool main.js");
    fs::write(&style_css_path, theme_style_css).expect("write theme style.css");
    handle_post_tool_hook(&coord, "antigravity-theme", "src/style.css").expect("post-tool style.css");

    // Agent 2 edits `src/main.js` and `src/style.css` in place on top of Agent 1's files on disk.
    let combined_main_js_on_disk = r#"import './style.css'

document.querySelector('#app').innerHTML = `
  <div>
    <button id="theme-toggle" type="button">Toggle Theme</button>
    <h1>Vite App</h1>
    <div class="card">
      <button id="counter" type="button">count is 0</button>
      <button id="decrement" type="button">-1</button>
      <button id="reset" type="button">Reset</button>
    </div>
  </div>
`
"#;
    let combined_style_css_on_disk = ":root {\n  color: #213547;\n  background-color: #ffffff;\n}\n.card {\n  padding: 2em;\n}\n.theme-toggle {\n  border: 1px solid #646cff;\n}\n.counter-controls {\n  display: flex;\n  gap: 8px;\n}\n";

    fs::write(&main_js_path, combined_main_js_on_disk).expect("write counter main.js on top of theme");
    handle_post_tool_hook(&coord, "antigravity-counter", "src/main.js").expect("post-tool counter main.js");
    fs::write(&style_css_path, combined_style_css_on_disk).expect("write counter style.css on top of theme");
    handle_post_tool_hook(&coord, "antigravity-counter", "src/style.css").expect("post-tool counter style.css");

    let m_theme = coord
        .merge_agents_with_message(
            &["antigravity-theme".to_string()],
            None,
            None,
            false,
            Some("feat(theme): add light/dark theme toggle"),
        )
        .expect("merge feature/theme");
    assert_eq!(m_theme.target_branch, "feature/theme");

    let m_counter = coord
        .merge_agents_with_message(
            &["antigravity-counter".to_string()],
            None,
            None,
            false,
            Some("feat(counter): add decrement and reset buttons"),
        )
        .expect("merge feature/counter-controls");
    assert_eq!(m_counter.target_branch, "feature/counter-controls");

    let theme_tree = coord
        .git()
        .read_tree_to_map(&m_theme.merged_tree_id)
        .expect("read theme tree");
    let theme_js_committed = String::from_utf8(
        coord
            .git()
            .read_blob(&theme_tree["src/main.js"].blob_id)
            .expect("read theme main.js"),
    )
    .expect("utf8");
    let theme_css_committed = String::from_utf8(
        coord
            .git()
            .read_blob(&theme_tree["src/style.css"].blob_id)
            .expect("read theme style.css"),
    )
    .expect("utf8");
    assert!(theme_js_committed.contains("theme-toggle"));
    assert!(
        !theme_js_committed.contains("decrement") && !theme_js_committed.contains("reset"),
        "feature/theme main.js must not contain counter-controls code:\n{theme_js_committed}"
    );
    assert!(theme_css_committed.contains(".theme-toggle"));
    assert!(
        !theme_css_committed.contains(".counter-controls"),
        "feature/theme style.css must not contain counter-controls CSS:\n{theme_css_committed}"
    );

    let counter_tree = coord
        .git()
        .read_tree_to_map(&m_counter.merged_tree_id)
        .expect("read counter tree");
    let counter_index_committed = String::from_utf8(
        coord
            .git()
            .read_blob(&counter_tree["index.html"].blob_id)
            .expect("read counter index.html"),
    )
    .expect("utf8");
    let counter_js_committed = String::from_utf8(
        coord
            .git()
            .read_blob(&counter_tree["src/main.js"].blob_id)
            .expect("read counter main.js"),
    )
    .expect("utf8");
    let counter_css_committed = String::from_utf8(
        coord
            .git()
            .read_blob(&counter_tree["src/style.css"].blob_id)
            .expect("read counter style.css"),
    )
    .expect("utf8");
    assert_eq!(
        counter_index_committed, base_index,
        "feature/counter-controls must not pick up index.html changes from feature/theme"
    );
    assert!(counter_js_committed.contains("decrement") && counter_js_committed.contains("reset"));
    assert!(
        !counter_js_committed.contains("theme-toggle"),
        "feature/counter-controls main.js must not contain theme-toggle code:\n{counter_js_committed}"
    );
    assert!(counter_css_committed.contains(".counter-controls"));
    assert!(
        !counter_css_committed.contains(".theme-toggle"),
        "feature/counter-controls style.css must not contain theme-toggle CSS:\n{counter_css_committed}"
    );
}

/// Verifies passive terminal tracking on `.git/HEAD` changes, `GIT_DIR` injection in sandboxes, revision range diffs, and `cherry-pick`.
#[test]
fn seamless_branch_switching_cherry_pick_range_diff_and_path_stash_e2e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(
        root.join("lib.rs"),
        "pub fn one() -> i32 {\n    1\n}\n\npub fn two() -> i32 {\n    2\n}\n",
    )
    .expect("write lib.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    let term_meta = coord
        .ensure_session("term-424242", None)
        .expect("ensure term-424242");
    assert_eq!(term_meta.target_branch.as_deref(), Some("main"));

    coord
        .switch_editor_branch("feat/ui")
        .expect("switch_editor_branch feat/ui");
    coord.sync_all_agents().expect("sync_all_agents after checkout");
    let cur = coord
        .oplog()
        .load_current_reconciled()
        .expect("load current");
    assert_eq!(
        cur.view
            .agents
            .get("term-424242")
            .and_then(|a| a.target_branch.as_deref()),
        Some("feat/ui"),
        "passive terminal session must follow .git/HEAD after post-checkout sync_all_agents"
    );

    let (_, _, env_map) = coord
        .prepare_branch_sandbox("term-424242", Some("feat/ui"))
        .expect("prepare_branch_sandbox");
    assert!(env_map.contains_key("GIT_DIR"));
    assert!(env_map.contains_key("GIT_WORK_TREE"));
    let exit_code = coord
        .exec_on_branch(
            "term-424242",
            Some("feat/ui"),
            &[
                "git".to_string(),
                "rev-parse".to_string(),
                "--verify".to_string(),
                "HEAD".to_string(),
            ],
        )
        .expect("exec git rev-parse HEAD in sandbox");
    assert_eq!(exit_code, 0);

    coord
        .switch_editor_branch("main")
        .expect("switch_editor_branch main");
    coord
        .claim_intent_and_symbols_with_branch(
            "agent-ui",
            Some("Add three()"),
            &["lib.rs".to_string()],
            Some("feat/ui"),
        )
        .expect("claim feat/ui");
    fs::write(
        root.join("lib.rs"),
        "pub fn one() -> i32 {\n    1\n}\n\npub fn two() -> i32 {\n    2\n}\n\npub fn three() -> i32 {\n    3\n}\n",
    )
    .expect("write three()");
    handle_post_tool_hook(&coord, "agent-ui", "lib.rs").expect("post-tool agent-ui");
    let ui_merge = coord
        .merge_agents_with_message(
            &["agent-ui".to_string()],
            Some("feat/ui"),
            None,
            true,
            Some("feat(ui): add three()"),
        )
        .expect("commit feat/ui");

    let tes_bin = env!("CARGO_BIN_EXE_tes");

    let diff_out = Command::new(tes_bin)
        .current_dir(root)
        .args(["diff", "main..feat/ui"])
        .output()
        .expect("tes diff main..feat/ui");
    assert!(diff_out.status.success());
    let diff_stdout = String::from_utf8_lossy(&diff_out.stdout);
    assert!(
        diff_stdout.contains("pub fn three()"),
        "revision range diff must show three(): {diff_stdout}"
    );

    let cp_out = Command::new(tes_bin)
        .current_dir(root)
        .args([
            "cherry-pick",
            ui_merge.merged_commit_id.as_str(),
            "--branch",
            "main",
            "--json",
        ])
        .output()
        .expect("tes cherry-pick");
    assert!(cp_out.status.success());
    let main_lib = fs::read_to_string(root.join("lib.rs")).expect("read lib.rs after cherry-pick");
    assert!(main_lib.contains("pub fn three()"));
}

/// Verifies Linux/BSD terminal session detection (`GNOME_TERMINAL_SCREEN`, `KONSOLE_DBUS_SESSION`) and multi-shell hook installers.
#[test]
fn linux_bsd_terminal_sessions_and_all_shell_env_hooks_e2e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn base() -> i32 { 1 }\n").expect("write lib.rs");

    let tes_bin = env!("CARGO_BIN_EXE_tes");

    let init_out = Command::new(tes_bin)
        .current_dir(root)
        .args(["init"])
        .output()
        .expect("tes init");
    assert!(init_out.status.success());

    // Simulate two concurrent Linux / BSD terminal tabs (GNOME Terminal vs KDE Konsole)
    // without TES_SESSION_ID set, verifying each gets isolated per-tab branch bindings.
    let sw_gnome = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("TES_SESSION_ID")
        .env_remove("ANTIGRAVITY_CONVERSATION_ID")
        .env_remove("TERM_SESSION_ID")
        .env_remove("ITERM_SESSION_ID")
        .env(
            "GNOME_TERMINAL_SCREEN",
            "/org/gnome/Terminal/screen/99887766_5544_3322_1100",
        )
        .args(["switch", "feat/linux-gnome"])
        .output()
        .expect("switch gnome");
    assert!(sw_gnome.status.success());

    let sw_konsole = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("TES_SESSION_ID")
        .env_remove("ANTIGRAVITY_CONVERSATION_ID")
        .env_remove("TERM_SESSION_ID")
        .env_remove("ITERM_SESSION_ID")
        .env("KONSOLE_DBUS_SESSION", "/Sessions/42")
        .args(["switch", "feat/bsd-konsole"])
        .output()
        .expect("switch konsole");
    assert!(sw_konsole.status.success());

    let env_gnome = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("TES_SESSION_ID")
        .env_remove("ANTIGRAVITY_CONVERSATION_ID")
        .env_remove("TERM_SESSION_ID")
        .env_remove("ITERM_SESSION_ID")
        .env(
            "GNOME_TERMINAL_SCREEN",
            "/org/gnome/Terminal/screen/99887766_5544_3322_1100",
        )
        .args(["env"])
        .output()
        .expect("env gnome");
    let gnome_stdout = String::from_utf8_lossy(&env_gnome.stdout);
    assert!(
        gnome_stdout.contains("TES_BRANCH='feat/linux-gnome'"),
        "GNOME Terminal tab must stay on feat/linux-gnome: {gnome_stdout}"
    );

    let env_konsole = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("TES_SESSION_ID")
        .env_remove("ANTIGRAVITY_CONVERSATION_ID")
        .env_remove("TERM_SESSION_ID")
        .env_remove("ITERM_SESSION_ID")
        .env("KONSOLE_DBUS_SESSION", "/Sessions/42")
        .args(["env"])
        .output()
        .expect("env konsole");
    let konsole_stdout = String::from_utf8_lossy(&env_konsole.stdout);
    assert!(
        konsole_stdout.contains("TES_BRANCH='feat/bsd-konsole'"),
        "KDE Konsole tab must stay on feat/bsd-konsole: {konsole_stdout}"
    );

    for flag in [
        "--zsh", "--bash", "--fish", "--sh", "--tcsh", "--nu", "--elvish", "--pwsh",
    ] {
        let out = Command::new(tes_bin)
            .current_dir(root)
            .args(["env", flag])
            .output()
            .expect("tes env flag");
        assert!(out.status.success(), "tes env {flag} failed");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            !text.trim().is_empty(),
            "tes env {flag} must emit non-empty hook script"
        );
    }

    let fake_home = root.join("fake-home");
    fs::create_dir_all(fake_home.join(".config/fish")).expect("mkdir fish");
    for rc in [".profile", ".kshrc", ".mkshrc", ".tcshrc", ".cshrc"] {
        fs::write(fake_home.join(rc), "# existing profile\n").expect("write rc");
    }

    let install_out = Command::new(tes_bin)
        .current_dir(root)
        .env("HOME", &fake_home)
        .args(["env", "--install", "--json"])
        .output()
        .expect("tes env --install");
    assert!(install_out.status.success());

    for rc in [
        ".zshrc",
        ".bashrc",
        ".profile",
        ".kshrc",
        ".mkshrc",
        ".tcshrc",
        ".cshrc",
        ".config/fish/config.fish",
    ] {
        let content = fs::read_to_string(fake_home.join(rc)).expect("read configured rc");
        assert!(
            content.contains("tes env"),
            "{rc} should contain `tes env` hook after `tes env --install`: {content}"
        );
    }
}

/// Verifies `tes config trust` gating for repo-tracked `tessera.toml`, shell single-quoting in `tes env`,
/// and rejection of path traversal and option injection across CLI/Coordinator boundaries.
#[test]
fn security_trust_gate_shell_escaping_and_path_confinement_e2e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn safe() -> i32 { 1 }\n").expect("write lib.rs");

    let tes_bin = env!("CARGO_BIN_EXE_tes");

    let init_out = Command::new(tes_bin)
        .current_dir(root)
        .args(["init"])
        .output()
        .expect("tes init");
    assert!(init_out.status.success());

    // Write an untrusted repo-root `tessera.toml` attempting to set `verify_command` and `custom_env`.
    fs::write(
        root.join("tessera.toml"),
        "verify_command = \"echo pwned\"\n[custom_env]\nLD_PRELOAD = \"/tmp/evil.so\"\nAPP_ENV = \"staging\"\n",
    )
    .expect("write untrusted tessera.toml");

    let coord = Coordinator::open(root).expect("open coordinator");
    let untrusted_cfg = coord.oplog().load_config().expect("load untrusted config");
    assert!(
        untrusted_cfg.verify_command.is_none(),
        "untrusted tessera.toml must not activate verify_command before `tes config trust`"
    );
    assert!(
        untrusted_cfg.custom_env.is_empty(),
        "untrusted tessera.toml must not activate custom_env before `tes config trust`"
    );

    // Explicitly trust `tessera.toml` via `tes config trust`.
    let trust_out = Command::new(tes_bin)
        .current_dir(root)
        .args(["config", "trust"])
        .output()
        .expect("tes config trust");
    assert!(trust_out.status.success());

    let trusted_cfg = coord.oplog().load_config().expect("load trusted config");
    assert_eq!(
        trusted_cfg.verify_command.as_deref(),
        Some("echo pwned")
    );
    assert_eq!(
        trusted_cfg.custom_env.get("APP_ENV").map(String::as_str),
        Some("staging")
    );

    // Modifying `tessera.toml` after trust invalidates the Blake3 trust hash automatically.
    fs::write(
        root.join("tessera.toml"),
        "verify_command = \"echo modified\"\n",
    )
    .expect("modify tessera.toml");
    let modified_cfg = coord.oplog().load_config().expect("load modified config");
    assert!(
        modified_cfg.verify_command.is_none(),
        "modified tessera.toml must require re-running `tes config trust`"
    );

    // Verify path traversal and flag injection are rejected at the Coordinator boundary.
    assert!(handle_post_tool_hook(&coord, "agent-sec", "../../etc/passwd").is_err());
    assert!(coord.switch_branch("agent-sec", "--upload-pack=evil").is_err());
    assert!(coord.switch_branch("agent-sec", "../evil").is_err());
}

/// Executes live shell integration hooks inside actual shell binaries (`zsh`, `bash`, `sh`, `dash`,
/// `ksh`, `tcsh`, `fish`, `nu`, `elvish`) and verifies per-terminal branch isolation and shell-injection safety.
#[test]
fn real_shell_binaries_zsh_bash_sh_dash_ksh_tcsh_fish_nu_elvish_e2e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Use a directory name containing single quotes and `$()` (without colons so it is legal on all filesystems)
    // to prove `eval "$(tes env)"` never executes command substitutions.
    let pwn_marker = tmp.path().join("pwned_marker");
    let tricky_name = "repo 'quoted' $(touch ../pwned_marker)";
    let root = tmp.path().join(tricky_name);
    fs::create_dir_all(&root).expect("mkdir tricky root");
    fs::write(root.join("app.rs"), "pub fn main() {}\n").expect("write app.rs");

    let tes_bin = std::path::Path::new(env!("CARGO_BIN_EXE_tes"));
    let bin_dir = tes_bin.parent().expect("tes bin parent");
    let orig_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin_dir.to_path_buf()];
    paths.extend(std::env::split_paths(&orig_path));
    let path_with_tes = std::env::join_paths(paths)
        .expect("join PATH")
        .into_string()
        .expect("PATH utf8");

    let init_out = Command::new(tes_bin)
        .current_dir(&root)
        .env("PATH", &path_with_tes)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["init"])
        .output()
        .expect("tes init");
    assert!(init_out.status.success());

    #[cfg(unix)]
    {
        for (shell_bin, init_flag, branch_slug) in [
            ("zsh", "--zsh", "feat/real-zsh"),
            ("bash", "--bash", "feat/real-bash"),
            ("sh", "--sh", "feat/real-sh"),
            ("dash", "--sh", "feat/real-dash"),
            ("ksh", "--sh", "feat/real-ksh"),
        ] {
            let Some(resolved_shell) = find_executable_in_path(shell_bin, &path_with_tes) else {
                continue;
            };

            let script = format!(
                "eval \"$(tes env {init_flag})\"\ntes switch {branch_slug} >/dev/null\neval \"$(tes env)\"\nprintf 'BRANCH=%s|PORT=%s|WS=%s\\n' \"$TES_BRANCH\" \"$TES_PORT\" \"$TES_WORKSPACE\"\n"
            );
            let out = Command::new(&resolved_shell)
                .current_dir(&root)
                .env("PATH", &path_with_tes)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("TES_SESSION_ID")
                .env_remove("ANTIGRAVITY_CONVERSATION_ID")
                .env_remove("TERM_SESSION_ID")
                .env_remove("ITERM_SESSION_ID")
                .args(["-c", &script])
                .output()
                .expect("run POSIX family shell");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "{shell_bin} failed (status={:?}): stdout={stdout}, stderr={stderr}",
                out.status.code()
            );
            assert!(
                stdout.contains(&format!("BRANCH={branch_slug}|")),
                "{shell_bin} did not set TES_BRANCH={branch_slug}: stdout={stdout}, stderr={stderr}"
            );
            assert!(
                !pwn_marker.exists(),
                "{shell_bin} executed injected command substitution from path!"
            );
        }

        if let Some(fish_bin) = find_executable_in_path("fish", &path_with_tes) {
            let script = "tes env --fish | source; tes switch feat/real-fish >/dev/null; printf 'BRANCH=%s|PORT=%s|WS=%s\\n' \"$TES_BRANCH\" \"$TES_PORT\" \"$TES_WORKSPACE\"";
            let out = Command::new(&fish_bin)
                .current_dir(&root)
                .env("PATH", &path_with_tes)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("TES_SESSION_ID")
                .env_remove("ANTIGRAVITY_CONVERSATION_ID")
                .args(["-c", script])
                .output()
                .expect("run fish");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "fish failed: stdout={stdout}, stderr={stderr}"
            );
            assert!(
                stdout.contains("BRANCH=feat/real-fish|"),
                "fish did not set TES_BRANCH=feat/real-fish: stdout={stdout}"
            );
        }

        if let Some(tcsh_bin) = find_executable_in_path("tcsh", &path_with_tes) {
            let script = "setenv TES_SESSION_ID term-$$; setenv TES_SHELL tcsh; tes switch feat/real-tcsh >/dev/null; eval \"`tes env`\"; echo \"BRANCH=${TES_BRANCH}|PORT=${TES_PORT}\"";
            let out = Command::new(&tcsh_bin)
                .current_dir(&root)
                .env("PATH", &path_with_tes)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("TES_SESSION_ID")
                .env_remove("ANTIGRAVITY_CONVERSATION_ID")
                .args(["-f", "-c", script])
                .output()
                .expect("run tcsh");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "tcsh failed: stdout={stdout}, stderr={stderr}"
            );
            assert!(
                stdout.contains("BRANCH=feat/real-tcsh|"),
                "tcsh did not set TES_BRANCH=feat/real-tcsh: stdout={stdout}"
            );
        }

        if let Some(elvish_bin) = find_executable_in_path("elvish", &path_with_tes) {
            let init_elvish = root.join("init.elv");
            let gen_out = Command::new(tes_bin)
                .current_dir(&root)
                .args(["env", "--elvish"])
                .output()
                .expect("tes env --elvish");
            let mut elv_code = String::from_utf8_lossy(&gen_out.stdout).to_string();
            elv_code.push_str("\ntes switch feat/real-elvish >/dev/null\nprint 'BRANCH='$E:TES_BRANCH'|PORT='$E:TES_PORT\"\\n\"\n");
            fs::write(&init_elvish, elv_code).expect("write init.elv");

            let out = Command::new(&elvish_bin)
                .current_dir(&root)
                .env("PATH", &path_with_tes)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("TES_SESSION_ID")
                .env_remove("ANTIGRAVITY_CONVERSATION_ID")
                .arg(&init_elvish)
                .output()
                .expect("run elvish");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "elvish failed: stdout={stdout}, stderr={stderr}"
            );
            assert!(
                stdout.contains("BRANCH=feat/real-elvish|"),
                "elvish did not set TES_BRANCH=feat/real-elvish: stdout={stdout}"
            );
        }
    }

    if let Some(nu_bin) = find_executable_in_path("nu", &path_with_tes) {
        let init_nu = root.join("init.nu");
        let gen_out = Command::new(tes_bin)
            .current_dir(&root)
            .args(["env", "--nu"])
            .output()
            .expect("tes env --nu");
        let mut nu_code = String::from_utf8_lossy(&gen_out.stdout).to_string();
        nu_code.push_str(
            "\ntes switch feat/real-nu | ignore\nprint $\"BRANCH=($env.TES_BRANCH)|PORT=($env.TES_PORT)\"\n",
        );
        fs::write(&init_nu, nu_code).expect("write init.nu");

        let out = Command::new(&nu_bin)
            .current_dir(&root)
            .env("PATH", &path_with_tes)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("TES_SESSION_ID")
            .env_remove("ANTIGRAVITY_CONVERSATION_ID")
            .arg(&init_nu)
            .output()
            .expect("run nu");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "nu failed: stdout={stdout}, stderr={stderr}"
        );
        assert!(
            stdout.contains("BRANCH=feat/real-nu|"),
            "nu did not set TES_BRANCH=feat/real-nu: stdout={stdout}"
        );
    }

    assert!(
        !pwn_marker.exists(),
        "shell hook executed injected command substitution from path!"
    );
}

/// Locates an executable binary in `path_env`.
fn find_executable_in_path(bin_name: &str, path_env: &str) -> Option<std::path::PathBuf> {
    for dir in std::env::split_paths(path_env) {
        let cand = dir.join(bin_name);
        if cand.is_file() {
            return Some(cand);
        }

        #[cfg(windows)]
        {
            let exe_cand = dir.join(format!("{bin_name}.exe"));
            if exe_cand.is_file() {
                return Some(exe_cand);
            }
        }
    }
    None
}

/// Simulates a real-world 5-agent concurrent polyglot development run (Rust + TypeScript + Vue + Cargo.toml + MCP stdio + secret `.env` filtering)
/// across multiple branches and shared working tree files, verifying AST reconciliation, Git `fsck` integrity, and hot-path latency.
#[test]
fn real_world_five_agent_concurrent_polyglot_and_mcp_stress_e2e() {
    use std::io::Write;
    use std::process::Stdio;
    use std::time::Instant;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::create_dir_all(root.join("src")).expect("mkdir src");

    let base_api_rs = r#"pub struct Order {
    pub id: u64,
    pub total_cents: u64,
}

pub fn health_check() -> &'static str {
    "ok"
}
"#;
    let base_format_ts = r#"export function formatCents(cents: number): string {
    return `$${(cents / 100).toFixed(2)}`;
}
"#;
    let base_dashboard_vue = r#"<template>
  <main class="dashboard">
    <h1>Orders</h1>
  </main>
</template>

<script setup lang="ts">
const title = "Orders";
</script>

<style scoped>
.dashboard {
  padding: 16px;
}
</style>
"#;
    let base_cargo_toml = r#"[package]
name = "commerce-service"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
"#;

    fs::write(root.join("src/api.rs"), base_api_rs).expect("write api.rs");
    fs::write(root.join("src/Format.ts"), base_format_ts).expect("write Format.ts");
    fs::write(root.join("src/Dashboard.vue"), base_dashboard_vue).expect("write Dashboard.vue");
    fs::write(root.join("Cargo.toml"), base_cargo_toml).expect("write Cargo.toml");

    let tes_bin = env!("CARGO_BIN_EXE_tes");

    let init_out = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["init"])
        .output()
        .expect("tes init");
    assert!(init_out.status.success());

    // Create an untracked `.env` file containing sensitive credentials.
    fs::write(
        root.join(".env"),
        "STRIPE_SECRET_KEY=sk_live_999999999999999999\nDATABASE_PASSWORD=top_secret_pw\n",
    )
    .expect("write .env");

    // Agent 1 (`claude-backend`) & Agent 2 (`cursor-auth`) claim `src/api.rs` and `Cargo.toml` on `main`.
    for (agent_id, intent) in [
        ("claude-backend", "Add create_order endpoint and uuid dependency"),
        ("cursor-auth", "Add verify_bearer_token helper and sha2 dependency"),
    ] {
        let out = Command::new(tes_bin)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args([
                "claim",
                "--agent",
                agent_id,
                "--intent",
                intent,
                "src/api.rs",
                "Cargo.toml",
            ])
            .output()
            .expect("claim main agent");
        assert!(out.status.success());

        for path in ["src/api.rs", "Cargo.toml"] {
            let pre = Command::new(tes_bin)
                .current_dir(root)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .args(["hook", "pre-tool", "--session", agent_id, "--path", path])
                .output()
                .expect("pre-tool");
            assert!(pre.status.success());
        }
    }

    // Agent 3 (`antigravity-ui`) claims `src/Dashboard.vue` and `src/Format.ts` on `feat/ui-redesign`.
    let claim_ui = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "claim",
            "--agent",
            "antigravity-ui",
            "--branch",
            "feat/ui-redesign",
            "--intent",
            "Add summary header to Dashboard.vue and formatDate to Format.ts",
            "src/Dashboard.vue",
            "src/Format.ts",
        ])
        .output()
        .expect("claim antigravity-ui");
    assert!(claim_ui.status.success());

    // Agent 4 (`codex-analytics`) claims `src/Dashboard.vue` and `src/Format.ts` on `feat/analytics`.
    let claim_analytics = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "claim",
            "--agent",
            "codex-analytics",
            "--branch",
            "feat/analytics",
            "--intent",
            "Add analytics card style to Dashboard.vue and formatMetric to Format.ts",
            "src/Dashboard.vue",
            "src/Format.ts",
        ])
        .output()
        .expect("claim codex-analytics");
    assert!(claim_analytics.status.success());

    // Agent 1 edits `src/api.rs` and `Cargo.toml` on `main`.
    let agent1_api_rs = r#"pub struct Order {
    pub id: u64,
    pub total_cents: u64,
}

pub fn health_check() -> &'static str {
    "ok"
}

pub fn create_order(id: u64, total_cents: u64) -> Order {
    Order { id, total_cents }
}
"#;
    let agent1_cargo = r#"[package]
name = "commerce-service"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
uuid = "1.10"
"#;
    fs::write(root.join("src/api.rs"), agent1_api_rs).expect("agent1 write api.rs");
    let t0 = Instant::now();
    let post1_api = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "hook",
            "post-tool",
            "--session",
            "claude-backend",
            "--path",
            "src/api.rs",
        ])
        .output()
        .expect("post-tool claude-backend api.rs");
    let post_tool_ms = t0.elapsed().as_millis();
    assert!(post1_api.status.success());

    fs::write(root.join("Cargo.toml"), agent1_cargo).expect("agent1 write Cargo.toml");
    let post1_cargo = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "hook",
            "post-tool",
            "--session",
            "claude-backend",
            "--path",
            "Cargo.toml",
        ])
        .output()
        .expect("post-tool claude-backend Cargo.toml");
    assert!(post1_cargo.status.success());

    // Agent 2 overwrites `src/api.rs` and `Cargo.toml` from its stale baseline (without Agent 1's edits),
    // and `tes hook post-tool` automatically 3-way AST-merges both files on disk!
    let agent2_api_rs = r#"pub struct Order {
    pub id: u64,
    pub total_cents: u64,
}

pub fn health_check() -> &'static str {
    "ok"
}

pub fn verify_bearer_token(token: &str) -> bool {
    token.starts_with("Bearer ")
}
"#;
    let agent2_cargo = r#"[package]
name = "commerce-service"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
sha2 = "0.10"
"#;
    fs::write(root.join("src/api.rs"), agent2_api_rs).expect("agent2 write api.rs");
    let post2_api = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "hook",
            "post-tool",
            "--session",
            "cursor-auth",
            "--path",
            "src/api.rs",
        ])
        .output()
        .expect("post-tool cursor-auth api.rs");
    assert!(post2_api.status.success());

    fs::write(root.join("Cargo.toml"), agent2_cargo).expect("agent2 write Cargo.toml");
    let post2_cargo = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "hook",
            "post-tool",
            "--session",
            "cursor-auth",
            "--path",
            "Cargo.toml",
        ])
        .output()
        .expect("post-tool cursor-auth Cargo.toml");
    assert!(post2_cargo.status.success());

    // Verify both Agent 1 and Agent 2 changes are live on disk on `main` right now!
    let reconciled_api = fs::read_to_string(root.join("src/api.rs")).expect("read reconciled api.rs");
    assert!(reconciled_api.contains("pub fn create_order"));
    assert!(reconciled_api.contains("pub fn verify_bearer_token"));
    let reconciled_cargo = fs::read_to_string(root.join("Cargo.toml")).expect("read reconciled Cargo.toml");
    assert!(reconciled_cargo.contains("uuid = \"1.10\""));
    assert!(reconciled_cargo.contains("sha2 = \"0.10\""));

    // Agent 3 (`antigravity-ui` on `feat/ui-redesign`) edits `src/Dashboard.vue` and `src/Format.ts`.
    let ui_vue = r#"<template>
  <main class="dashboard">
    <header class="summary-banner">Live Orders</header>
    <h1>Orders</h1>
  </main>
</template>

<script setup lang="ts">
const title = "Orders";
const subtitle = "Real-time feed";
</script>

<style scoped>
.dashboard {
  padding: 16px;
}
</style>
"#;
    let ui_format_ts = r#"export function formatCents(cents: number): string {
    return `$${(cents / 100).toFixed(2)}`;
}

export function formatDate(iso: string): string {
    return iso.slice(0, 10);
}
"#;
    fs::write(root.join("src/Dashboard.vue"), ui_vue).expect("ui write Dashboard.vue");
    assert!(
        Command::new(tes_bin)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args([
                "hook",
                "post-tool",
                "--session",
                "antigravity-ui",
                "--path",
                "src/Dashboard.vue",
            ])
            .output()
            .expect("post-tool ui vue")
            .status
            .success()
    );
    fs::write(root.join("src/Format.ts"), ui_format_ts).expect("ui write Format.ts");
    assert!(
        Command::new(tes_bin)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args([
                "hook",
                "post-tool",
                "--session",
                "antigravity-ui",
                "--path",
                "src/Format.ts",
            ])
            .output()
            .expect("post-tool ui Format.ts")
            .status
            .success()
    );

    // Agent 4 (`codex-analytics` on `feat/analytics`) edits `src/Dashboard.vue` and `src/Format.ts`.
    let analytics_vue = r#"<template>
  <main class="dashboard">
    <h1>Orders</h1>
  </main>
</template>

<script setup lang="ts">
const title = "Orders";
</script>

<style scoped>
.dashboard {
  padding: 16px;
}
.analytics-card {
  border-radius: 8px;
}
</style>
"#;
    let analytics_format_ts = r#"export function formatCents(cents: number): string {
    return `$${(cents / 100).toFixed(2)}`;
}

export function formatMetric(val: number): string {
    return `${val.toLocaleString()} ops/s`;
}
"#;
    fs::write(root.join("src/Dashboard.vue"), analytics_vue).expect("analytics write Dashboard.vue");
    assert!(
        Command::new(tes_bin)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args([
                "hook",
                "post-tool",
                "--session",
                "codex-analytics",
                "--path",
                "src/Dashboard.vue",
            ])
            .output()
            .expect("post-tool analytics vue")
            .status
            .success()
    );
    fs::write(root.join("src/Format.ts"), analytics_format_ts).expect("analytics write Format.ts");
    assert!(
        Command::new(tes_bin)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args([
                "hook",
                "post-tool",
                "--session",
                "codex-analytics",
                "--path",
                "src/Format.ts",
            ])
            .output()
            .expect("post-tool analytics Format.ts")
            .status
            .success()
    );

    // Agent 5 (`mcp-orchestrator`) queries `status`, `radar`, and creates a `checkpoint` over MCP stdio,
    // and also triggers `post-tool` on `.env` to verify secret files are never ingested.
    let _ = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["hook", "post-tool", "--session", "claude-backend", "--path", ".env"])
        .output();

    let mut mcp_child = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("TES_SESSION_ID", "mcp-orchestrator")
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn tes mcp");
    {
        let stdin = mcp_child.stdin.as_mut().expect("mcp stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
        )
        .expect("mcp init");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"radar","arguments":{{}}}}}}"#
        )
        .expect("mcp radar");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"checkpoint","arguments":{{"label":"pre-multi-merge"}}}}}}"#
        )
        .expect("mcp checkpoint");
    }
    let mcp_out = mcp_child.wait_with_output().expect("wait mcp");
    assert!(mcp_out.status.success());
    let mcp_stdout = String::from_utf8_lossy(&mcp_out.stdout);
    assert!(mcp_stdout.contains("claude-backend"));
    assert!(mcp_stdout.contains("cursor-auth"));
    assert!(mcp_stdout.contains("antigravity-ui"));
    assert!(mcp_stdout.contains("codex-analytics"));

    // Commit each branch (`feat/ui-redesign`, `feat/analytics`, and `main`) and AST-merge all branches into `main`.
    for (agent_id, branch_name, msg) in [
        ("antigravity-ui", "feat/ui-redesign", "feat(ui): add summary banner and formatDate"),
        ("codex-analytics", "feat/analytics", "feat(analytics): add analytics card and formatMetric"),
    ] {
        let c_out = Command::new(tes_bin)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args([
                "commit",
                "--agent",
                agent_id,
                "--branch",
                branch_name,
                "-m",
                msg,
            ])
            .output()
            .expect("commit feature branch");
        assert!(c_out.status.success());
    }

    let main_commit = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "merge",
            "claude-backend",
            "cursor-auth",
            "--into",
            "main",
            "-m",
            "feat(api): add create_order and verify_bearer_token",
        ])
        .output()
        .expect("merge main agents");
    assert!(main_commit.status.success());

    let all_merge = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args([
            "merge",
            "feat/ui-redesign",
            "feat/analytics",
            "--into",
            "main",
            "-m",
            "feat: merge ui-redesign and analytics into main",
        ])
        .output()
        .expect("merge feature branches into main");
    assert!(
        all_merge.status.success(),
        "branch merge failed: {}",
        String::from_utf8_lossy(&all_merge.stderr)
    );

    // Measure `tes env` fast-path latency.
    let env_t0 = Instant::now();
    let env_out = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["env"])
        .output()
        .expect("tes env");
    let env_ms = env_t0.elapsed().as_millis();
    assert!(env_out.status.success());
    assert!(
        env_ms < 250,
        "tes env fast path should complete quickly (took {env_ms}ms, post-tool took {post_tool_ms}ms)"
    );

    // Verify all 4 agents' codeacross Rust, TypeScript, Vue SFC, and Cargo.toml is cleanly present on `main`!
    let final_api = fs::read_to_string(root.join("src/api.rs")).expect("read final api.rs");
    assert!(final_api.contains("pub fn create_order"));
    assert!(final_api.contains("pub fn verify_bearer_token"));

    let final_cargo = fs::read_to_string(root.join("Cargo.toml")).expect("read final Cargo.toml");
    assert!(final_cargo.contains("uuid = \"1.10\""));
    assert!(final_cargo.contains("sha2 = \"0.10\""));

    let final_format = fs::read_to_string(root.join("src/Format.ts")).expect("read final Format.ts");
    assert!(final_format.contains("export function formatDate"));
    assert!(final_format.contains("export function formatMetric"));

    let final_vue = fs::read_to_string(root.join("src/Dashboard.vue")).expect("read final Dashboard.vue");
    assert!(final_vue.contains("summary-banner"));
    assert!(final_vue.contains("Real-time feed"));
    assert!(final_vue.contains(".analytics-card"));

    // Verify `.env` secret was never committed to the Git tree or recorded in `observed_blobs`.
    let coord = Coordinator::open(root).expect("open coord");
    let current = coord.oplog().load_current_reconciled().expect("load oplog");
    assert!(
        !current.view.observed_blobs.contains_key(".env"),
        ".env secret file must never be recorded in observed_blobs"
    );
    let head_cid = coord
        .git()
        .resolve_head_commit()
        .expect("resolve head")
        .expect("head commit");
    let head_commit = coord.git().read_commit(&head_cid).expect("read head commit");
    let head_tree = coord.git().read_tree_to_map(&head_commit.tree_id).expect("read head tree");
    assert!(
        !head_tree.contains_key(".env"),
        ".env secret file must never be committed into the Git tree"
    );

    let fsck = Command::new("git")
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["fsck", "--full"])
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck --full failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

/// Verifies Git-familiar CLI flags across `tes log`, `tes diff`, `tes status`, `tes commit --amend`, and `tes branch -v`.
#[test]
fn cli_git_familiar_flags_log_diff_status_commit_amend_and_branch_e2e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let docs_dir = root.join("docs");
    let src_dir = root.join("src");
    fs::create_dir_all(&docs_dir).expect("mkdir docs");
    fs::create_dir_all(&src_dir).expect("mkdir src");

    fs::write(docs_dir.join("guide.md"), "# Guide\n\nInitial guide.\n").expect("write guide.md");
    fs::write(src_dir.join("lib.rs"), "pub fn calc() -> i32 {\n    1\n}\n").expect("write lib.rs");

    let tes_bin = env!("CARGO_BIN_EXE_tes");

    let init_out = Command::new(tes_bin)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg("init")
        .output()
        .expect("tes init");
    assert!(init_out.status.success());

    // Claim and edit `docs/guide.md`.
    let claim_out = Command::new(tes_bin)
        .current_dir(root)
        .env("TES_SESSION_ID", "agent-docs")
        .args(["claim", "--intent", "Update guide", "docs/guide.md"])
        .output()
        .expect("tes claim docs/guide.md");
    assert!(claim_out.status.success());

    fs::write(
        docs_dir.join("guide.md"),
        "# Guide\n\nUpdated guide with examples.\n",
    )
    .expect("update guide.md");

    let hook_out = Command::new(tes_bin)
        .current_dir(root)
        .env("TES_SESSION_ID", "agent-docs")
        .args(["hook", "post-tool", "--path", "docs/guide.md"])
        .output()
        .expect("tes hook post-tool");
    assert!(hook_out.status.success());

    // Verify `tes status -sb` short format with branch header.
    let status_sb = Command::new(tes_bin)
        .current_dir(root)
        .env("TES_SESSION_ID", "agent-docs")
        .env("NO_COLOR", "1")
        .args(["status", "-sb"])
        .output()
        .expect("tes status -sb");
    assert!(status_sb.status.success());
    let status_sb_str = String::from_utf8_lossy(&status_sb.stdout);
    assert!(
        status_sb_str.contains("## main") && status_sb_str.contains("M  docs/guide.md"),
        "expected short status with branch header and staged M docs/guide.md, got:\n{status_sb_str}"
    );

    // Verify `tes diff --stat` and `tes diff --name-status -- docs/`.
    let diff_stat = Command::new(tes_bin)
        .current_dir(root)
        .env("TES_SESSION_ID", "agent-docs")
        .env("NO_COLOR", "1")
        .args(["diff", "--stat", "--", "docs/"])
        .output()
        .expect("tes diff --stat -- docs/");
    assert!(diff_stat.status.success());
    let diff_stat_str = String::from_utf8_lossy(&diff_stat.stdout);
    assert!(
        diff_stat_str.contains("docs/guide.md") && diff_stat_str.contains("1 file changed"),
        "expected diffstat for docs/guide.md, got:\n{diff_stat_str}"
    );

    let diff_ns = Command::new(tes_bin)
        .current_dir(root)
        .env("TES_SESSION_ID", "agent-docs")
        .env("NO_COLOR", "1")
        .args(["diff", "--name-status"])
        .output()
        .expect("tes diff --name-status");
    assert!(diff_ns.status.success());
    let diff_ns_str = String::from_utf8_lossy(&diff_ns.stdout);
    assert!(
        diff_ns_str.contains("M\tdocs/guide.md"),
        "expected M\\tdocs/guide.md in name-status output, got:\n{diff_ns_str}"
    );

    // Commit on `main`, then amend the commit message with `tes commit --amend`.
    let commit_out = Command::new(tes_bin)
        .current_dir(root)
        .env("TES_SESSION_ID", "agent-docs")
        .args(["commit", "-m", "docs: initial guide update"])
        .output()
        .expect("tes commit");
    assert!(commit_out.status.success());

    let amend_out = Command::new(tes_bin)
        .current_dir(root)
        .env("TES_SESSION_ID", "agent-docs")
        .args(["commit", "--amend", "-m", "docs(guide): add examples section"])
        .output()
        .expect("tes commit --amend");
    assert!(amend_out.status.success());

    // Verify `tes log -p -n 4 -- docs/` shows the amended commit message and patch for `docs/guide.md`.
    let log_patch = Command::new(tes_bin)
        .current_dir(root)
        .env("NO_COLOR", "1")
        .args(["-P", "log", "-p", "-n", "4", "--", "docs/"])
        .output()
        .expect("tes log -p -n 4 -- docs/");
    assert!(log_patch.status.success());
    let log_patch_str = String::from_utf8_lossy(&log_patch.stdout);
    assert!(
        log_patch_str.contains("docs(guide): add examples section")
            && !log_patch_str.contains("docs: initial guide update")
            && log_patch_str.contains("+Updated guide with examples."),
        "expected amended commit message and patch in tes log -p -n 4 -- docs/, got:\n{log_patch_str}"
    );

    // Verify `tes branch -v` displays the branch name, short SHA, and amended commit subject.
    let branch_v = Command::new(tes_bin)
        .current_dir(root)
        .env("NO_COLOR", "1")
        .args(["-P", "branch", "-v"])
        .output()
        .expect("tes branch -v");
    assert!(branch_v.status.success());
    let branch_v_str = String::from_utf8_lossy(&branch_v.stdout);
    assert!(
        branch_v_str.contains("main")
            && branch_v_str.contains("docs(guide): add examples section"),
        "expected branch -v to include branch name and commit subject, got:\n{branch_v_str}"
    );
}

#[test]
fn security_audit_hardening_invariants_e2e() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).expect("mkdir repo");
    fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").expect("write lib.rs");

    let tes_bin = env!("CARGO_BIN_EXE_tes");
    let init_out = Command::new(tes_bin)
        .current_dir(&root)
        .args(["init"])
        .output()
        .expect("tes init");
    assert!(init_out.status.success());

    let coord = Coordinator::open(&root).expect("open coordinator");

    for bad_ref in [
        "feat;id",
        "feat$(id)",
        "feat`id`",
        "feat|id",
        "feat&id",
        "feat'id",
        "feat\"id",
        ".tessera/evil",
    ] {
        assert!(
            core::validate_ref_name(bad_ref).is_err(),
            "expected validate_ref_name to reject '{bad_ref}'"
        );
        assert!(
            coord.switch_branch("sec-agent", bad_ref).is_err(),
            "expected switch_branch to reject '{bad_ref}'"
        );
    }

    assert!(core::validate_repo_rel_path(".tessera/repo_root").is_err());
    assert!(core::validate_repo_rel_path("sub/.tessera/config").is_err());
    assert!(handle_post_tool_hook(&coord, "sec-agent", ".tessera/repo_root").is_err());

    let forged_dir = tmp.path().join("forged-archive");
    fs::create_dir_all(forged_dir.join(".tessera")).expect("mkdir forged .tessera");
    fs::write(
        forged_dir.join(".tessera/repo_root"),
        root.display().to_string(),
    )
    .expect("write forged repo_root");
    assert!(
        core::is_valid_sandbox_repo_root(&forged_dir, None).is_none(),
        "forged .tessera/repo_root outside legitimate sandbox must be rejected"
    );
    assert!(
        core::GitBackend::discover(&forged_dir).is_err(),
        "GitBackend::discover must not follow forged .tessera/repo_root"
    );

    #[cfg(unix)]
    {
        let secret_file = tmp.path().join("external_secret.txt");
        fs::write(&secret_file, "TOP_SECRET_DATA").expect("write external secret");

        let _ = std::os::unix::fs::symlink(&secret_file, root.join("root_escape.txt"));
        fs::create_dir_all(root.join("subdir")).expect("mkdir subdir");
        let _ = std::os::unix::fs::symlink(&secret_file, root.join("subdir/nested_escape.txt"));
        let _ = std::os::unix::fs::symlink(
            std::path::Path::new("../lib.rs"),
            root.join("subdir/safe_rel.rs"),
        );

        let cow_dst = tmp.path().join("cow_ws");
        workspace::create_cow_workspace(&root, &cow_dst, &[], &[]).expect("create cow workspace");

        assert!(
            !cow_dst.join("root_escape.txt").exists(),
            "root-level symlink pointing outside repo must not be cloned"
        );
        assert!(
            !cow_dst.join("subdir/nested_escape.txt").exists(),
            "nested symlink pointing outside repo must be pruned even after clonefile"
        );
        assert!(
            cow_dst
                .join("subdir/safe_rel.rs")
                .symlink_metadata()
                .is_ok(),
            "safe relative symlink staying within workspace root must be preserved"
        );

        let _ = fs::remove_file(root.join("root_escape.txt"));
        let _ = fs::remove_file(root.join("subdir/nested_escape.txt"));
        let _ = fs::remove_file(root.join("subdir/safe_rel.rs"));
    }

    let mcp_req = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"merge\",\"arguments\":{\"verify\":\"echo pwned\"}}}\n";
    let mut mcp_child = Command::new(tes_bin)
        .current_dir(&root)
        .args(["mcp"])
        .stdin( std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn tes mcp");
    {
        use std::io::Write;
        let stdin = mcp_child.stdin.as_mut().expect("mcp stdin");
        stdin.write_all(mcp_req.as_bytes()).expect("write mcp req");
    }
    let mcp_out = mcp_child.wait_with_output().expect("wait mcp");
    let mcp_stdout = String::from_utf8_lossy(&mcp_out.stdout);
    assert!(
        mcp_stdout.contains("\"isError\":true")
            && mcp_stdout.contains("arbitrary verification shell commands are not permitted over MCP"),
        "expected MCP merge tool to reject arbitrary verify command, got: {mcp_stdout}"
    );

    for forbidden_key in [
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "GIT_SSH_COMMAND",
        "TES_PORT",
        "CARGO_BUILD_RUSTC",
        "RUSTC",
        "RUSTFLAGS",
        "HOME",
        "SHELL",
        "IFS",
        "RUBYOPT",
        "PERL5OPT",
        "JAVA_TOOL_OPTIONS",
        "DOTNET_STARTUP_HOOKS",
        "BASH_FUNC_myfn",
    ] {
        assert!(
            workspace::is_forbidden_custom_env_key(forbidden_key),
            "expected '{forbidden_key}' to be forbidden in custom_env"
        );
    }

    let set_env_out = Command::new(tes_bin)
        .current_dir(&root)
        .args(["config", "set-env", "RUBYOPT", "-e puts(1)"])
        .output()
        .expect("tes config set-env RUBYOPT");
    assert!(
        !set_env_out.status.success(),
        "tes config set-env must reject forbidden key RUBYOPT"
    );

    for secret_path in [
        ".envrc",
        "certs/client.p12",
        "certs/client.pfx",
        "keys/private.key",
        "id_ecdsa",
        ".pypirc",
        "credentials.json",
    ] {
        assert!(
            workspace::is_ignored_or_secret_path(&root, coord.git().git_dir(), secret_path, false),
            "expected untracked secret path '{secret_path}' to be excluded"
        );
    }

    let mut bad_conflict = core::ConflictRecord {
        id: core::ConflictId("abcdef0123456789".to_string()),
        path: "../../etc/passwd".to_string(),
        symbol: None,
        kind: core::ConflictKind::TextHunk,
        merge: core::Merge::resolved(None),
        agent_intents: std::collections::BTreeMap::new(),
        base_snippet: String::new(),
        left_snippet: String::new(),
        right_snippet: String::new(),
        diagnostics: None,
        created_at_ms: 0,
    };
    assert!(
        coord.oplog().save_conflict(&bad_conflict).is_err(),
        "save_conflict must reject path traversal"
    );
    bad_conflict.path = ".tessera/repo_root".to_string();
    assert!(
        coord.oplog().save_conflict(&bad_conflict).is_err(),
        "save_conflict must reject .tessera paths"
    );
}





