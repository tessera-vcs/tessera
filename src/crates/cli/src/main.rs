//! Command-line interface and built-in MCP server for Tessera (`tes`).
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

use clap::Parser;

pub mod args;
pub mod commands;
pub mod doctor;
pub mod env_hooks;
pub mod mcp;
pub mod output;

use args::{Cli, Commands};
use commands::branch::{BranchArgs, SwitchArgs, run_branch, run_switch};
use commands::commit_merge::{
    CommitArgs, MergeArgs, run_cherry_pick, run_commit, run_merge, run_sync,
};
use commands::diff_log::{DiffArgs, LogArgs, run_diff, run_log, run_oplog, run_radar, run_status};
use commands::ops::{
    run_agent, run_checkpoint, run_claim, run_complete, run_config, run_conflicts, run_exec,
    run_external, run_gc, run_hook, run_init, run_resolve, run_restore, run_stash, run_undo,
    run_watch,
};
use commands::push_pull::{PushArgs, run_pull, run_push};
use doctor::run_doctor;
use env_hooks::{EnvOptions, run_env};
use output::CliContext;

/// Entrypoint for the `tes` CLI and built-in MCP server.
fn main() -> anyhow::Result<()> {
    // Spawn the CLI dispatcher on a dedicated 16 MiB stack thread so unoptimized debug builds
    // (`cargo test`) never overflow Windows MSVC's 1 MiB default main-thread stack.
    let handle = std::thread::Builder::new()
        .name("tes-main".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(run_cli)?;

    match handle.join() {
        Ok(res) => res,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Executes the parsed `tes` CLI command or built-in MCP server.
fn run_cli() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cwd = match &cli.repo {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let ctx = CliContext {
        cwd,
        json: cli.json,
        no_pager: cli.no_pager,
        paginate: cli.paginate,
    };

    match cli.command {
        Commands::Init {
            path,
            branch,
            harness,
            local,
        } => run_init(&ctx, path, branch, harness, local),
        Commands::Doctor => run_doctor(&ctx.cwd, ctx.json, ctx.no_pager, ctx.paginate),
        Commands::Status {
            short,
            porcelain,
            branch,
            agent,
        } => run_status(&ctx, short, porcelain, branch, agent),
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
        } => run_diff(
            &ctx,
            DiffArgs {
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
            },
        ),
        Commands::Radar => run_radar(&ctx),
        Commands::Claim {
            intent,
            branch,
            from,
            agent,
            symbols,
        } => run_claim(&ctx, intent, branch, from, agent, symbols),
        Commands::Checkpoint { label, agent } => run_checkpoint(&ctx, label, agent),
        Commands::Agent { sub } => run_agent(&ctx, sub),
        Commands::Commit {
            message,
            branch,
            amend,
            no_edit,
            verify,
            verify_cmd,
            co_author,
            no_co_author,
            author,
            agent,
        } => run_commit(
            &ctx,
            CommitArgs {
                message,
                branch,
                amend,
                no_edit,
                verify,
                verify_cmd,
                co_author,
                no_co_author,
                author,
                agent,
            },
        ),
        Commands::Push {
            remote,
            branch,
            set_upstream,
            message,
            force,
            force_with_lease,
            tags,
            delete,
            dry_run,
            no_auto_commit,
            agent,
        } => run_push(
            &ctx,
            PushArgs {
                remote,
                branch,
                set_upstream,
                message,
                force,
                force_with_lease,
                tags,
                delete,
                dry_run,
                no_auto_commit,
                agent,
            },
        ),
        Commands::Pull {
            remote,
            branch,
            agent,
        } => run_pull(&ctx, remote, branch, agent),
        Commands::Stash {
            sub,
            message,
            branch,
            agent,
        } => run_stash(&ctx, sub, message, branch, agent),
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
        } => run_log(
            &ctx,
            LogArgs {
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
            },
        ),
        Commands::CherryPick {
            commit,
            branch,
            agent,
        } => run_cherry_pick(&ctx, commit, branch, agent),
        Commands::Merge {
            agents,
            into,
            message,
            verify,
            verify_cmd,
            dry_run,
            keep_open,
            co_author,
            no_co_author,
            author,
        } => run_merge(
            &ctx,
            MergeArgs {
                agents,
                into,
                message,
                verify,
                verify_cmd,
                dry_run,
                keep_open,
                co_author,
                no_co_author,
                author,
            },
        ),
        Commands::Sync => run_sync(&ctx),
        Commands::Conflicts => run_conflicts(&ctx),
        Commands::Resolve {
            path,
            strategy,
            content,
            content_file,
        } => run_resolve(&ctx, path, strategy, content, content_file),
        Commands::Oplog { limit } => run_oplog(&ctx, limit),
        Commands::Undo { op, checkpoint } => run_undo(&ctx, op, checkpoint),
        Commands::Restore {
            targets,
            source,
            branch,
            agent,
        } => run_restore(&ctx, targets, source, branch, agent),
        Commands::Gc => run_gc(&ctx),
        Commands::Watch {
            agent,
            interval_ms,
            once,
            auto_daemon,
        } => run_watch(&ctx, agent, interval_ms, once, auto_daemon),
        Commands::Hook { sub, session } => run_hook(&ctx, sub, session),
        Commands::Branch {
            branch,
            list,
            show_current,
            remotes,
            all,
            verbose,
            delete,
            force_delete,
            rename,
            editor,
            agent,
        } => run_branch(
            &ctx,
            BranchArgs {
                branch,
                list,
                show_current,
                remotes,
                all,
                verbose,
                delete,
                force_delete,
                rename,
                editor,
                agent,
            },
        ),
        Commands::Switch {
            branch,
            start_point,
            create,
            from,
            editor,
            agent,
        } => run_switch(
            &ctx,
            SwitchArgs {
                branch,
                start_point,
                create,
                from,
                editor,
                agent,
            },
        ),
        Commands::Exec { branch, agent, cmd } => run_exec(&ctx, branch, agent, cmd),
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
        } => run_env(
            &ctx,
            EnvOptions {
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
            },
        ),
        Commands::Config { sub, verify_cmd } => run_config(&ctx, sub, verify_cmd),
        Commands::Mcp => {
            mcp::run_mcp_stdio_server(&ctx.cwd)?;
            Ok(())
        }
        Commands::Complete(complete_args) => {
            run_complete(&ctx.cwd, &complete_args);
            Ok(())
        }
        Commands::External(ext_args) => run_external(&ctx, ext_args),
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use serde_json::Value;

    use crate::args::{AgentCommands, Cli, Commands, StashCommands};
    use crate::commands::branch::glob_matches;
    use crate::commands::ops::extract_hook_file_paths;
    use crate::output::{resolve_pager_command, resolve_user_pathspecs};

    #[test]
    fn clap_command_tree_passes_debug_assert() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_commit_and_merge_parse_co_author_and_author_flags() {
        let bare =
            Cli::try_parse_from(["tes", "commit", "-m", "feat: bare", "--co-author"]).unwrap();

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
            "--dry-run",
            "--verify",
            "--co-author=Pair <pair@example.com>",
            "--author",
            "Lead <lead@example.com>",
        ])
        .unwrap();
        assert!(merge_cmd.no_pager);

        match merge_cmd.command {
            Commands::Merge {
                agents,
                verify,
                dry_run,
                co_author,
                no_co_author,
                author,
                ..
            } => {
                assert_eq!(agents, vec!["feat-a".to_string()]);
                assert_eq!(verify.as_deref(), Some(""));
                assert!(dry_run);
                assert_eq!(co_author, vec!["Pair <pair@example.com>".to_string()]);
                assert!(!no_co_author);
                assert_eq!(author.as_deref(), Some("Lead <lead@example.com>"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let merge_verify_branch =
            Cli::try_parse_from(["tes", "merge", "--verify", "feat/a"]).unwrap();

        match merge_verify_branch.command {
            Commands::Merge { agents, verify, .. } => {
                assert_eq!(verify.as_deref(), Some(""));
                assert_eq!(agents, vec!["feat/a".to_string()]);
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
            Commands::Status {
                short,
                porcelain,
                branch,
                ..
            } => {
                assert!(short);
                assert!(!porcelain);
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

        assert!(Cli::try_parse_from(["tes", "branch", "-d", "foo", "-D", "bar"]).is_err());
        assert!(Cli::try_parse_from(["tes", "undo", "--op", "abc", "--checkpoint", "cp"]).is_err());

        let stash_pop = Cli::try_parse_from(["tes", "stash", "pop", "0", "-a", "editor"]).unwrap();

        match stash_pop.command {
            Commands::Stash {
                sub: Some(StashCommands::Pop { index }),
                agent,
                ..
            } => {
                assert_eq!(index.as_deref(), Some("0"));
                assert_eq!(agent.as_deref(), Some("editor"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_session_focus_and_pager_flags_parse() {
        let focus =
            Cli::try_parse_from(["tes", "--paginate", "session", "focus", "antigravity-6c7a"])
                .unwrap();
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

    #[test]
    fn glob_and_subdirectory_pathspecs_resolve_cleanly() {
        assert!(glob_matches("feat/*", "feat/auth"));
        assert!(glob_matches("feat/a?b", "feat/axb"));
        assert!(!glob_matches("feat/*", "fix/auth"));

        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("packages/api");
        std::fs::create_dir_all(&sub).unwrap();
        let resolved = resolve_user_pathspecs(
            &sub,
            tmp.path(),
            &[
                "src/lib.rs".to_string(),
                "../shared/mod.rs".to_string(),
                ".".to_string(),
                ":(exclude)target".to_string(),
            ],
        );
        assert_eq!(
            resolved,
            vec![
                "packages/api/src/lib.rs".to_string(),
                "packages/shared/mod.rs".to_string(),
                "packages/api".to_string(),
                ":(exclude)target".to_string(),
            ]
        );
    }
}
