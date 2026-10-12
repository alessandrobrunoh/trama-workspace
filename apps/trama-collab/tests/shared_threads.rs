//! HTTP + WebSocket harness for shared threads, ACLs, the durable event log
//! and the runner claim protocol (ADR 0008/0009).

use std::{net::SocketAddr, time::Duration};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{self, Message, client::IntoClientRequest},
};
use tower::ServiceExt;
use trama_collab::{AuthConfig, AuthService, router};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const WAIT: Duration = Duration::from_secs(5);

struct Harness {
    app: axum::Router,
    addr: SocketAddr,
    path: std::path::PathBuf,
    _dir: TempDir,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempdir().unwrap();
        let path = dir.path().join("collab.sqlite");
        let auth = AuthService::open(
            &path,
            AuthConfig {
                session_ttl: Duration::from_secs(600),
                ..AuthConfig::default()
            },
        )
        .unwrap();
        for user in ["alice", "bob", "carol"] {
            auth.create_user(user, &format!("{user}-secret-123"))
                .await
                .unwrap();
        }
        Self::serve(auth, path, dir).await
    }

    async fn serve(auth: AuthService, path: std::path::PathBuf, dir: TempDir) -> Self {
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
            path,
            _dir: dir,
        }
    }

    async fn call(
        &self,
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
        let body = if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        };
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
        ));
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn login(&self, user: &str) -> String {
        let (status, body) = self
            .call(
                "POST",
                "/auth/login",
                None,
                json!({"username": user, "password": format!("{user}-secret-123")}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        body["token"].as_str().unwrap().to_owned()
    }

    async fn create_thread(&self, token: &str) -> String {
        let (status, body) = self
            .call("POST", "/threads", Some(token), json!({"title": "Shared"}))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["thread_id"].as_str().unwrap().to_owned()
    }

    async fn invite(&self, owner: &str, thread: &str, username: &str) {
        let (status, body) = self
            .call(
                "POST",
                &format!("/threads/{thread}/invites"),
                Some(owner),
                json!({"username": username}),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    async fn send(&self, token: &str, thread: &str, command_id: &str, text: &str) -> Value {
        let (status, body) = self
            .call(
                "POST",
                &format!("/threads/{thread}/send"),
                Some(token),
                json!({"command_id": command_id, "text": text}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn events(&self, token: &str, thread: &str) -> Vec<Value> {
        let (status, body) = self
            .call(
                "GET",
                &format!("/threads/{thread}/events?after=0&limit=500"),
                Some(token),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["events"].as_array().unwrap().clone()
    }

    async fn commands(&self, token: &str, thread: &str) -> Vec<Value> {
        let (status, body) = self
            .call(
                "GET",
                &format!("/threads/{thread}/commands"),
                Some(token),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_array().unwrap().clone()
    }

    async fn runner_token(&self, token: &str, name: &str) -> String {
        let (status, body) = self
            .call("POST", "/runners", Some(token), json!({"name": name}))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["token"].as_str().unwrap().to_owned()
    }

    async fn ws(&self, path: &str, token: &str) -> Result<Ws, tungstenite::Error> {
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        connect_async(request).await.map(|(ws, _)| ws)
    }

    /// Opens a client socket and drains the hello/backfill until `caught_up`,
    /// returning the backfilled events.
    async fn subscribe(&self, thread: &str, token: &str, after: i64) -> (Ws, Vec<Value>) {
        let mut ws = self
            .ws(&format!("/threads/{thread}/ws?after={after}"), token)
            .await
            .unwrap();
        assert_eq!(next_frame(&mut ws).await["type"], "hello");
        let mut events = Vec::new();
        loop {
            let frame = next_frame(&mut ws).await;
            match frame["type"].as_str().unwrap() {
                "event" => events.push(frame["event"].clone()),
                "caught_up" => break,
                other => panic!("unexpected frame before caught_up: {other}"),
            }
        }
        (ws, events)
    }

    async fn runner(&self, token: &str) -> Ws {
        let mut ws = self.ws("/runner/ws", token).await.unwrap();
        assert_eq!(next_frame(&mut ws).await["type"], "hello");
        ws
    }
}

fn status_of(error: tungstenite::Error) -> u16 {
    match error {
        tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP handshake rejection, got {other:?}"),
    }
}

/// Next JSON text frame; `None` when the socket closes.
async fn try_frame(ws: &mut Ws, wait: Duration) -> Option<Value> {
    loop {
        match tokio::time::timeout(wait, ws.next()).await {
            Err(_) => panic!("timed out waiting for a frame"),
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return None,
            Ok(Some(Ok(Message::Text(text)))) => return Some(serde_json::from_str(&text).unwrap()),
            Ok(Some(Ok(_))) => continue,
        }
    }
}

async fn next_frame(ws: &mut Ws) -> Value {
    try_frame(ws, WAIT).await.expect("socket closed")
}

/// Next durable event, skipping ephemeral draft/presence frames.
async fn next_event(ws: &mut Ws) -> Value {
    loop {
        let frame = next_frame(ws).await;
        if frame["type"] == "event" {
            return frame["event"].clone();
        }
    }
}

/// Next frame of the given type, skipping others.
async fn next_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        let frame = next_frame(ws).await;
        if frame["type"] == kind {
            return frame;
        }
    }
}

async fn next_event_of(ws: &mut Ws, kind: &str) -> Value {
    loop {
        let event = next_event(ws).await;
        if event["kind"] == kind {
            return event;
        }
    }
}

/// Asserts the socket reports revocation and closes without further events.
async fn expect_revoked(ws: &mut Ws) {
    loop {
        match try_frame(ws, WAIT).await {
            Some(frame) if frame["type"] == "revoked" => break,
            Some(frame) if frame["type"] == "event" => {
                // Events committed before the revocation may still be in flight.
                continue;
            }
            Some(_) => continue,
            None => panic!("socket closed without a revoked frame"),
        }
    }
    assert!(
        try_frame(ws, WAIT).await.is_none(),
        "revoked socket must close"
    );
}

async fn send_json(ws: &mut Ws, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

fn kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_owned())
        .collect()
}

fn count(events: &[Value], kind: &str, command_id: &str) -> usize {
    events
        .iter()
        .filter(|event| event["kind"] == kind && event["command_id"] == command_id)
        .count()
}

#[tokio::test]
async fn two_invited_users_exchange_messages_live_in_the_same_order() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let bob = h.login("bob").await;
    let thread = h.create_thread(&alice).await;
    h.invite(&alice, &thread, "bob").await;

    let (status, listed) = h.call("GET", "/threads", Some(&bob), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["thread_id"], thread.as_str());
    assert_eq!(listed[0]["via"], "invite");
    assert_eq!(listed[0]["owner"], "alice");

    let (mut alice_ws, alice_backfill) = h.subscribe(&thread, &alice, 0).await;
    let (mut bob_ws, bob_backfill) = h.subscribe(&thread, &bob, 0).await;
    assert_eq!(alice_backfill, bob_backfill);
    assert_eq!(kinds(&bob_backfill), ["member_joined", "member_joined"]);

    let first = h.send(&alice, &thread, "a-1", "hello bob").await;
    assert_eq!(first["state"], "pending");
    h.send(&bob, &thread, "b-1", "hi alice").await;

    for ws in [&mut alice_ws, &mut bob_ws] {
        let one = next_event(ws).await;
        let two = next_event(ws).await;
        assert_eq!(one["kind"], "message_appended");
        assert_eq!(one["author"], "alice");
        assert_eq!(one["payload"]["text"], "hello bob");
        assert_eq!(one["seq"], first["seq"]);
        assert_eq!(two["author"], "bob");
        assert_eq!(two["command_id"], "b-1");
        assert_eq!(
            two["seq"].as_i64().unwrap(),
            one["seq"].as_i64().unwrap() + 1
        );
    }
    let detail = h
        .call(
            "GET",
            &format!("/threads/{thread}"),
            Some(&bob),
            Value::Null,
        )
        .await;
    assert_eq!(detail.0, StatusCode::OK);
    assert_eq!(detail.1["members"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn reconnect_from_cursor_yields_every_event_once_in_order_while_writes_continue() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let bob = h.login("bob").await;
    let thread = h.create_thread(&alice).await;
    h.invite(&alice, &thread, "bob").await;
    for i in 0..3 {
        h.send(&alice, &thread, &format!("pre-{i}"), "before").await;
    }
    let (ws, backfill) = h.subscribe(&thread, &bob, 0).await;
    let cursor = backfill.last().unwrap()["seq"].as_i64().unwrap();
    assert_eq!(cursor, 5);
    drop(ws);
    for i in 0..3 {
        h.send(&alice, &thread, &format!("offline-{i}"), "while away")
            .await;
    }

    // Writes keep landing while bob reconnects over and over: every
    // reconnect races catch-up against live delivery, which must join
    // without a gap or a duplicate.
    const WRITES: i64 = 150;
    let writers: Vec<_> = [alice.clone(), bob.clone()]
        .into_iter()
        .enumerate()
        .map(|(writer, token)| {
            let app = h.app.clone();
            let thread = thread.clone();
            tokio::spawn(async move {
                for i in 0..WRITES / 2 {
                    let mut request = Request::builder()
                        .method("POST")
                        .uri(format!("/threads/{thread}/send"))
                        .header("content-type", "application/json")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::from(
                            json!({"command_id": format!("live-{writer}-{i}"), "text": "live"})
                                .to_string(),
                        ))
                        .unwrap();
                    request.extensions_mut().insert(ConnectInfo(
                        "127.0.0.1:43210".parse::<SocketAddr>().unwrap(),
                    ));
                    let response = app.clone().oneshot(request).await.unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                }
            })
        })
        .collect();
    let final_seq = cursor + 3 + WRITES;
    let mut seen = Vec::new();
    let mut position = cursor;
    while position < final_seq {
        let mut ws = h
            .ws(&format!("/threads/{thread}/ws?after={position}"), &bob)
            .await
            .unwrap();
        // Read a few events past the catch-up point, then drop the socket.
        let mut live_after_catch_up = 0;
        let mut caught_up = false;
        while position < final_seq && live_after_catch_up < 3 {
            let frame = next_frame(&mut ws).await;
            match frame["type"].as_str().unwrap() {
                "event" => {
                    let seq = frame["event"]["seq"].as_i64().unwrap();
                    seen.push(seq);
                    position = seq;
                    if caught_up {
                        live_after_catch_up += 1;
                    }
                }
                "caught_up" => caught_up = true,
                _ => {}
            }
        }
    }
    for writer in writers {
        writer.await.unwrap();
    }
    let expected: Vec<i64> = (cursor + 1..=final_seq).collect();
    assert_eq!(seen, expected);

    // HTTP catch-up from the same cursor agrees with the socket.
    let (status, page) = h
        .call(
            "GET",
            &format!("/threads/{thread}/events?after={cursor}&limit=2"),
            Some(&bob),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["events"][0]["seq"], cursor + 1);
    assert_eq!(page["events"][1]["seq"], cursor + 2);
    assert_eq!(page["last_seq"], final_seq);
}

#[tokio::test]
async fn uninvited_users_cannot_list_read_subscribe_or_send() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let carol = h.login("carol").await;
    let thread = h.create_thread(&alice).await;
    h.send(&alice, &thread, "secret", "private text").await;

    let (status, listed) = h.call("GET", "/threads", Some(&carol), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed, json!([]));
    for path in [
        format!("/threads/{thread}"),
        format!("/threads/{thread}/events?after=0"),
        format!("/threads/{thread}/commands"),
    ] {
        let (status, body) = h.call("GET", &path, Some(&carol), Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(!body.to_string().contains("private text"));
    }
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/send"),
            Some(&carol),
            json!({"command_id": "x", "text": "let me in"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A non-member cannot invite themselves or change access either.
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/invites"),
            Some(&carol),
            json!({"username": "carol"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = h
        .ws(&format!("/threads/{thread}/ws?after=0"), &carol)
        .await
        .unwrap_err();
    assert_eq!(status_of(error), 404);
    // Unknown threads look identical to forbidden ones.
    let (status, _) = h
        .call(
            "GET",
            "/threads/00000000000000000000000000000000",
            Some(&carol),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Anonymous access fails before any lookup.
    let error = h
        .ws(&format!("/threads/{thread}/ws?after=0"), &"0".repeat(64))
        .await
        .unwrap_err();
    assert_eq!(status_of(error), 401);
    assert_eq!(h.events(&alice, &thread).await.len(), 2);
}

#[tokio::test]
async fn revoked_member_loses_access_and_stops_receiving_events_immediately() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let bob = h.login("bob").await;
    let thread = h.create_thread(&alice).await;
    h.invite(&alice, &thread, "bob").await;
    let (mut bob_ws, _) = h.subscribe(&thread, &bob, 0).await;
    let (mut alice_ws, _) = h.subscribe(&thread, &alice, 0).await;

    // Only the owner can remove members or change access.
    let (status, _) = h
        .call(
            "DELETE",
            &format!("/threads/{thread}/members/alice"),
            Some(&bob),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = h
        .call(
            "PUT",
            &format!("/threads/{thread}/access"),
            Some(&bob),
            json!({"access": "link"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = h
        .call(
            "DELETE",
            &format!("/threads/{thread}/members/bob"),
            Some(&alice),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    expect_revoked(&mut bob_ws).await;
    let removed = next_event(&mut alice_ws).await;
    assert_eq!(removed["kind"], "member_removed");
    assert_eq!(removed["payload"]["username"], "bob");

    h.send(&alice, &thread, "after-revoke", "bob must not see this")
        .await;
    assert_eq!(
        h.call("GET", "/threads", Some(&bob), Value::Null).await.1,
        json!([])
    );
    for path in [
        format!("/threads/{thread}"),
        format!("/threads/{thread}/events?after=0"),
    ] {
        assert_eq!(
            h.call("GET", &path, Some(&bob), Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
    }
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/send"),
            Some(&bob),
            json!({"command_id": "late", "text": "hello?"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = h
        .ws(&format!("/threads/{thread}/ws?after=0"), &bob)
        .await
        .unwrap_err();
    assert_eq!(status_of(error), 404);
}

#[tokio::test]
async fn logout_closes_open_sockets() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let thread = h.create_thread(&alice).await;
    let (mut ws, _) = h.subscribe(&thread, &alice, 0).await;
    let (status, _) = h
        .call("POST", "/auth/logout", Some(&alice), Value::Null)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    expect_revoked(&mut ws).await;
}

#[tokio::test]
async fn link_access_admits_holders_and_revocation_is_immediate() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let carol = h.login("carol").await;
    let thread = h.create_thread(&alice).await;

    // No link exists while access is invite-only.
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/join"),
            Some(&carol),
            json!({"link_token": "a".repeat(64)}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, access) = h
        .call(
            "PUT",
            &format!("/threads/{thread}/access"),
            Some(&alice),
            json!({"access": "link"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let link = access["link_token"].as_str().unwrap().to_owned();
    assert!(
        !h.events(&alice, &thread)
            .await
            .iter()
            .any(|event| event.to_string().contains(&link)),
        "the link token must never enter the log"
    );

    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/join"),
            Some(&carol),
            json!({"link_token": "b".repeat(64)}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, joined) = h
        .call(
            "POST",
            &format!("/threads/{thread}/join"),
            Some(&carol),
            json!({"link_token": link}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(joined["via"], "link");
    let (mut carol_ws, _) = h.subscribe(&thread, &carol, 0).await;
    h.send(&carol, &thread, "c-1", "joined by link").await;
    assert_eq!(next_event(&mut carol_ws).await["author"], "carol");

    // Rotating the link invalidates the old token.
    let (_, rotated) = h
        .call(
            "PUT",
            &format!("/threads/{thread}/access"),
            Some(&alice),
            json!({"access": "link"}),
        )
        .await;
    let new_link = rotated["link_token"].as_str().unwrap().to_owned();
    assert_ne!(new_link, link);

    // Revoking the link removes link members and closes their sockets.
    let (status, access) = h
        .call(
            "PUT",
            &format!("/threads/{thread}/access"),
            Some(&alice),
            json!({"access": "invited"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(access["link_token"], Value::Null);
    expect_revoked(&mut carol_ws).await;
    assert_eq!(
        h.call(
            "GET",
            &format!("/threads/{thread}/events?after=0"),
            Some(&carol),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for token in [&link, &new_link] {
        let (status, _) = h
            .call(
                "POST",
                &format!("/threads/{thread}/join"),
                Some(&carol),
                json!({"link_token": token}),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn retried_command_id_yields_one_message_and_one_claim() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let thread = h.create_thread(&alice).await;
    let runner_token = h.runner_token(&alice, "alice-laptop").await;
    let mut runner = h.runner(&runner_token).await;

    let first = h.send(&alice, &thread, "turn-1", "do the thing").await;
    let retried = h.send(&alice, &thread, "turn-1", "do the thing").await;
    assert_eq!(first, retried);
    // A retry with a different body still returns the original command.
    let different = h.send(&alice, &thread, "turn-1", "something else").await;
    assert_eq!(different["seq"], first["seq"]);
    // The same id cannot be reused for another thread.
    let other = h.create_thread(&alice).await;
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{other}/send"),
            Some(&alice),
            json!({"command_id": "turn-1", "text": "x"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let available = next_of(&mut runner, "command_available").await;
    assert_eq!(available["turn"]["command_id"], "turn-1");
    assert_eq!(available["turn"]["text"], "do the thing");
    send_json(
        &mut runner,
        json!({"type": "claim", "command_id": "turn-1"}),
    )
    .await;
    assert_eq!(
        next_of(&mut runner, "claimed").await["turn"]["seq"],
        first["seq"]
    );
    // The same connection retrying its claim does not claim twice.
    send_json(
        &mut runner,
        json!({"type": "claim", "command_id": "turn-1"}),
    )
    .await;
    assert_eq!(next_frame(&mut runner).await["type"], "claimed");

    let events = h.events(&alice, &thread).await;
    assert_eq!(count(&events, "message_appended", "turn-1"), 1);
    assert_eq!(count(&events, "command_claimed", "turn-1"), 1);
    let commands = h.commands(&alice, &thread).await;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["state"], "claimed");
    assert_eq!(commands[0]["runner"], "alice-laptop");
    assert_eq!(
        h.send(&alice, &thread, "turn-1", "do the thing").await["state"],
        "claimed"
    );
}

#[tokio::test]
async fn a_runner_only_sees_and_claims_turns_authored_by_its_owner() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let bob = h.login("bob").await;
    let thread = h.create_thread(&alice).await;
    h.invite(&alice, &thread, "bob").await;
    let alice_runner_token = h.runner_token(&alice, "alice-laptop").await;
    let mut alice_runner = h.runner(&alice_runner_token).await;

    // Bob has no runner: his turn stays pending, never routed to alice.
    let bob_turn = h.send(&bob, &thread, "bob-turn", "use alice's plan").await;
    let alice_turn = h.send(&alice, &thread, "alice-turn", "mine").await;
    let available = next_of(&mut alice_runner, "command_available").await;
    assert_eq!(available["turn"]["command_id"], "alice-turn");
    assert_eq!(available["turn"]["seq"], alice_turn["seq"]);

    send_json(
        &mut alice_runner,
        json!({"type": "claim", "command_id": "bob-turn"}),
    )
    .await;
    let rejected = next_frame(&mut alice_runner).await;
    assert_eq!(rejected["type"], "claim_rejected");
    assert_eq!(rejected["reason"], "not found");
    let commands = h.commands(&bob, &thread).await;
    let bob_state = commands
        .iter()
        .find(|command| command["command_id"] == "bob-turn")
        .unwrap();
    assert_eq!(bob_state["state"], "pending");
    assert_eq!(bob_state["seq"], bob_turn["seq"]);
    assert_eq!(bob_state["author"], "bob");

    // When bob's own runner comes online it receives his pending turn.
    let bob_runner_token = h.runner_token(&bob, "bob-desktop").await;
    let mut bob_runner = h.runner(&bob_runner_token).await;
    let available = next_of(&mut bob_runner, "command_available").await;
    assert_eq!(available["turn"]["command_id"], "bob-turn");

    // Credentials are not interchangeable: a runner token is not a session,
    // a session token cannot open the runner socket, and bob cannot revoke
    // alice's runner.
    let (status, _) = h
        .call("GET", "/threads", Some(&alice_runner_token), Value::Null)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        status_of(h.ws("/runner/ws", &alice).await.unwrap_err()),
        401
    );
    let (_, runners) = h.call("GET", "/runners", Some(&alice), Value::Null).await;
    let runner_id = runners[0]["runner_id"].as_i64().unwrap();
    let (status, _) = h
        .call(
            "DELETE",
            &format!("/runners/{runner_id}"),
            Some(&bob),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h
        .call(
            "DELETE",
            &format!("/runners/{runner_id}"),
            Some(&alice),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        next_of(&mut alice_runner, "revoked").await["type"],
        "revoked"
    );
    assert_eq!(
        status_of(h.ws("/runner/ws", &alice_runner_token).await.unwrap_err()),
        401
    );
}

#[tokio::test]
async fn a_second_runner_of_the_same_user_cannot_claim_a_claimed_turn() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let thread = h.create_thread(&alice).await;
    let one = h.runner_token(&alice, "laptop").await;
    let two = h.runner_token(&alice, "desktop").await;
    let mut first = h.runner(&one).await;
    let mut second = h.runner(&two).await;
    h.send(&alice, &thread, "t", "run once").await;
    next_of(&mut first, "command_available").await;
    next_of(&mut second, "command_available").await;

    send_json(&mut first, json!({"type": "claim", "command_id": "t"})).await;
    assert_eq!(next_frame(&mut first).await["type"], "claimed");
    send_json(&mut second, json!({"type": "claim", "command_id": "t"})).await;
    let rejected = next_frame(&mut second).await;
    assert_eq!(rejected["type"], "claim_rejected");
    assert_eq!(rejected["reason"], "already claimed");
    // The other runner cannot report or finish someone else's claim.
    send_json(
        &mut second,
        json!({"type": "report", "command_id": "t", "kind": "agent_event", "payload": {}}),
    )
    .await;
    assert_eq!(next_frame(&mut second).await["type"], "error");

    // The claiming runner streams events and finishes the turn.
    send_json(
        &mut first,
        json!({"type": "report", "command_id": "t", "kind": "agent_event", "payload": {"text": "working"}}),
    )
    .await;
    assert_eq!(next_frame(&mut first).await["type"], "reported");
    send_json(
        &mut first,
        json!({"type": "report", "command_id": "t", "kind": "changes_summary", "payload": {"files": 2}}),
    )
    .await;
    assert_eq!(next_frame(&mut first).await["type"], "reported");
    send_json(
        &mut first,
        json!({"type": "finish", "command_id": "t", "outcome": "completed", "result": {"ok": true}}),
    )
    .await;
    let finished = next_frame(&mut first).await;
    assert_eq!(finished["type"], "finished");
    assert_eq!(finished["state"], "completed");
    // Finishing again is refused; the turn stays completed.
    send_json(
        &mut first,
        json!({"type": "finish", "command_id": "t", "outcome": "failed"}),
    )
    .await;
    assert_eq!(next_frame(&mut first).await["type"], "error");

    let events = h.events(&alice, &thread).await;
    assert_eq!(count(&events, "command_claimed", "t"), 1);
    assert_eq!(count(&events, "agent_event", "t"), 1);
    assert_eq!(count(&events, "changes_summary", "t"), 1);
    assert_eq!(count(&events, "command_finished", "t"), 1);
    assert_eq!(h.commands(&alice, &thread).await[0]["state"], "completed");
}

#[tokio::test]
async fn runner_lost_after_claim_leaves_the_turn_ambiguous_and_never_retries_it() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let bob = h.login("bob").await;
    let thread = h.create_thread(&alice).await;
    h.invite(&alice, &thread, "bob").await;
    let (mut bob_ws, _) = h.subscribe(&thread, &bob, 0).await;
    let token = h.runner_token(&alice, "laptop").await;
    let mut runner = h.runner(&token).await;
    h.send(&alice, &thread, "t", "side effects").await;
    send_json(&mut runner, json!({"type": "claim", "command_id": "t"})).await;
    assert_eq!(next_of(&mut runner, "claimed").await["type"], "claimed");
    drop(runner);

    let ambiguous = next_event_of(&mut bob_ws, "command_ambiguous").await;
    assert_eq!(ambiguous["command_id"], "t");
    assert_eq!(ambiguous["author"], "alice");
    assert_eq!(h.commands(&bob, &thread).await[0]["state"], "ambiguous");

    // A new runner is not offered the turn and cannot claim it.
    let mut replacement = h.runner(&token).await;
    send_json(&mut replacement, json!({"type": "ping"})).await;
    assert_eq!(next_frame(&mut replacement).await["type"], "pong");
    send_json(
        &mut replacement,
        json!({"type": "claim", "command_id": "t"}),
    )
    .await;
    let rejected = next_frame(&mut replacement).await;
    assert_eq!(rejected["type"], "claim_rejected");
    assert_eq!(rejected["reason"], "not pending");
    let events = h.events(&alice, &thread).await;
    assert_eq!(count(&events, "command_claimed", "t"), 1);
    assert_eq!(count(&events, "command_ambiguous", "t"), 1);
}

#[tokio::test]
async fn claims_left_open_by_a_restart_become_ambiguous() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("collab.sqlite");
    {
        let h = Harness::serve(
            {
                let auth = AuthService::open(&path, AuthConfig::default()).unwrap();
                auth.create_user("alice", "alice-secret-123").await.unwrap();
                auth
            },
            path.clone(),
            tempdir().unwrap(),
        )
        .await;
        let alice = h.login("alice").await;
        let thread = h.create_thread(&alice).await;
        let token = h.runner_token(&alice, "laptop").await;
        let mut runner = h.runner(&token).await;
        h.send(&alice, &thread, "t", "x").await;
        send_json(&mut runner, json!({"type": "claim", "command_id": "t"})).await;
        assert_eq!(next_of(&mut runner, "claimed").await["type"], "claimed");
        // Simulate a crash: the claim is still recorded when the service
        // reopens the database, even though no socket ever closed cleanly.
        std::mem::forget(runner);
        let reopened = Harness::serve(
            AuthService::open(&h.path, AuthConfig::default()).unwrap(),
            h.path.clone(),
            tempdir().unwrap(),
        )
        .await;
        let alice = reopened.login("alice").await;
        assert_eq!(
            reopened.commands(&alice, &thread).await[0]["state"],
            "ambiguous"
        );
    }
}

#[tokio::test]
async fn drafts_are_broadcast_last_write_wins_but_never_logged() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let bob = h.login("bob").await;
    let thread = h.create_thread(&alice).await;
    h.invite(&alice, &thread, "bob").await;
    let (mut alice_ws, _) = h.subscribe(&thread, &alice, 0).await;
    let (mut bob_ws, _) = h.subscribe(&thread, &bob, 0).await;
    let before = h.events(&alice, &thread).await.len();

    send_json(&mut bob_ws, json!({"type": "draft", "text": "first"})).await;
    send_json(
        &mut bob_ws,
        json!({"type": "draft", "text": "second draft"}),
    )
    .await;
    let mut latest = Value::Null;
    while latest["text"] != "second draft" {
        latest = next_of(&mut alice_ws, "draft").await;
        assert_eq!(latest["username"], "bob");
    }
    assert_eq!(h.events(&alice, &thread).await.len(), before);

    // A late subscriber gets the current draft snapshot.
    let (mut carol_like, _) = h.subscribe(&thread, &alice, 0).await;
    let snapshot = next_of(&mut carol_like, "draft").await;
    assert_eq!(snapshot["text"], "second draft");

    // Send without text submits the sender's trailing draft and clears it.
    let (status, receipt) = h
        .call(
            "POST",
            &format!("/threads/{thread}/send"),
            Some(&bob),
            json!({"command_id": "from-draft"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let event = next_event_of(&mut alice_ws, "message_appended").await;
    assert_eq!(event["payload"]["text"], "second draft");
    assert_eq!(event["author"], "bob");
    let cleared = next_of(&mut alice_ws, "draft").await;
    assert_eq!(cleared["text"], "");
    let events = h.events(&alice, &thread).await;
    assert_eq!(events.len(), before + 1);
    assert!(events.iter().all(|event| event["kind"] != "draft_updated"));

    // With no draft and no text there is nothing to send.
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/send"),
            Some(&bob),
            json!({"command_id": "empty"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Presence is ephemeral too.
    send_json(&mut bob_ws, json!({"type": "presence", "state": "active"})).await;
    let presence = loop {
        let frame = next_of(&mut alice_ws, "presence").await;
        if frame["username"] == "bob" && frame["state"] == "active" {
            break frame;
        }
    };
    assert_eq!(presence["state"], "active");
    drop(bob_ws);
    loop {
        let frame = next_of(&mut alice_ws, "presence").await;
        if frame["username"] == "bob" && frame["state"] == "offline" {
            break;
        }
    }
}

#[tokio::test]
async fn comments_replies_resolutions_and_verdicts_are_logged_idempotently() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let bob = h.login("bob").await;
    let thread = h.create_thread(&alice).await;
    h.invite(&alice, &thread, "bob").await;

    let (status, comment) = h
        .call(
            "POST",
            &format!("/threads/{thread}/comments"),
            Some(&bob),
            json!({"command_id": "c1", "body": "rename this", "anchor": {"file": "src/lib.rs", "line": 3}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{comment}");
    assert_eq!(comment["state"], "applied");
    let comment_seq = comment["seq"].as_i64().unwrap();
    let (_, again) = h
        .call(
            "POST",
            &format!("/threads/{thread}/comments"),
            Some(&bob),
            json!({"command_id": "c1", "body": "rename this"}),
        )
        .await;
    assert_eq!(again, comment);

    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/comments"),
            Some(&alice),
            json!({"command_id": "r1", "body": "done", "parent_seq": comment_seq}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/comments"),
            Some(&alice),
            json!({"command_id": "r2", "body": "orphan", "parent_seq": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/comments/{comment_seq}/resolve"),
            Some(&alice),
            json!({"command_id": "res1"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/verdicts"),
            Some(&bob),
            json!({"command_id": "v1", "verdict": "approved", "body": "ship it"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let events = h.events(&alice, &thread).await;
    assert_eq!(
        &kinds(&events)[2..],
        [
            "comment_created",
            "comment_replied",
            "comment_resolved",
            "review_verdict"
        ]
    );
    assert_eq!(events[3]["payload"]["parent_seq"], comment_seq);
    assert_eq!(events[5]["payload"]["verdict"], "approved");
}

#[tokio::test]
async fn message_sizes_and_identifiers_are_bounded() {
    let h = Harness::new().await;
    let alice = h.login("alice").await;
    let thread = h.create_thread(&alice).await;
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/send"),
            Some(&alice),
            json!({"command_id": "big", "text": "x".repeat(40 * 1024)}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = h
        .call(
            "POST",
            &format!("/threads/{thread}/send"),
            Some(&alice),
            json!({"command_id": "huge", "text": "x".repeat(200 * 1024)}),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    for command_id in ["", "has space", &"c".repeat(65)] {
        let (status, _) = h
            .call(
                "POST",
                &format!("/threads/{thread}/send"),
                Some(&alice),
                json!({"command_id": command_id, "text": "x"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{command_id:?}");
    }
    let (status, _) = h
        .call(
            "POST",
            "/threads",
            Some(&alice),
            json!({"title": "line\u{2028}break"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A cursor beyond the log head is rejected instead of skipping events.
    let error = h
        .ws(&format!("/threads/{thread}/ws?after=99"), &alice)
        .await
        .unwrap_err();
    assert_eq!(status_of(error), 400);
    // Oversized WebSocket drafts are refused and the socket is closed.
    let (mut ws, _) = h.subscribe(&thread, &alice, 0).await;
    send_json(
        &mut ws,
        json!({"type": "draft", "text": "x".repeat(40 * 1024)}),
    )
    .await;
    assert_eq!(next_of(&mut ws, "error").await["type"], "error");
    assert!(h.events(&alice, &thread).await.len() == 1);
}
