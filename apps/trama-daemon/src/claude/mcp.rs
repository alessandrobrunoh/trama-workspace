//! Trama's MCP server for Claude Code (subagents as Trama threads).
//!
//! Each Claude session the daemon starts gets `--mcp-config` with one stdio
//! server, `trama-workspace`, whose command is `trama-daemon mcp`. Claude Code
//! merges it with the user's own MCP servers. The helper speaks MCP
//! (JSON-RPC 2.0, one message per line) on stdin/stdout and forwards every
//! `tools/call` to the daemon socket as [`ClientMessage::McpToolCall`] with
//! the session's token, like the approval hook does. The token maps to
//! exactly one thread, so a session can only act as its own thread.
//!
//! The built-in subagent tool (`Agent`, formerly `Task`) is disallowed by
//! default and the session's system prompt points the model at these tools,
//! so a subagent is a thread the user can open and chat with.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
};

/// `trama-daemon <this>` runs the MCP helper instead of the daemon.
pub const MCP_SUBCOMMAND: &str = "mcp";
/// Server name in the MCP config; tools are `mcp__trama-workspace__<tool>`.
pub const SERVER_NAME: &str = "trama-workspace";
/// Per-session capability that maps the helper back to its thread.
pub const ENV_MCP_TOKEN: &str = "TRAMA_MCP_TOKEN";
/// Permission rule that allows every tool of the server without a prompt.
pub const ALLOW_RULE: &str = "mcp__trama-workspace";
/// Claude Code's built-in subagent tool (2.1.296 names it `Agent` with the
/// alias `Task`); both are disallowed unless the config re-enables them.
pub const BUILTIN_SUBAGENT_TOOLS: [&str; 2] = ["Agent", "Task"];
/// Longest a single tool call may block (`wait_for_threads`, waiting spawn).
pub const MAX_WAIT: Duration = Duration::from_secs(60 * 60);
/// Per-server call timeout given to Claude Code; above [`MAX_WAIT`].
const SERVER_TIMEOUT_MS: u64 = 65 * 60 * 1000;
const FALLBACK_PROTOCOL: &str = "2025-06-18";

pub const TOOL_SPAWN: &str = "spawn_subagent";
pub const TOOL_STATUS: &str = "thread_status";
pub const TOOL_WAIT: &str = "wait_for_threads";
pub const TOOL_SEND: &str = "send_to_thread";
pub const TOOL_LIST: &str = "list_threads";

/// Appended to every session's system prompt while the server is attached.
/// Generic on purpose: Claude Code records the system prompt per
/// conversation, and a forked conversation keeps its parent's record.
pub const SYSTEM_PROMPT: &str = "You are running inside TramaWorkspace. To delegate work to a subagent, use the TramaWorkspace MCP tools (spawn_subagent, wait_for_threads, thread_status, send_to_thread, list_threads) and never a built-in Task or Agent tool: every Trama subagent is a real thread the user can open, follow and chat with. Choose a profile: worker (edits files in its own isolated copy of your checkout; its changes are merged into your checkout when it finishes and conflicts are reported to you), scout (read-only investigation directly in your checkout), reviewer (reviews your current changes and reports findings). Subagents do not see this conversation, so give each a self-contained prompt. Spawn several with wait=false and collect them with wait_for_threads, or use wait=true for one blocking task.";

/// Where the helper executable is and which socket it reaches.
#[derive(Debug, Clone)]
pub struct McpSettings {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub socket_path: PathBuf,
}

impl McpSettings {
    /// Helper = this daemon binary with the `mcp` subcommand.
    pub fn for_current_exe(socket_path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            command: std::env::current_exe()?,
            args: vec![MCP_SUBCOMMAND.to_string()],
            socket_path: socket_path.to_path_buf(),
        })
    }
}

/// `--mcp-config` content: `user` (an existing MCP config, if any) with the
/// Trama server added. The token and socket are passed in the server's own
/// `env`, because Claude Code does not forward its whole environment to
/// stdio servers.
pub fn mcp_config(settings: &McpSettings, token: &str, user: Option<Value>) -> Value {
    let mut config = match user {
        Some(Value::Object(map)) => Value::Object(map),
        _ => json!({}),
    };
    if !config.get("mcpServers").is_some_and(Value::is_object) {
        config["mcpServers"] = json!({});
    }
    config["mcpServers"][SERVER_NAME] = json!({
        "type": "stdio",
        "command": settings.command.to_string_lossy(),
        "args": settings.args,
        "env": {
            crate::claude::approval::ENV_SOCKET: settings.socket_path.to_string_lossy(),
            ENV_MCP_TOKEN: token,
        },
        "timeout": SERVER_TIMEOUT_MS,
    });
    config
}

/// The tools as MCP `tools/list` describes them.
pub fn tool_definitions() -> Value {
    json!([
        {
            "name": TOOL_SPAWN,
            "description": "Start a subagent as a new TramaWorkspace thread, a child of your thread. The user can open it, follow it and chat with it. Use this instead of any built-in Task/Agent tool. Profiles: `worker` (default) works in its own isolated copy of your current files with all tools; when it finishes, its changes are merged into your checkout automatically and conflicts are reported. `scout` investigates read-only directly in your checkout. `reviewer` reviews your current changes in a copy and reports findings. The subagent does not see your conversation: write a self-contained prompt. With wait=true the call blocks until the subagent finishes and returns its final message; otherwise it returns the new thread id at once (collect it later with wait_for_threads). A few subagents run at a time; extra ones are queued.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Short title shown in the thread list."},
                    "prompt": {"type": "string", "description": "Complete, self-contained task for the subagent."},
                    "profile": {"type": "string", "enum": ["worker", "scout", "reviewer"], "default": "worker"},
                    "wait": {"type": "boolean", "default": false, "description": "Block until the subagent finishes."}
                },
                "required": ["title", "prompt"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_STATUS,
            "description": "Status of a thread in this workspace: its title, kind, subagent status (queued, running, waiting_to_sync, completed, failed), whether a turn is running, and its latest final message.",
            "inputSchema": {
                "type": "object",
                "properties": {"thread_id": {"type": "string"}},
                "required": ["thread_id"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_WAIT,
            "description": "Wait until every listed thread is done (its subagent task finished, or no turn running), or until the timeout. Returns each thread's status and final message. A finished worker's changes are merged into your checkout before this returns.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Thread ids to wait for."},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_WAIT.as_secs(), "default": 600, "description": "Seconds to wait at most."}
                },
                "required": ["ids"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_SEND,
            "description": "Send a message to the agent of another thread in this workspace (a subagent you spawned, your parent, or any other thread). It appears in both transcripts. If that thread is busy, it is delivered when the thread's current turn ends.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": {"type": "string"},
                    "message": {"type": "string"}
                },
                "required": ["thread_id", "message"],
                "additionalProperties": false
            }
        },
        {
            "name": TOOL_LIST,
            "description": "List the threads of this workspace with their parent, depth, kind and subagent status. Your own thread is marked.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }
    ])
}

/// Outcome of one tool call: text for the model, and whether it failed.
pub type ToolOutcome = (bool, String);

/// Serve MCP over `reader`/`writer` until EOF. `call(tool, arguments,
/// tool_use_id)` performs a `tools/call`; calls run concurrently, and a
/// `notifications/cancelled` aborts the matching one.
pub async fn serve<R, W, F, Fut>(reader: R, writer: W, call: F)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Fn(String, Value, Option<String>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolOutcome> + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = out_rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&message) else {
                continue;
            };
            line.push(b'\n');
            if writer.write_all(&line).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });
    let call = Arc::new(call);
    let running: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>> = Arc::default();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                let _ = out_tx.send(rpc_error(
                    Value::Null,
                    -32700,
                    &format!("parse error: {error}"),
                ));
                continue;
            }
        };
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id.filter(|id| !id.is_null()) else {
            // Notifications (and stray responses) get no reply.
            if method == "notifications/cancelled" {
                if let Some(request) = params.get("requestId") {
                    if let Some(handle) = running.lock().await.remove(&request.to_string()) {
                        handle.abort();
                    }
                }
            }
            continue;
        };
        match method.as_str() {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(FALLBACK_PROTOCOL);
                let _ = out_tx.send(rpc_result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                        "instructions": SYSTEM_PROMPT,
                    }),
                ));
            }
            "ping" => {
                let _ = out_tx.send(rpc_result(id, json!({})));
            }
            "tools/list" => {
                let _ = out_tx.send(rpc_result(id, json!({"tools": tool_definitions()})));
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let tool_use_id = params
                    .get("_meta")
                    .and_then(|meta| meta.get("claudecode/toolUseId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let call = Arc::clone(&call);
                let out = out_tx.clone();
                let key = id.to_string();
                let registry = Arc::clone(&running);
                let task_key = key.clone();
                let handle = tokio::spawn(async move {
                    let (is_error, text) = call(name, arguments, tool_use_id).await;
                    registry.lock().await.remove(&task_key);
                    let _ = out.send(rpc_result(
                        id,
                        json!({
                            "content": [{"type": "text", "text": text}],
                            "isError": is_error,
                        }),
                    ));
                });
                running.lock().await.insert(key, handle.abort_handle());
            }
            other => {
                let _ = out_tx.send(rpc_error(id, -32601, &format!("method not found: {other}")));
            }
        }
    }
    for (_, handle) in running.lock().await.drain() {
        handle.abort();
    }
    drop(out_tx);
    let _ = writer_task.await;
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One tool call over an already connected daemon stream.
pub async fn call_over<S>(
    stream: S,
    token: &str,
    tool: String,
    arguments: Value,
    tool_use_id: Option<String>,
) -> ToolOutcome
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match exchange(stream, token, tool, arguments, tool_use_id).await {
        Ok(outcome) => outcome,
        Err(reason) => (
            true,
            format!("TramaWorkspace could not run the tool: {reason}"),
        ),
    }
}

async fn exchange<S>(
    stream: S,
    token: &str,
    tool: String,
    arguments: Value,
    tool_use_id: Option<String>,
) -> Result<ToolOutcome, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let send = |msg: ClientMessage| encode_json_line(&msg).map_err(|err| err.to_string());
    writer
        .write_all(&send(ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "trama-mcp".into(),
        })?)
        .await
        .map_err(|err| err.to_string())?;
    match next_message(&mut lines).await? {
        DaemonMessage::Welcome { accepted: true, .. } => {}
        DaemonMessage::Welcome { .. } => {
            return Err("the daemon rejected the helper's protocol version".into());
        }
        other => return Err(format!("unexpected handshake reply {other:?}")),
    }
    writer
        .write_all(&send(ClientMessage::McpToolCall {
            token: token.to_owned(),
            tool,
            arguments,
            tool_use_id,
        })?)
        .await
        .map_err(|err| err.to_string())?;
    writer.flush().await.map_err(|err| err.to_string())?;
    loop {
        match next_message(&mut lines).await? {
            DaemonMessage::McpToolResult { is_error, text } => return Ok((is_error, text)),
            DaemonMessage::Error { message, .. } => return Err(message),
            _ => continue,
        }
    }
}

async fn next_message<R>(lines: &mut tokio::io::Lines<R>) -> Result<DaemonMessage, String>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => return decode_json_line(&line).map_err(|err| err.to_string()),
            Ok(None) => return Err("the Trama daemon closed the connection".into()),
            Err(err) => return Err(err.to_string()),
        }
    }
}

/// Entry point for `trama-daemon mcp`: serve stdio until Claude closes it.
pub async fn run_server() -> i32 {
    let socket = std::env::var_os(crate::claude::approval::ENV_SOCKET).map(PathBuf::from);
    let token = std::env::var(ENV_MCP_TOKEN).ok();
    serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        move |tool, arguments, tool_use_id| {
            let socket = socket.clone();
            let token = token.clone();
            async move {
                let (Some(socket), Some(token)) = (socket, token) else {
                    return (
                        true,
                        "This session was not started by TramaWorkspace, so its tools are unavailable."
                            .to_owned(),
                    );
                };
                match trama_ipc::connect(&socket).await {
                    Ok(stream) => call_over(stream, &token, tool, arguments, tool_use_id).await,
                    Err(err) => (true, format!("The TramaWorkspace daemon is unreachable ({err}).")),
                }
            }
        },
    )
    .await;
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn config_adds_the_server_to_the_users_config() {
        let settings = McpSettings {
            command: PathBuf::from("/Applications/Trama.app/trama-daemon"),
            args: vec![MCP_SUBCOMMAND.into()],
            socket_path: PathBuf::from("/x/daemon.sock"),
        };
        let user = json!({"mcpServers": {"context7": {"command": "npx"}}, "other": 1});
        let config = mcp_config(&settings, "tok", Some(user));
        assert_eq!(config["mcpServers"]["context7"]["command"], "npx");
        assert_eq!(config["other"], 1);
        let server = &config["mcpServers"][SERVER_NAME];
        assert_eq!(server["type"], "stdio");
        assert_eq!(server["command"], "/Applications/Trama.app/trama-daemon");
        assert_eq!(server["args"], json!(["mcp"]));
        assert_eq!(server["env"][ENV_MCP_TOKEN], "tok");
        assert_eq!(server["env"]["TRAMA_DAEMON_SOCKET"], "/x/daemon.sock");
        assert!(server["timeout"].as_u64().unwrap() > MAX_WAIT.as_millis() as u64);
        let alone = mcp_config(&settings, "t2", None);
        assert_eq!(alone["mcpServers"].as_object().unwrap().len(), 1);
        let names: Vec<_> = tool_definitions()
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            [TOOL_SPAWN, TOOL_STATUS, TOOL_WAIT, TOOL_SEND, TOOL_LIST]
        );
    }

    /// Feed `input`, read `expected` replies, then close stdin and collect
    /// anything else the server still writes.
    async fn exchange_lines(input: &str, expected: usize) -> Vec<Value> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let serving = tokio::spawn(serve(
            server_read,
            server_write,
            move |tool: String, _args: Value, tool_use_id: Option<String>| async move {
                if tool == "slow" {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                let id = tool_use_id.unwrap_or_default();
                (tool == "bad", format!("ran {tool} {id}"))
            },
        ));
        let (client_read, mut client_write) = tokio::io::split(client);
        client_write.write_all(input.as_bytes()).await.unwrap();
        client_write.write_all(b"\n").await.unwrap();
        let mut lines = BufReader::new(client_read).lines();
        let mut replies = Vec::new();
        while replies.len() < expected {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .expect("reply")
                .unwrap()
                .expect("open");
            replies.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        client_write.shutdown().await.unwrap();
        drop(client_write);
        let mut rest = String::new();
        let mut reader = lines.into_inner();
        tokio::time::timeout(Duration::from_secs(5), reader.read_to_string(&mut rest))
            .await
            .expect("server finished")
            .unwrap();
        serving.await.unwrap();
        replies.extend(
            rest.lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap()),
        );
        replies
    }

    #[tokio::test]
    async fn speaks_mcp_over_json_lines() {
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-code","version":"2.1.296"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"spawn_subagent","arguments":{"title":"t","prompt":"p"},"_meta":{"claudecode/toolUseId":"toolu_7"}}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bad","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#,
            "not json",
            "",
        ]
        .join("\n");
        let replies = exchange_lines(&input, 6).await;
        let by_id = |id: i64| {
            replies
                .iter()
                .find(|reply| reply["id"] == id)
                .cloned()
                .unwrap_or_else(|| panic!("no reply {id}: {replies:?}"))
        };
        let init = by_id(1);
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert_eq!(by_id(2)["result"]["tools"].as_array().unwrap().len(), 5);
        let call = by_id(3);
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(
            call["result"]["content"][0]["text"],
            "ran spawn_subagent toolu_7"
        );
        assert_eq!(by_id(4)["result"]["isError"], true);
        assert_eq!(by_id(5)["error"]["code"], -32601);
        assert!(replies.iter().any(|reply| reply["error"]["code"] == -32700));
        // The notification got no reply.
        assert_eq!(replies.len(), 6);
    }

    #[tokio::test]
    async fn cancelled_calls_do_not_reply() {
        let input = [
            r#"{"jsonrpc":"2.0","id":"a","method":"tools/call","params":{"name":"slow","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"a"}}"#,
            r#"{"jsonrpc":"2.0","id":"b","method":"ping"}"#,
        ]
        .join("\n");
        let replies = exchange_lines(&input, 1).await;
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["id"], "b");
    }

    #[tokio::test]
    async fn closed_daemon_connection_is_an_error_result() {
        let (client, server) = tokio::io::duplex(4096);
        drop(server);
        let (is_error, text) = call_over(client, "tok", TOOL_LIST.into(), json!({}), None).await;
        assert!(is_error);
        assert!(text.contains("could not run"));
    }
}
