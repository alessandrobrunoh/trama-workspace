//! The TLS client against an in-process rustls server with a throwaway CA:
//! a valid chain connects (HTTP request and WebSocket); an unknown CA and a
//! wrong host name are rejected.

use super::http::{CollabError, Endpoint};
use super::tls;
use ::http::Method;
use futures_util::{SinkExt, StreamExt};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Ca {
    issuer: Issuer<'static, KeyPair>,
    pem: String,
}

fn ca(common_name: &str) -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    let key = KeyPair::generate().unwrap();
    let pem = params.self_signed(&key).unwrap().pem();
    Ca {
        issuer: Issuer::new(params, key),
        pem,
    }
}

/// A server certificate for `names` signed by `ca`, as a rustls config.
fn server_config(ca: &Ca, names: &[&str]) -> Arc<ServerConfig> {
    let params = CertificateParams::new(
        names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let key = KeyPair::generate().unwrap();
    let certificate = params.signed_by(&key, &ca.issuer).unwrap();
    let chain = vec![CertificateDer::from(certificate.der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap(),
    )
}

fn write_ca(dir: &tempfile::TempDir, ca: &Ca) -> PathBuf {
    let path = dir.path().join("ca.pem");
    std::fs::write(&path, &ca.pem).unwrap();
    path
}

/// What the TLS server saw: request heads (HTTP) and upgrade headers (WS).
type Seen = Arc<Mutex<Vec<String>>>;

/// A TLS listener on loopback. `websocket` upgrades and echoes one text
/// message; otherwise it answers every request with `{"username":"alice"}`.
async fn serve(config: Arc<ServerConfig>, websocket: bool) -> (SocketAddr, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let acceptor = TlsAcceptor::from(config);
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let record = record.clone();
            tokio::spawn(async move {
                // A failed handshake (the client rejected us) ends here.
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                if websocket {
                    let callback =
                        |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         response| {
                            record
                                .lock()
                                .unwrap()
                                .push(format!("{:?}", request.headers()));
                            Ok(response)
                        };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tls, callback).await
                    else {
                        return;
                    };
                    while let Some(Ok(message)) = ws.next().await {
                        if message.is_text() {
                            let _ = ws.send(message).await;
                        }
                    }
                } else {
                    let mut head = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match tls.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buffer[..n]),
                        }
                    }
                    record
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&head).into_owned());
                    let body = r#"{"username":"alice"}"#;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tls.write_all(reply.as_bytes()).await;
                    let _ = tls.shutdown().await;
                }
            });
        }
    });
    (addr, seen)
}

fn endpoint(host: &str, addr: SocketAddr, ca_file: Option<PathBuf>) -> Endpoint {
    let endpoint = Endpoint::parse(&format!("https://{host}:{}", addr.port())).unwrap();
    match ca_file {
        Some(path) => endpoint.with_ca_file(path),
        None => endpoint,
    }
}

#[tokio::test]
async fn valid_chain_connects_and_the_bearer_token_travels_inside_tls() {
    let dir = tempfile::tempdir().unwrap();
    let authority = ca("Trama test CA");
    let (addr, seen) = serve(server_config(&authority, &["localhost"]), false).await;
    let endpoint = endpoint("localhost", addr, Some(write_ca(&dir, &authority)));

    let body = endpoint
        .request(Method::GET, "/auth/me", Some(TOKEN), None)
        .await
        .unwrap();
    assert_eq!(&body[..], br#"{"username":"alice"}"#);
    let head = seen.lock().unwrap()[0].clone();
    assert!(head.starts_with("GET /auth/me HTTP/1.1"), "{head}");
    assert!(head.contains(&format!("Bearer {TOKEN}")));
}

#[tokio::test]
async fn valid_chain_connects_over_wss() {
    let dir = tempfile::tempdir().unwrap();
    let authority = ca("Trama test CA");
    let (addr, seen) = serve(server_config(&authority, &["localhost"]), true).await;
    let endpoint = endpoint("localhost", addr, Some(write_ca(&dir, &authority)));

    let mut ws = endpoint.websocket("/threads/t1/ws", TOKEN).await.unwrap();
    ws.send(Message::text("ping")).await.unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    assert_eq!(reply.into_text().unwrap().as_str(), "ping");
    assert!(seen.lock().unwrap()[0].contains("authorization"));
}

#[tokio::test]
async fn an_untrusted_ca_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let server_ca = ca("Server CA");
    let other_ca = ca("Some other CA");
    let (addr, _) = serve(server_config(&server_ca, &["localhost"]), false).await;

    // Only the system roots: the throwaway CA is unknown.
    let system_only = endpoint("localhost", addr, None);
    let error = system_only
        .request(Method::GET, "/auth/me", Some(TOKEN), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, CollabError::Transport(text) if text.contains("UnknownIssuer")),
        "{error}"
    );
    assert!(!error.to_string().contains(TOKEN));

    // A different private CA does not vouch for it either.
    let wrong_ca = endpoint("localhost", addr, Some(write_ca(&dir, &other_ca)));
    assert!(
        wrong_ca
            .request(Method::GET, "/auth/me", Some(TOKEN), None)
            .await
            .is_err()
    );
    assert!(wrong_ca.websocket("/runner/ws", TOKEN).await.is_err());
}

#[tokio::test]
async fn a_certificate_for_another_host_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let authority = ca("Trama test CA");
    // Trusted CA, valid chain, but the certificate names another host.
    let (addr, seen) = serve(server_config(&authority, &["other.example"]), false).await;
    let ca_file = write_ca(&dir, &authority);

    let by_name = endpoint("localhost", addr, Some(ca_file.clone()));
    let error = by_name
        .request(Method::GET, "/auth/me", Some(TOKEN), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, CollabError::Transport(text) if text.contains("not valid for name")),
        "{error}"
    );
    // By IP address there is no matching IP SAN either.
    let by_ip = endpoint("127.0.0.1", addr, Some(ca_file));
    assert!(
        by_ip
            .request(Method::GET, "/auth/me", Some(TOKEN), None)
            .await
            .is_err()
    );
    assert!(by_ip.websocket("/runner/ws", TOKEN).await.is_err());
    // The server never received a request, so no token reached it.
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn https_to_a_plain_http_listener_fails_the_handshake() {
    // https against a plain-HTTP listener: the handshake cannot succeed.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let _ = tcp
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    });
    let endpoint = Endpoint::parse(&format!("https://localhost:{}", addr.port())).unwrap();
    assert!(
        endpoint
            .request(Method::GET, "/auth/me", Some(TOKEN), None)
            .await
            .is_err()
    );
}

#[test]
fn ca_file_problems_are_reported_without_disabling_verification() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.pem");
    assert!(
        tls::client_config(Some(&missing))
            .unwrap_err()
            .contains("TRAMA_COLLAB_CA_FILE")
    );
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    assert!(
        tls::client_config(Some(&empty))
            .unwrap_err()
            .contains("no usable CA")
    );
    let junk = dir.path().join("junk.pem");
    std::fs::write(&junk, "not a certificate").unwrap();
    assert!(tls::client_config(Some(&junk)).is_err());
    // A valid bundle loads.
    let authority = ca("Trama test CA");
    assert!(tls::client_config(Some(&write_ca(&dir, &authority))).is_ok());
}
