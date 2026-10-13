//! Share/Join flow (protocol 14). Handlers land with the daemon wiring;
//! until then every message is answered with an `unsupported` error.

use super::*;

pub(super) async fn handle(_msg: ClientMessage, ctx: &mut Ctx<'_>) -> Result<ConnectionControl> {
    let _ = ctx.out_tx.send(DaemonMessage::Error {
        code: "unsupported".into(),
        message: "the share flow is not available in this build yet".into(),
    });
    Ok(ConnectionControl::Continue)
}
