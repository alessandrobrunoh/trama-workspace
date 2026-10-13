//! `/v3/shares` members listing, signing-key bindings and object parts.

use axum::http::StatusCode;
use serde_json::{Value, json};

mod share_harness;
use share_harness::H;

const KEY_A: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
const KEY_B: &str = "AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=";

#[tokio::test]
async fn members_list_with_devices() {
    let h = H::new().await;
    let (a, b, c) = h.share().await;
    let kp = json!({"device_id": "db", "key_package": "a3A="});
    h.call("POST", "/v3/shares/sh1/key-packages", &b, kp).await;
    let (status, members) = h
        .call("GET", "/v3/shares/sh1/members", &c, Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = members
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["username"].clone())
        .collect();
    assert_eq!(names, vec![json!("alice"), json!("bob"), json!("carol")]);
    assert_eq!(members[1]["devices"], json!(["db"]));
    assert_eq!(members[0]["role"], "owner");
    let _ = a;
}

#[tokio::test]
async fn signing_keys_bind_once_per_device_and_are_listed_per_share() {
    let h = H::new().await;
    let (a, b, _c) = h.share().await;
    let bind = |key: &str| json!({"device_id": "da", "public_key": key});
    let (status, _) = h.call("POST", "/v3/signing-keys", &a, bind(KEY_A)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h.call("POST", "/v3/signing-keys", &a, bind(KEY_A)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h.call("POST", "/v3/signing-keys", &a, bind(KEY_B)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = h.call("POST", "/v3/signing-keys", &b, bind(KEY_A)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let bad = json!({"device_id": "dz", "public_key": "AAAA"});
    let (status, _) = h.call("POST", "/v3/signing-keys", &a, bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, keys) = h
        .call("GET", "/v3/shares/sh1/signing-keys", &b, Value::Null)
        .await;
    assert_eq!(
        keys,
        json!([{"username": "alice", "device_id": "da", "public_key": KEY_A}])
    );
}

#[tokio::test]
async fn object_parts_are_owner_written_immutable_and_class_gated() {
    let h = H::new().await;
    let (a, b, c) = h.share().await;
    let path = |class: &str| format!("/v3/shares/sh1/objects/obj1/parts/0?class={class}");
    let (status, _) = h.raw("PUT", &path("content"), &b, b"x".to_vec()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = h.raw("PUT", &path("content"), &a, b"cipher".to_vec()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h.raw("PUT", &path("content"), &a, b"cipher".to_vec()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = h.raw("PUT", &path("content"), &a, b"other".to_vec()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let get = "/v3/shares/sh1/objects/obj1/parts/0";
    let (status, bytes) = h.raw("GET", get, &b, Vec::new()).await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, &b"cipher"[..]));
    let (status, _) = h.raw("GET", get, &c, Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let hist = "/v3/shares/sh1/objects/h1/parts/0?class=history";
    h.raw("PUT", hist, &a, b"slice".to_vec()).await;
    let (status, _) = h
        .raw("GET", "/v3/shares/sh1/objects/h1/parts/0", &c, Vec::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .raw("GET", "/v3/shares/sh1/objects/none/parts/0", &b, Vec::new())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
