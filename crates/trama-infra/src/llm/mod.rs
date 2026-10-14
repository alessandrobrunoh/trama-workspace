//! Model-provider client: a streaming OpenAI-compatible chat-completions
//! driver (`POST {base}/chat/completions` with `stream: true`, Server-Sent
//! Events) plus `GET {base}/models` for listing models and testing a
//! connection.
//!
//! One wire format covers OpenAI, OpenRouter, Groq, Together, Ollama
//! (`http://localhost:11434/v1`), LM Studio and other compatible servers.
//! This is the *plain chat* path: no tools, no files. Agentic turns stay on
//! the Claude Code CLI (ADR 0008, ADR 0015).
//!
//! Transport policy: `https://` to any host with verified TLS. Plain
//! `http://` is accepted for loopback, and for other hosts only when no API
//! key is configured (a LAN Ollama), so a key never crosses a network in
//! clear text. Errors never include the key.

mod sse;
mod tls;

pub use sse::SseParser;

use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HOST, USER_AGENT};
use http::{Method, Request};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use rustls_pki_types::ServerName;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Until response headers arrive (a cold local model can take a while).
const HEADERS_TIMEOUT: Duration = Duration::from_secs(120);
/// Silence allowed between two body frames of a stream.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SMALL_RESPONSE: usize = 4 * 1024 * 1024;

/// Why a provider call failed. Safe to show and to log (no key inside).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmError {
    InvalidEndpoint(String),
    Unauthorized,
    Status(u16, String),
    Transport(String),
    Protocol(String),
    Cancelled,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEndpoint(reason) => write!(f, "invalid provider URL: {reason}"),
            Self::Unauthorized => f.write_str("the provider rejected the API key (401/403)"),
            Self::Status(status, reason) if reason.is_empty() => {
                write!(f, "the provider answered HTTP {status}")
            }
            Self::Status(status, reason) => {
                write!(f, "the provider answered HTTP {status}: {reason}")
            }
            Self::Transport(reason) => write!(f, "provider unreachable: {reason}"),
            Self::Protocol(reason) => write!(f, "unexpected provider response: {reason}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for LlmError {}

/// One chat message in OpenAI format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_owned(),
            content: content.into(),
        }
    }
}

/// Token counts reported by the provider, when it reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A finished streamed completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// An OpenAI-compatible endpoint: base URL (path included, e.g. `/v1`) and
/// optional bearer key.
#[derive(Clone)]
pub struct OpenAiCompatClient {
    authority: String,
    connect_host: String,
    port: u16,
    tls: bool,
    /// Base path without trailing slash (`/v1`), possibly empty.
    base_path: String,
    api_key: Option<String>,
}

impl std::fmt::Debug for OpenAiCompatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatClient")
            .field("authority", &self.authority)
            .field("tls", &self.tls)
            .field("base_path", &self.base_path)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl OpenAiCompatClient {
    /// Validate `base_url` against the transport policy.
    pub fn new(base_url: &str, api_key: Option<String>) -> Result<Self, LlmError> {
        let invalid = |reason: &str| LlmError::InvalidEndpoint(reason.to_owned());
        let api_key = api_key.filter(|key| !key.trim().is_empty());
        let raw = base_url.trim().trim_end_matches('/');
        let uri: http::Uri = raw.parse().map_err(|_| invalid("not a valid URL"))?;
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(invalid("must start with https:// or http://")),
        };
        let authority = uri.authority().ok_or_else(|| invalid("no host"))?;
        if authority.as_str().contains('@') {
            return Err(invalid("must not contain credentials"));
        }
        if uri.query().is_some() {
            return Err(invalid("must not have a query"));
        }
        let host = authority.host();
        let bare = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        if tls {
            ServerName::try_from(bare.clone()).map_err(|_| invalid("invalid host name"))?;
        } else if api_key.is_some() && !is_loopback(&bare) {
            return Err(invalid(
                "plain http:// with an API key is only allowed for a server on this machine; use https://",
            ));
        }
        let port = authority.port_u16().unwrap_or(if tls { 443 } else { 80 });
        let base_path = uri.path().trim_end_matches('/').to_owned();
        Ok(Self {
            authority: authority.as_str().to_ascii_lowercase(),
            connect_host: bare,
            port,
            tls,
            base_path,
            api_key,
        })
    }

    async fn connect(&self) -> Result<Box<dyn Io>, LlmError> {
        let tcp = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((self.connect_host.as_str(), self.port)),
        )
        .await
        .map_err(|_| LlmError::Transport("connection timed out".into()))?
        .map_err(|err| LlmError::Transport(err.to_string()))?;
        if !self.tls {
            return Ok(Box::new(tcp));
        }
        let name = ServerName::try_from(self.connect_host.clone())
            .map_err(|_| LlmError::Transport("invalid host name".into()))?;
        let config = tls::client_config().map_err(LlmError::Transport)?;
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_rustls::TlsConnector::from(config).connect(name, tcp),
        )
        .await
        .map_err(|_| LlmError::Transport("TLS handshake timed out".into()))?
        .map_err(|err| LlmError::Transport(format!("TLS handshake failed: {err}")))?;
        Ok(Box::new(stream))
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        accept: &str,
    ) -> Result<http::Response<hyper::body::Incoming>, LlmError> {
        let transport = |err: &dyn std::fmt::Display| LlmError::Transport(err.to_string());
        let stream = self.connect().await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|err| transport(&err))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base_path))
            .header(HOST, &self.authority)
            .header(ACCEPT, accept)
            .header(USER_AGENT, "TramaWorkspace");
        if let Some(key) = &self.api_key {
            builder = builder.header(AUTHORIZATION, format!("Bearer {key}"));
        }
        let bytes = match body {
            Some(body) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                Bytes::from(body)
            }
            None => Bytes::new(),
        };
        let request = builder
            .body(Full::new(bytes))
            .map_err(|_| LlmError::Transport("invalid request".into()))?;
        let response = tokio::time::timeout(HEADERS_TIMEOUT, sender.send_request(request))
            .await
            .map_err(|_| LlmError::Transport("request timed out".into()))?
            .map_err(|err| transport(&err))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let body = Limited::new(response.into_body(), 64 * 1024)
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        Err(status_error(status, &body))
    }

    /// `GET {base}/models`: model ids, sorted. Doubles as a connection test.
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let response = self
            .send(Method::GET, "/models", None, "application/json")
            .await?;
        let body = Limited::new(response.into_body(), MAX_SMALL_RESPONSE)
            .collect()
            .await
            .map_err(|_| LlmError::Protocol("response too large".into()))?
            .to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| LlmError::Protocol("models list is not JSON".into()))?;
        let mut ids: Vec<String> = value["data"]
            .as_array()
            .or_else(|| value["models"].as_array())
            .ok_or_else(|| LlmError::Protocol("models list has no `data` array".into()))?
            .iter()
            .filter_map(|m| m["id"].as_str().or_else(|| m["name"].as_str()))
            .map(str::to_owned)
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// Stream a chat completion. `on_delta` receives each text fragment as
    /// it arrives; the joined text is returned at the end. `cancel` aborts
    /// the stream when it resolves.
    pub async fn stream_chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        mut on_delta: impl FnMut(&str) + Send,
        cancel: impl std::future::Future<Output = ()> + Send,
    ) -> Result<Completion, LlmError> {
        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        let body = serde_json::to_vec(&body).map_err(|err| LlmError::Protocol(err.to_string()))?;
        tokio::pin!(cancel);
        let response = tokio::select! {
            response = self.send(Method::POST, "/chat/completions", Some(body), "text/event-stream") => response?,
            () = &mut cancel => return Err(LlmError::Cancelled),
        };
        let mut body = response.into_body();
        let mut parser = SseParser::new();
        let mut completion = Completion::default();
        let mut done = false;
        while !done {
            let frame = tokio::select! {
                frame = tokio::time::timeout(IDLE_TIMEOUT, body.frame()) => frame
                    .map_err(|_| LlmError::Transport("the stream stalled".into()))?,
                () = &mut cancel => return Err(LlmError::Cancelled),
            };
            let events = match frame {
                None => {
                    done = true;
                    parser.finish().into_iter().collect()
                }
                Some(frame) => {
                    let frame = frame.map_err(|err| LlmError::Transport(err.to_string()))?;
                    match frame.into_data() {
                        Ok(data) => parser.push(&data),
                        Err(_) => Vec::new(),
                    }
                }
            };
            for event in events {
                if event.trim() == "[DONE]" {
                    done = true;
                    break;
                }
                apply_chunk(&event, &mut completion, &mut on_delta)?;
            }
        }
        Ok(completion)
    }
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn status_error(status: u16, body: &[u8]) -> LlmError {
    if status == 401 || status == 403 {
        return LlmError::Unauthorized;
    }
    let reason = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["message"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    LlmError::Status(status, reason.chars().take(300).collect())
}

/// Fold one `chat.completion.chunk` JSON event into `completion`.
fn apply_chunk(
    event: &str,
    completion: &mut Completion,
    on_delta: &mut impl FnMut(&str),
) -> Result<(), LlmError> {
    let value: serde_json::Value = serde_json::from_str(event)
        .map_err(|_| LlmError::Protocol("stream event is not JSON".into()))?;
    if let Some(error) = value.get("error") {
        let message = error["message"]
            .as_str()
            .or_else(|| error.as_str())
            .unwrap_or("error");
        return Err(LlmError::Status(0, message.chars().take(300).collect()));
    }
    if let Some(choice) = value["choices"].get(0) {
        if let Some(text) = choice["delta"]["content"].as_str()
            && !text.is_empty()
        {
            completion.text.push_str(text);
            on_delta(text);
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            completion.finish_reason = Some(reason.to_owned());
        }
    }
    if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
        completion.usage = Some(Usage {
            input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_policy() {
        assert!(OpenAiCompatClient::new("https://api.openai.com/v1", Some("k".into())).is_ok());
        assert!(OpenAiCompatClient::new("http://localhost:11434/v1", None).is_ok());
        assert!(OpenAiCompatClient::new("http://127.0.0.1:1234/v1", Some("k".into())).is_ok());
        assert!(OpenAiCompatClient::new("http://192.168.1.5:11434/v1", None).is_ok());
        assert!(OpenAiCompatClient::new("http://example.com/v1", Some("k".into())).is_err());
        assert!(OpenAiCompatClient::new("ftp://example.com", None).is_err());
        assert!(OpenAiCompatClient::new("https://u:p@example.com", None).is_err());
        let client = OpenAiCompatClient::new("https://openrouter.ai/api/v1/", None).unwrap();
        assert_eq!(client.base_path, "/api/v1");
    }

    #[test]
    fn debug_redacts_key() {
        let client =
            OpenAiCompatClient::new("https://x.test/v1", Some("sk-secret".into())).unwrap();
        assert!(!format!("{client:?}").contains("sk-secret"));
    }

    #[test]
    fn chunk_folding() {
        let mut completion = Completion::default();
        let mut seen = String::new();
        apply_chunk(
            r#"{"choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}"#,
            &mut completion,
            &mut |t: &str| seen.push_str(t),
        )
        .unwrap();
        apply_chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
            &mut completion,
            &mut |t: &str| seen.push_str(t),
        )
        .unwrap();
        assert_eq!(seen, "Hi");
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
        assert_eq!(completion.usage.unwrap().output_tokens, 1);
    }
}
