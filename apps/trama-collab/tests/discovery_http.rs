use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tempfile::tempdir;
use tower::ServiceExt;
use trama_collab::{AuthConfig, AuthService, router};
use trama_collab_protocol::COLLAB_PROTOCOL_VERSION;
use trama_collab_protocol::vnext::discovery::{DISCOVERY_PATH, ServerDiscovery};

async fn fetch(app: axum::Router) -> (StatusCode, ServerDiscovery) {
    let mut request = Request::builder()
        .uri(DISCOVERY_PATH)
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn discovery_is_public_stable_and_advertises_only_v2() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("collab.sqlite");
    let first = {
        let app = router(AuthService::open(&path, AuthConfig::default()).unwrap());
        let (status, doc) = fetch(app).await;
        assert_eq!(status, StatusCode::OK);
        doc
    };
    assert!(first.server_id.starts_with("srv_"));
    assert_eq!(first.min_client_protocol, COLLAB_PROTOCOL_VERSION);
    assert_eq!(first.max_client_protocol, COLLAB_PROTOCOL_VERSION);
    assert!(first.capabilities.is_empty());
    // The id survives a restart on the same database.
    let app = router(AuthService::open(&path, AuthConfig::default()).unwrap());
    let (_, again) = fetch(app).await;
    assert_eq!(again.server_id, first.server_id);
}
