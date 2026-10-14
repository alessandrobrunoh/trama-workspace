use super::*;

pub(super) async fn handle(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let Ctx {
        sessions,
        out_tx,
        subscriptions,
        ..
    } = ctx;
    match msg {
        ClientMessage::OpenTerminal {
            thread_id,
            repository_id,
            command,
            cols,
            rows,
        } => {
            let terminal = sessions
                .open_terminal(&thread_id, repository_id, command, cols, rows)
                .await?;
            if !subscriptions.contains_key(&thread_id) {
                let _ = out_tx.send(DaemonMessage::TerminalOpened {
                    thread_id,
                    terminal,
                });
            }
        }
        ClientMessage::TerminalInput { terminal_id, data } => {
            sessions.terminal_input(&terminal_id, data).await?;
        }
        ClientMessage::ResizeTerminal {
            terminal_id,
            cols,
            rows,
        } => {
            sessions.resize_terminal(&terminal_id, cols, rows)?;
        }
        ClientMessage::CloseTerminal { terminal_id } => {
            sessions.close_terminal(&terminal_id)?;
        }
        ClientMessage::ListTerminals { thread_id } => {
            let terminals = sessions.list_terminals(&thread_id);
            let _ = out_tx.send(DaemonMessage::Terminals {
                thread_id,
                terminals,
            });
        }
        ClientMessage::ReplayTerminal { terminal_id } => {
            let _ = out_tx.send(sessions.replay_terminal(&terminal_id)?);
        }
        _ => unreachable!("not a terminals message"),
    }
    Ok(ConnectionControl::Continue)
}
