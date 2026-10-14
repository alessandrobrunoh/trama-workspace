//! Blocking database access and session extraction.

use super::*;

/// Runs `op` in an IMMEDIATE transaction while holding the hub write lock,
/// then publishes its outbox before releasing the lock, so live delivery
/// order equals commit (sequence) order.
pub(crate) async fn write<T: Send + 'static>(
    state: &AppState,
    op: impl FnOnce(&Connection, &mut Outbox) -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    let database = state.auth.database.clone();
    let hub = state.hub.clone();
    tokio_blocking(move || {
        let mut connection = open_connection(&database).map_err(db)?;
        let _guard = hub
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let mut outbox = Outbox::default();
        let value = op(&transaction, &mut outbox)?;
        transaction.commit().map_err(db)?;
        hub.publish(outbox);
        Ok(value)
    })
    .await
}

pub(crate) async fn read<T: Send + 'static>(
    state: &AppState,
    op: impl FnOnce(&Connection) -> Result<T, AuthError> + Send + 'static,
) -> Result<T, AuthError> {
    let database = state.auth.database.clone();
    tokio_blocking(move || {
        let connection = open_connection(&database).map_err(db)?;
        op(&connection)
    })
    .await
}

pub(crate) async fn session(state: &AppState, headers: &HeaderMap) -> Result<Identity, AuthError> {
    let token = bearer_token(headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()))?;
    state.auth.authenticate(token).await
}

pub(crate) async fn mutating_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Identity, AuthError> {
    let identity = session(state, headers).await?;
    state
        .hub
        .allow_mutation(identity.user_id, state.auth.now())?;
    Ok(identity)
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------
