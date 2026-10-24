//! Model Context Protocol (MCP) JSON-RPC 2.0 server over `stdio`.
//!
//! Exposes Tessera multi-agent coordination, conflict resolution, and branch lifecycle
//! operations as structured MCP tools for autonomous coding agents.

use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use serde_json::{json, Value};
use agent::{detect_session_id, Coordinator, ResolveStrategy};
use core::{CoreError, CoreResult};

/// Maximum permitted size in bytes (16 MiB) for a single JSON-RPC line over MCP `stdio`.
const MAX_MCP_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

/// Runs the Model Context Protocol (MCP) JSON-RPC 2.0 server over `stdin`/`stdout`.
///
/// # Errors
/// Returns [`CoreError`] if reading or writing stdio streams fails or if a message exceeds [`MAX_MCP_MESSAGE_BYTES`].
pub fn run_mcp_stdio_server(cwd: &Path) -> CoreResult<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = io::stdout();
    let mut line_buf = Vec::new();

    loop {
        line_buf.clear();
        let n = (&mut reader)
            .take(MAX_MCP_MESSAGE_BYTES + 1)
            .read_until(b'\n', &mut line_buf)
            .map_err(|source| CoreError::Io {
                path: "stdin".to_string(),
                source,
            })?;
        if n == 0 {
            break;
        }

        if line_buf.len() as u64 > MAX_MCP_MESSAGE_BYTES && line_buf.last() != Some(&b'\n') {
            return Err(CoreError::InvalidArgument(format!(
                "MCP JSON-RPC message exceeds maximum size of {MAX_MCP_MESSAGE_BYTES} bytes"
            )));
        }

        let Ok(line) = std::str::from_utf8(&line_buf) else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if let Some(resp) = handle_mcp_request(cwd, &req) {
            let serialized = serde_json::to_string(&resp)
                .map_err(|e| CoreError::Serialization(e.to_string()))?;
            writeln!(stdout, "{serialized}").map_err(|source| CoreError::Io {
                path: "stdout".to_string(),
                source,
            })?;
            stdout.flush().map_err(|source| CoreError::Io {
                path: "stdout".to_string(),
                source,
            })?;
        }
    }

    Ok(())
}

/// Processes a single JSON-RPC 2.0 MCP request and returns `Some(response)` for requests
/// with an `id`, or `None` for notifications.
pub fn handle_mcp_request(cwd: &Path, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned()?;
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "initialize" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "tessera",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        })),
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        })),
        "resources/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "resources": []
            }
        })),
        "prompts/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "prompts": []
            }
        })),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": mcp_tool_definitions()
            }
        })),
        "tools/call" => {
            let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            match dispatch_tool_call(cwd, tool_name, &args) {
                Ok(payload) => {
                    let text =
                        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [
                                {
                                    "type": "text",
                                    "text": text
                                }
                            ],
                            "isError": false
                        }
                    }))
                }
                Err(err) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [
                            {
                                "type": "text",
                                "text": format!("Tessera error: {err}")
                            }
                        ],
                        "isError": true
                    }
                })),
            }
        }
        _ => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {method}")
            }
        })),
    }
}

/// Dispatches an incoming MCP `tools/call` invocation to the appropriate [`Coordinator`] operation.
///
/// # Errors
/// Returns [`CoreError`] if the repository cannot be opened, required tool parameters are missing,
/// or the underlying [`Coordinator`] operation fails.
fn dispatch_tool_call(cwd: &Path, tool_name: &str, args: &Value) -> CoreResult<Value> {
    let coord = Coordinator::open(cwd)?;
    let explicit_agent = args.get("agent_id").and_then(Value::as_str);
    let raw_session_id = detect_session_id(explicit_agent);
    let session_id = if explicit_agent.is_some()
        || (!raw_session_id.starts_with("term-") && !raw_session_id.starts_with("session-"))
    {
        raw_session_id
    } else {
        let requested_branch = args.get("branch").and_then(Value::as_str);
        let radar_opt = coord.radar().ok();
        let active_ai = radar_opt.as_ref().and_then(|r| {
            r.active_agents
                .iter()
                .filter(|a| {
                    !a.agent_id.starts_with("term-")
                        && !a.agent_id.starts_with("session-")
                        && a.agent_id != "editor"
                        && requested_branch.is_none_or(|b| a.target_branch.as_deref() == Some(b))
                })
                .max_by_key(|a| a.updated_at_ms)
                .map(|a| a.agent_id.clone())
        });

        if let Some(ai_id) = active_ai {
            ai_id
        } else if tool_name == "claim" {
            match requested_branch {
                Some(b) if !b.trim().is_empty() => format!("mcp-{}", b.trim()),
                _ => "mcp-claim".to_string(),
            }
        } else {
            raw_session_id
        }
    };

    match tool_name {
        "status" => {
            let _ = coord.watch_tick(&session_id);
            let meta = coord.snapshot_agent(&session_id, true)?;
            serde_json::to_value(meta).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "radar" => {
            let report = coord.radar()?;
            serde_json::to_value(report)
                .map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "claim" => {
            let intent = args.get("intent").and_then(Value::as_str);
            let branch = args.get("branch").and_then(Value::as_str);
            let symbols: Vec<String> = args
                .get("symbols")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let meta = coord.claim_intent_and_symbols_with_branch(
                &session_id,
                intent,
                &symbols,
                branch,
            )?;
            serde_json::to_value(meta).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "checkpoint" => {
            let label = args
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("checkpoint");
            let op_id = coord.create_checkpoint(&session_id, label)?;
            Ok(json!({
                "session_id": session_id,
                "label": label,
                "operation_id": op_id
            }))
        }
        "conflicts" => {
            let radar = coord.radar()?;
            serde_json::to_value(radar.active_conflicts)
                .map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "resolve" => {
            let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
                CoreError::InvalidArgument("missing 'path' argument".to_string())
            })?;
            let strategy_str = args
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or("working");
            let strategy = match strategy_str {
                "ours" => ResolveStrategy::Ours,
                "theirs" => ResolveStrategy::Theirs,
                "ast-union" => ResolveStrategy::AstUnion,
                "content" => {
                    let content = args
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    ResolveStrategy::ExplicitContent(content)
                }
                _ => ResolveStrategy::WorkingFile,
            };
            let blob_id = coord.resolve_conflict(path, strategy)?;
            Ok(json!({
                "resolved_path": path,
                "blob_id": blob_id
            }))
        }
        "sync" => {
            let rebased = coord.sync_all_agents()?;
            serde_json::to_value(rebased)
                .map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "undo" => {
            let target = args
                .get("op")
                .and_then(Value::as_str)
                .or_else(|| args.get("checkpoint").and_then(Value::as_str));
            let undo_op = coord.undo(target)?;
            serde_json::to_value(undo_op)
                .map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "merge" => {
            let agents: Vec<String> = args
                .get("agents")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let into = args.get("into").and_then(Value::as_str);
            let message = args.get("message").and_then(Value::as_str);
            let cfg = coord.oplog().load_config()?;
            let verify_override = match args.get("verify") {
                None | Some(Value::Null) | Some(Value::Bool(false)) => None,
                Some(Value::Bool(true)) => Some(cfg.verify_command.ok_or_else(|| {
                    CoreError::InvalidArgument(
                        "no trusted verify_command configured; set one via 'tes config --verify-cmd'"
                            .to_string(),
                    )
                })?),
                Some(Value::String(s)) if s.trim().is_empty() || s.trim() == "false" => None,
                Some(Value::String(s)) if s.trim() == "true" => {
                    Some(cfg.verify_command.ok_or_else(|| {
                        CoreError::InvalidArgument(
                            "no trusted verify_command configured; set one via 'tes config --verify-cmd'"
                                .to_string(),
                        )
                    })?)
                }
                Some(Value::String(s))
                    if cfg.verify_command.as_deref() == Some(s.as_str()) =>
                {
                    Some(s.clone())
                }
                Some(_) => {
                    return Err(CoreError::InvalidArgument(
                        "arbitrary verification shell commands are not permitted over MCP; configure a trusted verify_command via 'tes config --verify-cmd'".to_string(),
                    ));
                }
            };
            let keep_open = args
                .get("keep_open")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let report = coord.merge_agents_with_message(
                &agents,
                into,
                verify_override.as_deref(),
                keep_open,
                message,
            )?;
            serde_json::to_value(report)
                .map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "restore" => {
            let source = args
                .get("source")
                .and_then(Value::as_str)
                .or_else(|| args.get("revision").and_then(Value::as_str));
            let branch = args.get("branch").and_then(Value::as_str);
            let paths: Vec<String> = args
                .get("paths")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let report = coord.restore(&session_id, source, branch, &paths)?;
            serde_json::to_value(report)
                .map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "branch" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("list");
            let branch = args.get("branch").and_then(Value::as_str);
            let new_branch = args.get("new_branch").and_then(Value::as_str);
            let force = args
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            match action {
                "delete" => {
                    let b = branch.ok_or_else(|| {
                        CoreError::InvalidArgument(
                            "missing 'branch' argument for delete".to_string(),
                        )
                    })?;
                    let report = coord.delete_branch(&session_id, b, force)?;
                    serde_json::to_value(report)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                "rename" => {
                    let next_name = new_branch.or(branch).ok_or_else(|| {
                        CoreError::InvalidArgument(
                            "missing 'new_branch' argument for rename".to_string(),
                        )
                    })?;
                    let old_name = match if new_branch.is_some() { branch } else { None } {
                        Some(b) => b.to_string(),
                        None => {
                            let cfg = coord.oplog().load_config()?;
                            let current = coord.oplog().load_current_reconciled()?;
                            let editor_br = coord
                                .git()
                                .current_branch_name()
                                .unwrap_or_else(|_| cfg.trunk_branch.clone());
                            current
                                .view
                                .agents
                                .get(&session_id)
                                .and_then(|a| a.target_branch.clone())
                                .unwrap_or(editor_br)
                        }
                    };
                    let report = coord.rename_branch(&session_id, &old_name, next_name)?;
                    serde_json::to_value(report)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                "switch" => {
                    let b = branch.ok_or_else(|| {
                        CoreError::InvalidArgument(
                            "missing 'branch' argument for switch".to_string(),
                        )
                    })?;
                    let meta = coord.switch_branch(&session_id, b)?;
                    serde_json::to_value(meta)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                _ => {
                    let current = coord.oplog().load_current_reconciled()?;
                    let mut branches: std::collections::BTreeSet<String> =
                        current.view.branches.keys().cloned().collect();
                    for b in coord.git().list_local_branches() {
                        branches.insert(b);
                    }
                    Ok(json!({ "branches": branches }))
                }
            }
        }
        "stash" => {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("push");
            let message = args.get("message").and_then(Value::as_str);
            let branch = args.get("branch").and_then(Value::as_str);
            let index = args
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let paths: Vec<String> = args
                .get("paths")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default();

            match action {
                "push" | "save" => {
                    let entry =
                        coord.stash_push_with_paths(&session_id, message, branch, &paths)?;
                    serde_json::to_value(entry)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                "pop" => {
                    let report = coord.stash_pop(&session_id, index, false)?;
                    serde_json::to_value(report)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                "apply" => {
                    let report = coord.stash_pop(&session_id, index, true)?;
                    serde_json::to_value(report)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                "list" => {
                    let stashes = coord.stash_list()?;
                    serde_json::to_value(stashes)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                "drop" => {
                    let dropped = coord.stash_drop(&session_id, Some(index))?;
                    serde_json::to_value(dropped)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                "clear" => {
                    let dropped = coord.stash_drop(&session_id, None)?;
                    serde_json::to_value(dropped)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                other => Err(CoreError::InvalidArgument(format!(
                    "unknown stash action '{other}'"
                ))),
            }
        }
        "pull" => {
            let remote = args
                .get("remote")
                .and_then(Value::as_str)
                .unwrap_or("origin");
            let branch = args.get("branch").and_then(Value::as_str);
            let report = coord.pull_branch(&session_id, remote, branch)?;
            serde_json::to_value(report)
                .map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "cherry-pick" => {
            let commit = args
                .get("commit")
                .and_then(Value::as_str)
                .ok_or_else(|| CoreError::InvalidArgument("missing 'commit'".to_string()))?;
            let branch = args.get("branch").and_then(Value::as_str);
            let report = coord.cherry_pick(&session_id, commit, branch)?;
            serde_json::to_value(report)
                .map_err(|e| CoreError::Serialization(e.to_string()))
        }
        other => Err(CoreError::InvalidArgument(format!(
            "unknown MCP tool '{other}'"
        ))),
    }
}

/// Returns the JSON Schema definitions for all tools exposed by the Tessera MCP server.
fn mcp_tool_definitions() -> Value {
    json!([
        {
            "name": "status",
            "description": "Snapshots the current agent session and returns its ChangeId, commit SHA, intent, and claimed AST symbols.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "Optional agent session ID (auto-detected if omitted)." }
                }
            }
        },
        {
            "name": "radar",
            "description": "Returns the live multi-agent Symbol Collision Radar showing all active agents, their intents, and any overlapping file or AST symbol edits.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "claim",
            "description": "Declares the current agent session's natural-language intent, optional target branch, and target AST symbols or file paths.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "intent": { "type": "string", "description": "Natural-language goal or invariant of this agent task." },
                    "branch": { "type": "string", "description": "Optional target Git branch for this session's edits." },
                    "symbols": { "type": "array", "items": { "type": "string" }, "description": "File paths or path::symbol names being modified." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "checkpoint",
            "description": "Creates a named atomic checkpoint in the lock-free Operation Log for instant rollback via undo or restore.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "label": { "type": "string", "description": "Checkpoint name." },
                    "agent_id": { "type": "string" }
                },
                "required": ["label"]
            }
        },
        {
            "name": "restore",
            "description": "Restores a branch or specific files to any Git commit SHA, HEAD~N, branch, tag, or named checkpoint while keeping branch sandboxes and the Operation Log in sync.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "Optional revision, commit SHA, HEAD~N, branch, or checkpoint label." },
                    "branch": { "type": "string", "description": "Optional target branch to restore (defaults to active session branch)." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional file paths to restore (omit to restore the entire branch tree)." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "branch",
            "description": "Lists, switches, deletes, or renames a Tessera branch across Git refs, Copy-on-Write sandboxes, and active sessions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "switch", "delete", "rename"], "description": "Branch operation to perform." },
                    "branch": { "type": "string", "description": "Target branch name (or old branch name when renaming)." },
                    "new_branch": { "type": "string", "description": "New branch name when action is 'rename'." },
                    "force": { "type": "boolean", "description": "Force delete even if unmerged (-D)." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "stash",
            "description": "Stashes or restores session-isolated uncommitted changes via 3-way AST merge without wiping concurrent peer sessions on the same branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["push", "pop", "apply", "list", "drop", "clear"], "description": "Stash operation to perform." },
                    "message": { "type": "string", "description": "Optional stash description." },
                    "branch": { "type": "string", "description": "Optional target branch override." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional file paths to stash." },
                    "index": { "type": "integer", "description": "Stash index (defaults to 0)." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "pull",
            "description": "Fetches from a Git remote, fast-forwards or 3-way AST-merges into the active branch, and auto-rebases active sessions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "remote": { "type": "string", "description": "Git remote name (defaults to 'origin')." },
                    "branch": { "type": "string", "description": "Branch to pull (defaults to active session branch)." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "cherry-pick",
            "description": "Applies the delta introduced by an existing commit onto the active (or specified) branch via 3-way AST merge.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "commit": { "type": "string", "description": "Commit SHA, branch name, or revision (HEAD~1, checkpoint) to cherry-pick." },
                    "branch": { "type": "string", "description": "Optional target branch (defaults to active session branch)." },
                    "agent_id": { "type": "string" }
                },
                "required": ["commit"]
            }
        },
        {
            "name": "conflicts",
            "description": "Returns all active first-class algebraic conflicts with AST context snippets and competing agent intents.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "resolve",
            "description": "Resolves a first-class conflict at `path` using 'ours', 'theirs', 'ast-union', 'working', or 'content'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "strategy": { "type": "string", "enum": ["ours", "theirs", "ast-union", "working", "content"] },
                    "content": { "type": "string" }
                },
                "required": ["path", "strategy"]
            }
        },
        {
            "name": "sync",
            "description": "Non-blocking auto-rebase of all active agent sessions onto the latest trunk commit using Tree-sitter 3-way AST merge.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "undo",
            "description": "Atomically rolls back repository state and working tree to the previous operation, an OpId prefix, or a named checkpoint.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "op": { "type": "string", "description": "Optional target operation ID prefix." },
                    "checkpoint": { "type": "string", "description": "Optional target checkpoint label." }
                }
            }
        },
        {
            "name": "merge",
            "description": "Octopus AST-merges active agent sessions into the target branch with optional speculative verification gate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agents": { "type": "array", "items": { "type": "string" }, "description": "Optional agent IDs or branch names to merge (defaults to all active sessions)." },
                    "into": { "type": "string", "description": "Optional target branch (defaults to active Git branch)." },
                    "message": { "type": "string", "description": "Optional custom Git commit message." },
                    "verify": { "type": "boolean", "description": "Run the repository's configured verification command (verify_command) before finalizing the merge." },
                    "keep_open": { "type": "boolean", "description": "Keep merged agent sessions open after a clean merge." }
                }
            }
        }
    ])
}
