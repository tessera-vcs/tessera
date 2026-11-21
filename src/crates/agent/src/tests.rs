//! Unit and integration tests for the `agent` crate, covering all session, sandbox, merge,
//! conflict, hook, stash, pull, cherry-pick, undo, and harness auto-installer invariants.

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use core::{ConflictKind, SessionWaypoint};

use crate::harness::{
    strip_jsonc, upsert_claude_hooks_json, upsert_git_hook, upsert_marked_section, upsert_mcp_json,
    upsert_opencode_json, upsert_vscode_mcp_json,
};
use crate::hooks::{is_contaminated_by_foreign_peer, strip_peer_inserted_lines};
use crate::merge::apply_synced_agent_to_view;
use crate::session::{
    detect_session_id_with_env, detect_session_waypoint_with_env, is_ide_process_name,
    is_shell_or_cli_harness_process, percent_encode_uri_component,
};
use crate::worktree::{has_git_conflict_markers, is_harness_config_path};
use crate::{
    CommitOptions, Coordinator, ResolveStrategy, focus_session_waypoint, handle_post_tool_hook,
    handle_pre_tool_hook, install_harness_integrations_for,
};

#[test]
fn branch_delete_and_rename_clean_up_refs_sandboxes_and_sessions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("app.rs"), "pub fn main() {}\n").expect("write app.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    coord
        .claim_intent_and_symbols_with_branch(
            "agent-1",
            Some("Add feature A"),
            &["app.rs".to_string()],
            Some("feat/old-name"),
        )
        .expect("claim feat/old-name");

    fs::write(
        root.join("app.rs"),
        "pub fn main() {}\npub fn feat_a() -> i32 { 42 }\n",
    )
    .expect("write feat_a");
    handle_post_tool_hook(&coord, "agent-1", "app.rs").expect("post-tool feat_a");
    coord
        .merge_agents_with_message(
            &["agent-1".to_string()],
            Some("feat/old-name"),
            None,
            true,
            Some("feat: add feat_a"),
        )
        .expect("commit feat/old-name");

    let rename_rep = coord
        .rename_branch("agent-1", "feat/old-name", "feat/new-name")
        .expect("rename_branch");
    assert_eq!(rename_rep.old_branch, "feat/old-name");
    assert_eq!(rename_rep.new_branch, "feat/new-name");
    assert!(
        coord
            .git()
            .resolve_ref("refs/heads/feat/old-name")
            .expect("resolve old")
            .is_none()
    );
    assert!(
        coord
            .git()
            .resolve_ref("refs/heads/feat/new-name")
            .expect("resolve new")
            .is_some()
    );

    let cur = coord
        .oplog()
        .load_current_reconciled()
        .expect("load current");
    assert!(!cur.view.branches.contains_key("feat/old-name"));
    assert!(cur.view.branches.contains_key("feat/new-name"));
    assert_eq!(
        cur.view
            .agents
            .get("agent-1")
            .and_then(|a| a.target_branch.as_deref()),
        Some("feat/new-name")
    );

    let safe_err = coord.delete_branch("agent-1", "feat/new-name", false);
    assert!(
        safe_err.is_err(),
        "unmerged branch must be rejected when force = false"
    );

    let del_rep = coord
        .delete_branch("agent-1", "feat/new-name", true)
        .expect("force delete_branch");
    assert_eq!(del_rep.deleted_branch, "feat/new-name");
    assert!(del_rep.detached_sessions.contains(&"agent-1".to_string()));
    assert!(
        coord
            .git()
            .resolve_ref("refs/heads/feat/new-name")
            .expect("resolve deleted")
            .is_none()
    );
    assert!(!coord.branch_sandbox_dir("feat/new-name").exists());

    let cur_after = coord.oplog().load_current_reconciled().expect("load after");
    assert!(!cur_after.view.branches.contains_key("feat/new-name"));
    assert_eq!(
        cur_after
            .view
            .agents
            .get("agent-1")
            .and_then(|a| a.target_branch.as_deref()),
        None
    );
}

#[test]
fn session_isolated_stash_preserves_concurrent_peer_edits_and_ast_merges_on_pop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let initial_code = "pub fn alpha() -> i32 {\n    1\n}\n\npub fn beta() -> i32 {\n    2\n}\n";
    fs::write(root.join("lib.rs"), initial_code).expect("write lib.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    coord
        .claim_intent_and_symbols("agent-alpha", Some("Update alpha"), &["lib.rs".to_string()])
        .expect("claim alpha");
    coord
        .claim_intent_and_symbols("agent-beta", Some("Update beta"), &["lib.rs".to_string()])
        .expect("claim beta");

    let after_alpha = "pub fn alpha() -> i32 {\n    100\n}\n\npub fn beta() -> i32 {\n    2\n}\n";
    fs::write(root.join("lib.rs"), after_alpha).expect("write alpha edit");
    handle_post_tool_hook(&coord, "agent-alpha", "lib.rs").expect("post-tool alpha");

    let after_both = "pub fn alpha() -> i32 {\n    100\n}\n\npub fn beta() -> i32 {\n    200\n}\n";
    fs::write(root.join("lib.rs"), after_both).expect("write beta edit");
    handle_post_tool_hook(&coord, "agent-beta", "lib.rs").expect("post-tool beta");

    let stash_entry = coord
        .stash_push("agent-alpha", Some("WIP alpha"), None)
        .expect("stash_push alpha");
    assert_eq!(stash_entry.session_id, "agent-alpha");
    assert!(stash_entry.own_blobs.contains_key("lib.rs"));

    let disk_after_stash = fs::read_to_string(root.join("lib.rs")).expect("read lib.rs");
    assert!(
        disk_after_stash.contains("1\n") && !disk_after_stash.contains("100"),
        "alpha's stashed change should be removed from disk: {disk_after_stash}"
    );
    assert!(
        disk_after_stash.contains("200"),
        "beta's concurrent change must remain intact on disk: {disk_after_stash}"
    );

    let stashes = coord.stash_list().expect("stash_list");
    assert_eq!(stashes.len(), 1);

    let pop_rep = coord
        .stash_pop("agent-alpha", 0, false)
        .expect("stash_pop alpha");
    assert!(!pop_rep.has_conflicts);
    assert!(pop_rep.dropped);
    assert!(coord.stash_list().expect("stash_list after pop").is_empty());

    let disk_after_pop = fs::read_to_string(root.join("lib.rs")).expect("read lib.rs after pop");
    assert!(
        disk_after_pop.contains("100") && disk_after_pop.contains("200"),
        "both alpha (100) and beta (200) must be present after stash pop: {disk_after_pop}"
    );

    // Verify `agent-alpha.own_blobs["lib.rs"]` did NOT steal `agent-beta`'s uncommitted `200`.
    let after_pop_view = coord.oplog().load_current_reconciled().expect("view").view;
    let alpha_own_bid = after_pop_view
        .agents
        .get("agent-alpha")
        .and_then(|a| a.own_blobs.get("lib.rs"))
        .expect("alpha own blob");
    let alpha_own_text = String::from_utf8_lossy(
        &coord
            .git()
            .read_blob(alpha_own_bid)
            .expect("read alpha blob"),
    )
    .into_owned();
    assert!(
        alpha_own_text.contains("100") && !alpha_own_text.contains("200"),
        "stash_pop must not steal peer's 200 into alpha's own_blobs: {alpha_own_text}"
    );

    let log_entries = coord.commit_log(None, 5).expect("commit_log");
    assert!(!log_entries.is_empty());
}

#[test]
fn path_scoped_and_unclaimed_stash_plus_ast_cherry_pick() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("a.rs"), "pub fn a() -> i32 { 1 }\n").expect("write a.rs");
    fs::write(root.join("b.rs"), "pub fn b() -> i32 { 2 }\n").expect("write b.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    fs::write(root.join("a.rs"), "pub fn a() -> i32 { 10 }\n").expect("edit a.rs");
    fs::write(root.join("b.rs"), "pub fn b() -> i32 { 20 }\n").expect("edit b.rs");

    let stash_a = coord
        .stash_push_with_paths(
            "term-999999",
            Some("stash only a"),
            None,
            &["a.rs".to_string()],
        )
        .expect("stash_push_with_paths a.rs");
    assert_eq!(stash_a.own_blobs.len(), 1);
    assert!(stash_a.own_blobs.contains_key("a.rs"));
    assert_eq!(
        fs::read_to_string(root.join("a.rs")).expect("read a.rs"),
        "pub fn a() -> i32 { 1 }\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("b.rs")).expect("read b.rs"),
        "pub fn b() -> i32 { 20 }\n"
    );

    let stash_b = coord
        .stash_push("term-999999", Some("stash b"), None)
        .expect("stash_push b.rs");
    assert_eq!(stash_b.own_blobs.len(), 1);
    assert!(stash_b.own_blobs.contains_key("b.rs"));
    assert_eq!(
        fs::read_to_string(root.join("b.rs")).expect("read b.rs"),
        "pub fn b() -> i32 { 2 }\n"
    );

    coord
        .claim_intent_and_symbols_with_branch(
            "agent-cp",
            Some("Add helper to a.rs"),
            &["a.rs".to_string()],
            Some("feat/cherry"),
        )
        .expect("claim feat/cherry");
    fs::write(
        root.join("a.rs"),
        "pub fn a() -> i32 { 1 }\npub fn helper() -> i32 { 99 }\n",
    )
    .expect("write helper");
    handle_post_tool_hook(&coord, "agent-cp", "a.rs").expect("post-tool helper");
    let merge_rep = coord
        .merge_agents_with_message(
            &["agent-cp".to_string()],
            Some("feat/cherry"),
            None,
            true,
            Some("feat: add helper to a.rs"),
        )
        .expect("commit feat/cherry");

    let cp_rep = coord
        .cherry_pick("editor", merge_rep.merged_commit_id.as_str(), Some("main"))
        .expect("cherry_pick onto main");
    assert_eq!(cp_rep.target_branch, "main");
    assert!(cp_rep.conflicts.is_empty());
    assert_eq!(cp_rep.touched_paths, vec!["a.rs".to_string()]);
    let main_a = fs::read_to_string(root.join("a.rs")).expect("read a.rs on main");
    assert!(
        main_a.contains("pub fn helper() -> i32 { 99 }"),
        "cherry-picked helper must be materialized on main: {main_a}"
    );
}

#[test]
fn commit_options_author_and_co_author_flags_override_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").expect("write lib.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    coord
        .claim_intent_and_symbols("agent-1", Some("Update v"), &["lib.rs".to_string()])
        .expect("claim");
    fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 2 }\n").expect("edit 1");
    handle_post_tool_hook(&coord, "agent-1", "lib.rs").expect("post-tool 1");

    let bad_opts = CommitOptions {
        message: Some("feat: invalid author".to_string()),
        author: Some("NotAnEmail".to_string()),
        ..Default::default()
    };
    assert!(
        coord
            .merge_agents_with_options(&["agent-1".to_string()], None, None, true, &bad_opts)
            .is_err()
    );

    let opts_with_trailer = CommitOptions {
        message: Some("feat: custom author and co-author".to_string()),
        author: Some("Alice Dev <alice@example.com>".to_string()),
        co_author: Some(true),
        co_author_trailers: vec![String::new(), "Bob Reviewer <bob@example.com>".to_string()],
        ..Default::default()
    };
    let rep1 = coord
        .merge_agents_with_options(
            &["agent-1".to_string()],
            None,
            None,
            true,
            &opts_with_trailer,
        )
        .expect("commit with trailer");
    let c1 = coord
        .git()
        .read_commit(&rep1.merged_commit_id)
        .expect("read c1");
    assert!(
        c1.author.starts_with("Alice Dev <alice@example.com> "),
        "author should be overridden: {}",
        c1.author
    );
    assert!(
        c1.message
            .contains("Co-authored-by: Tessera <noreply@tessera.getlunaris.dev>"),
        "default trailer should be included: {}",
        c1.message
    );
    assert!(
        c1.message
            .contains("Co-authored-by: Bob Reviewer <bob@example.com>"),
        "custom trailer should be included: {}",
        c1.message
    );

    let mut cfg = coord.oplog().load_config().expect("load_config");
    cfg.co_author = true;
    coord.oplog().save_config(&cfg).expect("save_config");

    fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 3 }\n").expect("edit 2");
    handle_post_tool_hook(&coord, "agent-1", "lib.rs").expect("post-tool 2");

    let opts_no_co_author = CommitOptions {
        message: Some("feat: suppress co-author".to_string()),
        co_author: Some(false),
        ..Default::default()
    };
    let rep2 = coord
        .merge_agents_with_options(
            &["agent-1".to_string()],
            None,
            None,
            false,
            &opts_no_co_author,
        )
        .expect("commit with --no-co-author");
    let c2 = coord
        .git()
        .read_commit(&rep2.merged_commit_id)
        .expect("read c2");
    assert!(
        !c2.message.contains("Co-authored-by:"),
        "--no-co-author must suppress trailer even when cfg.co_author is true: {}",
        c2.message
    );

    let opts_amend = CommitOptions {
        message: Some("feat: amended commit message".to_string()),
        co_author: Some(false),
        amend: true,
        ..Default::default()
    };
    let rep_amend = coord
        .merge_agents_with_options(&["agent-1".to_string()], None, None, false, &opts_amend)
        .expect("commit --amend");
    let c_amend = coord
        .git()
        .read_commit(&rep_amend.merged_commit_id)
        .expect("read amended commit");
    assert_eq!(c_amend.parents, c2.parents);
    assert_eq!(c_amend.message.trim(), "feat: amended commit message");
}

#[test]
fn session_waypoint_captured_and_focusable_by_id_or_prefix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("app.rs"), "pub fn run() {}\n").expect("write app.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    let meta = coord
        .ensure_session("antigravity-99887766-aa", Some("Implement navigation"))
        .expect("ensure_session");
    assert!(
        meta.waypoint.is_some(),
        "ensure_session must record a SessionWaypoint for non-editor sessions"
    );
    let wp = meta.waypoint.as_ref().expect("waypoint");
    assert_eq!(wp.harness, "antigravity");

    let focus_exact = coord
        .focus_session("antigravity-99887766-aa")
        .expect("focus exact");
    assert_eq!(focus_exact.session_id, "antigravity-99887766-aa");
    assert_eq!(focus_exact.harness, "antigravity");

    let focus_prefix = coord.focus_session("99887766").expect("focus substring");
    assert_eq!(focus_prefix.session_id, "antigravity-99887766-aa");
}

#[test]
fn codex_harness_integration_and_waypoint_support() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("app.rs"), "pub fn run() {}\n").expect("write app.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");
    let installed =
        install_harness_integrations_for(root, coord.git().common_git_dir(), Some("codex"))
            .expect("install codex harness");

    assert!(installed.contains(&".codex/config.toml".to_string()));
    assert!(installed.contains(&".codex/hooks.json".to_string()));
    assert!(installed.contains(&"AGENTS.md".to_string()));

    let config_toml =
        fs::read_to_string(root.join(".codex/config.toml")).expect("read .codex/config.toml");
    assert!(config_toml.contains("[mcp_servers.tessera]"));
    assert!(config_toml.contains("command = \"tes\""));
    assert!(config_toml.contains("args = [\"mcp\"]"));

    let hooks_json =
        fs::read_to_string(root.join(".codex/hooks.json")).expect("read .codex/hooks.json");
    assert!(hooks_json.contains("apply_patch"));
    assert!(hooks_json.contains("tes hook post-tool"));

    assert!(is_harness_config_path(".codex/config.toml"));
    assert!(is_harness_config_path(".codex/hooks.json"));
    assert!(is_harness_config_path("AGENTS.override.md"));

    let meta = coord
        .ensure_session("codex-01972a3b-4c", Some("Refactor module"))
        .expect("ensure codex session");
    assert_eq!(
        meta.waypoint.as_ref().map(|w| w.harness.as_str()),
        Some("codex")
    );
}

#[test]
fn restore_file_clears_claimed_symbols_and_resets_session_trees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("README.md"), "# Title\n\nLine 1\n\nLine 2\n").expect("write README");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    let mut cfg = coord.oplog().load_config().expect("load_config");
    cfg.auto_isolate_dirty_claims = false;
    coord.oplog().save_config(&cfg).expect("save_config");

    coord
        .claim_intent_and_symbols("agent-a", Some("Edit line 1"), &["README.md".to_string()])
        .expect("claim a");
    fs::write(
        root.join("README.md"),
        "# Title\n\nLine 1 edited\n\nLine 2\n",
    )
    .expect("edit a");
    handle_post_tool_hook(&coord, "agent-a", "README.md").expect("post-tool a");

    coord
        .claim_intent_and_symbols("agent-b", Some("Edit line 2"), &["README.md".to_string()])
        .expect("claim b");
    fs::write(
        root.join("README.md"),
        "# Title\n\nLine 1 edited\n\nLine 2 edited\n",
    )
    .expect("edit b");
    handle_post_tool_hook(&coord, "agent-b", "README.md").expect("post-tool b");

    let rep = coord
        .restore("agent-a", None, None, &["README.md".to_string()])
        .expect("restore README.md");
    assert_eq!(rep.restored_paths, vec!["README.md".to_string()]);

    let radar = coord.radar().expect("radar after restore");
    assert!(
        radar.collisions.is_empty(),
        "restoring README.md must clear collisions: {:?}",
        radar.collisions
    );
    assert!(
        radar.file_attributions.is_empty(),
        "restoring README.md must clear file_attributions: {:?}",
        radar.file_attributions
    );

    for agent in &radar.active_agents {
        assert!(
            agent.own_blobs.is_empty() && agent.claimed_symbols.is_empty(),
            "agent {} must have empty own_blobs and claimed_symbols after restore",
            agent.agent_id
        );
        assert_eq!(
            agent.head_commit, agent.base_commit,
            "agent {} head_commit must equal base_commit after restore",
            agent.agent_id
        );
    }
}

#[test]
fn multi_file_session_rebase_preserves_interleaved_peer_commits_and_flags_conflict_markers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    fs::write(
        root.join("config.ts"),
        "export function renderDoc(): string {\n  return 'base';\n}\n\nexport function formatTitle(): string {\n  return 'title-v1';\n}\n",
    )
    .expect("write config.ts");
    fs::write(
        root.join("cli.md"),
        "# CLI Reference\n\n## Flags\n- `--json`\n\n## Notes\nStandard notes.\n",
    )
    .expect("write cli.md");
    fs::write(
        root.join("style.md"),
        "# Style Guide\n\nKeep formatting clean.\n",
    )
    .expect("write style.md");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    coord
        .claim_intent_and_symbols(
            "agent-style",
            Some("Update style across docs"),
            &[
                "config.ts".to_string(),
                "cli.md".to_string(),
                "style.md".to_string(),
            ],
        )
        .expect("claim agent-style");

    coord
        .claim_intent_and_symbols(
            "agent-feat",
            Some("Add EditorPreview replacement and pager flags"),
            &["config.ts".to_string(), "cli.md".to_string()],
        )
        .expect("claim agent-feat");

    fs::write(
        root.join("config.ts"),
        "export function renderDoc(): string {\n  return 'base';\n}\n\nexport function formatTitle(): string {\n  return 'title-v2-clean';\n}\n",
    )
    .expect("style edit config.ts");
    handle_post_tool_hook(&coord, "agent-style", "config.ts").expect("post-tool style config.ts");

    fs::write(
        root.join("style.md"),
        "# Style Guide\n\nKeep formatting clean without dashes.\n",
    )
    .expect("style edit style.md");
    handle_post_tool_hook(&coord, "agent-style", "style.md").expect("post-tool style style.md");

    fs::write(
        root.join("config.ts"),
        "export function renderDoc(): string {\n  return 'replaced-editor-preview';\n}\n\nexport function formatTitle(): string {\n  return 'title-v2-clean';\n}\n",
    )
    .expect("feat edit config.ts");
    handle_post_tool_hook(&coord, "agent-feat", "config.ts").expect("post-tool feat config.ts");

    fs::write(
        root.join("cli.md"),
        "# CLI Reference\n\n## Flags\n- `--json`\n- `-P, --no-pager`\n\n## Notes\nStandard notes.\n",
    )
    .expect("feat edit cli.md");
    handle_post_tool_hook(&coord, "agent-feat", "cli.md").expect("post-tool feat cli.md");

    let feat_commit = coord
        .merge_agents_with_message(
            &["agent-feat".to_string()],
            Some("main"),
            None,
            false,
            Some("feat: replace EditorPreview and add --no-pager"),
        )
        .expect("commit agent-feat");
    assert!(feat_commit.conflicts.is_empty());

    fs::write(
        root.join("cli.md"),
        "# CLI Reference\n\n## Flags\n- `--json`\n- `-P, --no-pager`\n\n## Notes\nUpdated notes by style session.\n",
    )
    .expect("style edit cli.md");
    handle_post_tool_hook(&coord, "agent-style", "cli.md").expect("post-tool style cli.md");

    let style_commit = coord
        .merge_agents_with_message(
            &["agent-style".to_string()],
            Some("main"),
            None,
            false,
            Some("style: update notes and title"),
        )
        .expect("commit agent-style");
    assert!(style_commit.conflicts.is_empty());

    let final_config = fs::read_to_string(root.join("config.ts")).expect("read final config.ts");
    assert!(
        final_config.contains("replaced-editor-preview"),
        "agent-style commit must NOT revert agent-feat's committed change in config.ts:\n{final_config}"
    );
    assert!(
        final_config.contains("title-v2-clean"),
        "agent-style commit must include its own change in config.ts:\n{final_config}"
    );

    let final_cli = fs::read_to_string(root.join("cli.md")).expect("read final cli.md");
    assert!(
        final_cli.contains("- `-P, --no-pager`")
            && final_cli.contains("Updated notes by style session."),
        "cli.md must preserve both sessions' changes:\n{final_cli}"
    );
}

#[test]
fn adjacent_eof_edits_on_separate_branches_stay_isolated_and_preserve_harness_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

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

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");
    assert!(root.join(".agents/rules/tessera.md").is_file());

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

    let css_after_a1 = "body {\n  margin: 0;\n}\n.pricing {\n  display: grid;\n}\n";
    fs::write(root.join("styles.css"), css_after_a1).expect("write a1 css");
    handle_post_tool_hook(&coord, "agent-pricing", "styles.css").expect("post-tool a1 css");

    let html_after_a1 = "<main>\n  <section id=\"hero\">Hero</section>\n  <section id=\"pricing\">Pricing</section>\n</main>\n";
    fs::write(root.join("index.html"), html_after_a1).expect("write a1 html");
    handle_post_tool_hook(&coord, "agent-pricing", "index.html").expect("post-tool a1 html");

    let css_after_a2 = "body {\n  margin: 0;\n}\n.pricing {\n  display: grid;\n}\n.testimonials {\n  display: flex;\n}\n";
    fs::write(root.join("styles.css"), css_after_a2).expect("write a2 css");
    handle_post_tool_hook(&coord, "agent-faq", "styles.css").expect("post-tool a2 css");

    let html_after_a2 = "<main>\n  <section id=\"hero\">Hero</section>\n  <section id=\"pricing\">Pricing</section>\n  <section id=\"testimonials\">Testimonials</section>\n</main>\n";
    fs::write(root.join("index.html"), html_after_a2).expect("write a2 html");
    handle_post_tool_hook(&coord, "agent-faq", "index.html").expect("post-tool a2 html");

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

    assert!(root.join(".agents/rules/tessera.md").is_file());

    let (_, pricing_ws, _) = coord
        .prepare_branch_sandbox("term-1", Some("feat/pricing-calculator"))
        .expect("pricing sandbox");
    let pricing_css = fs::read_to_string(pricing_ws.join("styles.css")).expect("read pricing css");
    let pricing_html =
        fs::read_to_string(pricing_ws.join("index.html")).expect("read pricing html");
    assert!(
        pricing_css.contains(".pricing"),
        "pricing_css: {pricing_css}"
    );
    assert!(
        !pricing_css.contains(".testimonials"),
        "pricing_css leaked testimonials: {pricing_css}"
    );
    assert!(
        pricing_html.contains("id=\"pricing\""),
        "pricing_html: {pricing_html}"
    );
    assert!(
        !pricing_html.contains("id=\"testimonials\""),
        "pricing_html leaked testimonials: {pricing_html}"
    );

    let (_, faq_ws, _) = coord
        .prepare_branch_sandbox("term-2", Some("feat/testimonials-faq"))
        .expect("faq sandbox");
    let faq_css = fs::read_to_string(faq_ws.join("styles.css")).expect("read faq css");
    let faq_html = fs::read_to_string(faq_ws.join("index.html")).expect("read faq html");
    assert!(faq_css.contains(".testimonials"), "faq_css: {faq_css}");
    assert!(
        !faq_css.contains(".pricing"),
        "faq_css leaked pricing: {faq_css}"
    );
    assert!(
        faq_html.contains("id=\"testimonials\""),
        "faq_html: {faq_html}"
    );
    assert!(
        !faq_html.contains("id=\"pricing\""),
        "faq_html leaked pricing: {faq_html}"
    );
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

    fs::create_dir_all(root.join("node_modules/.bin")).expect("mkdir node_modules/.bin");
    fs::write(
        root.join("node_modules/.bin/vue-tsc"),
        "#!/bin/sh\nexit 0\n",
    )
    .expect("write vue-tsc");

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

    let t1_init = coord.ensure_session("term-1001", None).expect("t1 init");
    assert_eq!(t1_init.target_branch.as_deref(), Some("main"));
    let t1 = coord
        .switch_branch("term-1001", "feat/testimonials-faq")
        .expect("t1 switch");
    assert_eq!(t1.target_branch.as_deref(), Some("feat/testimonials-faq"));

    let _ = coord.ensure_session("term-1002", None).expect("t2 init");
    let t2 = coord
        .switch_branch("term-1002", "feat/pricing-calculator")
        .expect("t2 switch");
    assert_eq!(t2.target_branch.as_deref(), Some("feat/pricing-calculator"));

    let t3 = coord.ensure_session("term-1003", None).expect("t3 init");
    assert_eq!(t3.target_branch.as_deref(), Some("main"));

    let switched_ed = coord
        .switch_editor_branch("feat/pricing-calculator")
        .expect("switch editor");
    assert_eq!(switched_ed, "feat/pricing-calculator");

    let cur = coord
        .oplog()
        .load_current_reconciled()
        .expect("load current");
    assert_eq!(
        cur.view
            .agents
            .get("term-1001")
            .and_then(|a| a.target_branch.as_deref()),
        Some("feat/testimonials-faq"),
        "Terminal 1 must stay on feat/testimonials-faq"
    );
    assert_eq!(
        cur.view
            .agents
            .get("term-1002")
            .and_then(|a| a.target_branch.as_deref()),
        Some("feat/pricing-calculator"),
        "Terminal 2 must stay on feat/pricing-calculator"
    );
    assert_eq!(
        cur.view
            .agents
            .get("term-1003")
            .and_then(|a| a.target_branch.as_deref()),
        Some("main"),
        "Terminal 3 must stay pinned to main even after VS Code switches editor branch"
    );

    coord
        .switch_editor_branch("main")
        .expect("switch editor to main");
    let (_, main_ws, _) = coord
        .prepare_branch_sandbox("term-1003", Some("main"))
        .expect("prepare main sandbox");
    assert!(main_ws.join("README.md").is_file());
    fs::remove_file(main_ws.join("README.md")).expect("rm README.md in sandbox");

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

    let radar = coord.radar().expect("radar");
    assert!(
        radar.collisions.is_empty(),
        "sessions on separate branches must not report radar collisions: {:?}",
        radar.collisions
    );

    let disk_after_blue = fs::read_to_string(root.join("src/App.vue")).expect("read disk");
    assert_eq!(
        disk_after_blue, blue_vue,
        "post-tool hook must never strip shared lines from a full-file replacement on another branch"
    );

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

    let pomodoro_vue = "<script setup lang=\"ts\">\nimport { ref } from 'vue'\nimport HelloWorld from './components/HelloWorld.vue'\nconst secondsLeft = ref(1500)\n</script>\n\n<template>\n  <HelloWorld />\n  <section id=\"pomodoro-timer\" class=\"timer-panel\">\n    <h2>Focus Timer (feat/pomodoro-timer)</h2>\n  </section>\n</template>\n";
    fs::write(root.join("src/App.vue"), pomodoro_vue).expect("write pomodoro App.vue");
    handle_post_tool_hook(&coord, "session-pomodoro", "src/App.vue").expect("post-tool pomodoro");

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

    coord
        .switch_editor_branch("feat/pomodoro-timer")
        .expect("switch editor to pomodoro");

    let _ = coord
        .ensure_session("session-merge", None)
        .expect("ensure session-merge");
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
    assert!(
        report.conflicts.is_empty(),
        "merge into main must have 0 conflicts"
    );
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

    coord
        .switch_branch("session-a", "feat/retries-5")
        .expect("switch a");
    coord
        .claim_intent_and_symbols(
            "session-a",
            Some("Set MAX_RETRIES to 5"),
            &["src/config.ts".to_string()],
        )
        .expect("claim a");
    fs::write(
        root.join("src/config.ts"),
        "export const MAX_RETRIES = 5;\n",
    )
    .expect("write a");
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

    coord
        .switch_branch("session-b", "feat/retries-10")
        .expect("switch b");
    coord
        .claim_intent_and_symbols(
            "session-b",
            Some("Set MAX_RETRIES to 10"),
            &["src/config.ts".to_string()],
        )
        .expect("claim b");
    fs::write(
        root.join("src/config.ts"),
        "export const MAX_RETRIES = 10;\n",
    )
    .expect("write b");
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

    let conflicted_disk = fs::read_to_string(root.join("src/config.ts")).expect("read conflicted");
    assert!(
        conflicted_disk.contains("<<<<<<< ")
            && conflicted_disk.contains("=======")
            && conflicted_disk.contains(">>>>>>> ")
            && !conflicted_disk.contains("||||||| base"),
        "working file must contain standard Git 2-way conflict markers:\n{conflicted_disk}"
    );

    coord
        .switch_branch("session-resolve", "main")
        .expect("switch resolve");
    fs::write(
        root.join("src/config.ts"),
        "export const MAX_RETRIES = 10;\n",
    )
    .expect("write resolved config.ts");
    handle_post_tool_hook(&coord, "session-resolve", "src/config.ts").expect("post-tool resolve");

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
    assert!(is_shell_or_cli_harness_process(
        "/Users/user/.local/bin/agy"
    ));
    assert!(is_shell_or_cli_harness_process("/usr/local/bin/claude"));
    assert!(is_shell_or_cli_harness_process(
        "codex-aarch64-apple-darwin"
    ));
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
    assert!(!is_ide_process_name(
        "/Users/user/code/project/target/debug/tes"
    ));
    assert!(!is_ide_process_name("/Users/user/.local/bin/agy"));
    assert!(!is_ide_process_name("/usr/local/bin/codex"));
}

#[test]
fn switch_editor_and_terminal_branch_preserves_unhooked_in_flight_agent_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");

    coord
        .claim_intent_and_symbols_with_branch(
            "agent-video",
            Some("Create product video"),
            &["video".to_string()],
            Some("feat/product-video"),
        )
        .expect("claim feat/product-video");

    let pkg_path = tmp.path().join("video/package.json");
    let root_path = tmp.path().join("video/src/Root.tsx");
    fs::create_dir_all(root_path.parent().expect("parent")).expect("mkdir");
    fs::write(&pkg_path, "{\"name\":\"video\"}\n").expect("write package.json");
    fs::write(&root_path, "export const RemotionRoot = () => null;\n").expect("write Root.tsx");

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
        fs::read_to_string(sb_dir.join("video/package.json")).expect("read sandbox package.json"),
        "{\"name\":\"video\"}\n"
    );
    assert_eq!(
        fs::read_to_string(sb_dir.join("video/src/Root.tsx")).expect("read sandbox Root.tsx"),
        "export const RemotionRoot = () => null;\n"
    );

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
        .merge_agents(&["agent-main".to_string()], Some("main"), None, false)
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
        fs::read_to_string(&scene_path).expect("Scene1Problem.tsx must exist on main after merge"),
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

    coord
        .merge_agents(
            &["agent-pomodoro".to_string()],
            Some("feat/pomodoro-timer"),
            None,
            false,
        )
        .expect("commit feat/pomodoro-timer");

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
fn branching_off_feature_branch_via_from_or_switch_stages_parent_baseline_and_rejects_invalid_base()
{
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
            .switch_branch_with_base("agent-sec", "feat/evil", Some("--upload-pack=evil"))
            .is_err()
    );
    assert!(
        coord
            .switch_branch_with_base("agent-sec", "feat/evil", Some("../evil"))
            .is_err()
    );
    assert!(
        coord
            .switch_branch_with_base("agent-sec", "feat/missing", Some("feat/does-not-exist"))
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

#[test]
fn switch_and_claim_from_refuse_to_overwrite_existing_branch_commits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").expect("write lib.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    coord
        .claim_intent_and_symbols_with_branch(
            "agent-feat",
            Some("Advance feat/x"),
            &["lib.rs".to_string()],
            Some("feat/x"),
        )
        .expect("claim feat/x");
    fs::write(root.join("lib.rs"), "pub fn v() -> i32 { 42 }\n").expect("edit lib.rs");
    handle_post_tool_hook(&coord, "agent-feat", "lib.rs").expect("post-tool");
    let feat_commit = coord
        .merge_agents_with_message(
            &["agent-feat".to_string()],
            Some("feat/x"),
            None,
            false,
            Some("feat: set v = 42"),
        )
        .expect("commit feat/x");

    let switch_err = coord.switch_branch_with_base("agent-other", "feat/x", Some("main"));
    assert!(
        switch_err.is_err(),
        "switch --from main must refuse to reset existing branch feat/x"
    );

    let claim_err = coord.claim_intent_and_symbols_with_branch_and_base(
        "agent-other",
        Some("Try reset"),
        &["lib.rs".to_string()],
        Some("feat/x"),
        Some("main"),
    );
    assert!(
        claim_err.is_err(),
        "claim --branch feat/x --from main must refuse to reset existing branch feat/x"
    );

    let ref_after = coord
        .git()
        .resolve_ref("refs/heads/feat/x")
        .expect("resolve")
        .expect("feat/x exists");
    assert_eq!(
        ref_after, feat_commit.merged_commit_id,
        "feat/x ref must remain untouched"
    );
}

#[test]
fn pull_branch_records_conflicts_without_advancing_ref_and_preserves_new_files() {
    let bare_tmp = tempfile::tempdir().expect("bare tempdir");
    let local_tmp = tempfile::tempdir().expect("local tempdir");
    let peer_tmp = tempfile::tempdir().expect("peer tempdir");

    let bare = bare_tmp.path();
    let local = local_tmp.path();
    let peer = peer_tmp.path();

    assert!(
        Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .current_dir(bare)
            .status()
            .expect("git init --bare")
            .success()
    );

    assert!(
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(local)
            .status()
            .expect("git init local")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(local)
            .status()
            .expect("config name")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(local)
            .status()
            .expect("config email")
            .success()
    );

    fs::write(local.join("config.rs"), "pub const LIMIT: u32 = 10;\n").expect("write config.rs");
    assert!(
        Command::new("git")
            .args(["add", "config.rs"])
            .current_dir(local)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(local)
            .status()
            .expect("git commit")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["remote", "add", "origin", &bare.display().to_string()])
            .current_dir(local)
            .status()
            .expect("git remote add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["push", "-u", "origin", "main"])
            .current_dir(local)
            .status()
            .expect("git push")
            .success()
    );

    assert!(
        Command::new("git")
            .args(["clone", &bare.display().to_string(), "."])
            .current_dir(peer)
            .status()
            .expect("git clone")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.name", "Peer User"])
            .current_dir(peer)
            .status()
            .expect("peer name")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["config", "user.email", "peer@example.com"])
            .current_dir(peer)
            .status()
            .expect("peer email")
            .success()
    );

    let (coord, _) = Coordinator::init_repo(local).expect("init_repo local");

    // Clean fast-forward pull must preserve caller's uncommitted new file and own_blobs.
    fs::write(peer.join("remote_added.rs"), "pub fn remote_fn() {}\n").expect("write remote_added");
    assert!(
        Command::new("git")
            .args(["add", "remote_added.rs"])
            .current_dir(peer)
            .status()
            .expect("peer add")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "-m", "remote add file"])
            .current_dir(peer)
            .status()
            .expect("peer commit")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(peer)
            .status()
            .expect("peer push")
            .success()
    );

    coord
        .claim_intent_and_symbols(
            "agent-wip",
            Some("Create uncommitted new file"),
            &["local_new.rs".to_string()],
        )
        .expect("claim wip");
    fs::write(local.join("local_new.rs"), "pub fn local_wip() {}\n").expect("write local_new");
    handle_post_tool_hook(&coord, "agent-wip", "local_new.rs").expect("post-tool local_new");
    fs::write(local.join("untracked_notes.txt"), "human scratch notes\n")
        .expect("write untracked_notes");

    let ff_report = coord
        .pull_branch("agent-wip", "origin", Some("main"))
        .expect("clean pull");
    assert!(ff_report.fast_forward);
    assert!(ff_report.conflicts.is_empty());
    assert!(local.join("remote_added.rs").is_file());
    assert!(
        local.join("local_new.rs").is_file(),
        "clean pull must preserve caller's uncommitted new file"
    );
    assert!(
        local.join("untracked_notes.txt").is_file(),
        "clean pull must preserve untracked files"
    );

    // Now create divergent conflicting edits to `config.rs` locally and remotely.
    coord
        .claim_intent_and_symbols(
            "agent-local",
            Some("Set LIMIT to 20"),
            &["config.rs".to_string()],
        )
        .expect("claim local");
    fs::write(local.join("config.rs"), "pub const LIMIT: u32 = 20;\n").expect("local config");
    handle_post_tool_hook(&coord, "agent-local", "config.rs").expect("post-tool local config");
    let local_commit = coord
        .merge_agents_with_message(
            &["agent-local".to_string()],
            Some("main"),
            None,
            false,
            Some("local limit 20"),
        )
        .expect("commit local limit 20");

    fs::write(peer.join("config.rs"), "pub const LIMIT: u32 = 99;\n").expect("peer config");
    assert!(
        Command::new("git")
            .args(["commit", "-am", "remote limit 99"])
            .current_dir(peer)
            .status()
            .expect("peer commit conflict")
            .success()
    );
    assert!(
        Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(peer)
            .status()
            .expect("peer push conflict")
            .success()
    );

    let conflict_pull = coord
        .pull_branch("agent-local", "origin", Some("main"))
        .expect("conflicted pull");
    assert!(
        !conflict_pull.conflicts.is_empty(),
        "conflicted pull must return recorded conflicts"
    );
    assert_eq!(
        conflict_pull.new_commit, local_commit.merged_commit_id,
        "conflicted pull must NOT advance refs/heads/main with conflict markers baked into history"
    );
    assert_eq!(
        coord
            .git()
            .resolve_ref("refs/heads/main")
            .expect("resolve main"),
        Some(local_commit.merged_commit_id)
    );

    let radar = coord.radar().expect("radar");
    assert!(
        radar.active_conflicts.iter().any(|c| c.path == "config.rs"),
        "conflicted pull must register conflict in view.conflicts for `tes conflicts` / `tes resolve`"
    );

    let disk_config = fs::read_to_string(local.join("config.rs")).expect("read conflicted config");
    assert!(
        has_git_conflict_markers(&disk_config),
        "conflicted pull must materialize conflict markers onto working tree:\n{disk_config}"
    );
    assert!(
        local.join("untracked_notes.txt").is_file(),
        "conflicted pull must preserve untracked files"
    );
}

#[test]
fn cherry_pick_preserves_uncommitted_new_files_and_own_blobs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("base.rs"), "pub fn base() {}\n").expect("write base.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    coord
        .claim_intent_and_symbols_with_branch(
            "agent-src",
            Some("Add cherry file"),
            &["picked.rs".to_string()],
            Some("feat/src"),
        )
        .expect("claim feat/src");
    fs::write(root.join("picked.rs"), "pub fn picked() -> i32 { 7 }\n").expect("write picked.rs");
    handle_post_tool_hook(&coord, "agent-src", "picked.rs").expect("post-tool picked");
    let src_rep = coord
        .merge_agents_with_message(
            &["agent-src".to_string()],
            Some("feat/src"),
            None,
            false,
            Some("feat: add picked.rs"),
        )
        .expect("commit feat/src");

    // Create both an uncommitted session-owned new file and an untracked file on `main`.
    coord
        .claim_intent_and_symbols(
            "agent-main-wip",
            Some("Create wip file"),
            &["wip_new.rs".to_string()],
        )
        .expect("claim main wip");
    fs::write(root.join("wip_new.rs"), "pub fn wip() {}\n").expect("write wip_new.rs");
    handle_post_tool_hook(&coord, "agent-main-wip", "wip_new.rs").expect("post-tool wip_new");
    fs::write(root.join("scratch.txt"), "keep me\n").expect("write scratch.txt");

    let cp = coord
        .cherry_pick(
            "agent-main-wip",
            src_rep.merged_commit_id.as_str(),
            Some("main"),
        )
        .expect("cherry_pick");
    assert!(cp.conflicts.is_empty());
    assert!(root.join("picked.rs").is_file());
    assert!(
        root.join("wip_new.rs").is_file(),
        "cherry_pick must preserve uncommitted session-owned new files"
    );
    assert!(
        root.join("scratch.txt").is_file(),
        "cherry_pick must preserve untracked files"
    );
}

#[test]
fn upsert_claude_hooks_json_merges_and_preserves_existing_user_hooks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(".claude/settings.json");
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");

    let initial = r#"{
  // User comment in Claude settings
  "permissions": {
    "allow": ["Bash(cargo test)"],
  },
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "./scripts/user-guard.sh" }]
      },
    ],
    "Stop": [
      {
        "hooks": [{ "type": "command", "command": "notify-send done" }]
      }
    ],
  },
}
"#;
    fs::write(&path, initial).expect("write settings.json");

    upsert_claude_hooks_json(&path).expect("first upsert");
    upsert_claude_hooks_json(&path).expect("second idempotent upsert");

    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("read settings"))
            .expect("parse updated settings");

    assert_eq!(parsed["permissions"]["allow"][0], "Bash(cargo test)");
    assert_eq!(
        parsed["hooks"]["Stop"][0]["hooks"][0]["command"],
        "notify-send done"
    );

    let pre_tool = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse array");
    assert_eq!(
        pre_tool.len(),
        2,
        "PreToolUse must contain user hook + exactly one Tessera hook: {pre_tool:?}"
    );
    assert_eq!(
        pre_tool[0]["hooks"][0]["command"],
        "./scripts/user-guard.sh"
    );
    assert_eq!(pre_tool[1]["hooks"][0]["command"], "tes hook pre-tool");

    let post_tool = parsed["hooks"]["PostToolUse"]
        .as_array()
        .expect("PostToolUse array");
    assert_eq!(post_tool.len(), 1);
    assert_eq!(post_tool[0]["hooks"][0]["command"], "tes hook post-tool");
}

#[test]
fn jsonc_config_files_preserve_existing_settings_without_wiping() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let stripped = strip_jsonc(
        r#"{
  // line comment with url inside string below
  "url": "https://opencode.ai/config.json", /* block comment */
  "list": [1, 2, ],
}"#,
    );
    let parsed_check: serde_json::Value =
        serde_json::from_str(&stripped).expect("strip_jsonc produces valid JSON");
    assert_eq!(parsed_check["url"], "https://opencode.ai/config.json");
    assert_eq!(parsed_check["list"], serde_json::json!([1, 2]));

    let vscode_path = tmp.path().join(".vscode/mcp.json");
    fs::create_dir_all(vscode_path.parent().expect("parent")).expect("mkdir .vscode");
    fs::write(
        &vscode_path,
        r#"{
  // VS Code MCP config
  "servers": {
    "github": {
      "type": "stdio",
      "command": "gh-mcp",
    },
  },
}
"#,
    )
    .expect("write .vscode/mcp.json");
    upsert_vscode_mcp_json(&vscode_path).expect("upsert vscode mcp");
    let vscode_val: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&vscode_path).expect("read vscode"))
            .expect("parse vscode");
    assert_eq!(vscode_val["servers"]["github"]["command"], "gh-mcp");
    assert_eq!(vscode_val["servers"]["tessera"]["command"], "tes");

    let opencode_path = tmp.path().join("opencode.json");
    fs::write(
        &opencode_path,
        r#"{
  /* OpenCode config with comments and trailing comma */
  "$schema": "https://opencode.ai/config.json",
  "theme": "dark",
  "mcp": {
    "custom": { "type": "local", "command": ["my-mcp"], },
  },
}
"#,
    )
    .expect("write opencode.json");
    upsert_opencode_json(&opencode_path).expect("upsert opencode");
    let oc_val: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&opencode_path).expect("read opencode"))
            .expect("parse opencode");
    assert_eq!(oc_val["theme"], "dark");
    assert_eq!(oc_val["mcp"]["custom"]["command"][0], "my-mcp");
    assert_eq!(oc_val["mcp"]["tessera"]["command"][0], "tes");

    let mcp_path = tmp.path().join(".mcp.json");
    fs::write(
        &mcp_path,
        "{\n  // comment\n  \"mcpServers\": { \"existing\": { \"command\": \"foo\", }, },\n}\n",
    )
    .expect("write .mcp.json");
    upsert_mcp_json(&mcp_path).expect("upsert .mcp.json");
    let mcp_val: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&mcp_path).expect("read .mcp.json"))
            .expect("parse .mcp.json");
    assert_eq!(mcp_val["mcpServers"]["existing"]["command"], "foo");
    assert_eq!(mcp_val["mcpServers"]["tessera"]["command"], "tes");
}

#[test]
fn detect_session_id_handles_non_ascii_utf8_uuidv7_and_prefers_cursor_session_id() {
    let env_map = |pairs: &[(&str, &str)]| {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    };

    // Multi-byte UTF-8 characters crossing byte index 12 and 32 must never panic.
    let utf8_id = detect_session_id_with_env(
        None,
        env_map(&[("CLAUDE_SESSION_ID", "сессия-🔥-日本語-token-extra-long")]),
    );
    assert!(utf8_id.starts_with("claude-"));

    let utf8_tes = detect_session_id_with_env(
        None,
        env_map(&[(
            "TES_SESSION_ID",
            "🚀-многобайтовый-идентификатор-сессии-длинный",
        )]),
    );
    assert!(!utf8_tes.is_empty() && !utf8_tes.starts_with('-'));

    // Two UUIDv7 tokens sharing the first 24 characters must not collide after truncation.
    let uuid_a = detect_session_id_with_env(
        None,
        env_map(&[(
            "ANTIGRAVITY_CONVERSATION_ID",
            "01972a3b-1234-7abc-8def-111111111111",
        )]),
    );
    let uuid_b = detect_session_id_with_env(
        None,
        env_map(&[(
            "ANTIGRAVITY_CONVERSATION_ID",
            "01972a3b-1234-7abc-8def-222222222222",
        )]),
    );
    assert_ne!(
        uuid_a, uuid_b,
        "UUIDv7 IDs sharing a leading prefix must produce distinct truncated session IDs"
    );

    // `CURSOR_SESSION_ID` must take priority over per-request `CURSOR_TRACE_ID`.
    let cursor_id = detect_session_id_with_env(
        None,
        env_map(&[
            ("CURSOR_TRACE_ID", "trace-ephemeral-999"),
            ("CURSOR_SESSION_ID", "sess-stable"),
        ]),
    );
    assert_eq!(cursor_id, "cursor-sess-stable");
}

#[test]
fn merge_dry_run_computes_report_without_mutating_refs_disk_or_oplog() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn base() -> i32 { 1 }\n").expect("write lib.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    coord
        .claim_intent_and_symbols_with_branch(
            "agent-a",
            Some("Add fn_a"),
            &["lib.rs".to_string()],
            Some("feat/a"),
        )
        .expect("claim feat/a");
    fs::write(
        root.join("lib.rs"),
        "pub fn base() -> i32 { 1 }\npub fn fn_a() -> i32 { 2 }\n",
    )
    .expect("edit feat/a");
    handle_post_tool_hook(&coord, "agent-a", "lib.rs").expect("post-tool a");
    coord
        .merge_agents_with_message(
            &["agent-a".to_string()],
            Some("feat/a"),
            None,
            false,
            Some("feat: add fn_a"),
        )
        .expect("commit feat/a");

    let main_before = coord
        .git()
        .resolve_ref("refs/heads/main")
        .expect("resolve main")
        .expect("main exists");
    let op_before = coord
        .oplog()
        .load_current_reconciled()
        .expect("op before")
        .id;
    let disk_before = fs::read_to_string(root.join("lib.rs")).expect("read disk before");

    let dry_opts = CommitOptions {
        dry_run: true,
        ..Default::default()
    };
    let report = coord
        .merge_agents_with_options(
            &["feat/a".to_string()],
            Some("main"),
            Some("true"),
            false,
            &dry_opts,
        )
        .expect("dry_run merge");

    assert!(!report.created_commit);
    assert!(report.verification_passed);
    assert!(report.conflicts.is_empty());
    assert!(report.closed_agents.is_empty());
    assert_eq!(report.merged_commit_id, main_before);
    assert_ne!(
        report.merged_tree_id,
        coord
            .git()
            .read_commit(&main_before)
            .expect("read main")
            .tree_id
    );

    assert_eq!(
        coord
            .git()
            .resolve_ref("refs/heads/main")
            .expect("resolve main after"),
        Some(main_before),
        "dry_run must not advance refs/heads/main"
    );
    assert_eq!(
        coord
            .oplog()
            .load_current_reconciled()
            .expect("op after")
            .id,
        op_before,
        "dry_run must not append to OpLog"
    );
    assert_eq!(
        fs::read_to_string(root.join("lib.rs")).expect("read disk after"),
        disk_before,
        "dry_run must not mutate repo_root"
    );
}

#[test]
fn sanitize_branch_name_prevents_slash_hyphen_and_case_collisions() {
    assert_eq!(Coordinator::sanitize_branch_name("main"), "main");
    assert_eq!(Coordinator::sanitize_branch_name("feat-auth"), "feat-auth");

    let slash = Coordinator::sanitize_branch_name("feat/auth");
    let hyphen = Coordinator::sanitize_branch_name("feat-auth");
    assert_ne!(slash, hyphen, "feat/auth must not collide with feat-auth");
    assert!(slash.starts_with("feat-auth-"));

    let upper = Coordinator::sanitize_branch_name("Feat/A");
    let lower = Coordinator::sanitize_branch_name("feat/a");
    assert_ne!(
        upper.to_ascii_lowercase(),
        lower.to_ascii_lowercase(),
        "Feat/A and feat/a must never collide even on case-insensitive filesystems"
    );
}

#[test]
fn init_repo_local_only_adds_newly_installed_harness_files_to_git_exclude() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("app.rs"), "pub fn main() {}\n").expect("write app.rs");
    // Pre-existing AGENTS.md must NOT be forced into info/exclude by `--local`.
    fs::write(root.join("AGENTS.md"), "# Team Guide\n").expect("write existing AGENTS.md");

    let (coord, report) =
        Coordinator::init_repo_with_full_options(root, None, Some("antigravity,claude"), true)
            .expect("init local_only");

    assert!(
        report
            .harness_files_installed
            .contains(&".agents/rules/tessera.md".to_string())
    );
    assert!(
        report
            .harness_files_installed
            .contains(&".claude/settings.json".to_string())
    );

    let exclude_content = fs::read_to_string(coord.git().common_git_dir().join("info/exclude"))
        .expect("read info/exclude");
    assert!(exclude_content.contains(".tessera/"));
    assert!(exclude_content.contains(".agents/rules/tessera.md"));
    assert!(exclude_content.contains(".claude/settings.json"));
    assert!(exclude_content.contains(".mcp.json"));
    assert!(exclude_content.contains("CLAUDE.md"));
    assert!(
        !exclude_content.lines().any(|l| l.trim() == "AGENTS.md"),
        "pre-existing AGENTS.md must not be added to info/exclude: {exclude_content}"
    );
}

#[test]
fn has_git_conflict_markers_ignores_markdown_underlines_and_requires_col0_blocks() {
    let md_doc =
        "Summary\n=======\n\nIndented example:\n  <<<<<<< ours\n  =======\n  >>>>>>> theirs\n";
    assert!(
        !has_git_conflict_markers(md_doc),
        "Markdown Setext underline and indented examples must not false-positive"
    );

    let malformed = "<<<<<<<< too-many\n=======\n>>>>>>> ok\n";
    assert!(!has_git_conflict_markers(malformed));

    let empty_label = "<<<<<<<   \n=======\n>>>>>>> theirs\n";
    assert!(!has_git_conflict_markers(empty_label));

    let real_conflict = "line 1\n<<<<<<< main\nlet x = 1;\n=======\nlet x = 2;\n>>>>>>> feat/x\n";
    assert!(has_git_conflict_markers(real_conflict));
}

#[test]
fn is_contaminated_and_strip_ignore_shared_one_line_imports_and_preserve_following_lines() {
    let base = "pub fn init() {}\n";
    let peer = "use std::fs;\npub fn init() {}\n";
    let mine = "use std::fs;\npub fn init() {}\npub fn mine() {}\n";

    assert!(
        !is_contaminated_by_foreign_peer(base, base, peer, mine),
        "a single shared import line must never be flagged as foreign-peer contamination"
    );
    assert!(
        strip_peer_inserted_lines(base, base, peer, mine).is_none(),
        "strip_peer_inserted_lines must not strip a standalone common 1-line import"
    );

    // Verify removing a peer hunk never deletes an unrelated identical line that follows it.
    let base2 = "lineA\nkeep_me\n";
    let peer2 = "lineA\nkeep_me\npeer_line_1\npeer_line_2\n";
    let disk2 = "lineA\nkeep_me\npeer_line_1\npeer_line_2\nkeep_me\nmy_new_line\n";
    let stripped = strip_peer_inserted_lines(base2, base2, peer2, disk2).expect("strip hunk");
    assert_eq!(
        stripped.matches("keep_me").count(),
        2,
        "line following stripped peer hunk must never be deleted: {stripped}"
    );
}

#[test]
fn post_tool_and_stash_pop_handle_binary_conflicts_without_corrupting_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let base_bin: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0x00, 0x01, 0xff, 0xfe];
    fs::write(root.join("icon.png"), &base_bin).expect("write icon.png");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");
    let mut cfg = coord.oplog().load_config().expect("load_config");
    cfg.auto_isolate_dirty_claims = false;
    coord.oplog().save_config(&cfg).expect("save_config");

    coord
        .claim_intent_and_symbols("agent-1", Some("Update icon A"), &["icon.png".to_string()])
        .expect("claim 1");
    coord
        .claim_intent_and_symbols("agent-2", Some("Update icon B"), &["icon.png".to_string()])
        .expect("claim 2");

    let bin_a: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0x00, 0xaa, 0xbb, 0xff];
    fs::write(root.join("icon.png"), &bin_a).expect("write bin_a");
    let o1 = handle_post_tool_hook(&coord, "agent-1", "icon.png").expect("post-tool 1");
    assert!(!o1.has_conflict);

    let bin_b: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0x00, 0xcc, 0xdd, 0xee];
    fs::write(root.join("icon.png"), &bin_b).expect("write bin_b");
    let o2 = handle_post_tool_hook(&coord, "agent-2", "icon.png").expect("post-tool 2");
    assert!(
        o2.has_conflict,
        "divergent binary edit must report conflict"
    );

    // Disk bytes must remain exact valid binary bytes (never UTF-8 lossy replacement or conflict markers).
    let disk_bytes = fs::read(root.join("icon.png")).expect("read icon.png");
    assert_eq!(disk_bytes, bin_b);

    let radar = coord.radar().expect("radar");
    assert_eq!(radar.active_conflicts.len(), 1);
    assert_eq!(radar.active_conflicts[0].kind, ConflictKind::Binary);

    // `ast-union` must fail cleanly on a binary conflict, while `ours` resolves cleanly.
    assert!(
        coord
            .resolve_conflict("icon.png", ResolveStrategy::AstUnion)
            .is_err()
    );
    coord
        .resolve_conflict("icon.png", ResolveStrategy::Ours)
        .expect("resolve binary with ours");
    assert_eq!(
        fs::read(root.join("icon.png")).expect("read resolved"),
        bin_a
    );
}

#[test]
fn focus_session_rejects_ambiguous_substring_and_waypoint_prefers_tmux_over_term_program() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");

    coord
        .ensure_session("agent-shared-111", Some("Task 1"))
        .expect("s1");
    coord
        .ensure_session("agent-shared-222", Some("Task 2"))
        .expect("s2");

    let err = coord.focus_session("shared");
    assert!(
        err.is_err(),
        "ambiguous substring query matching two sessions must return an error"
    );

    let env_map = |pairs: &[(&str, &str)]| {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    };

    let wp = detect_session_waypoint_with_env(
        "term-tmux-42",
        env_map(&[("TERM_PROGRAM", "iTerm.app"), ("TMUX_PANE", "%42")]),
    );
    assert_eq!(wp.term_program.as_deref(), Some("tmux"));
    assert_eq!(wp.term_session_id.as_deref(), Some("%42"));

    assert_eq!(
        percent_encode_uri_component("agent/a&b=c?d"),
        "agent%2Fa%26b%3Dc%3Fd"
    );
}

#[test]
fn resolve_conflict_rejects_unresolved_working_file_and_scopes_to_active_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("cfg.rs"), "pub const N: i32 = 1;\n").expect("write cfg.rs");
    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    let mut cfg = coord.oplog().load_config().expect("load_config");
    cfg.auto_isolate_dirty_claims = false;
    coord.oplog().save_config(&cfg).expect("save_config");

    coord
        .claim_intent_and_symbols("agent-a", Some("Set N=2"), &["cfg.rs".to_string()])
        .expect("claim a");
    coord
        .claim_intent_and_symbols("agent-b", Some("Set N=3"), &["cfg.rs".to_string()])
        .expect("claim b");
    coord
        .claim_intent_and_symbols_with_branch(
            "agent-other-branch",
            Some("Edit cfg.rs on feat/other"),
            &["cfg.rs".to_string()],
            Some("feat/other"),
        )
        .expect("claim feat/other");
    fs::write(root.join("cfg.rs"), "pub const N: i32 = 999;\n").expect("edit other");
    handle_post_tool_hook(&coord, "agent-other-branch", "cfg.rs").expect("post-tool other");
    let other_blob_before = coord
        .oplog()
        .load_current_reconciled()
        .expect("view")
        .view
        .agents
        .get("agent-other-branch")
        .and_then(|a| a.own_blobs.get("cfg.rs"))
        .copied()
        .expect("other own blob");

    fs::write(root.join("cfg.rs"), "pub const N: i32 = 2;\n").expect("edit a");
    handle_post_tool_hook(&coord, "agent-a", "cfg.rs").expect("post-tool a");
    fs::write(root.join("cfg.rs"), "pub const N: i32 = 3;\n").expect("edit b");
    let o_b = handle_post_tool_hook(&coord, "agent-b", "cfg.rs").expect("post-tool b");
    assert!(o_b.has_conflict);

    // Attempting `ResolveStrategy::WorkingFile` while markers remain on disk must fail!
    assert!(
        coord
            .resolve_conflict("cfg.rs", ResolveStrategy::WorkingFile)
            .is_err(),
        "resolve_conflict(WorkingFile) must reject files still containing conflict markers"
    );

    // Edit file cleanly and resolve.
    fs::write(root.join("cfg.rs"), "pub const N: i32 = 5;\n").expect("edit resolved");
    coord
        .resolve_conflict("cfg.rs", ResolveStrategy::WorkingFile)
        .expect("resolve working file");

    // Verify `agent-other-branch`'s own_blob was NOT corrupted by resolving `main`'s conflict!
    let other_blob_after = coord
        .oplog()
        .load_current_reconciled()
        .expect("view after")
        .view
        .agents
        .get("agent-other-branch")
        .and_then(|a| a.own_blobs.get("cfg.rs"))
        .copied()
        .expect("other own blob after");
    assert_eq!(
        other_blob_after, other_blob_before,
        "resolve_conflict must not overwrite own_blobs of agents on other branches"
    );
}

#[test]
fn ensure_session_never_adopts_dirty_mcp_or_explicit_human_term_claim() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("app.rs"), "pub fn main() {}\n").expect("write app.rs");
    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    // Explicit human terminal claim (`term-777`) must NEVER be stolen by a new AI session.
    coord
        .claim_intent_and_symbols_with_branch(
            "term-777",
            Some("Human feature work"),
            &["app.rs".to_string()],
            Some("feat/human"),
        )
        .expect("claim term-777");

    // Dirty `mcp-dirty` session must NEVER be adopted (which would drop its own_blobs).
    coord
        .claim_intent_and_symbols("mcp-dirty", Some("MCP edit"), &["app.rs".to_string()])
        .expect("claim mcp-dirty");
    fs::write(root.join("app.rs"), "pub fn main() { /* mcp */ }\n").expect("edit mcp");
    handle_post_tool_hook(&coord, "mcp-dirty", "app.rs").expect("post-tool mcp-dirty");

    let new_ai = coord
        .ensure_session("claude-new-1", None)
        .expect("ensure claude-new-1");
    assert_ne!(new_ai.target_branch.as_deref(), Some("feat/human"));

    let view = coord.oplog().load_current_reconciled().expect("view").view;
    assert!(
        view.agents.contains_key("term-777"),
        "explicit human term-777 claim must not be stolen"
    );
    assert!(
        view.agents
            .get("mcp-dirty")
            .is_some_and(|a| a.has_own_changes()),
        "dirty mcp-dirty session must not be adopted or have its own_blobs discarded"
    );
}

#[test]
fn branch_slot_avoids_trunk_session_slot_collisions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("app.rs"), "pub fn main() {}\n").expect("write app.rs");
    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    let a1 = coord.ensure_session("agent-trunk-1", None).expect("a1");
    let a2 = coord.ensure_session("agent-trunk-2", None).expect("a2");
    assert_eq!(a1.slot, 1);
    assert_eq!(a2.slot, 2);

    let view = coord.oplog().load_current_reconciled().expect("view").view;
    let feat_slot = coord.branch_slot(&view, "feat/no-collide", "main");
    assert_ne!(
        feat_slot, a2.slot,
        "new feature branch slot must not collide with active trunk session on slot 2"
    );
    assert_eq!(feat_slot, 3);
}

#[test]
fn undo_skips_ephemeral_ops_bridges_gc_and_preserves_concurrent_peer_edits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("a.rs"), "pub fn a() -> i32 { 1 }\n").expect("write a.rs");
    fs::write(root.join("b.rs"), "pub fn b() -> i32 { 1 }\n").expect("write b.rs");
    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    let mut cfg = coord.oplog().load_config().expect("load_config");
    cfg.auto_isolate_dirty_claims = false;
    coord.oplog().save_config(&cfg).expect("save_config");

    coord
        .claim_intent_and_symbols("agent-a", Some("Update a"), &["a.rs".to_string()])
        .expect("claim a");
    coord
        .claim_intent_and_symbols("agent-b", Some("Update b"), &["b.rs".to_string()])
        .expect("claim b");

    fs::write(root.join("a.rs"), "pub fn a() -> i32 { 10 }\n").expect("edit a");
    handle_post_tool_hook(&coord, "agent-a", "a.rs").expect("post-tool a");

    fs::write(root.join("b.rs"), "pub fn b() -> i32 { 20 }\n").expect("edit b");
    handle_post_tool_hook(&coord, "agent-b", "b.rs").expect("post-tool b");

    // Only `agent-a` commits onto `main`; `agent-b` remains uncommitted.
    coord
        .merge_agents_with_message(
            &["agent-a".to_string()],
            Some("main"),
            None,
            false,
            Some("feat: update a"),
        )
        .expect("commit a");

    // Append passive ephemeral ops (pre-tool + watcher observe) and run GC compaction before bare `undo`.
    handle_pre_tool_hook(&coord, "agent-b", "b.rs").expect("pre-tool b");
    coord
        .oplog()
        .record_tx(
            "watcher",
            "observe intermediate blob for b.rs",
            true,
            |_| Ok(()),
        )
        .expect("watcher op");

    let compacted = coord.oplog().gc_compact().expect("gc_compact");
    assert!(
        compacted >= 1,
        "gc_compact must prune intermediate ephemeral ops before undo"
    );

    // Run bare undo: must bridge GC-compacted parents, skip ephemeral ops, undo `agent-a`'s commit, AND keep `agent-b`'s edit!
    let _undo_op = coord.undo(None).expect("bare undo");

    assert_eq!(
        fs::read_to_string(root.join("a.rs")).expect("read a.rs"),
        "pub fn a() -> i32 { 1 }\n",
        "undo must roll back agent-a's merged commit"
    );
    assert_eq!(
        fs::read_to_string(root.join("b.rs")).expect("read b.rs"),
        "pub fn b() -> i32 { 20 }\n",
        "undo must preserve innocent concurrent peer agent-b's uncommitted edit on disk"
    );

    let view_after = coord.oplog().load_current_reconciled().expect("view").view;
    assert!(
        view_after
            .agents
            .get("agent-b")
            .is_some_and(|b| b.own_blobs.contains_key("b.rs")),
        "undo must preserve innocent concurrent peer agent-b's own_blobs"
    );
}

#[test]
fn pre_tool_protects_unowned_human_edits_and_post_tool_preserves_executable_mode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let script_path = root.join("deploy.sh");
    fs::write(&script_path, "#!/bin/sh\necho base\n").expect("write deploy.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("chmod +x deploy.sh");
    }

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    // Feature branch commits a different version of `deploy.sh`.
    coord
        .claim_intent_and_symbols_with_branch(
            "agent-feat",
            Some("Update deploy script"),
            &["deploy.sh".to_string()],
            Some("feat/deploy"),
        )
        .expect("claim feat/deploy");
    fs::write(&script_path, "#!/bin/sh\necho feat-v1\n").expect("write feat-v1");
    handle_post_tool_hook(&coord, "agent-feat", "deploy.sh").expect("post-tool feat");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script_path)
            .expect("stat deploy.sh")
            .permissions()
            .mode();
        assert_ne!(
            mode & 0o111,
            0,
            "post-tool must preserve executable (+x) permissions on deploy.sh"
        );
    }

    coord
        .merge_agents_with_message(
            &["agent-feat".to_string()],
            Some("feat/deploy"),
            None,
            false,
            Some("feat: deploy v1"),
        )
        .expect("commit feat/deploy");

    // Human makes an unowned uncommitted edit to `deploy.sh` directly in `repo_root`.
    let human_edit = "#!/bin/sh\necho human-wip-do-not-overwrite\n";
    fs::write(&script_path, human_edit).expect("human edit deploy.sh");

    // A feature-branch agent running `pre-tool` must NOT silently overwrite the human's edit!
    handle_pre_tool_hook(&coord, "agent-feat", "deploy.sh").expect("pre-tool feat");
    assert_eq!(
        fs::read_to_string(&script_path).expect("read deploy.sh"),
        human_edit,
        "pre-tool must never silently overwrite unowned uncommitted human edits in repo_root"
    );
}

#[test]
fn watcher_skips_head_blobs_and_drops_stale_pre_commit_observations() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("other.txt"), "hello\n").expect("write other.txt");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");
    let initial_head = coord
        .git()
        .resolve_head_commit()
        .expect("head")
        .expect("cid");
    let initial_tree = coord
        .git()
        .read_commit(&initial_head)
        .expect("commit")
        .tree_id;
    let hello_blob = coord.git().write_blob(b"hello\n").expect("hello blob");

    coord
        .claim_intent_and_symbols("term-diff", Some("Watch other"), &["other.txt".to_string()])
        .expect("claim term-diff");

    // Unmodified HEAD blob must never be recorded into `observed_blobs`.
    assert!(
        !crate::watcher::record_watched_blob_if_fresh(
            &coord,
            "other.txt",
            hello_blob,
            Some(initial_head),
            Some(initial_tree),
        ),
        "watcher must skip blobs that already match HEAD"
    );

    // Advance HEAD via commit, then verify a stale pre-commit observation is rejected.
    fs::write(root.join("other.txt"), "trunk v3\n").expect("write v3");
    handle_post_tool_hook(&coord, "term-diff", "other.txt").expect("post-tool v3");
    coord
        .merge_agents_with_message(
            &["term-diff".to_string()],
            Some("main"),
            None,
            false,
            Some("feat: trunk v3"),
        )
        .expect("commit v3");

    let stale_edit_blob = coord
        .git()
        .write_blob(b"unstaged edit\n")
        .expect("stale blob");
    assert!(
        !crate::watcher::record_watched_blob_if_fresh(
            &coord,
            "other.txt",
            stale_edit_blob,
            Some(initial_head),
            Some(initial_tree),
        ),
        "watcher must drop observations whose HEAD advanced concurrently"
    );

    // Subsequent same-branch concurrent edits must record the real conflict on `r2` without a phantom conflict on `r1`.
    coord
        .claim_intent_and_symbols_with_branch(
            "r1",
            Some("Left"),
            &["other.txt".to_string()],
            Some("main"),
        )
        .expect("claim r1");
    coord
        .claim_intent_and_symbols_with_branch(
            "r2",
            Some("Right"),
            &["other.txt".to_string()],
            Some("main"),
        )
        .expect("claim r2");

    fs::write(root.join("other.txt"), "left version\n").expect("write left");
    let out1 = handle_post_tool_hook(&coord, "r1", "other.txt").expect("post-tool r1");
    assert!(
        !out1.has_conflict,
        "first editor r1 must not hit a phantom conflict"
    );

    fs::write(root.join("other.txt"), "right version\n").expect("write right");
    let out2 = handle_post_tool_hook(&coord, "r2", "other.txt").expect("post-tool r2");
    assert!(
        out2.has_conflict,
        "second editor r2 must detect the real conflict"
    );
    let cur = coord.oplog().load_current_reconciled().expect("load oplog");
    assert!(
        cur.view.conflicts.contains_key("other.txt"),
        "active conflict must remain registered for tes resolve"
    );
}

#[test]
fn harness_inverted_markers_never_duplicate_contents() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let md_path = tmp.path().join("AGENTS.md");
    let inverted_md = "<!-- tessera:end -->\nunique user heading\n<!-- tessera:start -->\n";
    fs::write(&md_path, inverted_md).expect("write inverted md");
    upsert_marked_section(
        &md_path,
        "<!-- tessera:start -->\nmanaged block\n<!-- tessera:end -->\n",
    )
    .expect("upsert inverted tessera md");
    let after_md = fs::read_to_string(&md_path).expect("read md");
    assert_eq!(
        after_md.matches("unique user heading").count(),
        1,
        "inverted tessera markers must not duplicate existing markdown content: {after_md}"
    );
    assert_eq!(
        after_md.matches("managed block").count(),
        1,
        "managed block should be appended once: {after_md}"
    );

    let lunr_path = tmp.path().join("CLAUDE.md");
    let inverted_lunr = "<!-- lunr:end -->\nkeep legacy note\n<!-- lunr:start -->\n";
    fs::write(&lunr_path, inverted_lunr).expect("write inverted lunr md");
    upsert_marked_section(
        &lunr_path,
        "<!-- tessera:start -->\nnew block\n<!-- tessera:end -->\n",
    )
    .expect("upsert inverted lunr md");
    let after_lunr = fs::read_to_string(&lunr_path).expect("read lunr md");
    assert_eq!(
        after_lunr.matches("keep legacy note").count(),
        1,
        "inverted lunr markers must not duplicate existing content: {after_lunr}"
    );

    let hook_path = tmp.path().join("post-commit");
    let inverted_hook = "#!/bin/sh\necho keep-hook\n# tessera:end\necho middle\n# tessera:start\n";
    fs::write(&hook_path, inverted_hook).expect("write inverted hook");
    upsert_git_hook(&hook_path, "# tessera:start\necho synced\n# tessera:end\n")
        .expect("upsert inverted git hook");
    let after_hook = fs::read_to_string(&hook_path).expect("read hook");
    assert_eq!(
        after_hook.matches("echo keep-hook").count(),
        1,
        "inverted hook markers must not duplicate hook prefix: {after_hook}"
    );
    assert_eq!(
        after_hook.matches("echo middle").count(),
        1,
        "inverted hook markers must not duplicate middle content: {after_hook}"
    );
    assert_eq!(
        after_hook.matches("echo synced").count(),
        1,
        "hook snippet should be appended once: {after_hook}"
    );
}

#[test]
fn merge_agents_with_options_preserves_concurrent_peer_edits_during_verify() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("a.rs"), "pub fn a() -> i32 { 1 }\n").expect("write a.rs");
    fs::write(root.join("b1.rs"), "pub fn b1() -> i32 { 1 }\n").expect("write b1.rs");
    fs::write(root.join("b2.rs"), "pub fn b2() -> i32 { 1 }\n").expect("write b2.rs");

    let (coord, init_report) = Coordinator::init_repo(root).expect("init_repo");
    let trunk_branch = init_report.trunk_branch;

    coord
        .claim_intent_and_symbols("agent-a", Some("Update a"), &["a.rs".to_string()])
        .expect("claim a");
    coord
        .claim_intent_and_symbols("agent-b", Some("Update b"), &["b1.rs".to_string()])
        .expect("claim b");

    fs::write(root.join("a.rs"), "pub fn a() -> i32 { 2 }\n").expect("edit a.rs");
    handle_post_tool_hook(&coord, "agent-a", "a.rs").expect("post-tool a");

    fs::write(root.join("b1.rs"), "pub fn b1() -> i32 { 10 }\n").expect("edit b1.rs");
    handle_post_tool_hook(&coord, "agent-b", "b1.rs").expect("post-tool b1");

    let sync_tmp = tempfile::tempdir().expect("sync tempdir");
    let started_flag = sync_tmp.path().join("verify_started");
    let done_flag = sync_tmp.path().join("peer_done");

    #[cfg(unix)]
    let verify_cmd = format!(
        "touch '{}' && while [ ! -f '{}' ]; do sleep 0.02; done",
        started_flag.display(),
        done_flag.display()
    );

    #[cfg(windows)]
    let verify_cmd = {
        let script_path = sync_tmp.path().join("verify.cmd");
        let cmd_body = "@echo off\r\necho ok > \"%~dp0verify_started\"\r\n:wait\r\nif exist \"%~dp0peer_done\" exit /b 0\r\nping -n 2 127.0.0.1 >nul\r\ngoto wait\r\n";
        fs::write(&script_path, cmd_body).expect("write verify.cmd");
        format!("call \"{}\"", script_path.display())
    };

    let report = std::thread::scope(|s| {
        let root_ref = root;
        let started_ref = &started_flag;
        let done_ref = &done_flag;

        let peer_handle = s.spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);

            while !started_ref.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            assert!(
                started_ref.exists(),
                "timed out waiting for --verify command to start"
            );

            let peer_coord = Coordinator::open(root_ref).expect("open peer coord");
            fs::write(root_ref.join("b2.rs"), "pub fn b2() -> i32 { 77 }\n")
                .expect("peer edit b2.rs");
            handle_post_tool_hook(&peer_coord, "agent-b", "b2.rs").expect("peer post-tool b2");
            peer_coord
                .claim_intent_and_symbols("agent-b", None, &["b2.rs::b2".to_string()])
                .expect("peer claim b2");
            fs::write(done_ref, b"ok").expect("write done flag");
        });

        let merge_res = coord.merge_agents_with_options(
            &["agent-a".to_string()],
            Some(&trunk_branch),
            Some(&verify_cmd),
            false,
            &CommitOptions {
                message: Some("feat: agent a".to_string()),
                ..Default::default()
            },
        );
        peer_handle.join().expect("peer thread panicked");
        merge_res.expect("merge_agents_with_options")
    });

    assert!(report.verification_passed);

    let cur = coord
        .oplog()
        .load_current_reconciled()
        .expect("load oplog after merge");
    let agent_b = cur
        .view
        .agents
        .get("agent-b")
        .expect("agent-b must remain active");
    assert_eq!(agent_b.base_commit, report.merged_commit_id);
    assert!(
        agent_b.own_blobs.contains_key("b1.rs"),
        "agent-b original edit b1.rs must remain tracked: {:?}",
        agent_b.own_blobs
    );
    assert!(
        agent_b.own_blobs.contains_key("b2.rs"),
        "agent-b concurrent edit b2.rs recorded during --verify must not be lost: {:?}",
        agent_b.own_blobs
    );
    assert!(
        agent_b.claimed_symbols.contains("b2.rs::b2"),
        "agent-b concurrent symbol claim must not be lost: {:?}",
        agent_b.claimed_symbols
    );

    let head_map = coord
        .git()
        .read_tree_to_map(&agent_b.head_tree)
        .expect("read agent-b head_tree");
    let b2_bytes = coord
        .git()
        .read_blob(&head_map.get("b2.rs").expect("b2.rs in head_tree").blob_id)
        .expect("read b2.rs blob");
    assert_eq!(
        String::from_utf8_lossy(&b2_bytes),
        "pub fn b2() -> i32 { 77 }\n"
    );
    let a_bytes = coord
        .git()
        .read_blob(&head_map.get("a.rs").expect("a.rs in head_tree").blob_id)
        .expect("read a.rs blob");
    assert_eq!(
        String::from_utf8_lossy(&a_bytes),
        "pub fn a() -> i32 { 2 }\n"
    );
}

#[test]
fn sync_all_agents_skips_unchanged_and_does_not_clobber_concurrent_edits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("a.rs"), "pub fn a() -> i32 { 1 }\n").expect("write a.rs");
    fs::write(root.join("b.rs"), "pub fn b() -> i32 { 1 }\n").expect("write b.rs");

    let (coord, init_report) = Coordinator::init_repo(root).expect("init_repo");
    let trunk_ref = format!("refs/heads/{}", init_report.trunk_branch);

    coord
        .claim_intent_and_symbols("agent-stale", Some("Edit b"), &["b.rs".to_string()])
        .expect("claim stale");
    fs::write(root.join("b.rs"), "pub fn b() -> i32 { 2 }\n").expect("edit b.rs");
    handle_post_tool_hook(&coord, "agent-stale", "b.rs").expect("post-tool stale");

    // Advance trunk directly in Git so `agent-stale` falls behind while `agent-fresh`
    // starts at the new tip.
    let old_main = coord
        .git()
        .resolve_ref(&trunk_ref)
        .expect("resolve trunk")
        .expect("trunk cid");
    let old_tree = coord
        .git()
        .read_commit(&old_main)
        .expect("read trunk")
        .tree_id;
    let mut next_map = coord.git().read_tree_to_map(&old_tree).expect("read tree");
    let new_a_blob = coord
        .git()
        .write_blob(b"pub fn a() -> i32 { 99 }\n")
        .expect("write a blob");
    next_map.insert(
        "a.rs".to_string(),
        core::FileEntry::from_mode(new_a_blob, core::EntryMode::NormalFile),
    );
    let next_tree = coord
        .git()
        .write_tree_from_map(&next_map)
        .expect("write next tree");
    let author = coord.git().resolve_git_author();
    let new_main = coord
        .git()
        .write_commit(
            &next_tree,
            &[old_main],
            &author,
            "feat: external trunk update",
        )
        .expect("write next commit");
    coord
        .git()
        .update_ref(&trunk_ref, &new_main)
        .expect("update trunk");

    coord
        .claim_intent_and_symbols("agent-fresh", Some("Fresh agent"), &["a.rs".to_string()])
        .expect("claim fresh");

    let rebased = coord.sync_all_agents().expect("sync_all_agents");
    assert!(
        rebased.iter().any(|a| a.agent_id == "agent-stale"),
        "stale agent must be rebased: {rebased:?}"
    );
    assert!(
        !rebased.iter().any(|a| a.agent_id == "agent-fresh"),
        "unchanged agent already at target_cid must not be pushed into rebased_agents: {rebased:?}"
    );

    // Verify concurrent view mutations inside `apply_synced_agent_to_view` are never clobbered.
    let cur = coord.oplog().load_current_reconciled().expect("load oplog");
    let before = cur
        .view
        .agents
        .get("agent-stale")
        .expect("agent-stale")
        .clone();
    let mut rebased_candidate = before.clone();
    rebased_candidate.base_commit = old_main;
    rebased_candidate.head_commit = old_main;
    rebased_candidate.head_tree = old_tree;
    rebased_candidate.read_blobs.clear();

    // A concurrently closed agent must not be resurrected.
    let mut closed_view = cur.view.clone();
    closed_view.agents.remove("agent-stale");
    assert!(
        apply_synced_agent_to_view(&mut closed_view, &rebased_candidate, Some(&before)).is_none()
    );
    assert!(!closed_view.agents.contains_key("agent-stale"));

    // An agent that recorded new `own_blobs` or switched branches concurrently must be skipped.
    let mut concurrent_edit_view = cur.view.clone();
    let live_edit = concurrent_edit_view
        .agents
        .get_mut("agent-stale")
        .expect("live edit");
    live_edit.own_blobs.insert("c.rs".to_string(), new_a_blob);
    assert!(
        apply_synced_agent_to_view(&mut concurrent_edit_view, &rebased_candidate, Some(&before))
            .is_none()
    );
    assert!(
        concurrent_edit_view.agents["agent-stale"]
            .own_blobs
            .contains_key("c.rs"),
        "concurrent own_blobs edit must not be clobbered"
    );

    // Concurrent metadata-only updates (`claimed_symbols`, `intent`, new `read_blobs`)
    // are preserved while the rebase commits/trees are applied.
    let mut metadata_view = cur.view.clone();
    let live_meta = metadata_view
        .agents
        .get_mut("agent-stale")
        .expect("live meta");
    live_meta
        .claimed_symbols
        .insert("b.rs::concurrent_sym".to_string());
    live_meta.intent = "Refined concurrent intent".to_string();
    live_meta
        .read_blobs
        .insert("extra.rs".to_string(), new_a_blob);
    let applied = apply_synced_agent_to_view(&mut metadata_view, &rebased_candidate, Some(&before))
        .expect("metadata-only concurrent update should still apply rebase");
    assert_eq!(applied.base_commit, old_main);
    assert_eq!(applied.intent, "Refined concurrent intent");
    assert!(applied.claimed_symbols.contains("b.rs::concurrent_sym"));
    assert_eq!(applied.read_blobs.get("extra.rs"), Some(&new_a_blob));
}

#[test]
fn gc_prunes_orphaned_sandboxes_sidecars_and_dead_leases_while_preserving_active_ones() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn base() -> i32 { 1 }\n").expect("write lib.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");
    let (_, main_ws, _) = coord
        .prepare_branch_sandbox("term-main", Some("main"))
        .expect("prepare main sandbox");
    assert!(main_ws.is_dir(), "main sandbox should exist");

    let branches_root = coord.branches_root();
    let orphan_idle = branches_root.join("legacy-orphan-branch");
    let orphan_live = branches_root.join("legacy-live-branch");
    fs::create_dir_all(&orphan_idle).expect("mkdir orphan_idle");
    fs::write(orphan_idle.join("stale.txt"), b"old").expect("write stale.txt");
    fs::write(branches_root.join("legacy-orphan-branch.slot"), b"9").expect("write orphan slot");
    fs::write(
        branches_root.join("legacy-orphan-branch.tree"),
        b"0000000000000000000000000000000000000000",
    )
    .expect("write orphan tree");
    fs::write(
        branches_root.join("legacy-orphan-branch.pending-merge"),
        b"1",
    )
    .expect("write orphan pending-merge");

    fs::create_dir_all(&orphan_live).expect("mkdir orphan_live");
    fs::write(branches_root.join("legacy-live-branch.slot"), b"10").expect("write live slot");

    let leases_dir = coord.sandboxes_root().join("bin").join(".leases");
    let dead_lease = leases_dir.join("3999");
    let live_lease = leases_dir.join("3998");
    fs::create_dir_all(&dead_lease).expect("mkdir dead_lease");
    fs::write(dead_lease.join("pid"), b"99999999").expect("write dead pid");
    fs::write(
        dead_lease.join("workspace"),
        orphan_idle.display().to_string(),
    )
    .expect("write dead ws");
    fs::write(leases_dir.join("3999.lock"), b"").expect("write non-numeric lock file");

    fs::create_dir_all(&live_lease).expect("mkdir live_lease");
    fs::write(live_lease.join("pid"), std::process::id().to_string()).expect("write live pid");
    fs::write(
        live_lease.join("workspace"),
        orphan_live.display().to_string(),
    )
    .expect("write live ws");

    // Dry-run must preview everything without deleting any file or directory.
    let preview = coord.gc(true).expect("gc dry-run");
    assert!(preview.dry_run);
    assert_eq!(preview.pruned_sandboxes, vec!["legacy-orphan-branch"]);
    assert_eq!(
        preview.pruned_sidecars,
        vec![
            "legacy-orphan-branch.pending-merge",
            "legacy-orphan-branch.slot",
            "legacy-orphan-branch.tree",
        ]
    );
    assert_eq!(preview.pruned_dead_leases, vec!["3999"]);
    assert_eq!(preview.skipped_active_sandboxes, vec!["legacy-live-branch"]);
    assert!(orphan_idle.is_dir(), "dry-run must not delete orphan_idle");
    assert!(dead_lease.is_dir(), "dry-run must not delete dead_lease");

    // Real gc must prune idle orphan + sidecars + dead lease while keeping live-leased & real sandboxes.
    let report = coord.gc(false).expect("gc apply");
    assert!(!report.dry_run);
    assert_eq!(report.pruned_sandboxes, vec!["legacy-orphan-branch"]);
    assert_eq!(
        report.pruned_sidecars,
        vec![
            "legacy-orphan-branch.pending-merge",
            "legacy-orphan-branch.slot",
            "legacy-orphan-branch.tree",
        ]
    );
    assert_eq!(report.pruned_dead_leases, vec!["3999"]);
    assert_eq!(report.skipped_active_sandboxes, vec!["legacy-live-branch"]);

    assert!(!orphan_idle.exists(), "orphan_idle must be removed");
    assert!(!branches_root.join("legacy-orphan-branch.slot").exists());
    assert!(!branches_root.join("legacy-orphan-branch.tree").exists());
    assert!(
        !branches_root
            .join("legacy-orphan-branch.pending-merge")
            .exists()
    );
    assert!(!dead_lease.exists(), "dead_lease must be removed");
    assert!(
        leases_dir.join("3999.lock").is_file(),
        "lock files must not be deleted"
    );
    assert!(
        orphan_live.is_dir() && branches_root.join("legacy-live-branch.slot").is_file(),
        "orphan sandbox with live lease must be preserved"
    );
    assert!(live_lease.is_dir(), "live lease must be preserved");
    assert!(main_ws.is_dir(), "known branch sandbox must be preserved");
}

#[test]
fn temp_repo_sandboxes_stay_contained_with_cache_hygiene_and_repo_marker() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("README.md"), "# temp repo\n").expect("write readme");

    let (coord, _) = Coordinator::init_repo(tmp.path()).expect("init_repo");
    let (_, branch_ws, _) = coord
        .prepare_branch_sandbox("agent-hygiene", Some("main"))
        .expect("prepare_branch_sandbox");
    let sb_root = coord.sandboxes_root();
    let base_dir = sb_root.parent().expect("sandboxes_root parent");

    assert!(
        core::sandbox_paths::path_is_within(&sb_root, tmp.path()),
        "tempdir repo sandbox must stay contained inside the tempdir, got: {}",
        sb_root.display()
    );
    assert!(
        branch_ws.is_dir(),
        "prepared branch sandbox must exist at {}",
        branch_ws.display()
    );
    assert!(
        base_dir.join("CACHEDIR.TAG").is_file(),
        "workspaces base must contain CACHEDIR.TAG"
    );

    #[cfg(target_os = "macos")]
    assert!(
        base_dir.join(".metadata_never_index").is_file(),
        "macOS workspaces base must contain .metadata_never_index"
    );

    let repo_marker = fs::read_to_string(sb_root.join(".repo_root")).expect(".repo_root marker");
    assert_eq!(
        std::path::PathBuf::from(repo_marker.trim())
            .canonicalize()
            .expect("canon marker"),
        tmp.path().canonicalize().expect("canon tmp"),
        ".repo_root marker must point to the repository root"
    );
    assert_eq!(
        core::is_valid_sandbox_repo_root(&branch_ws, Some(tmp.path()))
            .map(|p| { p.canonicalize().unwrap_or(p) }),
        Some(tmp.path().canonicalize().expect("canon tmp")),
        "contained tempdir branch sandbox must validate via is_valid_sandbox_repo_root"
    );
}

#[test]
fn gc_reconciles_deleted_refs_prunes_merged_verify_tmp_caches_and_preserves_dirty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    fs::write(root.join("lib.rs"), "pub fn base() -> i32 { 1 }\n").expect("write lib.rs");

    let (coord, _) = Coordinator::init_repo(root).expect("init_repo");

    // Simulate a branch deleted externally (Git ref removed while OpLog `view.branches` still lists it).
    coord
        .switch_branch("a-ext", "feat/ext-del")
        .expect("create feat/ext-del");
    let (_, ext_ws, _) = coord
        .prepare_branch_sandbox("a-ext", Some("feat/ext-del"))
        .expect("prepare feat/ext-del sandbox");
    assert!(ext_ws.is_dir());
    coord.close_agent("a-ext").expect("close a-ext session");
    let ext_ref = coord.git().common_git_dir().join("refs/heads/feat/ext-del");
    fs::remove_file(&ext_ref).expect("simulate raw git branch -d");

    // Seed three merged branches: one clean & idle (prunable), one dirty (must skip),
    // and one clean but bound to an active session (must skip so live shell CWDs stay intact).
    coord
        .switch_branch("a-merged", "feat/merged-clean")
        .expect("create feat/merged-clean");
    let (_, merged_clean_ws, _) = coord
        .prepare_branch_sandbox("a-merged", Some("feat/merged-clean"))
        .expect("prepare merged-clean sandbox");
    coord.close_agent("a-merged").expect("close a-merged");

    coord
        .switch_branch("a-dirty", "feat/merged-dirty")
        .expect("create feat/merged-dirty");
    let (_, merged_dirty_ws, _) = coord
        .prepare_branch_sandbox("a-dirty", Some("feat/merged-dirty"))
        .expect("prepare merged-dirty sandbox");
    fs::write(
        merged_dirty_ws.join("lib.rs"),
        "pub fn base() -> i32 { 999 }\n",
    )
    .expect("dirty merged-dirty sandbox");
    coord.close_agent("a-dirty").expect("close a-dirty");

    coord
        .switch_branch("a-bound", "feat/merged-bound")
        .expect("create feat/merged-bound");
    let (_, merged_bound_ws, _) = coord
        .prepare_branch_sandbox("a-bound", Some("feat/merged-bound"))
        .expect("prepare merged-bound sandbox");

    // Seed an unmerged branch with a build cache (`target/`) for `--keep-merged`, `--prune-caches`, and `--stale 0`.
    coord
        .switch_branch("a-unmerged", "feat/unmerged-work")
        .expect("create feat/unmerged-work");
    let (_, unmerged_ws, _) = coord
        .prepare_branch_sandbox("a-unmerged", Some("feat/unmerged-work"))
        .expect("prepare unmerged sandbox");
    let unmerged_target = unmerged_ws.join("target");
    fs::create_dir_all(&unmerged_target).expect("mkdir target");
    fs::write(unmerged_target.join("artifact.bin"), vec![b'X'; 65_536]).expect("write artifact");
    // Commit on feat/unmerged-work so it is NOT merged into trunk.
    fs::write(unmerged_ws.join("unmerged.rs"), "pub fn u() -> i32 { 2 }\n")
        .expect("write unmerged");
    let unmerged_blob = coord
        .git()
        .write_blob(b"pub fn u() -> i32 { 2 }\n")
        .expect("write blob");
    let main_cid = coord
        .git()
        .resolve_ref("refs/heads/main")
        .expect("resolve main")
        .expect("main cid");
    let main_commit = coord.git().read_commit(&main_cid).expect("read main");
    let mut tree_map = coord
        .git()
        .read_tree_to_map(&main_commit.tree_id)
        .expect("read tree");
    tree_map.insert(
        "unmerged.rs".to_string(),
        core::FileEntry::regular(unmerged_blob, false),
    );
    let new_tree = coord
        .git()
        .write_tree_from_map(&tree_map)
        .expect("write tree");
    let new_cid = coord
        .git()
        .write_commit(
            &new_tree,
            &[main_cid],
            "Tester <test@example.com>",
            "feat: unmerged commit",
        )
        .expect("write commit");
    coord
        .git()
        .update_ref("refs/heads/feat/unmerged-work", &new_cid)
        .expect("update ref");
    coord.close_agent("a-unmerged").expect("close a-unmerged");

    // Seed crashed + live verify sandboxes, orphaned agent workspace, stale tmp dir, lock, and expired session slot.
    let dead_verify = coord.sandboxes_root().join("verify-sandbox-99999999-1-1");
    let live_verify = coord
        .sandboxes_root()
        .join(format!("verify-sandbox-{}-1-1", std::process::id()));
    fs::create_dir_all(&dead_verify).expect("mkdir dead verify");
    fs::write(dead_verify.join("tmp.bin"), vec![b'V'; 8192]).expect("write dead verify file");
    fs::create_dir_all(&live_verify).expect("mkdir live verify");

    let orphan_agent_ws = root.join(".tessera").join("workspaces").join("ghost-agent");
    fs::create_dir_all(&orphan_agent_ws).expect("mkdir orphan agent ws");
    fs::write(orphan_agent_ws.join("stale.rs"), b"// old").expect("write stale.rs");

    let stale_tmp = root.join(".tessera").join("tmp").join("ghost-agent");
    fs::create_dir_all(&stale_tmp).expect("mkdir stale tmp");
    fs::write(stale_tmp.join("scratch.txt"), b"scratch").expect("write scratch");

    let stale_lock = coord
        .git()
        .common_git_dir()
        .join("tessera")
        .join("locks")
        .join("sandbox-ghost-branch.lock");
    fs::create_dir_all(stale_lock.parent().expect("locks parent")).expect("mkdir locks");
    fs::write(&stale_lock, b"").expect("write stale lock");

    let expired_slot = coord
        .git()
        .common_git_dir()
        .join("tessera")
        .join("session-slots")
        .join("ghost-session.slot");
    fs::create_dir_all(expired_slot.parent().expect("slots parent")).expect("mkdir slots");
    fs::write(&expired_slot, b"5\n1000\n").expect("write expired slot");

    let ext_safe = Coordinator::sanitize_branch_name("feat/ext-del");
    let merged_clean_safe = Coordinator::sanitize_branch_name("feat/merged-clean");
    let merged_dirty_safe = Coordinator::sanitize_branch_name("feat/merged-dirty");
    let merged_bound_safe = Coordinator::sanitize_branch_name("feat/merged-bound");
    let unmerged_safe = Coordinator::sanitize_branch_name("feat/unmerged-work");

    // Dry run with `--keep-merged` must preserve merged-clean and mutate nothing.
    let keep_preview = coord
        .gc_with_options(&crate::GcOptions {
            dry_run: true,
            keep_merged: true,
            ..crate::GcOptions::default()
        })
        .expect("gc dry-run keep-merged");
    assert!(keep_preview.pruned_merged_sandboxes.is_empty());
    assert_eq!(keep_preview.pruned_sandboxes, vec![ext_safe.clone()]);
    assert_eq!(
        keep_preview.pruned_stale_view_branches,
        vec!["feat/ext-del"]
    );
    assert_eq!(
        keep_preview.pruned_verify_sandboxes,
        vec!["verify-sandbox-99999999-1-1"]
    );
    assert_eq!(keep_preview.pruned_agent_workspaces, vec!["ghost-agent"]);
    assert!(
        keep_preview
            .pruned_tmp_dirs
            .contains(&"repo/ghost-agent".to_string())
    );
    assert!(keep_preview.pruned_locks >= 1);
    assert!(keep_preview.pruned_session_slots >= 1);
    assert!(ext_ws.is_dir(), "dry-run must not remove ext_ws");
    assert!(dead_verify.is_dir(), "dry-run must not remove dead_verify");

    // Default apply + `--prune-caches` prunes everything safe, strips unmerged target/, and skips dirty or session-bound sandboxes.
    let report = coord
        .gc_with_options(&crate::GcOptions {
            dry_run: false,
            keep_merged: false,
            prune_caches: true,
            ..crate::GcOptions::default()
        })
        .expect("gc apply");
    assert_eq!(report.pruned_sandboxes, vec![ext_safe]);
    assert_eq!(report.pruned_stale_view_branches, vec!["feat/ext-del"]);
    assert_eq!(report.pruned_merged_sandboxes, vec![merged_clean_safe]);
    assert!(
        report.skipped_active_sandboxes.contains(&merged_dirty_safe),
        "dirty merged sandbox must be skipped: {report:?}"
    );
    assert!(
        report.skipped_active_sandboxes.contains(&merged_bound_safe),
        "sandbox bound to an active session must be skipped: {report:?}"
    );
    assert_eq!(
        report.pruned_verify_sandboxes,
        vec!["verify-sandbox-99999999-1-1"]
    );
    assert_eq!(report.pruned_agent_workspaces, vec!["ghost-agent"]);
    assert!(
        report
            .pruned_caches
            .contains(&format!("{unmerged_safe}/target")),
        "idle sandbox cache should be pruned: {report:?}"
    );
    assert!(report.reclaimed_bytes > 0);

    assert!(!ext_ws.exists());
    assert!(!merged_clean_ws.exists());
    assert!(
        merged_dirty_ws.is_dir(),
        "dirty merged sandbox must survive"
    );
    assert!(
        merged_bound_ws.is_dir(),
        "active-session merged sandbox must survive"
    );
    assert!(!dead_verify.exists());
    assert!(live_verify.is_dir(), "live verify sandbox must survive");
    assert!(!orphan_agent_ws.exists());
    assert!(!stale_tmp.exists());
    assert!(!stale_lock.exists());
    assert!(!expired_slot.exists());
    assert!(
        unmerged_ws.is_dir(),
        "unmerged sandbox must survive without --stale"
    );
    assert!(
        !unmerged_target.exists(),
        "target cache inside unmerged sandbox must be stripped"
    );

    // Confirm `feat/ext-del` was removed from OpLog `view.branches`.
    let view_after = coord
        .oplog()
        .load_current_reconciled()
        .expect("load view")
        .view;
    assert!(
        !view_after.branches.contains_key("feat/ext-del"),
        "stale view.branches entry must be pruned"
    );

    // `--stale 0` evicts clean unmerged sandbox while leaving trunk intact.
    let stale_report = coord
        .gc_with_options(&crate::GcOptions {
            stale_days: Some(0),
            ..crate::GcOptions::default()
        })
        .expect("gc stale 0");
    assert_eq!(stale_report.pruned_stale_sandboxes, vec![unmerged_safe]);
    assert!(!unmerged_ws.exists());

    // `close_agent()` cleans up `.tessera/tmp/<agent_id>`.
    coord
        .switch_branch("a-tmp-close", "main")
        .expect("spawn a-tmp-close");
    let agent_tmp_dir = root.join(".tessera").join("tmp").join("a-tmp-close");
    fs::create_dir_all(&agent_tmp_dir).expect("mkdir agent_tmp_dir");
    fs::write(agent_tmp_dir.join("buf.txt"), b"ephemeral").expect("write buf");
    coord.close_agent("a-tmp-close").expect("close a-tmp-close");
    assert!(
        !agent_tmp_dir.exists(),
        "close_agent must clean up .tessera/tmp/<agent_id>"
    );

    coord.close_agent("a-bound").expect("close a-bound");
    let _ = fs::remove_dir_all(&live_verify);
}
