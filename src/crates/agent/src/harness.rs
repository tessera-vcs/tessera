//! Repository-local AI harness rule, MCP server, lifecycle hook, and Git hook auto-installer,
//! including JSONC parsing support and non-destructive hook array merging.

use std::fs;
use std::path::Path;

use core::{CoreError, CoreResult, atomic_write_file};

/// Known repository-relative harness configuration and rule file paths that may be created by
/// [`install_harness_integrations_for`].
pub(crate) const KNOWN_HARNESS_PATHS: &[&str] = &[
    ".agents/rules/tessera.md",
    ".claude/settings.json",
    ".mcp.json",
    ".cursor/mcp.json",
    ".cursor/rules/tessera.mdc",
    ".windsurf/rules/tessera.md",
    ".kiro/steering/tessera.md",
    ".kiro/settings/mcp.json",
    "opencode.json",
    ".codex/config.toml",
    ".codex/hooks.json",
    ".vscode/mcp.json",
    "AGENTS.md",
    "CLAUDE.md",
];

/// Strips JSONC `//` line comments, `/* ... */` block comments, and trailing commas before `]` or
/// `}` outside of quoted string literals so existing VS Code, OpenCode, and Claude JSONC config
/// files can be parsed without losing user settings.
#[must_use]
pub(crate) fn strip_jsonc(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let len = bytes.len();
    let mut no_comments = Vec::with_capacity(len);
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;

    while i < len {
        let b = bytes[i];

        if in_string {
            no_comments.push(b);

            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }

            i += 1;
            continue;
        }

        if b == b'"' {
            in_string = true;
            no_comments.push(b);
            i += 1;
            continue;
        }

        if b == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            i += 2;

            while i < len && bytes[i] != b'\n' {
                i += 1;
            }

            continue;
        }

        if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            i += 2;

            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    no_comments.push(b'\n');
                }

                i += 1;
            }

            if i + 1 < len {
                i += 2;
            } else {
                i = len;
            }

            continue;
        }

        no_comments.push(b);
        i += 1;
    }

    let nc_len = no_comments.len();
    let mut out = Vec::with_capacity(nc_len);
    let mut j = 0;
    let mut in_str = false;
    let mut esc = false;

    while j < nc_len {
        let b = no_comments[j];

        if in_str {
            out.push(b);

            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }

            j += 1;
            continue;
        }

        if b == b'"' {
            in_str = true;
            out.push(b);
            j += 1;
            continue;
        }

        if b == b',' {
            let mut look = j + 1;

            while look < nc_len && no_comments[look].is_ascii_whitespace() {
                look += 1;
            }

            if look < nc_len && matches!(no_comments[look], b'}' | b']') {
                j += 1;
                continue;
            }
        }

        out.push(b);
        j += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| raw.to_string())
}

/// Parses an existing JSON or JSONC config file into a [`serde_json::Value`], returning
/// `default_obj` when the file does not exist or is whitespace-only.
///
/// # Errors
/// Returns [`CoreError::Serialization`] if an existing non-empty file cannot be parsed even after
/// stripping JSONC comments and trailing commas, preventing accidental clobbering of user config.
fn read_jsonc_object(path: &Path, default_obj: serde_json::Value) -> CoreResult<serde_json::Value> {
    if !path.is_file() {
        return Ok(default_obj);
    }

    let raw = fs::read_to_string(path).map_err(|source| CoreError::Io {
        path: path.display().to_string(),
        source,
    })?;

    if raw.trim().is_empty() {
        return Ok(default_obj);
    }

    let cleaned = strip_jsonc(&raw);
    let parsed: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        CoreError::Serialization(format!(
            "failed to parse existing JSON/JSONC config '{}': {e}",
            path.display()
        ))
    })?;

    if parsed.is_object() {
        Ok(parsed)
    } else {
        Err(CoreError::Serialization(format!(
            "expected top-level JSON object in '{}'",
            path.display()
        )))
    }
}

/// Ensures every path in `entries` is present in `<common_git_dir>/info/exclude` without duplicating
/// existing lines.
///
/// # Errors
/// Returns [`CoreError::Io`] if `<common_git_dir>/info/exclude` cannot be written.
pub(crate) fn add_paths_to_git_exclude(
    common_git_dir: &Path,
    entries: &[String],
) -> CoreResult<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let exclude_path = common_git_dir.join("info/exclude");
    let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
    let mut lines_to_add = Vec::new();

    for entry in entries {
        let trimmed = entry.trim();

        if trimmed.is_empty() {
            continue;
        }

        let already_present = existing.lines().any(|line| {
            let lt = line.trim();
            lt == trimmed || lt.trim_end_matches('/') == trimmed.trim_end_matches('/')
        }) || lines_to_add
            .iter()
            .any(|pending: &String| pending == trimmed);

        if !already_present {
            lines_to_add.push(trimmed.to_string());
        }
    }

    if lines_to_add.is_empty() {
        return Ok(());
    }

    let mut updated = existing;

    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }

    for line in lines_to_add {
        updated.push_str(&line);
        updated.push('\n');
    }

    atomic_write_file(&exclude_path, updated.as_bytes())
}

/// Installs repo-local harness discovery rules, lifecycle hooks, MCP server configurations,
/// and Git hooks during `tes init` using automatic harness detection (or `all` when requested).
///
/// # Errors
/// Returns [`CoreError::Io`] or [`CoreError::Serialization`] if any configuration file cannot be updated.
pub fn install_harness_integrations(repo_root: &Path, git_dir: &Path) -> CoreResult<Vec<String>> {
    install_harness_integrations_for(repo_root, git_dir, None)
}

/// Installs repo-local harness discovery rules, lifecycle hooks, MCP server configurations,
/// and Git hooks for the specified `harness_filter` (`"antigravity"`, `"claude"`, `"cursor"`,
/// `"windsurf"`, `"kiro"`, `"opencode"`, `"codex"`, `"vscode"`, `"all"`, or `None` for smart
/// auto-detection).
///
/// `git_dir` should be the repository's shared common Git directory (`common_git_dir`).
///
/// # Errors
/// Returns [`CoreError::Io`] or [`CoreError::Serialization`] if any configuration file cannot be updated.
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
            || std::env::var_os("CLAUDE_SESSION_ID").is_some()
            || std::env::var_os("CLAUDECODE").is_some()
    } else {
        has_tag("claude", "")
    };
    let want_cursor = if is_auto {
        repo_root.join(".cursor").exists()
            || std::env::var_os("CURSOR_SESSION_ID").is_some()
            || std::env::var_os("CURSOR_TRACE_ID").is_some()
    } else {
        has_tag("cursor", "")
    };
    let want_windsurf = if is_auto {
        repo_root.join(".windsurf").exists()
            || repo_root.join(".windsurfrules").exists()
            || std::env::var_os("WINDSURF_SESSION_ID").is_some()
            || std::env::var_os("CODEIUM_SESSION_ID").is_some()
    } else {
        has_tag("windsurf", "")
    };
    let want_kiro = if is_auto {
        repo_root.join(".kiro").exists() || std::env::var_os("KIRO_SESSION_ID").is_some()
    } else {
        has_tag("kiro", "")
    };
    let want_opencode = if is_auto {
        repo_root.join("opencode.json").exists()
            || repo_root.join(".opencode").exists()
            || std::env::var_os("OPENCODE_SESSION_ID").is_some()
    } else {
        has_tag("opencode", "")
    };
    let want_codex = if is_auto {
        repo_root.join(".codex").exists()
            || std::env::var_os("CODEX_THREAD_ID").is_some()
            || std::env::var_os("CODEX_SESSION_ID").is_some()
            || std::env::var_os("OPENAI_CODEX_SESSION_ID").is_some()
            || std::env::var_os("CODEX_SANDBOX").is_some()
            || std::env::var_os("CODEX_CI").is_some()
    } else {
        has_tag("codex", "openai")
    };
    let want_vscode = if is_auto {
        repo_root.join(".vscode/mcp.json").exists()
    } else {
        has_tag("vscode", "copilot")
    };

    // Clean up legacy pre-rename rule files so agents never read stale rules.
    for legacy_rel in [
        ".agents/rules/lunr.md",
        ".cursor/rules/lunr.mdc",
        ".windsurf/rules/lunr.md",
        ".kiro/steering/lunr.md",
    ] {
        let _ = fs::remove_file(repo_root.join(legacy_rel));
    }

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

    if want_opencode || want_codex || want_vscode || repo_root.join("AGENTS.md").exists() {
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

    // Ensure `<common_git_dir>/info/exclude` ignores `.tessera/` so COW workspaces and temp dirs never pollute `git status`.
    add_paths_to_git_exclude(git_dir, &[".tessera/".to_string()])?;

    Ok(installed)
}

/// Inserts or updates a `<!-- tessera:start --> ... <!-- tessera:end -->` markdown section in `path`.
pub(crate) fn upsert_marked_section(path: &Path, block: &str) -> CoreResult<()> {
    let mut existing = if path.is_file() {
        fs::read_to_string(path).map_err(|source| CoreError::Io {
            path: path.display().to_string(),
            source,
        })?
    } else {
        String::new()
    };

    let legacy_start_marker = "<!-- lunr:start -->";
    let legacy_end_marker = "<!-- lunr:end -->";

    if let Some((l_start, after)) = existing.find(legacy_start_marker).and_then(|start| {
        let search_from = start + legacy_start_marker.len();
        existing[search_from..]
            .find(legacy_end_marker)
            .map(|rel_end| (start, search_from + rel_end + legacy_end_marker.len()))
    }) {
        existing = format!("{}{}", &existing[..l_start], &existing[after..])
            .trim()
            .to_string();
    }

    let start_marker = "<!-- tessera:start -->";
    let end_marker = "<!-- tessera:end -->";
    let updated = if let Some((start, after_end)) = existing.find(start_marker).and_then(|start| {
        let search_from = start + start_marker.len();
        existing[search_from..]
            .find(end_marker)
            .map(|rel_end| (start, search_from + rel_end + end_marker.len()))
    }) {
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

/// Upserts the `tessera` MCP server entry under `mcpServers` in a standard `.mcp.json` file
/// while preserving any existing JSONC settings.
pub(crate) fn upsert_mcp_json(path: &Path) -> CoreResult<()> {
    let mut root = read_jsonc_object(path, serde_json::json!({}))?;

    let Some(obj) = root.as_object_mut() else {
        return Err(CoreError::Serialization(format!(
            "expected JSON object in '{}'",
            path.display()
        )));
    };

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
        serde_json::to_string_pretty(&root).map_err(|e| CoreError::Serialization(e.to_string()))?
    );
    atomic_write_file(path, formatted.as_bytes())
}

/// Upserts the `tessera` MCP server entry in an OpenCode `opencode.json` file while preserving
/// existing JSONC settings.
pub(crate) fn upsert_opencode_json(path: &Path) -> CoreResult<()> {
    let mut root = read_jsonc_object(
        path,
        serde_json::json!({ "$schema": "https://opencode.ai/config.json" }),
    )?;

    let Some(obj) = root.as_object_mut() else {
        return Err(CoreError::Serialization(format!(
            "expected JSON object in '{}'",
            path.display()
        )));
    };

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
        serde_json::to_string_pretty(&root).map_err(|e| CoreError::Serialization(e.to_string()))?
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

/// Upserts the `tessera` MCP server entry under `servers` in `.vscode/mcp.json` while preserving
/// existing JSONC settings.
pub(crate) fn upsert_vscode_mcp_json(path: &Path) -> CoreResult<()> {
    let mut root = read_jsonc_object(path, serde_json::json!({}))?;

    let Some(obj) = root.as_object_mut() else {
        return Err(CoreError::Serialization(format!(
            "expected JSON object in '{}'",
            path.display()
        )));
    };

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
        serde_json::to_string_pretty(&root).map_err(|e| CoreError::Serialization(e.to_string()))?
    );
    atomic_write_file(path, formatted.as_bytes())
}

/// Returns `true` if a hook group object inside a Claude/Codex hook event array already invokes
/// `expected_cmd` (or any `tes hook ...` subcommand matching `expected_cmd`).
fn hook_entry_matches_tessera(entry: &serde_json::Value, expected_cmd: &str) -> bool {
    if let Some(cmd) = entry.get("command").and_then(serde_json::Value::as_str) {
        if cmd.trim() == expected_cmd {
            return true;
        }
    }

    entry
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|c| c.trim() == expected_cmd)
            })
        })
}

/// Merges a single Tessera hook entry into `hooks_obj[event_name]` without removing or overwriting
/// any existing user-defined hook entries in that event array.
fn merge_hook_event_entry(
    hooks_obj: &mut serde_json::Map<String, serde_json::Value>,
    event_name: &str,
    expected_cmd: &str,
    tessera_entry: serde_json::Value,
) {
    let entry_val = hooks_obj
        .entry(event_name.to_string())
        .or_insert_with(|| serde_json::json!([]));

    if !entry_val.is_array() {
        *entry_val = serde_json::json!([]);
    }

    if let Some(arr) = entry_val.as_array_mut() {
        if let Some(existing) = arr
            .iter_mut()
            .find(|item| hook_entry_matches_tessera(item, expected_cmd))
        {
            *existing = tessera_entry;
        } else {
            arr.push(tessera_entry);
        }
    }
}

/// Upserts lifecycle hooks (`UserPromptSubmit`, `PreToolUse`, `PostToolUse`) in
/// `.claude/settings.json` or `.codex/hooks.json`, merging into existing event hook arrays so
/// user-defined hooks and JSONC settings are preserved.
pub(crate) fn upsert_claude_hooks_json(path: &Path) -> CoreResult<()> {
    let mut root = read_jsonc_object(path, serde_json::json!({}))?;

    let Some(obj) = root.as_object_mut() else {
        return Err(CoreError::Serialization(format!(
            "expected JSON object in '{}'",
            path.display()
        )));
    };

    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));

    if !hooks.is_object() {
        *hooks = serde_json::json!({});
    }

    if let Some(hooks_obj) = hooks.as_object_mut() {
        merge_hook_event_entry(
            hooks_obj,
            "UserPromptSubmit",
            "tes hook prompt",
            serde_json::json!({
                "hooks": [{ "type": "command", "command": "tes hook prompt" }]
            }),
        );
        merge_hook_event_entry(
            hooks_obj,
            "PreToolUse",
            "tes hook pre-tool",
            serde_json::json!({
                "matcher": "Read|Edit|Write|MultiEdit|apply_patch",
                "hooks": [{ "type": "command", "command": "tes hook pre-tool" }]
            }),
        );
        merge_hook_event_entry(
            hooks_obj,
            "PostToolUse",
            "tes hook post-tool",
            serde_json::json!({
                "matcher": "Edit|Write|MultiEdit|apply_patch",
                "hooks": [{ "type": "command", "command": "tes hook post-tool" }]
            }),
        );
    }

    let formatted = format!(
        "{}\n",
        serde_json::to_string_pretty(&root).map_err(|e| CoreError::Serialization(e.to_string()))?
    );
    atomic_write_file(path, formatted.as_bytes())
}

/// Inserts or updates a `# tessera:start ... # tessera:end` shell hook block in `.git/hooks/<hook>`.
pub(crate) fn upsert_git_hook(path: &Path, snippet: &str) -> CoreResult<()> {
    let existing = if path.is_file() {
        fs::read_to_string(path).unwrap_or_default()
    } else {
        String::new()
    };

    let start_marker = "# tessera:start";
    let end_marker = "# tessera:end";
    let updated = if let Some((start, after_end)) = existing.find(start_marker).and_then(|start| {
        let search_from = start + start_marker.len();
        existing[search_from..]
            .find(end_marker)
            .map(|rel_end| (start, search_from + rel_end + end_marker.len()))
    }) {
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
