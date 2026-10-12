//! Claude Code approval channel (ADR 0002).
//!
//! Claude Code runs `trama-daemon approval-hook` as its `PermissionRequest`
//! hook (injected with `--settings`) whenever a tool call would prompt. The
//! hook reads the request from stdin, connects to the daemon socket named in
//! its environment, sends [`ClientMessage::ApprovalHook`] with the session's
//! token, blocks until [`DaemonMessage::ApprovalHookDecision`], and prints the
//! documented hook decision JSON on stdout.
//!
//! Every failure path prints a deny (never an allow), and the hook always
//! exits 0 so Claude Code reads the decision instead of treating the hook as
//! failed. If the hook is cancelled (Claude's hook timeout), Claude Code finds
//! no decision and denies the call itself, since `-p` has no prompt to fall
//! back to.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use trama_domain::agent::{ApprovalDecision, ApprovalHookConfig};
use trama_protocol::{
    ClientMessage, DaemonMessage, PROTOCOL_VERSION, decode_json_line, encode_json_line,
};

/// `trama-daemon <this>` runs the hook instead of the daemon.
pub const APPROVAL_HOOK_SUBCOMMAND: &str = "approval-hook";
/// Socket the hook connects to. Set on the `claude` child, inherited by hooks.
pub const ENV_SOCKET: &str = "TRAMA_DAEMON_SOCKET";
/// Per-session capability that maps the hook back to its thread.
pub const ENV_TOKEN: &str = "TRAMA_APPROVAL_TOKEN";

/// How long the daemon waits for a person before denying.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Claude Code's hook timeout is the daemon timeout plus this slack, so the
/// daemon's deny (with an explanation) normally wins over a hook kill.
const HOOK_TIMEOUT_SLACK: Duration = Duration::from_secs(30);

/// A decision as the hook reports it to Claude Code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDecision {
    pub decision: ApprovalDecision,
    pub message: Option<String>,
    pub interrupt: bool,
}

impl HookDecision {
    pub fn allow(decision: ApprovalDecision) -> Self {
        Self {
            decision,
            message: None,
            interrupt: false,
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self {
            decision: ApprovalDecision::Deny,
            message: Some(message.into()),
            interrupt: false,
        }
    }

    pub fn is_allow(&self) -> bool {
        !matches!(self.decision, ApprovalDecision::Deny)
    }
}

/// Where the hook executable is and how long a request may wait.
#[derive(Debug, Clone)]
pub struct ApprovalHookSettings {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub socket_path: PathBuf,
    pub timeout: Duration,
}

impl ApprovalHookSettings {
    /// Hook = this daemon binary with the `approval-hook` subcommand.
    pub fn for_current_exe(socket_path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            command: std::env::current_exe()?,
            args: vec![APPROVAL_HOOK_SUBCOMMAND.to_string()],
            socket_path: socket_path.to_path_buf(),
            timeout: DEFAULT_APPROVAL_TIMEOUT,
        })
    }

    pub fn hook_config(&self) -> ApprovalHookConfig {
        let timeout = self.timeout + HOOK_TIMEOUT_SLACK;
        ApprovalHookConfig {
            command: self.command.clone(),
            args: self.args.clone(),
            timeout_secs: u32::try_from(timeout.as_secs()).unwrap_or(u32::MAX),
        }
    }
}

/// Fresh random token. The socket is already owner-only (0600); the token
/// scopes a hook to the one session whose environment carries it.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
        .is_ok();
    if filled {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    } else {
        format!(
            "{}{}",
            trama_domain::ids::AgentSessionId::generate(),
            trama_domain::ids::AgentSessionId::generate()
        )
    }
}

/// Hook stdout for Claude Code (`hooks` reference, PermissionRequest
/// decision control). `updatedInput` is omitted: the tool runs as requested.
pub fn hook_output(decision: &HookDecision) -> serde_json::Value {
    let body = if decision.is_allow() {
        serde_json::json!({ "behavior": "allow" })
    } else {
        let mut body = serde_json::json!({
            "behavior": "deny",
            "message": decision
                .message
                .clone()
                .unwrap_or_else(|| "Denied in Trama.".to_string()),
        });
        if decision.interrupt {
            body["interrupt"] = serde_json::Value::Bool(true);
        }
        body
    };
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": body,
        }
    })
}

/// `tool_name` / `tool_input` from the hook's stdin JSON.
pub fn parse_hook_input(raw: &str) -> Result<(String, serde_json::Value), String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|err| format!("invalid hook input: {err}"))?;
    let tool_name = value
        .get("tool_name")
        .and_then(|v| v.as_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "hook input has no tool_name".to_string())?
        .to_string();
    let tool_input = value
        .get("tool_input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok((tool_name, tool_input))
}

/// Ask the daemon on an already connected stream. Never returns an allow
/// unless the daemon sent one.
pub async fn request_over<S>(
    stream: S,
    token: &str,
    tool_name: &str,
    tool_input: serde_json::Value,
) -> HookDecision
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match exchange(stream, token, tool_name, tool_input).await {
        Ok(decision) => decision,
        Err(reason) => HookDecision::deny(format!(
            "Trama could not get an approval decision ({reason}), so this tool call was denied."
        )),
    }
}

async fn exchange<S>(
    stream: S,
    token: &str,
    tool_name: &str,
    tool_input: serde_json::Value,
) -> Result<HookDecision, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let send = |msg: ClientMessage| encode_json_line(&msg).map_err(|err| err.to_string());

    writer
        .write_all(&send(ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "trama-approval-hook".into(),
        })?)
        .await
        .map_err(|err| err.to_string())?;
    match next_message(&mut lines).await? {
        DaemonMessage::Welcome { accepted: true, .. } => {}
        DaemonMessage::Welcome { .. } => {
            return Err("daemon rejected the hook's protocol version".into());
        }
        other => return Err(format!("unexpected handshake reply {other:?}")),
    }

    writer
        .write_all(&send(ClientMessage::ApprovalHook {
            token: token.to_string(),
            tool_name: tool_name.to_string(),
            tool_input,
        })?)
        .await
        .map_err(|err| err.to_string())?;
    writer.flush().await.map_err(|err| err.to_string())?;

    loop {
        match next_message(&mut lines).await? {
            DaemonMessage::ApprovalHookDecision {
                decision,
                message,
                interrupt,
            } => {
                return Ok(HookDecision {
                    decision,
                    message,
                    interrupt,
                });
            }
            DaemonMessage::Error { message, .. } => return Err(message),
            _ => continue,
        }
    }
}

async fn next_message<R>(lines: &mut tokio::io::Lines<R>) -> Result<DaemonMessage, String>
where
    R: tokio::io::AsyncBufRead + Unpin,
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

/// Entry point for `trama-daemon approval-hook`. Prints one decision and
/// returns the process exit code (always 0, see module docs).
pub async fn run_hook() -> i32 {
    let mut raw = String::new();
    let decision = match tokio::io::stdin().read_to_string(&mut raw).await {
        Err(err) => HookDecision::deny(format!("Trama could not read the hook input: {err}")),
        Ok(_) => decide_from_env(&raw).await,
    };
    println!("{}", hook_output(&decision));
    0
}

async fn decide_from_env(raw: &str) -> HookDecision {
    let (tool_name, tool_input) = match parse_hook_input(raw) {
        Ok(parsed) => parsed,
        Err(err) => return HookDecision::deny(format!("Trama: {err}")),
    };
    let (Some(socket), Some(token)) = (std::env::var_os(ENV_SOCKET), std::env::var(ENV_TOKEN).ok())
    else {
        return HookDecision::deny(
            "This session was not started by Trama, so no one can approve tool calls.",
        );
    };
    match tokio::net::UnixStream::connect(PathBuf::from(socket)).await {
        Ok(stream) => request_over(stream, &token, &tool_name, tool_input).await,
        Err(err) => HookDecision::deny(format!(
            "Trama daemon is unreachable ({err}), so this tool call was denied."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_output_matches_documented_decision_shape() {
        assert_eq!(
            hook_output(&HookDecision::allow(ApprovalDecision::AllowOnce)),
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": { "behavior": "allow" }
                }
            })
        );
        assert_eq!(
            hook_output(&HookDecision::allow(ApprovalDecision::AllowSession)),
            hook_output(&HookDecision::allow(ApprovalDecision::AllowOnce))
        );
        assert_eq!(
            hook_output(&HookDecision::deny("nope")),
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": { "behavior": "deny", "message": "nope" }
                }
            })
        );
        let mut interrupted = HookDecision::deny("stop");
        interrupted.interrupt = true;
        assert_eq!(
            hook_output(&interrupted)["hookSpecificOutput"]["decision"],
            serde_json::json!({ "behavior": "deny", "message": "stop", "interrupt": true })
        );
    }

    #[test]
    fn parses_real_permission_request_input() {
        // Captured from Claude Code 2.1.296 (paths shortened).
        let raw = r#"{"session_id":"ee3e","transcript_path":"/t.jsonl","cwd":"/repo","prompt_id":"b2a6","permission_mode":"default","effort":{"level":"medium"},"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"touch created-by-claude.txt","description":"Create empty file"},"permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]}"#;
        let (name, input) = parse_hook_input(raw).unwrap();
        assert_eq!(name, "Bash");
        assert_eq!(input["command"], "touch created-by-claude.txt");
        assert!(parse_hook_input("{}").is_err());
        assert!(parse_hook_input("not json").is_err());
    }

    #[test]
    fn hook_timeout_exceeds_daemon_timeout() {
        let settings = ApprovalHookSettings {
            command: PathBuf::from("/x/trama-daemon"),
            args: vec![APPROVAL_HOOK_SUBCOMMAND.into()],
            socket_path: PathBuf::from("/x/daemon.sock"),
            timeout: Duration::from_secs(60),
        };
        let config = settings.hook_config();
        assert_eq!(config.timeout_secs, 90);
        assert_eq!(config.args, vec!["approval-hook".to_string()]);
    }

    #[test]
    fn tokens_are_unique_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert!(a.len() >= 32);
    }

    #[tokio::test]
    async fn closed_daemon_connection_denies() {
        let (client, server) = tokio::io::duplex(4096);
        drop(server);
        let decision = request_over(client, "tok", "Bash", serde_json::json!({})).await;
        assert_eq!(decision.decision, ApprovalDecision::Deny);
        assert!(decision.message.unwrap().contains("denied"));
    }
}
