//! Model Context Protocol (MCP) JSON-RPC 2.0 server over `stdio`.
//!
//! Exposes Tessera multi-agent coordination, conflict resolution, inspection, diagnostics,
//! resources, prompts, and branch lifecycle operations as structured MCP endpoints.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent::{CommitOptions, Coordinator, ResolveStrategy, detect_session_id};
use core::{CoreError, CoreResult, validate_safe_identifier};
use serde_json::{Value, json};

use crate::commands::diff_log::{
    DiffArgs, collect_diff_files_json, compute_diff_trees, render_git_diff,
};
use crate::commands::push_pull::{PushArgs, execute_push};
use crate::doctor::collect_doctor_report;

/// Maximum permitted size in bytes (16 MiB) for a single JSON-RPC line over MCP `stdio`.
const MAX_MCP_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

/// Supported MCP protocol versions, newest first.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];

/// Canonical list of all 22 MCP tools exposed by this server.
const KNOWN_MCP_TOOLS: &[&str] = &[
    "status",
    "radar",
    "claim",
    "checkpoint",
    "restore",
    "branch",
    "stash",
    "pull",
    "cherry-pick",
    "conflicts",
    "resolve",
    "sync",
    "undo",
    "merge",
    "commit",
    "diff",
    "log",
    "oplog",
    "push",
    "agent",
    "config",
    "doctor",
];

/// Generates a stable, safe per-connection MCP session identifier (`mcp-<pid>-<8hex>`).
fn generate_mcp_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);

    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id();
    let mut z = nanos ^ (u64::from(pid) << 32) ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    let short = z as u32;

    format!("mcp-{pid}-{short:08x}")
}

/// Resolves the session identifier for an MCP tool call without hijacking peer sessions or
/// constructing invalid slash-containing identifiers.
fn resolve_mcp_session_id(
    explicit_agent: Option<&str>,
    conn_session_id: &str,
) -> CoreResult<String> {
    if let Some(raw) = explicit_agent.map(str::trim).filter(|s| !s.is_empty()) {
        validate_safe_identifier(raw, "agent_id")?;
        return Ok(raw.to_string());
    }

    let detected = detect_session_id(None);

    if !detected.starts_with("term-")
        && !detected.starts_with("session-")
        && detected != "editor"
        && validate_safe_identifier(&detected, "session_id").is_ok()
    {
        return Ok(detected);
    }

    Ok(conn_session_id.to_string())
}

/// Builds a JSON-RPC 2.0 error object.
fn json_rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

/// Serializes and flushes a single JSON-RPC response line to `stdout`.
fn write_mcp_response(stdout: &mut io::Stdout, resp: &Value) -> CoreResult<()> {
    let serialized =
        serde_json::to_string(resp).map_err(|e| CoreError::Serialization(e.to_string()))?;

    writeln!(stdout, "{serialized}").map_err(|source| CoreError::Io {
        path: "stdout".to_string(),
        source,
    })?;

    stdout.flush().map_err(|source| CoreError::Io {
        path: "stdout".to_string(),
        source,
    })
}

/// Runs the Model Context Protocol (MCP) JSON-RPC 2.0 server over `stdin`/`stdout`.
///
/// # Errors
/// Returns [`CoreError`] if reading from `stdin` or writing to `stdout` fails with an I/O error.
pub fn run_mcp_stdio_server(cwd: &Path) -> CoreResult<()> {
    let conn_session_id = generate_mcp_session_id();
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
            loop {
                let available = reader.fill_buf().map_err(|source| CoreError::Io {
                    path: "stdin".to_string(),
                    source,
                })?;

                if available.is_empty() {
                    break;
                }

                if let Some(pos) = available.iter().position(|&b| b == b'\n') {
                    reader.consume(pos + 1);
                    break;
                }

                let len = available.len();
                reader.consume(len);
            }

            let err_resp = json_rpc_error(
                Value::Null,
                -32600,
                format!(
                    "MCP JSON-RPC message exceeds maximum size of {MAX_MCP_MESSAGE_BYTES} bytes"
                ),
            );
            write_mcp_response(&mut stdout, &err_resp)?;
            continue;
        }

        let Ok(line) = std::str::from_utf8(&line_buf) else {
            let err_resp = json_rpc_error(Value::Null, -32700, "Parse error: invalid UTF-8");
            write_mcp_response(&mut stdout, &err_resp)?;
            continue;
        };

        let trimmed = line.trim();

        if trimmed.is_empty() {
            continue;
        }

        let Ok(req) = serde_json::from_str::<Value>(trimmed) else {
            let err_resp = json_rpc_error(Value::Null, -32700, "Parse error: malformed JSON");
            write_mcp_response(&mut stdout, &err_resp)?;
            continue;
        };

        if let Some(resp) = handle_mcp_request_with_session(cwd, &conn_session_id, &req) {
            write_mcp_response(&mut stdout, &resp)?;
        }
    }

    Ok(())
}

/// Processes a single JSON-RPC 2.0 MCP request using a generated fallback session ID.
pub fn handle_mcp_request(cwd: &Path, req: &Value) -> Option<Value> {
    let conn_session_id = generate_mcp_session_id();
    handle_mcp_request_with_session(cwd, &conn_session_id, req)
}

/// Processes a single JSON-RPC 2.0 MCP request bound to a stable `conn_session_id`,
/// returning `Some(response)` for requests or protocol errors, and `None` for notifications or
/// client responses.
pub fn handle_mcp_request_with_session(
    cwd: &Path,
    conn_session_id: &str,
    req: &Value,
) -> Option<Value> {
    let Some(obj) = req.as_object() else {
        return Some(json_rpc_error(
            Value::Null,
            -32600,
            "Invalid Request: expected JSON object",
        ));
    };

    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        let id = obj.get("id").cloned().unwrap_or(Value::Null);
        return Some(json_rpc_error(
            id,
            -32600,
            "Invalid Request: missing or invalid 'jsonrpc' field (expected \"2.0\")",
        ));
    }

    // Ignore client responses (messages containing `result` or `error` without `method`).
    if !obj.contains_key("method") && (obj.contains_key("result") || obj.contains_key("error")) {
        return None;
    }

    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        let id = obj.get("id").cloned().unwrap_or(Value::Null);
        return Some(json_rpc_error(
            id,
            -32600,
            "Invalid Request: missing string 'method' field",
        ));
    };

    // Notifications omit `id` and must not produce a response.
    let id = obj.get("id").cloned()?;
    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "initialize" => {
            let requested_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let negotiated = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested_version) {
                requested_version
            } else {
                SUPPORTED_PROTOCOL_VERSIONS[0]
            };

            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": {},
                        "resources": {},
                        "prompts": {}
                    },
                    "serverInfo": {
                        "name": "tessera",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }))
        }
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        })),
        "resources/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "resources": mcp_resource_definitions()
            }
        })),
        "resources/read" => {
            let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                return Some(json_rpc_error(
                    id,
                    -32602,
                    "Invalid params: missing string 'uri'",
                ));
            };

            match read_mcp_resource(cwd, conn_session_id, uri) {
                Ok(text) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "contents": [
                            {
                                "uri": uri,
                                "mimeType": "application/json",
                                "text": text
                            }
                        ]
                    }
                })),
                Err(McpParamOrExecError::InvalidParams(msg)) => {
                    Some(json_rpc_error(id, -32602, msg))
                }
                Err(McpParamOrExecError::Execution(msg)) => Some(json_rpc_error(id, -32603, msg)),
            }
        }
        "prompts/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "prompts": mcp_prompt_definitions()
            }
        })),
        "prompts/get" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(json_rpc_error(
                    id,
                    -32602,
                    "Invalid params: missing string 'name'",
                ));
            };
            let prompt_args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            match get_mcp_prompt(cwd, name, &prompt_args) {
                Ok(result) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result
                })),
                Err(McpParamOrExecError::InvalidParams(msg)) => {
                    Some(json_rpc_error(id, -32602, msg))
                }
                Err(McpParamOrExecError::Execution(msg)) => Some(json_rpc_error(id, -32603, msg)),
            }
        }
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": mcp_tool_definitions()
            }
        })),
        "tools/call" => {
            let Some(params_obj) = params.as_object() else {
                return Some(json_rpc_error(
                    id,
                    -32602,
                    "Invalid params: expected object",
                ));
            };

            let Some(tool_name) = params_obj
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return Some(json_rpc_error(
                    id,
                    -32602,
                    "Invalid params: missing non-empty tool 'name'",
                ));
            };

            if !KNOWN_MCP_TOOLS.contains(&tool_name) {
                return Some(json_rpc_error(
                    id,
                    -32602,
                    format!("Unknown MCP tool '{tool_name}'"),
                ));
            }

            let args = match params_obj.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(v) if v.is_object() => v.clone(),
                Some(_) => {
                    return Some(json_rpc_error(
                        id,
                        -32602,
                        "Invalid params: 'arguments' must be a JSON object",
                    ));
                }
            };

            match dispatch_tool_call(cwd, conn_session_id, tool_name, &args) {
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
        _ => Some(json_rpc_error(
            id,
            -32601,
            format!("Method not found: {method}"),
        )),
    }
}

/// Distinguishes JSON-RPC invalid-params errors (`-32602`) from execution errors (`-32603`).
enum McpParamOrExecError {
    InvalidParams(String),
    Execution(String),
}

/// Resolves the trusted verification command for MCP `commit` / `merge` calls, disallowing
/// arbitrary unconfigured shell commands over MCP.
fn resolve_mcp_verify_override(
    args: &Value,
    configured_verify: Option<&str>,
) -> CoreResult<Option<String>> {
    match args.get("verify") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(None),
        Some(Value::Bool(true)) => {
            let cmd = configured_verify
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CoreError::InvalidArgument(
                        "no trusted verify_command configured; set one via 'tes config --verify-cmd'"
                            .to_string(),
                    )
                })?;
            Ok(Some(cmd.to_string()))
        }
        Some(Value::String(s)) if s.trim().is_empty() || s.trim() == "false" => Ok(None),
        Some(Value::String(s)) if s.trim() == "true" => {
            let cmd = configured_verify
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    CoreError::InvalidArgument(
                        "no trusted verify_command configured; set one via 'tes config --verify-cmd'"
                            .to_string(),
                    )
                })?;
            Ok(Some(cmd.to_string()))
        }
        Some(Value::String(s)) if configured_verify == Some(s.as_str()) => Ok(Some(s.clone())),
        Some(_) => Err(CoreError::InvalidArgument(
            "arbitrary verification shell commands are not permitted over MCP; configure a trusted verify_command via 'tes config --verify-cmd'"
                .to_string(),
        )),
    }
}

/// Parses optional `CommitOptions` fields from MCP tool arguments (`message`, `author`,
/// `co_author`, `no_co_author`, `amend`, `dry_run`).
fn parse_mcp_commit_options(args: &Value, allow_amend: bool) -> CommitOptions {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let author = args
        .get("author")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let no_co_author = args
        .get("no_co_author")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let amend = allow_amend && args.get("amend").and_then(Value::as_bool).unwrap_or(false);
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (co_author, co_author_trailers) = if no_co_author {
        (Some(false), Vec::new())
    } else {
        match args.get("co_author") {
            None | Some(Value::Null) => (None, Vec::new()),
            Some(Value::Bool(b)) => (Some(*b), Vec::new()),
            Some(Value::String(s)) => (Some(true), vec![s.clone()]),
            Some(Value::Array(arr)) => {
                let trailers: Vec<String> = arr
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect();
                (Some(true), trailers)
            }
            Some(_) => (None, Vec::new()),
        }
    };

    let delete_branch = args
        .get("delete_branch")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    CommitOptions {
        message,
        author,
        co_author,
        co_author_trailers,
        amend,
        dry_run,
        delete_branch,
    }
}

/// Extracts a string array parameter from `args`.
fn extract_string_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Reads one of the live Tessera MCP resources (`tessera://status`, `tessera://radar`, `tessera://conflicts`).
fn read_mcp_resource(
    cwd: &Path,
    conn_session_id: &str,
    uri: &str,
) -> Result<String, McpParamOrExecError> {
    match uri {
        "tessera://status" => {
            let coord = Coordinator::open(cwd)
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;
            let session_id = resolve_mcp_session_id(None, conn_session_id)
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;
            let _ = coord.watch_tick(&session_id);
            let meta = coord
                .snapshot_agent(&session_id, true)
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;
            serde_json::to_string_pretty(&meta)
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))
        }
        "tessera://radar" => {
            let coord = Coordinator::open(cwd)
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;
            let report = coord
                .radar()
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;
            serde_json::to_string_pretty(&report)
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))
        }
        "tessera://conflicts" => {
            let coord = Coordinator::open(cwd)
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;
            let radar = coord
                .radar()
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;
            serde_json::to_string_pretty(&radar.active_conflicts)
                .map_err(|e| McpParamOrExecError::Execution(e.to_string()))
        }
        other => Err(McpParamOrExecError::InvalidParams(format!(
            "Unknown resource URI '{other}'"
        ))),
    }
}

/// Builds the prompt payload for `prompts/get`.
fn get_mcp_prompt(cwd: &Path, name: &str, args: &Value) -> Result<Value, McpParamOrExecError> {
    if name != "resolve-conflicts" {
        return Err(McpParamOrExecError::InvalidParams(format!(
            "Unknown prompt '{name}'"
        )));
    }

    let path_filter = args.get("path").and_then(Value::as_str);
    let coord =
        Coordinator::open(cwd).map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;
    let radar = coord
        .radar()
        .map_err(|e| McpParamOrExecError::Execution(e.to_string()))?;

    let matching: Vec<&core::ConflictRecord> = radar
        .active_conflicts
        .iter()
        .filter(|c| path_filter.is_none_or(|p| c.path == p))
        .collect();

    let mut prompt_text = String::new();

    if matching.is_empty() {
        prompt_text.push_str("There are currently no active Tessera conflicts to resolve.");
    } else {
        let _ = writeln!(
            prompt_text,
            "Resolve the following {} active Tessera conflict(s) while preserving each competing agent's intent:\n",
            matching.len()
        );

        for c in matching {
            let _ = writeln!(prompt_text, "File: {}", c.path);

            if let Some(sym) = &c.symbol {
                let _ = writeln!(prompt_text, "Symbol: {sym}");
            }

            let _ = writeln!(prompt_text, "Kind: {:?}", c.kind);

            if !c.agent_intents.is_empty() {
                let _ = writeln!(prompt_text, "Competing agent intents:");

                for (agent_id, intent) in &c.agent_intents {
                    let _ = writeln!(prompt_text, "  - {agent_id}: {intent}");
                }
            }

            if !c.left_snippet.trim().is_empty() {
                let _ = writeln!(prompt_text, "Ours snippet:\n{}", c.left_snippet.trim());
            }

            if !c.right_snippet.trim().is_empty() {
                let _ = writeln!(prompt_text, "Theirs snippet:\n{}", c.right_snippet.trim());
            }

            let _ = writeln!(
                prompt_text,
                "Recommended action: inspect both sides and call the `resolve` tool with `path: \"{}\"` and strategy `ast-union`, `ours`, `theirs`, or `content`.\n",
                c.path
            );
        }
    }

    Ok(json!({
        "description": "Guided conflict resolution workflow populated with active Tessera conflicts and agent intents.",
        "messages": [
            {
                "role": "user",
                "content": {
                    "type": "text",
                    "text": prompt_text
                }
            }
        ]
    }))
}

/// Dispatches an incoming MCP `tools/call` invocation to the appropriate [`Coordinator`] operation.
///
/// # Errors
/// Returns [`CoreError`] if the repository cannot be opened, required tool parameters are missing
/// or invalid, or the underlying operation fails.
fn dispatch_tool_call(
    cwd: &Path,
    conn_session_id: &str,
    tool_name: &str,
    args: &Value,
) -> CoreResult<Value> {
    if tool_name == "doctor" {
        return Ok(collect_doctor_report(cwd));
    }

    let coord = Coordinator::open(cwd)?;
    let explicit_agent = args.get("agent_id").and_then(Value::as_str);
    let session_id = resolve_mcp_session_id(explicit_agent, conn_session_id)?;

    match tool_name {
        "status" => {
            let _ = coord.watch_tick(&session_id);
            let meta = coord.snapshot_agent(&session_id, true)?;
            serde_json::to_value(meta).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "radar" => {
            let report = coord.radar()?;
            serde_json::to_value(report).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "claim" => {
            let intent = args.get("intent").and_then(Value::as_str);
            let branch = args.get("branch").and_then(Value::as_str);
            let from = args.get("from").and_then(Value::as_str);
            let symbols = extract_string_array(args, "symbols");
            let meta = coord.claim_intent_and_symbols_with_branch_and_base(
                &session_id,
                intent,
                &symbols,
                branch,
                from,
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
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| CoreError::InvalidArgument("missing 'path' argument".to_string()))?;
            let strategy_str = args
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or("working");
            let strategy = match strategy_str {
                "ours" => ResolveStrategy::Ours,
                "theirs" => ResolveStrategy::Theirs,
                "ast-union" => ResolveStrategy::AstUnion,
                "working" => ResolveStrategy::WorkingFile,
                "content" => {
                    let content = args
                        .get("content")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            CoreError::InvalidArgument(
                                "strategy 'content' requires a string 'content' argument"
                                    .to_string(),
                            )
                        })?
                        .to_string();
                    ResolveStrategy::ExplicitContent(content)
                }
                other => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown conflict resolution strategy '{other}' (expected: ours, theirs, ast-union, working, content)"
                    )));
                }
            };
            let blob_id = coord.resolve_conflict(path, strategy)?;
            Ok(json!({
                "resolved_path": path,
                "blob_id": blob_id
            }))
        }
        "sync" => {
            let rebased = coord.sync_all_agents()?;
            serde_json::to_value(rebased).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "undo" => {
            let target = args
                .get("op")
                .and_then(Value::as_str)
                .or_else(|| args.get("checkpoint").and_then(Value::as_str));
            let undo_op = coord.undo(target)?;
            serde_json::to_value(undo_op).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "commit" => {
            let cfg = coord.oplog().load_config()?;
            let verify_override = resolve_mcp_verify_override(args, cfg.verify_command.as_deref())?;
            let branch = args.get("branch").and_then(Value::as_str);
            let commit_options = parse_mcp_commit_options(args, true);
            let _ = coord.watch_tick(&session_id);
            let report = coord.merge_agents_with_options(
                std::slice::from_ref(&session_id),
                branch,
                verify_override.as_deref(),
                true,
                &commit_options,
            )?;
            serde_json::to_value(report).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "merge" => {
            let agents = extract_string_array(args, "agents");
            let into = args.get("into").and_then(Value::as_str);
            let cfg = coord.oplog().load_config()?;
            let verify_override = resolve_mcp_verify_override(args, cfg.verify_command.as_deref())?;
            let keep_open = args
                .get("keep_open")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let commit_options = parse_mcp_commit_options(args, false);
            let report = coord.merge_agents_with_options(
                &agents,
                into,
                verify_override.as_deref(),
                keep_open,
                &commit_options,
            )?;
            serde_json::to_value(report).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "restore" => {
            let source = args
                .get("source")
                .and_then(Value::as_str)
                .or_else(|| args.get("revision").and_then(Value::as_str));
            let branch = args.get("branch").and_then(Value::as_str);
            let paths = extract_string_array(args, "paths");
            let report = coord.restore(&session_id, branch, source, &paths)?;
            serde_json::to_value(report).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "branch" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
            let branch = args.get("branch").and_then(Value::as_str);
            let new_branch = args.get("new_branch").and_then(Value::as_str);
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);

            match action {
                "list" => {
                    let current = coord.oplog().load_current_reconciled()?;
                    let mut branches: BTreeSet<String> =
                        current.view.branches.keys().cloned().collect();

                    for b in coord.git().list_local_branches() {
                        branches.insert(b);
                    }

                    Ok(json!({ "branches": branches }))
                }
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
                    let from = args.get("from").and_then(Value::as_str);
                    let meta = coord.switch_branch_with_base(&session_id, b, from)?;
                    serde_json::to_value(meta).map_err(|e| CoreError::Serialization(e.to_string()))
                }
                other => Err(CoreError::InvalidArgument(format!(
                    "unknown branch action '{other}' (expected: list, switch, delete, rename)"
                ))),
            }
        }
        "stash" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("push");
            let message = args.get("message").and_then(Value::as_str);
            let branch = args.get("branch").and_then(Value::as_str);
            let index = args.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let paths = extract_string_array(args, "paths");

            match action {
                "push" | "save" => {
                    let entry =
                        coord.stash_push_with_paths(&session_id, message, branch, &paths)?;
                    serde_json::to_value(entry).map_err(|e| CoreError::Serialization(e.to_string()))
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
            serde_json::to_value(report).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "push" => {
            let push_args = PushArgs {
                remote: args
                    .get("remote")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                branch: args
                    .get("branch")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                set_upstream: args
                    .get("set_upstream")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                message: args
                    .get("message")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                force: args.get("force").and_then(Value::as_bool).unwrap_or(false),
                force_with_lease: args
                    .get("force_with_lease")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                tags: args.get("tags").and_then(Value::as_bool).unwrap_or(false),
                delete: args.get("delete").and_then(Value::as_bool).unwrap_or(false),
                dry_run: args
                    .get("dry_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                no_auto_commit: args
                    .get("no_auto_commit")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                agent: Some(session_id.clone()),
            };
            let outcome = execute_push(&coord, &session_id, &push_args, true, false)
                .map_err(|e| CoreError::InvalidArgument(e.to_string()))?;

            if outcome.exit_code != 0 {
                let detail = outcome
                    .payload
                    .get("stderr")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        outcome
                            .payload
                            .get("reason")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                    })
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("exit code {}", outcome.exit_code));
                let payload_json = serde_json::to_string(&outcome.payload).unwrap_or_default();

                return Err(CoreError::InvalidArgument(format!(
                    "push failed (exit code {}): {detail} ({payload_json})",
                    outcome.exit_code
                )));
            }

            Ok(outcome.payload)
        }
        "cherry-pick" => {
            let commit = args
                .get("commit")
                .and_then(Value::as_str)
                .ok_or_else(|| CoreError::InvalidArgument("missing 'commit'".to_string()))?;
            let branch = args.get("branch").and_then(Value::as_str);
            let report = coord.cherry_pick(&session_id, commit, branch)?;
            serde_json::to_value(report).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "diff" => {
            let mut targets = Vec::new();

            if let Some(range) = args.get("range").and_then(Value::as_str) {
                if !range.trim().is_empty() {
                    targets.push(range.trim().to_string());
                }
            }

            let diff_args = DiffArgs {
                targets,
                staged: args.get("staged").and_then(Value::as_bool).unwrap_or(false),
                all: args.get("all").and_then(Value::as_bool).unwrap_or(false),
                stat: args.get("stat").and_then(Value::as_bool).unwrap_or(false),
                name_status: args
                    .get("name_status")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                branch: args
                    .get("branch")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                agent: Some(session_id.clone()),
                pathspecs: extract_string_array(args, "paths"),
                ..DiffArgs::default()
            };
            let computed = compute_diff_trees(&coord, cwd, &session_id, &diff_args)
                .map_err(|e| CoreError::InvalidArgument(e.to_string()))?;
            let diff_text = render_git_diff(&coord, &computed, &diff_args, false)
                .map_err(|e| CoreError::InvalidArgument(e.to_string()))?;
            let files = collect_diff_files_json(&coord, &computed);

            Ok(json!({
                "base": computed.base_label,
                "target": computed.target_label,
                "files": files,
                "diff": diff_text,
            }))
        }
        "log" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            let branch = args.get("branch").and_then(Value::as_str);
            let eff_branch = match branch {
                Some(b) if !b.trim().is_empty() => Some(b.trim().to_string()),
                _ => coord.oplog().load_current_reconciled().ok().and_then(|op| {
                    op.view
                        .agents
                        .get(&session_id)
                        .and_then(|a| a.target_branch.clone())
                }),
            };

            if limit == 0 {
                return Ok(json!([]));
            }

            let entries = coord.commit_log(eff_branch.as_deref(), limit)?;
            serde_json::to_value(entries).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "oplog" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            let ops = coord.oplog().list_history(limit)?;
            serde_json::to_value(ops).map_err(|e| CoreError::Serialization(e.to_string()))
        }
        "agent" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("list");

            match action {
                "list" => {
                    let report = coord.radar()?;
                    serde_json::to_value(report.active_agents)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                "close" => {
                    let close_all = args.get("all").and_then(Value::as_bool).unwrap_or(false);

                    if close_all {
                        let removed = coord.clear_agents(true)?;
                        serde_json::to_value(removed)
                            .map_err(|e| CoreError::Serialization(e.to_string()))
                    } else {
                        let target = args
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(&session_id);
                        let closed = coord.close_agent(target)?;
                        serde_json::to_value(closed)
                            .map_err(|e| CoreError::Serialization(e.to_string()))
                    }
                }
                "clear" => {
                    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                    let removed = coord.clear_agents(force)?;
                    serde_json::to_value(removed)
                        .map_err(|e| CoreError::Serialization(e.to_string()))
                }
                other => Err(CoreError::InvalidArgument(format!(
                    "unknown agent action '{other}' (expected: list, close, clear)"
                ))),
            }
        }
        "config" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("show");

            if action != "show" {
                return Err(CoreError::InvalidArgument(format!(
                    "unsupported MCP config action '{action}' (only 'show' is permitted over MCP)"
                )));
            }

            let cfg = coord.oplog().load_config()?;
            let trusted = coord.oplog().is_repo_config_trusted();
            let mut val =
                serde_json::to_value(&cfg).map_err(|e| CoreError::Serialization(e.to_string()))?;

            if let Some(obj) = val.as_object_mut() {
                obj.insert("repo_config_trusted".to_string(), json!(trusted));
            }

            Ok(val)
        }
        other => Err(CoreError::InvalidArgument(format!(
            "unknown MCP tool '{other}'"
        ))),
    }
}

/// Returns the MCP resource descriptors exposed by `resources/list`.
fn mcp_resource_definitions() -> Value {
    json!([
        {
            "uri": "tessera://status",
            "name": "Repository Status",
            "description": "Live snapshot metadata for the current Tessera agent session.",
            "mimeType": "application/json"
        },
        {
            "uri": "tessera://radar",
            "name": "Symbol Collision Radar",
            "description": "Active agent sessions, overlapping symbol edits, and live file attributions.",
            "mimeType": "application/json"
        },
        {
            "uri": "tessera://conflicts",
            "name": "Active Conflicts",
            "description": "All unresolved algebraic conflicts with snippets and competing agent intents.",
            "mimeType": "application/json"
        }
    ])
}

/// Returns the MCP prompt descriptors exposed by `prompts/list`.
fn mcp_prompt_definitions() -> Value {
    json!([
        {
            "name": "resolve-conflicts",
            "description": "Guided workflow to inspect and resolve active Tessera AST/text conflicts using competing agent intents.",
            "arguments": [
                {
                    "name": "path",
                    "description": "Optional repository-relative file path to scope conflict guidance.",
                    "required": false
                }
            ]
        }
    ])
}

/// Returns the JSON Schema definitions for all 22 tools exposed by the Tessera MCP server.
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
            "description": "Declares the current agent session's natural-language intent, optional target branch, optional parent base (`from`), and target AST symbols or file paths.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "intent": { "type": "string", "description": "Natural-language goal or invariant of this agent task." },
                    "branch": { "type": "string", "description": "Optional target Git branch for this session's edits." },
                    "from": { "type": "string", "description": "Optional parent branch or revision to fork `branch` from." },
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
                    "revision": { "type": "string", "description": "Alias for `source`: revision, commit SHA, HEAD~N, branch, or checkpoint label." },
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
                    "from": { "type": "string", "description": "Optional parent branch or revision to fork from when action is 'switch'." },
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
            "name": "push",
            "description": "Pushes the active or specified branch to a Git remote, auto-committing pending session changes unless `no_auto_commit` is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "remote": { "type": "string", "description": "Git remote name (defaults to configured remote or 'origin')." },
                    "branch": { "type": "string", "description": "Branch or refspec to push." },
                    "message": { "type": "string", "description": "Optional commit message if auto-committing pending changes." },
                    "set_upstream": { "type": "boolean", "description": "Set upstream tracking reference (-u)." },
                    "force": { "type": "boolean", "description": "Force push (--force)." },
                    "force_with_lease": { "type": "boolean", "description": "Safe force push (--force-with-lease)." },
                    "tags": { "type": "boolean", "description": "Push all tags (--tags)." },
                    "delete": { "type": "boolean", "description": "Delete remote branch (--delete)." },
                    "dry_run": { "type": "boolean", "description": "Simulate push without updating remote refs." },
                    "no_auto_commit": { "type": "boolean", "description": "Skip auto-committing uncommitted session edits." },
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
                    "content": { "type": "string", "description": "Required resolved file text when strategy is 'content'." }
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
            "name": "commit",
            "description": "Commits the current agent session's isolated changes onto its active or specified branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Commit message." },
                    "branch": { "type": "string", "description": "Optional target branch override." },
                    "amend": { "type": "boolean", "description": "Amend the tip commit of the target branch." },
                    "dry_run": { "type": "boolean", "description": "Simulate commit and optional verification without mutating repository state." },
                    "verify": { "type": "boolean", "description": "Run the repository's configured trusted verify_command before advancing the branch." },
                    "author": { "type": "string", "description": "Optional author override (\"Name <email>\")." },
                    "co_author": { "description": "Optional boolean or Co-authored-by trailer string/array." },
                    "no_co_author": { "type": "boolean", "description": "Suppress Co-authored-by trailers." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "merge",
            "description": "Octopus AST-merges active agent sessions or branches into the target branch with optional speculative verification gate and dry-run simulation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agents": { "type": "array", "items": { "type": "string" }, "description": "Optional agent IDs or branch names to merge (defaults to all active sessions)." },
                    "into": { "type": "string", "description": "Optional target branch (defaults to active Git branch)." },
                    "message": { "type": "string", "description": "Optional custom Git commit message." },
                    "verify": { "type": "boolean", "description": "Run the repository's configured trusted verification command before finalizing the merge." },
                    "dry_run": { "type": "boolean", "description": "Simulate the merge without mutating Git refs, working trees, or the Operation Log." },
                    "keep_open": { "type": "boolean", "description": "Keep merged agent sessions open after a clean merge." },
                    "delete_branch": { "type": "boolean", "description": "Delete merged non-trunk source branches and their sandboxes after a clean, verified merge." },
                    "author": { "type": "string", "description": "Optional author override (\"Name <email>\")." },
                    "co_author": { "description": "Optional boolean or Co-authored-by trailer string/array." },
                    "no_co_author": { "type": "boolean", "description": "Suppress Co-authored-by trailers." }
                }
            }
        },
        {
            "name": "diff",
            "description": "Computes unified diffs and changed file lists for staged/session, working tree, branch, or revision range changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "range": { "type": "string", "description": "Optional revision or range (e.g. 'HEAD~1', 'main..feat/a', 'main...feat/a')." },
                    "branch": { "type": "string", "description": "Optional branch to compare against its merge-base with trunk." },
                    "staged": { "type": "boolean", "description": "Diff only staged/session-owned changes." },
                    "all": { "type": "boolean", "description": "Include combined changes across all active sessions on the branch." },
                    "stat": { "type": "boolean", "description": "Return diffstat summary." },
                    "name_status": { "type": "boolean", "description": "Return only file names and status letters." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional file/directory path filters." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "log",
            "description": "Returns Git commit history entries for the active or specified branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branch": { "type": "string", "description": "Optional branch or revision (defaults to active session branch)." },
                    "limit": { "type": "integer", "description": "Maximum number of commits to return (defaults to 20)." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "oplog",
            "description": "Returns lock-free Operation Log history entries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "Maximum number of operations to return (defaults to 20)." }
                }
            }
        },
        {
            "name": "agent",
            "description": "Lists, closes, or clears active Tessera agent sessions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "close", "clear"], "description": "Session management action." },
                    "name": { "type": "string", "description": "Target agent session ID for 'close'." },
                    "all": { "type": "boolean", "description": "Close all sessions when action is 'close'." },
                    "force": { "type": "boolean", "description": "Also clear dirty sessions when action is 'clear'." },
                    "agent_id": { "type": "string" }
                }
            }
        },
        {
            "name": "config",
            "description": "Displays the effective Tessera repository configuration and tessera.toml trust status (read-only over MCP).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["show"], "description": "Configuration action (only 'show' is supported)." }
                }
            }
        },
        {
            "name": "doctor",
            "description": "Runs repository health, CoW filesystem, config trust, Git hook, sandbox, lease, and conflict diagnostics.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }
    ])
}
