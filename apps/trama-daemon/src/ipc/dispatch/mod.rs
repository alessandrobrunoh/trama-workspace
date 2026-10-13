//! Dispatch of one client line, split by message family.

use super::*;

mod collab;
mod comments;
mod shares;
mod subthreads;
mod terminals;
mod turns;
mod workspaces;

/// Borrowed connection state handed to each family handler.
pub(super) struct Ctx<'a> {
    pub sessions: &'a SessionManager,
    pub collab: &'a CollabService,
    pub out_tx: &'a mpsc::UnboundedSender<DaemonMessage>,
    pub subscriptions: &'a mut HashMap<ThreadId, SubscriptionId>,
    pub shutdown: &'a watch::Sender<bool>,
}

/// Message families. Exhaustive so a new message must pick one.
enum Family {
    Workspaces,
    Turns,
    Subthreads,
    Comments,
    Terminals,
    Collab,
    Shares,
    Control,
}

fn family(msg: &ClientMessage) -> Family {
    match msg {
        ClientMessage::ListWorkspaces { .. }
        | ClientMessage::ListThreads { .. }
        | ClientMessage::CreateThread { .. }
        | ClientMessage::CreateWorkspace { .. }
        | ClientMessage::AddRepository { .. }
        | ClientMessage::ListRepositories { .. }
        | ClientMessage::MigrateLegacyThread { .. }
        | ClientMessage::GetThreadCheckouts { .. }
        | ClientMessage::GetDiskUsage { .. }
        | ClientMessage::ArchiveThread { .. }
        | ClientMessage::UnarchiveThread { .. }
        | ClientMessage::DeleteThreadCheckouts { .. }
        | ClientMessage::ForkThread { .. } => Family::Workspaces,
        ClientMessage::SubscribeThread { .. }
        | ClientMessage::UnsubscribeThread { .. }
        | ClientMessage::SendPrompt { .. }
        | ClientMessage::Interrupt { .. }
        | ClientMessage::RespondApproval { .. }
        | ClientMessage::ListMentionables { .. }
        | ClientMessage::ListSkills { .. }
        | ClientMessage::SetThreadModel { .. }
        | ClientMessage::ListModels { .. } => Family::Turns,
        ClientMessage::CreateSubthread { .. }
        | ClientMessage::ListSubthreads { .. }
        | ClientMessage::GetChanges { .. }
        | ClientMessage::PullChanges { .. }
        | ClientMessage::SubmitReviewVerdict { .. }
        | ClientMessage::GetRemoteChanges { .. } => Family::Subthreads,
        ClientMessage::SendComments { .. }
        | ClientMessage::CreateComment { .. }
        | ClientMessage::ListComments { .. }
        | ClientMessage::ResolveComment { .. }
        | ClientMessage::ReopenComment { .. }
        | ClientMessage::DeleteComment { .. } => Family::Comments,
        ClientMessage::OpenTerminal { .. }
        | ClientMessage::TerminalInput { .. }
        | ClientMessage::ResizeTerminal { .. }
        | ClientMessage::CloseTerminal { .. }
        | ClientMessage::ListTerminals { .. }
        | ClientMessage::ReplayTerminal { .. } => Family::Terminals,
        ClientMessage::ConnectCollab { .. }
        | ClientMessage::DisconnectCollab { .. }
        | ClientMessage::GetCollabStatus { .. }
        | ClientMessage::DiscoverCollabServer { .. }
        | ClientMessage::ShareThread { .. }
        | ClientMessage::GetSharedThread { .. }
        | ClientMessage::RemoveSharedThreadMember { .. }
        | ClientMessage::JoinSharedThread { .. }
        | ClientMessage::ListSharedThreads { .. }
        | ClientMessage::UnlinkSharedThread { .. }
        | ClientMessage::SetCollabDraft { .. }
        | ClientMessage::SetCollabPresence { .. } => Family::Collab,
        ClientMessage::SharePreflight { .. }
        | ClientMessage::CreateShare { .. }
        | ClientMessage::ListShares
        | ClientMessage::RequestShareJoin { .. }
        | ClientMessage::ListShareJoinRequests { .. }
        | ClientMessage::ApproveShareJoin { .. }
        | ClientMessage::CompleteShareJoin { .. }
        | ClientMessage::PublishShareSnapshot { .. }
        | ClientMessage::AcquireShareLease { .. }
        | ClientMessage::ReleaseShareLease { .. }
        | ClientMessage::ListShareLeases { .. } => Family::Shares,
        ClientMessage::Hello { .. }
        | ClientMessage::ApprovalHook { .. }
        | ClientMessage::McpToolCall { .. }
        | ClientMessage::Shutdown { .. } => Family::Control,
    }
}

pub(super) async fn handle_line(
    line: &str,
    sessions: &SessionManager,
    collab: &CollabService,
    out_tx: &mpsc::UnboundedSender<DaemonMessage>,
    subscriptions: &mut HashMap<ThreadId, SubscriptionId>,
    shutdown: &watch::Sender<bool>,
) -> Result<ConnectionControl> {
    let msg: ClientMessage = match decode_json_line(line) {
        Ok(msg) => msg,
        Err(err) => {
            let _ = out_tx.send(DaemonMessage::Error {
                code: "invalid_json".into(),
                message: err.to_string(),
            });
            return Ok(ConnectionControl::Continue);
        }
    };

    let mut ctx = Ctx {
        sessions,
        collab,
        out_tx,
        subscriptions,
        shutdown,
    };
    match family(&msg) {
        Family::Workspaces => workspaces::handle(msg, &mut ctx).await,
        Family::Turns => turns::handle(msg, &mut ctx).await,
        Family::Subthreads => subthreads::handle(msg, &mut ctx).await,
        Family::Comments => comments::handle(msg, &mut ctx).await,
        Family::Terminals => terminals::handle(msg, &mut ctx).await,
        Family::Collab => collab::handle(msg, &mut ctx).await,
        Family::Shares => shares::handle(msg, &mut ctx).await,
        Family::Control => control(msg, &mut ctx).await,
    }
}

async fn control(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let Ctx {
        out_tx, shutdown, ..
    } = ctx;
    match msg {
        ClientMessage::Hello { .. } => {
            let _ = out_tx.send(DaemonMessage::Error {
                code: "protocol".into(),
                message: "hello already accepted".into(),
            });
        }
        ClientMessage::ApprovalHook { .. } => {
            // Intercepted by `handle_connection` before reaching here.
            let _ = out_tx.send(DaemonMessage::Error {
                code: "protocol".into(),
                message: "approval_hook is only accepted from the approval hook".into(),
            });
        }
        ClientMessage::McpToolCall { .. } => {
            // Intercepted by `handle_connection` before reaching here.
            let _ = out_tx.send(DaemonMessage::Error {
                code: "protocol".into(),
                message: "mcp_tool_call is only accepted from the Trama MCP helper".into(),
            });
        }
        ClientMessage::Shutdown => {
            let _ = shutdown.send(true);
            return Ok(ConnectionControl::Close);
        }
        _ => unreachable!("not a control message"),
    }
    Ok(ConnectionControl::Continue)
}
