//! The OpenAI-compatible client against a mock SSE server on loopback.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use trama_infra::llm::{ChatMessage, LlmError, OpenAiCompatClient};

/// Serve one connection: read the request head+body, then write `response`
/// in pieces with small pauses (to exercise chunk splitting). Returns the
/// raw request text.
async fn serve_once(chunks: Vec<String>) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request).to_string();
            if let Some(head_end) = text.find("\r\n\r\n") {
                let length = text[..head_end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + length {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        for chunk in chunks {
            socket.write_all(chunk.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = socket.shutdown().await;
        String::from_utf8_lossy(&request).to_string()
    });
    (format!("http://{addr}/v1"), handle)
}

fn chunked(parts: &[&str]) -> Vec<String> {
    let mut out = vec![
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_owned(),
    ];
    for part in parts {
        out.push(format!("{:x}\r\n{part}\r\n", part.len()));
    }
    out.push("0\r\n\r\n".to_owned());
    out
}

#[tokio::test]
async fn streams_deltas_and_usage() {
    let (base, server) = serve_once(chunked(&[
        ": ping\n\n",
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n",
        "\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    ]))
    .await;
    let client = OpenAiCompatClient::new(&base, Some("sk-test".into())).unwrap();
    let mut deltas = Vec::new();
    let completion = client
        .stream_chat(
            "gpt-test",
            &[ChatMessage::new("user", "hi")],
            |d| deltas.push(d.to_owned()),
            std::future::pending(),
        )
        .await
        .unwrap();
    assert_eq!(deltas, vec!["Hel", "lo"]);
    assert_eq!(completion.text, "Hello");
    assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
    assert_eq!(completion.usage.unwrap().input_tokens, 7);
    let request = server.await.unwrap();
    assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(request.contains("authorization: Bearer sk-test"));
    assert!(request.contains("\"stream\":true"));
    assert!(request.contains("\"model\":\"gpt-test\""));
}

#[tokio::test]
async fn unauthorized_maps_to_typed_error() {
    let body = r#"{"error":{"message":"bad key sk-test"}}"#;
    let (base, _server) = serve_once(vec![format!(
        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )])
    .await;
    let client = OpenAiCompatClient::new(&base, Some("sk-test".into())).unwrap();
    let err = client
        .stream_chat("m", &[], |_| {}, std::future::pending())
        .await
        .unwrap_err();
    assert_eq!(err, LlmError::Unauthorized);
    assert!(!err.to_string().contains("sk-test"));
}

#[tokio::test]
async fn server_error_carries_message() {
    let body = r#"{"error":{"message":"model not found"}}"#;
    let (base, _server) = serve_once(vec![format!(
        "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )])
    .await;
    let client = OpenAiCompatClient::new(&base, None).unwrap();
    let err = client
        .stream_chat("nope", &[], |_| {}, std::future::pending())
        .await
        .unwrap_err();
    assert_eq!(err, LlmError::Status(404, "model not found".into()));
}

#[tokio::test]
async fn lists_models() {
    let body = r#"{"object":"list","data":[{"id":"llama3"},{"id":"gpt-4o"}]}"#;
    let (base, server) = serve_once(vec![format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )])
    .await;
    let client = OpenAiCompatClient::new(&base, None).unwrap();
    assert_eq!(
        client.list_models().await.unwrap(),
        vec!["gpt-4o", "llama3"]
    );
    let request = server.await.unwrap();
    assert!(request.starts_with("GET /v1/models HTTP/1.1"));
    assert!(!request.to_ascii_lowercase().contains("authorization"));
}

#[tokio::test]
async fn cancel_stops_a_stalled_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let client = OpenAiCompatClient::new(&format!("http://{addr}/v1"), None).unwrap();
    let err = client
        .stream_chat(
            "m",
            &[],
            |_| {},
            tokio::time::sleep(Duration::from_millis(100)),
        )
        .await
        .unwrap_err();
    assert_eq!(err, LlmError::Cancelled);
}
