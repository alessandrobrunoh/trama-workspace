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
use trama_collab_protocol::vnext::VNEXT_PROTOCOL_VERSION;
use trama_collab_protocol::vnext::discovery::{Capability, DISCOVERY_PATH, ServerDiscovery};

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
async fn discovery_is_public_stable_and_advertises_existing_v3_routes() {
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
    assert_eq!(first.max_client_protocol, VNEXT_PROTOCOL_VERSION);
    assert_eq!(
        first.capabilities,
        vec![
            Capability::E2ee,
            Capability::EventStream,
            Capability::ManifestStore,
            Capability::WriterLeases
        ]
    );
    // The id survives a restart on the same database.
    let app = router(AuthService::open(&path, AuthConfig::default()).unwrap());
    let (_, again) = fetch(app).await;
    assert_eq!(again.server_id, first.server_id);
}

#[tokio::test]
async fn operator_server_id_overrides_the_persisted_one_and_is_validated() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("collab.sqlite");
    let config = AuthConfig {
        server_id: Some("acme-collab-eu1".into()),
        ..AuthConfig::default()
    };
    let (_, doc) = fetch(router(AuthService::open(&path, config).unwrap())).await;
    assert_eq!(doc.server_id, "acme-collab-eu1");
    let bad = AuthConfig {
        server_id: Some("has space".into()),
        ..AuthConfig::default()
    };
    assert!(AuthService::open(&path, bad).is_err());
}
