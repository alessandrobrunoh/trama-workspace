//! `/v3/shares`: membership, sequenced events with replay protection,
//! manifest CAS, writer leases and the event stream.

use std::{net::SocketAddr, time::Duration};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tower::ServiceExt;
use trama_collab::{AuthConfig, AuthService, router};

struct H {
    app: axum::Router,
    addr: SocketAddr,
    _dir: TempDir,
}

impl H {
    async fn new() -> Self {
        let dir = tempdir().unwrap();
        let auth = AuthService::open(dir.path().join("c.sqlite"), AuthConfig::default()).unwrap();
        for user in ["alice", "bob", "carol"] {
            auth.create_user(user, &format!("{user}-secret-123"))
                .await
                .unwrap();
        }
        let app = router(auth);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = app.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                served.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            app,
            addr,
            _dir: dir,
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(if body.is_null() {
                Body::empty()
            } else {
                Body::from(body.to_string())
            })
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
        ));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn login(&self, user: &str) -> String {
        let mut request = Request::builder()
            .method("POST")
            .uri("/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"username": user, "password": format!("{user}-secret-123")}).to_string(),
            ))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
        ));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        body["token"].as_str().unwrap().to_owned()
    }

    /// alice owns `sh1`; bob is a writer with read_code; carol a reader.
    async fn share(&self) -> (String, String, String) {
        let (a, b, c) = (
            self.login("alice").await,
            self.login("bob").await,
            self.login("carol").await,
        );
        let (status, _) = self
            .call("POST", "/v3/shares", &a, json!({"share_id": "sh1"}))
            .await;
        assert_eq!(status, StatusCode::CREATED);
        for (user, role, code) in [("bob", "writer", true), ("carol", "reader", false)] {
            let (status, body) = self
                .call(
                    "POST",
                    "/v3/shares/sh1/members",
                    &a,
                    json!({"username": user, "role": role, "read_code": code}),
                )
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        }
        (a, b, c)
    }
}

fn event(id: &str, device: &str, seq: u64, prev: Option<&str>, kind: &str) -> Value {
    let thread = matches!(kind, "history_appended" | "thread_added").then_some("t1");
    json!({
        "event_id": id, "share_id": "sh1", "kind": kind, "thread_gid": thread,
        "author_device": device, "author_seq": seq, "prev_author_event": prev,
        "parents": [], "key_epoch": 0, "cipher_payload": "AAAA", "signature": "c2ln",
        "created_at": 0
    })
}

async fn publish(h: &H, token: &str, events: Vec<Value>) -> (StatusCode, Value) {
    h.call(
        "POST",
        "/v3/shares/sh1/events",
        token,
        json!({ "events": events }),
    )
    .await
}

#[tokio::test]
async fn events_are_sequenced_idempotent_and_acl_checked() {
    let h = H::new().await;
    let (a, b, c) = h.share().await;
    let (status, body) = publish(&h, &a, vec![event("e1", "da", 1, None, "share_created")]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["events"][0]["server_seq"], 1);
    // Retry is a duplicate with the same seq.
    let (_, body) = publish(&h, &a, vec![event("e1", "da", 1, None, "share_created")]).await;
    assert_eq!(body["events"][0]["duplicate"], true);
    // Gap and fork are rejected.
    let (status, _) = publish(
        &h,
        &a,
        vec![event("e3", "da", 3, Some("e2"), "history_appended")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = publish(
        &h,
        &a,
        vec![event("x2", "da", 2, Some("zz"), "history_appended")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Writer: no owner-only kinds, cannot author as alice's device.
    let (status, _) = publish(
        &h,
        &b,
        vec![event("b1", "db", 1, None, "membership_changed")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = publish(
        &h,
        &b,
        vec![event("b1", "da", 2, Some("e1"), "history_appended")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = publish(&h, &b, vec![event("b1", "db", 1, None, "history_appended")]).await;
    assert_eq!(status, StatusCode::OK);
    // Reader may only acknowledge.
    let (status, _) = publish(&h, &c, vec![event("c1", "dc", 1, None, "history_appended")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = publish(
        &h,
        &c,
        vec![event("c1", "dc", 1, None, "peer_acknowledged")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, page) = h
        .call("GET", "/v3/shares/sh1/events?after=1", &c, Value::Null)
        .await;
    assert_eq!(page["last_server_seq"], 3);
    let ids: Vec<_> = page["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_id"].clone())
        .collect();
    assert_eq!(ids, vec![json!("b1"), json!("c1")]);
    // Outsiders see nothing.
    let (status, _) = h
        .call("GET", "/v3/shares/nope/events", &a, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manifests_use_cas_and_content_needs_read_code() {
    let h = H::new().await;
    let (a, b, c) = h.share().await;
    let commit =
        json!({"expected_version": 0, "version": 1, "history": "aGlzdA==", "content": "Y29kZQ=="});
    let (status, _) = h
        .call(
            "POST",
            "/v3/shares/sh1/snapshots/commit",
            &c,
            commit.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = h
        .call(
            "POST",
            "/v3/shares/sh1/snapshots/commit",
            &b,
            commit.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h
        .call("POST", "/v3/shares/sh1/snapshots/commit", &a, commit)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, latest) = h
        .call("GET", "/v3/shares/sh1/manifest/0", &c, Value::Null)
        .await;
    assert_eq!(latest["version"], 1);
    assert_eq!(latest["ciphertext"], "aGlzdA==");
    let (status, _) = h
        .call(
            "GET",
            "/v3/shares/sh1/manifest/1?part=content",
            &c,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, content) = h
        .call(
            "GET",
            "/v3/shares/sh1/manifest/1?part=content",
            &b,
            Value::Null,
        )
        .await;
    assert_eq!(content["ciphertext"], "Y29kZQ==");
}

#[tokio::test]
async fn writer_lease_is_exclusive_and_guards_history() {
    let h = H::new().await;
    let (a, b, _) = h.share().await;
    let lease = |device: &str| json!({"thread_gid": "t1", "device_id": device, "ttl_secs": 60});
    let (status, _) = h
        .call("POST", "/v3/shares/sh1/leases", &a, lease("da"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .call("POST", "/v3/shares/sh1/leases", &b, lease("db"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = publish(&h, &b, vec![event("b1", "db", 1, None, "history_appended")]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = h
        .call("POST", "/v3/shares/sh1/leases/t1/release", &a, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h
        .call("POST", "/v3/shares/sh1/leases", &b, lease("db"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = h
        .call("GET", "/v3/shares/sh1/leases", &a, Value::Null)
        .await;
    assert_eq!(list[0]["holder"], "bob");
}

#[tokio::test]
async fn stream_backfills_then_delivers_live_in_order() {
    let h = H::new().await;
    let (a, b, _) = h.share().await;
    publish(&h, &a, vec![event("e1", "da", 1, None, "share_created")]).await;
    let mut request = format!("ws://{}/v3/shares/sh1/stream?after=0", h.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {b}").parse().unwrap());
    let (mut ws, _) = connect_async(request).await.unwrap();
    let mut next = async || -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let Message::Text(text) = message {
                return serde_json::from_str(&text).unwrap();
            }
        }
    };
    assert_eq!(next().await["type"], "hello");
    let first = next().await;
    assert_eq!(first["event"]["event_id"], "e1");
    publish(
        &h,
        &a,
        vec![event("e2", "da", 2, Some("e1"), "history_appended")],
    )
    .await;
    let second = next().await;
    assert_eq!(second["event"]["server_seq"], 2);
    h.call(
        "POST",
        "/v3/shares/sh1/snapshots/commit",
        &a,
        json!({"expected_version": 0, "version": 1, "history": "aA=="}),
    )
    .await;
    assert_eq!(next().await["type"], "manifest_committed");
}

#[tokio::test]
async fn key_packages_and_welcomes_flow_between_owner_and_device() {
    let h = H::new().await;
    let (a, b, c) = h.share().await;
    let kp = json!({"device_id": "db", "key_package": "a3A="});
    let (status, _) = h.call("POST", "/v3/shares/sh1/key-packages", &b, kp).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h
        .call("GET", "/v3/shares/sh1/key-packages", &b, Value::Null)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, pending) = h
        .call("GET", "/v3/shares/sh1/key-packages", &a, Value::Null)
        .await;
    assert_eq!(pending[0]["username"], "bob");
    let (status, _) = h
        .call("GET", "/v3/shares/sh1/welcomes/db", &b, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let welcome = json!({"device_id": "db", "welcome": "d2Vs", "epoch": 1});
    let (status, _) = h.call("POST", "/v3/shares/sh1/welcomes", &a, welcome).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h
        .call("GET", "/v3/shares/sh1/welcomes/db", &c, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, got) = h
        .call("GET", "/v3/shares/sh1/welcomes/db", &b, Value::Null)
        .await;
    assert_eq!(got["epoch"], 1);
    let (_, pending) = h
        .call("GET", "/v3/shares/sh1/key-packages", &a, Value::Null)
        .await;
    assert!(pending.as_array().unwrap().is_empty());
    // Carol cannot post for bob's device.
    let kp = json!({"device_id": "db", "key_package": "a3A="});
    let (status, _) = h.call("POST", "/v3/shares/sh1/key-packages", &c, kp).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn owner_revokes_a_member_and_gets_their_devices() {
    let h = H::new().await;
    let (a, b, _c) = h.share().await;
    let kp = json!({"device_id": "db", "key_package": "a3A="});
    let (status, _) = h.call("POST", "/v3/shares/sh1/key-packages", &b, kp).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h
        .call(
            "POST",
            "/v3/shares/sh1/members/alice/revoke",
            &b,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = h
        .call(
            "POST",
            "/v3/shares/sh1/members/alice/revoke",
            &a,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, got) = h
        .call("POST", "/v3/shares/sh1/members/bob/revoke", &a, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["devices"], json!(["db"]));
    let (status, _) = h.call("GET", "/v3/shares/sh1", &b, Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, pending) = h
        .call("GET", "/v3/shares/sh1/key-packages", &a, Value::Null)
        .await;
    assert!(pending.as_array().unwrap().is_empty());
    let (status, _) = h
        .call("POST", "/v3/shares/sh1/members/bob/revoke", &a, Value::Null)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
