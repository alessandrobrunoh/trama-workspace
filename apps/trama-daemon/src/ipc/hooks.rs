use super::*;

/// Block until the request is answered, timed out, or the hook hangs up,
/// then write the single decision. A hang-up (Claude's hook timeout, an
/// interrupted or killed CLI) withdraws the request so clients stop showing
/// it.
pub(super) async fn serve_approval_hook<R, W>(
    sessions: &SessionManager,
    lines: &mut tokio::io::Lines<R>,
    writer: &mut W,
    token: &str,
    tool_name: String,
    tool_input: serde_json::Value,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let decision = match sessions
        .request_approval(token, tool_name, tool_input)
        .await
    {
        ApprovalWait::Decided(decision) => decision,
        ApprovalWait::Pending { request_id, mut rx } => {
            let timeout = sessions.approval_timeout();
            tokio::select! {
                answered = tokio::time::timeout(timeout, &mut rx) => match answered {
                    Ok(Ok(decision)) => decision,
                    Ok(Err(_)) => HookDecision::deny("The approval request was dropped by Trama."),
                    Err(_) => {
                        let message = format!(
                            "No decision in Trama within {} minutes, so the tool call was denied.",
                            timeout.as_secs().div_ceil(60)
                        );
                        if sessions.cancel_approval(&request_id, &message).await {
                            HookDecision::deny(message)
                        } else {
                            // Answered as the timer fired: report that answer.
                            rx.try_recv().unwrap_or_else(|_| HookDecision::deny(message))
                        }
                    }
                },
                _ = hook_gone(lines) => {
                    sessions
                        .cancel_approval(
                            &request_id,
                            "The agent stopped waiting for this approval.",
                        )
                        .await;
                    return Ok(());
                }
            }
        }
    };
    write_msg(
        writer,
        &DaemonMessage::ApprovalHookDecision {
            decision: decision.decision,
            message: decision.message,
            interrupt: decision.interrupt,
        },
    )
    .await
}

/// Run one Trama MCP tool call and write its single result. If the helper
/// hangs up first (the agent cancelled the call), a waiting tool stops
/// waiting; what the call already changed stays.
pub(super) async fn serve_mcp_tool<R, W>(
    sessions: &SessionManager,
    lines: &mut tokio::io::Lines<R>,
    writer: &mut W,
    token: &str,
    tool: &str,
    arguments: serde_json::Value,
    tool_use_id: Option<String>,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (is_error, text) = tokio::select! {
        outcome = sessions.mcp_tool_call(token, tool, arguments, tool_use_id) => outcome,
        _ = hook_gone(lines) => return Ok(()),
    };
    write_msg(writer, &DaemonMessage::McpToolResult { is_error, text }).await
}

/// Resolves when the hook side closes or errors. Extra lines are ignored.
pub(super) async fn hook_gone<R: tokio::io::AsyncBufRead + Unpin>(lines: &mut tokio::io::Lines<R>) {
    loop {
        match lines.next_line().await {
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return,
        }
    }
}
