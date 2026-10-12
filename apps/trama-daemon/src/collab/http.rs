//! Collab client transport: HTTP/1.1 requests and WebSocket upgrades to a
//! `trama-collab` service.
//!
//! There is no TLS client here, so only loopback `http://` endpoints are
//! accepted: bearer tokens never cross a network in clear text. Requests and
//! errors never include a credential in their text.

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use http::{Method, Request};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::net::IpAddr;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Above the service's 96 KiB frame limit, with room for envelopes.
const MAX_WS_MESSAGE_BYTES: usize = 256 * 1024;

/// Why a collab call failed. Messages are safe to show and to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CollabError {
    /// 401: the session (or runner token) is missing, expired or revoked.
    Unauthorized,
    /// 403: owner-only action.
    Forbidden,
    /// 404: no such thread, or not a member (indistinguishable on purpose).
    NotFound,
    /// 400 and 409.
    Rejected(u16, String),
    /// 413 and 429.
    Limited(u16),
    Transport(String),
}

impl std::fmt::Display for CollabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => {
                f.write_str("the collab service rejected the credential (expired or revoked)")
            }
            Self::Forbidden => f.write_str("only the thread owner can do that"),
            Self::NotFound => f.write_str("not found, or you are not a member of that thread"),
            Self::Rejected(status, reason) if reason.is_empty() => {
                write!(f, "the collab service rejected the request ({status})")
            }
            Self::Rejected(status, reason) => {
                write!(
                    f,
                    "the collab service rejected the request ({status}): {reason}"
                )
            }
            Self::Limited(413) => f.write_str("the request is too large for the collab service"),
            Self::Limited(_) => f.write_str("the collab service is rate limiting this user"),
            Self::Transport(reason) => write!(f, "collab service unreachable: {reason}"),
        }
    }
}

fn status_error(status: u16, body: &[u8]) -> CollabError {
    match status {
        401 => CollabError::Unauthorized,
        403 => CollabError::Forbidden,
        404 => CollabError::NotFound,
        413 | 429 => CollabError::Limited(status),
        _ => {
            // Handler errors are `{"error": "..."}` and never echo inputs.
            let reason = serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_default();
            CollabError::Rejected(status, reason.chars().take(200).collect())
        }
    }
}

/// A validated loopback `http://host:port` service address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    /// Normalized `http://host:port`, the identity stored with links.
    url: String,
    /// `host:port` for the `Host` header and WebSocket URLs.
    authority: String,
    /// Host for `TcpStream::connect` (no IPv6 brackets).
    connect_host: String,
    port: u16,
}

impl Endpoint {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim().trim_end_matches('/');
        let uri: http::Uri = raw
            .parse()
            .map_err(|_| "the collab server URL is not a valid URL".to_owned())?;
        if uri.scheme_str() != Some("http") {
            return Err(
                "only http:// collab servers on this machine are supported (no TLS client yet)"
                    .into(),
            );
        }
        if uri.path_and_query().is_some_and(|p| p.as_str() != "/") {
            return Err("the collab server URL must not have a path or query".into());
        }
        let authority = uri
            .authority()
            .ok_or_else(|| "the collab server URL has no host".to_owned())?;
        if authority.as_str().contains('@') {
            return Err("the collab server URL must not contain credentials".into());
        }
        let host = authority.host();
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let loopback = bare.eq_ignore_ascii_case("localhost")
            || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(
                "the collab server must be on this machine (loopback); exposing it needs TLS"
                    .into(),
            );
        }
        let port = authority.port_u16().unwrap_or(80);
        let host = host.to_ascii_lowercase();
        Ok(Self {
            url: format!("http://{host}:{port}"),
            authority: format!("{host}:{port}"),
            connect_host: bare.to_ascii_lowercase(),
            port,
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// Plain request; `Ok` for 2xx with the raw body, a typed error otherwise.
    pub(crate) async fn request(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<Bytes, CollabError> {
        let transport = |err: &dyn std::fmt::Display| CollabError::Transport(err.to_string());
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((self.connect_host.as_str(), self.port)),
        )
        .await
        .map_err(|_| CollabError::Transport("connection timed out".into()))?
        .map_err(|err| transport(&err))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|err| transport(&err))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, &self.authority);
        if let Some(token) = bearer {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let bytes = match body {
            Some(value) => {
                builder = builder.header(CONTENT_TYPE, "application/json");
                Bytes::from(serde_json::to_vec(value).map_err(|err| transport(&err))?)
            }
            None => Bytes::new(),
        };
        let request = builder
            .body(Full::new(bytes))
            .map_err(|_| CollabError::Transport("invalid request".into()))?;
        let response = tokio::time::timeout(REQUEST_TIMEOUT, async {
            let response = sender.send_request(request).await?;
            let status = response.status().as_u16();
            let body = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map(|collected| collected.to_bytes());
            Ok::<_, hyper::Error>((status, body))
        })
        .await
        .map_err(|_| CollabError::Transport("request timed out".into()))?
        .map_err(|err| transport(&err))?;
        let (status, body) = response;
        let body = body.map_err(|_| CollabError::Transport("response too large".into()))?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(status_error(status, &body))
        }
    }

    /// JSON request with a JSON reply.
    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&impl Serialize>,
    ) -> Result<T, CollabError> {
        let body = body
            .map(serde_json::to_value)
            .transpose()
            .map_err(|err| CollabError::Transport(err.to_string()))?;
        let bytes = self.request(method, path, bearer, body.as_ref()).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| CollabError::Transport("unexpected response from the service".into()))
    }

    /// JSON request whose reply body is ignored (`204`).
    pub(crate) async fn call_empty(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&impl Serialize>,
    ) -> Result<(), CollabError> {
        let body = body
            .map(serde_json::to_value)
            .transpose()
            .map_err(|err| CollabError::Transport(err.to_string()))?;
        self.request(method, path, bearer, body.as_ref())
            .await
            .map(|_| ())
    }

    /// Open a WebSocket authenticated with `bearer`.
    pub(crate) async fn websocket(&self, path: &str, bearer: &str) -> Result<Ws, CollabError> {
        let mut request = format!("ws://{}{path}", self.authority)
            .into_client_request()
            .map_err(|_| CollabError::Transport("invalid socket URL".into()))?;
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {bearer}")
                .parse()
                .map_err(|_| CollabError::Transport("invalid credential".into()))?,
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_WS_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_WS_MESSAGE_BYTES));
        let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), true);
        match tokio::time::timeout(REQUEST_TIMEOUT, connect).await {
            Err(_) => Err(CollabError::Transport("socket handshake timed out".into())),
            Ok(Ok((ws, _))) => Ok(ws),
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                let body = response.body().clone().unwrap_or_default();
                Err(status_error(response.status().as_u16(), &body))
            }
            Ok(Err(err)) => Err(CollabError::Transport(err.to_string())),
        }
    }
}

/// Identifiers that end up in URL paths: thread ids and usernames.
pub(crate) fn validate_path_segment(value: &str, what: &str) -> Result<(), String> {
    let ok = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'@'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid {what}: use 1-64 ASCII letters, digits, `.`, `_`, `-` or `@`"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_http_endpoints_are_accepted() {
        let endpoint = Endpoint::parse("http://127.0.0.1:8787/").unwrap();
        assert_eq!(endpoint.url(), "http://127.0.0.1:8787");
        assert_eq!(
            Endpoint::parse("http://LOCALHOST:9").unwrap().url(),
            "http://localhost:9"
        );
        assert_eq!(
            Endpoint::parse("http://[::1]:9").unwrap().url(),
            "http://[::1]:9"
        );
        for bad in [
            "https://127.0.0.1:8787",
            "http://example.com:8787",
            "http://10.0.0.2:8787",
            "http://user:pw@127.0.0.1:8787",
            "http://127.0.0.1:8787/api",
            "ws://127.0.0.1:8787",
            "not a url",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn path_segments_reject_traversal_and_separators() {
        assert!(validate_path_segment("01JABCD-x_y.z", "id").is_ok());
        for bad in ["", "../x", "a/b", "a?b", "a b", &"x".repeat(65)] {
            assert!(validate_path_segment(bad, "id").is_err(), "{bad}");
        }
    }
}
