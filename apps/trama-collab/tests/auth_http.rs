use std::{net::SocketAddr, time::Duration};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::ServiceExt;
use trama_collab::{
    AuthConfig, AuthService, ForwardedHeader, ProxyTrust, probe_health, router, router_with_proxy,
};

async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let mut request = builder.body(Body::from(body.to_string())).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn service(path: &std::path::Path) -> AuthService {
    AuthService::open(
        path,
        AuthConfig {
            session_ttl: Duration::from_secs(600),
            ..AuthConfig::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn users_login_independently_and_logout_revokes_session() {
    let dir = tempdir().unwrap();
    let auth = service(&dir.path().join("auth.sqlite"));
    auth.create_user("alice", "alice-secret-123").await.unwrap();
    auth.create_user("bob", "bob-secret-456").await.unwrap();
    let app = router(auth.clone());

    let (status, alice) = request(
        app.clone(),
        "POST",
        "/auth/login",
        None,
        json!({"username":"alice","password":"alice-secret-123"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let alice_token = alice["token"].as_str().unwrap().to_owned();
    let (status, bob) = request(
        app.clone(),
        "POST",
        "/auth/login",
        None,
        json!({"username":"bob","password":"bob-secret-456"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let bob_token = bob["token"].as_str().unwrap();
    let (status, who) = request(
        app.clone(),
        "GET",
        "/auth/me",
        Some(&alice_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(who["username"], "alice");
    let (status, _) = request(
        app.clone(),
        "POST",
        "/auth/logout",
        Some(&alice_token),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            "/auth/me",
            Some(&alice_token),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app, "GET", "/auth/me", Some(bob_token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn invalid_credentials_are_indistinguishable_and_sessions_survive_reopen_without_plaintext_secrets()
 {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.sqlite");
    let auth = service(&path);
    auth.create_user("known", "known-secret-123").await.unwrap();
    let app = router(auth.clone());
    let known = request(
        app.clone(),
        "POST",
        "/auth/login",
        None,
        json!({"username":"known","password":"wrong-password"}),
    )
    .await;
    let unknown = request(
        app,
        "POST",
        "/auth/login",
        None,
        json!({"username":"missing","password":"wrong-password"}),
    )
    .await;
    assert_eq!(known, unknown);
    assert_eq!(known.0, StatusCode::UNAUTHORIZED);

    let (status, login) = request(
        router(auth),
        "POST",
        "/auth/login",
        None,
        json!({"username":"known","password":"known-secret-123"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = login["token"].as_str().unwrap().to_owned();
    drop(login);
    let reopened = service(&path);
    assert_eq!(
        request(
            router(reopened),
            "GET",
            "/auth/me",
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::OK
    );
    let bytes = std::fs::read(path).unwrap();
    let contents = String::from_utf8_lossy(&bytes);
    assert!(!contents.contains("known-secret-123"));
    assert!(!contents.contains(&token));
}

#[tokio::test]
async fn anonymous_expired_and_malformed_tokens_fail_and_duplicate_user_is_safe() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.sqlite");
    let now = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1_700_000_000));
    let clock = now.clone();
    let auth = AuthService::open(
        &path,
        AuthConfig {
            session_ttl: Duration::from_secs(10),
            clock: std::sync::Arc::new(move || clock.load(std::sync::atomic::Ordering::SeqCst)),
            ..AuthConfig::default()
        },
    )
    .unwrap();
    auth.create_user("alice", "alice-secret-123").await.unwrap();
    assert!(
        auth.create_user("alice", "another-secret-123")
            .await
            .is_err()
    );
    let app = router(auth);
    assert_eq!(
        request(app.clone(), "GET", "/auth/me", None, Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app.clone(), "GET", "/auth/me", Some("bad"), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, login) = request(
        app.clone(),
        "POST",
        "/auth/login",
        None,
        json!({"username":"alice","password":"alice-secret-123"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = login["token"].as_str().unwrap();
    now.fetch_add(11, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        request(app, "GET", "/auth/me", Some(token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn login_throttles_and_bounds_request_body() {
    let dir = tempdir().unwrap();
    let mut config = AuthConfig::default();
    config.max_login_attempts = 1;
    config.login_window = Duration::from_secs(60);
    let app = router(AuthService::open(&dir.path().join("auth.sqlite"), config).unwrap());
    let wrong = json!({"username":"absent","password":"wrong-password"});
    assert_eq!(
        request(app.clone(), "POST", "/auth/login", None, wrong.clone())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app.clone(), "POST", "/auth/login", None, wrong)
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/auth/login",
            None,
            json!({"username":"x".repeat(129),"password":"valid-password"}),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut huge = Request::builder()
        .method("POST")
        .uri("/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(" ".repeat(20_000)))
        .unwrap();
    huge.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
    ));
    assert_eq!(
        app.oneshot(huge).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

/// A login attempt from TCP peer `peer`, optionally with `X-Forwarded-For`.
async fn login_attempt(
    app: axum::Router,
    peer: &str,
    forwarded_for: Option<&str>,
    username: &str,
) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/auth/login")
        .header("content-type", "application/json");
    if let Some(value) = forwarded_for {
        builder = builder.header("x-forwarded-for", value);
    }
    let body = json!({"username": username, "password": "wrong-password"});
    let mut request = builder.body(Body::from(body.to_string())).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    app.oneshot(request).await.unwrap().status()
}

fn one_attempt_service(dir: &std::path::Path) -> AuthService {
    let mut config = AuthConfig::default();
    config.max_login_attempts = 1;
    config.max_peer_attempts = 2;
    AuthService::open(dir.join("auth.sqlite"), config).unwrap()
}

fn proxy() -> Option<ProxyTrust> {
    Some(ProxyTrust::new(
        ProxyTrust::default_networks(),
        ForwardedHeader::XForwardedFor,
    ))
}

#[tokio::test]
async fn forwarding_headers_are_ignored_without_the_proxy_opt_in() {
    let dir = tempdir().unwrap();
    let app = router(one_attempt_service(dir.path()));
    // Even a loopback peer cannot choose its throttle key.
    let first = login_attempt(app.clone(), "127.0.0.1:1", Some("198.51.100.1"), "a").await;
    let second = login_attempt(app, "127.0.0.1:1", Some("198.51.100.2"), "a").await;
    assert_eq!(first, StatusCode::UNAUTHORIZED);
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn spoofed_forwarding_header_from_an_untrusted_peer_is_ignored() {
    let dir = tempdir().unwrap();
    let app = router_with_proxy(one_attempt_service(dir.path()), proxy());
    // A direct internet peer rotates the header to dodge the per-key limit.
    let first = login_attempt(app.clone(), "203.0.113.9:1", Some("10.9.9.1"), "a").await;
    let second = login_attempt(app.clone(), "203.0.113.9:1", Some("10.9.9.2"), "a").await;
    assert_eq!(first, StatusCode::UNAUTHORIZED);
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn throttling_keys_on_the_forwarded_address_from_a_trusted_proxy() {
    let dir = tempdir().unwrap();
    let app = router_with_proxy(one_attempt_service(dir.path()), proxy());
    let proxy_peer = "172.18.0.2:5000";
    // Same username from two real clients behind one proxy: independent.
    assert_eq!(
        login_attempt(app.clone(), proxy_peer, Some("198.51.100.1"), "a").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login_attempt(app.clone(), proxy_peer, Some("198.51.100.2"), "a").await,
        StatusCode::UNAUTHORIZED
    );
    // The same real client is throttled even though the TCP peer is shared.
    assert_eq!(
        login_attempt(app.clone(), proxy_peer, Some("198.51.100.1"), "a").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A client-supplied left-most entry does not create a fresh key.
    assert_eq!(
        login_attempt(app.clone(), proxy_peer, Some("6.6.6.6, 198.51.100.1"), "a").await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // The per-address total across usernames applies to the forwarded address.
    assert_eq!(
        login_attempt(app.clone(), proxy_peer, Some("198.51.100.3"), "b").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login_attempt(app.clone(), proxy_peer, Some("198.51.100.3"), "c").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login_attempt(app, proxy_peer, Some("198.51.100.3"), "d").await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn healthz_is_unauthenticated_and_probeable_over_tcp() {
    let dir = tempdir().unwrap();
    let app = router(service(&dir.path().join("auth.sqlite")));
    let mut request = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");

    // The container health check: a real TCP round trip on loopback.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    tokio::task::spawn_blocking(move || probe_health(address, Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn probe_health_fails_when_nothing_listens() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    assert!(probe_health(address, Duration::from_secs(1)).is_err());
}

#[tokio::test]
async fn public_signup_is_not_exposed() {
    let dir = tempdir().unwrap();
    let app = router(service(&dir.path().join("auth.sqlite")));
    assert_eq!(
        request(
            app,
            "POST",
            "/auth/signup",
            None,
            json!({"username":"new-user","password":"new-user-password"}),
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn listener_must_be_loopback() {
    assert!(trama_collab::validate_bind("127.0.0.1:9000".parse().unwrap()).is_ok());
    assert!(trama_collab::validate_bind("[::1]:9000".parse().unwrap()).is_ok());
    for external in [
        "0.0.0.0:9000",
        "[::]:9000",
        "10.0.0.5:9000",
        "192.0.2.1:9000",
    ] {
        let error = trama_collab::validate_bind(external.parse().unwrap()).unwrap_err();
        // The refusal names the explicit opt-in.
        assert!(error.contains("--behind-proxy"), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn new_database_and_new_parent_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let parent = dir.path().join("new-private-dir");
    let database = parent.join("auth.sqlite");
    let _auth = service(&database);
    assert_eq!(
        std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[tokio::test]
async fn pre_existing_world_readable_database_file_is_tightened_before_use() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.sqlite");
    std::fs::write(&path, b"").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    let auth = service(&path);
    auth.create_user("alice", "alice-secret-123").await.unwrap();
    // Hashes must never sit in a file readable by group or other.
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[tokio::test]
async fn database_permissions_are_reverified_on_every_reopen() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let path = dir.path().join("auth.sqlite");
    let auth = service(&path);
    auth.create_user("alice", "alice-secret-123").await.unwrap();
    drop(auth);
    // Simulate permission drift between runs.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let reopened = service(&path);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let app = router(reopened);
    let (status, _) = request(
        app,
        "POST",
        "/auth/login",
        None,
        json!({"username":"alice","password":"alice-secret-123"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[cfg(unix)]
#[test]
fn symlinked_database_is_rejected_without_writing_the_target() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("attacker-target.sqlite");
    std::fs::write(&target, b"attacker-owned bytes").unwrap();
    let link = dir.path().join("auth.sqlite");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(AuthService::open(&link, AuthConfig::default()).is_err());
    // No password hash or session digest was written through the symlink.
    assert_eq!(std::fs::read(&target).unwrap(), b"attacker-owned bytes");
}

#[cfg(unix)]
#[test]
fn dangling_symlink_database_is_rejected_and_not_replaced() {
    let dir = tempdir().unwrap();
    let link = dir.path().join("auth.sqlite");
    std::os::unix::fs::symlink(dir.path().join("does-not-exist.sqlite"), &link).unwrap();
    assert!(AuthService::open(&link, AuthConfig::default()).is_err());
    // The user's (dangling) symlink must not be silently replaced or followed.
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn sqlite_uri_database_strings_are_rejected() {
    assert!(
        AuthService::open(
            std::path::Path::new("file:auth.sqlite?mode=ro"),
            AuthConfig::default(),
        )
        .is_err()
    );
}

#[tokio::test]
async fn uppercase_token_variants_are_rejected_and_never_revoke_sessions() {
    let dir = tempdir().unwrap();
    let auth = service(&dir.path().join("auth.sqlite"));
    auth.create_user("alice", "alice-secret-123").await.unwrap();
    let app = router(auth);
    let (status, login) = request(
        app.clone(),
        "POST",
        "/auth/login",
        None,
        json!({"username":"alice","password":"alice-secret-123"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = login["token"].as_str().unwrap().to_owned();
    let upper = token.to_uppercase();
    assert_eq!(
        request(app.clone(), "GET", "/auth/me", Some(&upper), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // An uppercase logout attempt must not claim success for the issued token.
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/auth/logout",
            Some(&upper),
            Value::Null
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(app.clone(), "GET", "/auth/me", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::OK
    );
    // Durable revocation of the exact issued token.
    assert_eq!(
        request(
            app.clone(),
            "POST",
            "/auth/logout",
            Some(&token),
            Value::Null
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(app.clone(), "GET", "/auth/me", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // Repeated logout is a documented idempotent no-op.
    assert_eq!(
        request(app, "POST", "/auth/logout", Some(&token), Value::Null)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
}

fn injected_clock() -> (
    std::sync::Arc<std::sync::atomic::AtomicI64>,
    std::sync::Arc<dyn Fn() -> i64 + Send + Sync>,
) {
    let now = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1_700_000_000));
    let clock = now.clone();
    (
        now,
        std::sync::Arc::new(move || clock.load(std::sync::atomic::Ordering::SeqCst)),
    )
}

#[tokio::test]
async fn fresh_username_rotation_is_capped_per_peer() {
    let dir = tempdir().unwrap();
    let (_now, clock) = injected_clock();
    let config = AuthConfig {
        max_login_attempts: 100,
        max_peer_attempts: 3,
        login_window: Duration::from_secs(60),
        clock,
        ..AuthConfig::default()
    };
    // All requests come from the same peer (127.0.0.1) in the request helper.
    let app = router(AuthService::open(&dir.path().join("auth.sqlite"), config).unwrap());
    for index in 0..3 {
        let (status, _) = request(
            app.clone(),
            "POST",
            "/auth/login",
            None,
            json!({"username": format!("spray-{index}"), "password": "wrong-password"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    // A brand-new username from the same peer is still throttled.
    let (status, _) = request(
        app.clone(),
        "POST",
        "/auth/login",
        None,
        json!({"username": "spray-new", "password": "wrong-password"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn throttle_key_map_is_bounded_and_prunes_expired_keys() {
    let dir = tempdir().unwrap();
    let (now, clock) = injected_clock();
    let config = AuthConfig {
        max_login_attempts: 100,
        max_peer_attempts: 100,
        max_throttle_keys: 2,
        login_window: Duration::from_secs(60),
        clock,
        ..AuthConfig::default()
    };
    let app = router(AuthService::open(&dir.path().join("auth.sqlite"), config).unwrap());
    for name in ["first", "second"] {
        let (status, _) = request(
            app.clone(),
            "POST",
            "/auth/login",
            None,
            json!({"username": name, "password": "wrong-password"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    // Map is at its key cap with only fresh entries: a new key is refused.
    let (status, _) = request(
        app.clone(),
        "POST",
        "/auth/login",
        None,
        json!({"username": "third", "password": "wrong-password"}),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    // After the window passes, expired keys are pruned and the new key fits.
    now.fetch_add(61, std::sync::atomic::Ordering::SeqCst);
    let (status, _) = request(
        app,
        "POST",
        "/auth/login",
        None,
        json!({"username": "third", "password": "wrong-password"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn usernames_are_ascii_only_and_credentials_are_never_echoed() {
    let dir = tempdir().unwrap();
    let auth = service(&dir.path().join("auth.sqlite"));
    auth.create_user("alice", "alice-secret-123").await.unwrap();
    assert!(
        auth.create_user("bad\u{2028}name", "valid-password-123")
            .await
            .is_err()
    );
    let app = router(auth);
    // U+2028 and other non-alphabet characters (including non-ASCII and
    // control bytes) must be rejected as invalid input, never authenticated.
    for bad in [
        "line\u{2028}sep",
        "space name",
        "tab\tname",
        "tëst",
        "semi;colon",
    ] {
        let (status, body) = request(
            app.clone(),
            "POST",
            "/auth/login",
            None,
            json!({"username": bad, "password": "wrong-password"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "username {bad:?}");
        assert!(!body.to_string().contains("wrong-password"));
    }
    let (status, _) = request(
        app.clone(),
        "POST",
        "/auth/login",
        None,
        json!({"username": "a".repeat(65), "password": "wrong-password"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A wrong password must fail without echoing the submitted credential.
    let (status, body) = request(
        app,
        "POST",
        "/auth/login",
        None,
        json!({"username": "alice", "password": "another-secret-456"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!body.to_string().contains("another-secret-456"));
}

#[test]
fn read_secret_bounds_input_and_strips_trailing_newlines() {
    let secret = "a-very-secret-value";
    assert_eq!(
        trama_collab::read_secret(std::io::Cursor::new(format!("{secret}\n").into_bytes()), 64,)
            .unwrap(),
        secret
    );
    assert_eq!(
        trama_collab::read_secret(
            std::io::Cursor::new(format!("{secret}\r\n").into_bytes()),
            64,
        )
        .unwrap(),
        secret
    );
    assert_eq!(
        trama_collab::read_secret(std::io::Cursor::new(secret.as_bytes().to_vec()), 64).unwrap(),
        secret
    );
    let exact = "x".repeat(64);
    assert_eq!(
        trama_collab::read_secret(std::io::Cursor::new(exact.clone().into_bytes()), 64).unwrap(),
        exact
    );
    let too_long = "y".repeat(65);
    let error = trama_collab::read_secret(std::io::Cursor::new(too_long.clone().into_bytes()), 64)
        .unwrap_err();
    assert!(!error.contains(&too_long));
}
