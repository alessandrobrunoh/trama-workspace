//! Collab server discovery (docs/plan/03 §6): fetch the public
//! `/.well-known/trama-collab` document and negotiate a capability profile
//! before any vNext share is created.
//!
//! A service that answers 404 predates discovery and is treated as collab
//! protocol v2 with no vNext capabilities. Discovery sends no credential.

use ::http::Method;
use trama_collab_protocol::COLLAB_PROTOCOL_VERSION;
use trama_collab_protocol::vnext::discovery::{
    Capability, DISCOVERY_PATH, MAX_DISCOVERY_BYTES, ServerDiscovery,
};
use trama_protocol::CollabServerInfo;

use super::CollabService;
use super::http::{CollabError, Endpoint};
use crate::error::{DaemonError, Result};

/// Collab protocol versions this daemon speaks.
pub(crate) const CLIENT_MIN_PROTOCOL: u32 = COLLAB_PROTOCOL_VERSION;
pub(crate) const CLIENT_MAX_PROTOCOL: u32 = trama_collab_protocol::vnext::VNEXT_PROTOCOL_VERSION;
/// vNext capabilities this daemon implements: the transport and MLS
/// payload encryption. The app must not offer features the client lacks.
pub(crate) const CLIENT_CAPABILITIES: &[Capability] = &[
    Capability::E2ee,
    Capability::EventStream,
    Capability::ManifestStore,
    Capability::WriterLeases,
];

/// Fetches and negotiates. Transport failures are errors; a malformed or
/// incompatible document is reported in `CollabServerInfo.error`.
pub(crate) async fn discover(endpoint: &Endpoint) -> Result<CollabServerInfo> {
    let server_url = endpoint.url().to_owned();
    let body = match endpoint
        .request(Method::GET, DISCOVERY_PATH, None, None)
        .await
    {
        Ok(body) => body,
        Err(CollabError::NotFound) => {
            return Ok(CollabServerInfo {
                server_url,
                discovery: None,
                profile: None,
                error: None,
            });
        }
        Err(err) => return Err(DaemonError::Collab(err.to_string())),
    };
    Ok(evaluate(server_url, &body))
}

pub(crate) fn evaluate(server_url: String, body: &[u8]) -> CollabServerInfo {
    let mut info = CollabServerInfo {
        server_url,
        discovery: None,
        profile: None,
        error: None,
    };
    if body.len() > MAX_DISCOVERY_BYTES {
        info.error = Some("discovery document too large".into());
        return info;
    }
    let doc: ServerDiscovery = match serde_json::from_slice(body) {
        Ok(doc) => doc,
        Err(_) => {
            info.error = Some("malformed discovery document".into());
            return info;
        }
    };
    match doc.negotiate(
        CLIENT_MIN_PROTOCOL,
        CLIENT_MAX_PROTOCOL,
        CLIENT_CAPABILITIES,
    ) {
        Ok(profile) => info.profile = Some(profile),
        Err(err) => info.error = Some(err.to_string()),
    }
    info.discovery = Some(doc);
    info
}

impl CollabService {
    /// `DiscoverCollabServer`: `server_url`, or the configured service.
    pub async fn discover(&self, server_url: Option<String>) -> Result<CollabServerInfo> {
        let endpoint = match server_url {
            Some(url) => Endpoint::parse(&url).map_err(DaemonError::Invalid)?,
            None => self.require_connection()?.endpoint,
        };
        discover(&endpoint).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_negotiates_and_reports_bad_documents() {
        let ok = evaluate(
            "u".into(),
            br#"{"server_id":"s","min_client_protocol":2,"max_client_protocol":3,"capabilities":["e2ee"]}"#,
        );
        let profile = ok.profile.unwrap();
        assert_eq!(profile.protocol, 3);
        assert!(profile.capabilities.is_empty());
        let newer = evaluate(
            "u".into(),
            br#"{"server_id":"s","min_client_protocol":4,"max_client_protocol":4}"#,
        );
        assert!(newer.profile.is_none() && newer.error.is_some() && newer.discovery.is_some());
        let junk = evaluate("u".into(), b"<html>");
        assert!(junk.discovery.is_none() && junk.error.is_some());
    }

    #[tokio::test]
    async fn discovers_a_running_collab_service() {
        use std::net::SocketAddr;
        let dir = tempfile::tempdir().unwrap();
        let auth = trama_collab::AuthService::open(
            dir.path().join("collab.sqlite"),
            trama_collab::AuthConfig::default(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = trama_collab::router(auth);
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let endpoint = Endpoint::parse(&format!("http://{addr}")).unwrap();
        let info = discover(&endpoint).await.unwrap();
        assert!(info.error.is_none(), "{:?}", info.error);
        let profile = info.profile.unwrap();
        assert_eq!(profile.protocol, 3);
        assert!(profile.supports(Capability::EventStream));
        assert!(profile.server_id.starts_with("srv_"));
    }
}
