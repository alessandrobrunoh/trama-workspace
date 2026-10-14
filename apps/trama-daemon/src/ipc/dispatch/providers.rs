//! Model provider CRUD and connection tests (ADR 0015).

use super::*;

pub(super) async fn handle(msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let Ctx {
        sessions, out_tx, ..
    } = ctx;
    let store = sessions.providers();
    match msg {
        ClientMessage::ListProviders => {}
        ClientMessage::SaveProvider { provider, api_key } => {
            store.save(provider, api_key)?;
        }
        ClientMessage::RemoveProvider { id } => store.remove(&id)?,
        ClientMessage::SetDefaultProvider { id } => store.set_default(&id)?,
        ClientMessage::TestProvider {
            id,
            provider,
            api_key,
        } => {
            let report = store.test(&id, provider, api_key).await;
            let _ = out_tx.send(DaemonMessage::ProviderTestResult { id, report });
        }
        _ => unreachable!("not a providers message"),
    }
    let _ = out_tx.send(DaemonMessage::Providers {
        items: store.snapshots(),
    });
    Ok(ConnectionControl::Continue)
}
