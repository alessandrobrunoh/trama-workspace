//! Typed calls to the `/v3/shares` routes with the user's session.

use ::http::Method;
use trama_collab_protocol::vnext::api::{
    CommitManifestRequest, LeaseInfo, LeaseRequest, ManifestBlob, ManifestPart,
    PublishShareEventsRequest, PublishShareEventsResponse, RegisterShareRequest, ShareEventsPage,
    ShareInfo, ShareMemberRequest,
};
use trama_collab_protocol::vnext::api::{KeyPackageRequest, PendingKeyPackage, WelcomeBlob};
use trama_collab_protocol::vnext::events::ShareEventEnvelope;

use super::super::http::{Endpoint, Ws};
use super::super::{Connection, collab_error};
use crate::error::{DaemonError, Result};

/// Ids that go into URL paths (same rules as share event ids).
pub(crate) fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if ok {
        Ok(())
    } else {
        Err(DaemonError::Invalid("invalid share id".into()))
    }
}

#[derive(Clone)]
pub(crate) struct ShareClient {
    endpoint: Endpoint,
    token: String,
}

impl ShareClient {
    pub fn new(endpoint: Endpoint, token: String) -> Self {
        Self { endpoint, token }
    }

    pub fn from_connection(connection: &Connection) -> Result<Self> {
        Ok(Self::new(
            connection.endpoint.clone(),
            connection.session()?.to_owned(),
        ))
    }

    fn path(share_id: &str, rest: &str) -> Result<String> {
        validate_id(share_id)?;
        Ok(format!("/v3/shares/{share_id}{rest}"))
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl serde::Serialize>,
    ) -> Result<T> {
        self.endpoint
            .call(method, path, Some(&self.token), body)
            .await
            .map_err(collab_error)
    }

    async fn call_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl serde::Serialize>,
    ) -> Result<()> {
        self.endpoint
            .call_empty(method, path, Some(&self.token), body)
            .await
            .map_err(collab_error)
    }

    pub async fn register(&self, share_id: &str) -> Result<ShareInfo> {
        validate_id(share_id)?;
        let body = RegisterShareRequest {
            share_id: share_id.to_owned(),
        };
        self.call(Method::POST, "/v3/shares", Some(&body)).await
    }

    pub async fn info(&self, share_id: &str) -> Result<ShareInfo> {
        self.call(Method::GET, &Self::path(share_id, "")?, None::<&()>)
            .await
    }

    pub async fn add_member(&self, share_id: &str, grant: &ShareMemberRequest) -> Result<()> {
        self.call_empty(
            Method::POST,
            &Self::path(share_id, "/members")?,
            Some(grant),
        )
        .await
    }

    pub async fn publish(
        &self,
        share_id: &str,
        events: Vec<ShareEventEnvelope>,
    ) -> Result<PublishShareEventsResponse> {
        let body = PublishShareEventsRequest { events };
        self.call(Method::POST, &Self::path(share_id, "/events")?, Some(&body))
            .await
    }

    #[allow(dead_code)] // catch-up without a socket; tests use it
    pub async fn events(&self, share_id: &str, after: u64) -> Result<ShareEventsPage> {
        let path = Self::path(share_id, &format!("/events?after={after}"))?;
        self.call(Method::GET, &path, None::<&()>).await
    }

    pub async fn commit_manifest(
        &self,
        share_id: &str,
        request: &CommitManifestRequest,
    ) -> Result<()> {
        let path = Self::path(share_id, "/snapshots/commit")?;
        self.call_empty(Method::POST, &path, Some(request)).await
    }

    pub async fn manifest(
        &self,
        share_id: &str,
        version: u64,
        part: ManifestPart,
    ) -> Result<ManifestBlob> {
        let part = match part {
            ManifestPart::History => "history",
            ManifestPart::Content => "content",
        };
        let path = Self::path(share_id, &format!("/manifest/{version}?part={part}"))?;
        self.call(Method::GET, &path, None::<&()>).await
    }

    pub async fn acquire_lease(&self, share_id: &str, lease: &LeaseRequest) -> Result<LeaseInfo> {
        self.call(Method::POST, &Self::path(share_id, "/leases")?, Some(lease))
            .await
    }

    pub async fn release_lease(&self, share_id: &str, thread_gid: &str) -> Result<()> {
        validate_id(thread_gid)?;
        let path = Self::path(share_id, &format!("/leases/{thread_gid}/release"))?;
        self.call_empty(Method::POST, &path, None::<&()>).await
    }

    pub async fn leases(&self, share_id: &str) -> Result<Vec<LeaseInfo>> {
        self.call(Method::GET, &Self::path(share_id, "/leases")?, None::<&()>)
            .await
    }

    pub async fn stream(&self, share_id: &str, after: u64) -> Result<Ws> {
        let path = Self::path(share_id, &format!("/stream?after={after}"))?;
        self.endpoint
            .websocket(&path, &self.token)
            .await
            .map_err(collab_error)
    }

    pub async fn post_key_package(&self, share_id: &str, body: &KeyPackageRequest) -> Result<()> {
        let path = Self::path(share_id, "/key-packages")?;
        self.call_empty(Method::POST, &path, Some(body)).await
    }

    pub async fn pending_key_packages(&self, share_id: &str) -> Result<Vec<PendingKeyPackage>> {
        let path = Self::path(share_id, "/key-packages")?;
        self.call(Method::GET, &path, None::<&()>).await
    }

    pub async fn post_welcome(&self, share_id: &str, body: &WelcomeBlob) -> Result<()> {
        let path = Self::path(share_id, "/welcomes")?;
        self.call_empty(Method::POST, &path, Some(body)).await
    }

    pub async fn welcome(&self, share_id: &str, device_id: &str) -> Result<WelcomeBlob> {
        validate_id(device_id)?;
        let path = Self::path(share_id, &format!("/welcomes/{device_id}"))?;
        self.call(Method::GET, &path, None::<&()>).await
    }
}
