//! Auth HTTP router and handlers.

use crate::*;

/// The service for direct (loopback) use: forwarding headers are ignored.
pub fn router(auth: AuthService) -> Router {
    router_with_proxy(auth, None)
}

/// The service behind a reverse proxy. With `Some(trust)`, the client address
/// used for login throttling is taken from the configured forwarding header,
/// but only when the TCP peer is a trusted proxy (see [`ProxyTrust`]).
pub fn router_with_proxy(auth: AuthService, proxy: Option<ProxyTrust>) -> Router {
    let state = AppState {
        auth,
        hash_slots: Arc::new(Semaphore::new(8)),
        throttle: Arc::new(Mutex::new(ThrottleState::default())),
        hub: Arc::new(Hub::default()),
        proxy: proxy.map(Arc::new),
    };
    let auth_routes = Router::new()
        .route("/auth/login", post(login))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));
    auth_routes
        .route("/healthz", get(healthz))
        .route(
            trama_collab_protocol::vnext::discovery::DISCOVERY_PATH,
            get(super::discovery::well_known),
        )
        .merge(collab::routes())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            acquire_hash_slot,
        ))
        .with_state(state)
}

pub(crate) async fn acquire_hash_slot(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // Only login performs password hashing; protect the CPU-heavy path from unbounded work.
    if request.uri().path() != "/auth/login" {
        return next.run(request).await;
    }
    let Ok(permit) = state.hash_slots.clone().try_acquire_owned() else {
        return AuthError::Throttled.into_response();
    };
    let response = next.run(request).await;
    drop(permit);
    response
}

/// Liveness and database reachability for container health checks. It is
/// unauthenticated and returns no data; the proxy should not publish it.
pub(crate) async fn healthz(State(state): State<AppState>) -> Response {
    let database = state.auth.database.clone();
    let healthy = tokio_blocking(move || {
        open_connection(&database)
            .and_then(|connection| {
                connection
                    .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                    .map_err(|e| e.to_string())
            })
            .map(|_| ())
            .map_err(|_| AuthError::Database)
    })
    .await
    .is_ok();
    if healthy {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response()
    }
}

/// The address a request counts as coming from for throttling.
pub(crate) fn client_ip(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    match &state.proxy {
        Some(trust) => trust.client_ip(peer.ip(), headers),
        None => peer.ip(),
    }
}

pub(crate) async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AuthError> {
    validate_credentials(&input.username, &input.password).map_err(|_| AuthError::InvalidInput)?;
    let ip = client_ip(&state, peer, &headers);
    if !allowed_attempt(&state, &input.username, ip) {
        return Err(AuthError::Throttled);
    }
    state
        .auth
        .login(input.username, input.password)
        .await
        .map(Json)
}

pub(crate) async fn me(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<UserResponse>, AuthError> {
    let token = bearer_token(
        request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )?;
    let username = state.auth.current_user(token).await?;
    Ok(Json(UserResponse { username }))
}

pub(crate) async fn logout(
    State(state): State<AppState>,
    request: Request,
) -> Result<StatusCode, AuthError> {
    // Revocation is idempotent by design: unknown and already-revoked tokens
    // also return 204, so logout never reveals whether a token existed.
    let token = bearer_token(
        request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )?;
    state.auth.logout(token).await?;
    // Open sockets re-validate their session and close if it is gone.
    state.hub.credentials_changed();
    Ok(StatusCode::NO_CONTENT)
}
