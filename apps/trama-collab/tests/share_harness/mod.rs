//! Shared harness for the `/v3/shares` HTTP tests.
#![allow(dead_code)]

use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tower::ServiceExt;
use trama_collab::{AuthConfig, AuthService, router};

pub struct H {
    pub app: axum::Router,
    pub addr: SocketAddr,
    _dir: TempDir,
}

impl H {
    pub async fn new() -> Self {
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

    pub async fn call(
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

    pub async fn login(&self, user: &str) -> String {
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
    pub async fn share(&self) -> (String, String, String) {
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

impl H {
    /// Raw-bytes request; returns status and body bytes.
    pub async fn raw(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
        ));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }
}
