//! Create, list and join shares (docs/plan/02 §4–§6).
//!
//! Join: a granted member's device posts an MLS key package
//! (`RequestShareJoin`); the owner adds it to the group, posts the
//! Welcome, broadcasts the commit as `membership_changed` and republishes
//! the snapshot at the new epoch (`ApproveShareJoin`); the device then
//! joins from the Welcome, verifies the snapshot and maps its threads
//! into a local workspace (`CompleteShareJoin`).

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use trama_collab_protocol::vnext::api::{KeyPackageRequest, ShareMemberRequest, WelcomeBlob};
use trama_collab_protocol::vnext::discovery::Capability;
use trama_collab_protocol::vnext::events::ShareEventKind;
use trama_db::{CollaborationServerRow, ShareRow, ShareStatus, ShareThreadRow};
use trama_domain::ids::{MessageId, ThreadId, WorkspaceId};
use trama_domain::share::{ServerId, ShareId, SharePolicy, ShareScope, ThreadGid};
use trama_domain::thread::{Thread, ThreadKind};
use trama_e2ee::ShareGroup;
use trama_protocol::{
    ShareBlocker, ShareInvite, SharePreflightReport, ShareRole, ShareSnapshot, SharedThreadKind,
};

use super::super::CollabService;
use super::super::e2ee::mls_err;

use super::outbound::Payload;
use super::snapshot::kind_str;
use crate::error::{DaemonError, Result};

/// Capabilities the flow needs from the server.
pub(crate) const REQUIRED: &[Capability] = &[
    Capability::E2ee,
    Capability::EventStream,
    Capability::ManifestStore,
    Capability::WriterLeases,
];

pub(crate) fn role_key(share_id: &str) -> String {
    format!("share_role:{share_id}")
}

fn status_of(status: ShareStatus) -> trama_protocol::ShareStatus {
    use trama_protocol::ShareStatus as S;
    match status {
        ShareStatus::Preparing => S::Preparing,
        ShareStatus::Incomplete => S::Incomplete,
        ShareStatus::Ready => S::Ready,
        ShareStatus::Paused => S::Paused,
        ShareStatus::Closed => S::Closed,
    }
}

fn thread_kind(kind: SharedThreadKind) -> ThreadKind {
    match kind {
        SharedThreadKind::Main => ThreadKind::Top,
        SharedThreadKind::Review => ThreadKind::Review,
        SharedThreadKind::Fork => ThreadKind::Fork,
        SharedThreadKind::Land => ThreadKind::Land,
        _ => ThreadKind::Sub,
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| DaemonError::Collab(format!("blocking task: {e}")))?
}

impl CollabService {
    /// Registers the connected server locally; returns its id.
    async fn bind_server(
        &self,
        client_url: &str,
    ) -> Result<std::result::Result<ServerId, Vec<String>>> {
        let connection = self.require_connection()?;
        let info = super::super::discovery::discover(&connection.endpoint).await?;
        let Some(profile) = info.profile else {
            return Ok(Err(REQUIRED.iter().map(|c| format!("{c:?}")).collect()));
        };
        let missing: Vec<String> = REQUIRED
            .iter()
            .filter(|c| !profile.supports(**c))
            .map(|c| format!("{c:?}"))
            .collect();
        if !missing.is_empty() {
            return Ok(Err(missing));
        }
        let server_id = ServerId(profile.server_id.clone());
        self.db()
            .upsert_collaboration_server(CollaborationServerRow {
                server_id: server_id.clone(),
                label: client_url.to_owned(),
                base_url: client_url.to_owned(),
                server_fingerprint: None,
                trust_policy: "user_session".into(),
                created_at: Utc::now().to_rfc3339(),
            })
            .await?;
        Ok(Ok(server_id))
    }

    pub(crate) async fn create_share(
        &self,
        thread_id: &ThreadId,
        scope: ShareScope,
        members: Vec<ShareInvite>,
    ) -> Result<std::result::Result<ShareSnapshot, SharePreflightReport>> {
        if members.iter().any(|m| m.role == ShareRole::Owner) {
            return Err(DaemonError::Invalid(
                "members cannot be granted the owner role".into(),
            ));
        }
        let e2ee = self.e2ee()?;
        let (connection, client) = self.share_client()?;
        let url = connection.endpoint.url().to_owned();
        let server_id = match self.bind_server(&url).await? {
            Ok(id) => id,
            Err(missing) => {
                let mut report = self.share_preflight(thread_id, scope).await?;
                report
                    .blockers
                    .insert(0, ShareBlocker::ServerUnsupported { missing });
                report.ok = false;
                return Ok(Err(report));
            }
        };
        let plan = self.share_plan(thread_id, scope).await?;
        if !plan.report.ok {
            return Ok(Err(plan.report));
        }
        let share_id = format!("sh_{}", MessageId::generate());
        client.register(&share_id).await?;
        let gids: Vec<(ThreadId, ThreadGid)> = plan
            .report
            .threads
            .iter()
            .map(|t| {
                (
                    t.thread_id.clone(),
                    ThreadGid(format!("tg_{}", MessageId::generate())),
                )
            })
            .collect();
        let gid_of = |id: &ThreadId| gids.iter().find(|(t, _)| t == id).map(|(_, g)| g.clone());
        let share = ShareId::from(share_id.as_str());
        let root_gid = gid_of(thread_id);
        self.db()
            .insert_share(ShareRow {
                share_id: share.clone(),
                server_id: Some(server_id),
                scope,
                policy: SharePolicy::default(),
                root_gid: root_gid.clone(),
                current_snapshot_version: 0,
                status: ShareStatus::Preparing,
                created_at: Utc::now().to_rfc3339(),
            })
            .await?;
        self.db()
            .upsert_share_threads(
                plan.report
                    .threads
                    .iter()
                    .map(|t| ShareThreadRow {
                        share_id: share.clone(),
                        thread_gid: gid_of(&t.thread_id).expect("planned"),
                        local_thread_id: Some(t.thread_id.to_string()),
                        parent_gid: t.parent_thread_id.as_ref().and_then(gid_of),
                        source_scope_root: t.is_scope_root,
                        kind: kind_str(t.kind),
                    })
                    .collect(),
            )
            .await?;
        self.db()
            .collab_setting_or_insert(&role_key(&share_id), "owner")
            .await?;
        let (owner, id) = (e2ee.clone(), share_id.clone());
        blocking(move || {
            let group = ShareGroup::create(&owner.identity, &id).map_err(mls_err)?;
            owner.cipher.insert(group).map_err(mls_err)
        })
        .await?;
        for member in &members {
            client
                .add_member(
                    &share_id,
                    &ShareMemberRequest {
                        username: member.username.clone(),
                        role: member.role,
                        read_code: member.read_code,
                    },
                )
                .await?;
        }
        self.emit_share_event(
            &client,
            &share_id,
            ShareEventKind::ShareCreated,
            None,
            Payload::Plain(b"{}".to_vec()),
        )
        .await?;
        // Marks first: the snapshot's message slices end at them.
        self.init_history_marks(&share_id).await?;
        self.publish_history_snapshot(&client, &share_id, false)
            .await?;
        self.db()
            .set_share_status(&share, ShareStatus::Ready)
            .await?;
        self.ensure_share_stream(&share_id, connection.generation);
        Ok(Ok(self.share_snapshot(&share_id).await?))
    }

    pub(crate) async fn share_snapshot(&self, share_id: &str) -> Result<ShareSnapshot> {
        let share = ShareId::from(share_id);
        let row = self
            .db()
            .share(&share)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("share {share_id}")))?;
        let role = self
            .db()
            .collab_setting_or_insert(&role_key(share_id), "reader")
            .await?;
        let role =
            serde_json::from_value(serde_json::Value::String(role)).unwrap_or(ShareRole::Reader);
        let servers = self.db().collaboration_servers().await?;
        let server_url = row
            .server_id
            .as_ref()
            .and_then(|id| servers.iter().find(|s| &s.server_id == id))
            .map(|s| s.base_url.clone());
        let root_thread_id = match &row.root_gid {
            Some(gid) => self.local_thread(&share, Some(&gid.0)).await?,
            None => None,
        };
        let key_epoch = self.share_epoch(share_id).await.ok();
        Ok(ShareSnapshot {
            share_id: share_id.to_owned(),
            scope: row.scope,
            status: status_of(row.status),
            role,
            server_url,
            root_thread_id,
            snapshot_version: row.current_snapshot_version,
            key_epoch,
        })
    }

    pub(crate) async fn list_shares(&self) -> Result<Vec<ShareSnapshot>> {
        let mut out = Vec::new();
        for row in self.db().shares().await? {
            out.push(self.share_snapshot(&row.share_id.0).await?);
        }
        Ok(out)
    }

    /// Joiner: publishes this device's key package. Returns the device id.
    pub(crate) async fn request_share_join(&self, share_id: &str) -> Result<String> {
        let e2ee = self.e2ee()?;
        let (_, client) = self.share_client()?;
        client.info(share_id).await?; // must be granted
        let (joiner, id) = (e2ee.clone(), share_id.to_owned());
        let key_package = blocking(move || joiner.begin_join(&id)).await?;
        let device_id = e2ee.device_id().to_owned();
        client
            .post_key_package(
                share_id,
                &KeyPackageRequest {
                    device_id: device_id.clone(),
                    key_package: STANDARD.encode(key_package),
                },
            )
            .await?;
        Ok(device_id)
    }

    pub(crate) async fn list_share_join_requests(
        &self,
        share_id: &str,
    ) -> Result<Vec<trama_protocol::ShareJoinRequest>> {
        let (_, client) = self.share_client()?;
        Ok(client
            .pending_key_packages(share_id)
            .await?
            .into_iter()
            .map(|p| trama_protocol::ShareJoinRequest {
                username: p.username,
                device_id: p.device_id,
            })
            .collect())
    }

    /// Owner: admits `device_id`; returns the new epoch.
    pub(crate) async fn approve_share_join(&self, share_id: &str, device_id: &str) -> Result<u64> {
        let e2ee = self.e2ee()?;
        let (_, client) = self.share_client()?;
        let pending = client.pending_key_packages(share_id).await?;
        let request = pending
            .into_iter()
            .find(|p| p.device_id == device_id)
            .ok_or_else(|| DaemonError::NotFound(format!("join request of {device_id}")))?;
        let key_package = STANDARD
            .decode(&request.key_package)
            .map_err(|_| DaemonError::Collab("key package is not base64".into()))?;
        let (owner, id) = (e2ee.clone(), share_id.to_owned());
        let change = blocking(move || {
            owner
                .cipher
                .with_group(&id, |g, me| g.add_members(me, &[&key_package]))
                .map_err(mls_err)
        })
        .await?;
        let welcome = change
            .welcome
            .ok_or_else(|| DaemonError::Collab("MLS add produced no Welcome".into()))?;
        // The group already moved: Welcome, commit and republish are queued
        // and retried until delivered (no rollback).
        self.enqueue_welcome(
            share_id,
            &WelcomeBlob {
                device_id: device_id.to_owned(),
                welcome: STANDARD.encode(welcome),
                epoch: change.epoch,
            },
        )
        .await?;
        self.emit_share_event(
            &client,
            share_id,
            ShareEventKind::MembershipChanged,
            None,
            Payload::Raw {
                bytes: change.commit,
                epoch: change.epoch,
            },
        )
        .await?;
        // Snapshots sealed at older epochs are unreadable for the new device.
        self.enqueue_snapshot(share_id).await?;
        self.flush_share_ops(Some(share_id)).await;
        Ok(change.epoch)
    }

    /// Joiner: joins the MLS group from the Welcome, verifies the snapshot
    /// and creates the local threads in `workspace_id`.
    pub(crate) async fn complete_share_join(
        &self,
        share_id: &str,
        workspace_id: &WorkspaceId,
    ) -> Result<ShareSnapshot> {
        let e2ee = self.e2ee()?;
        let (connection, client) = self.share_client()?;
        let share = ShareId::from(share_id);
        if self.db().share(&share).await?.is_some() {
            return Err(DaemonError::Invalid("this share is already joined".into()));
        }
        let url = connection.endpoint.url().to_owned();
        let server_id = self.bind_server(&url).await?.map_err(|missing| {
            DaemonError::Collab(format!("server lacks {}", missing.join(", ")))
        })?;
        let welcome = client.welcome(share_id, e2ee.device_id()).await?;
        let bytes = STANDARD
            .decode(&welcome.welcome)
            .map_err(|_| DaemonError::Collab("welcome is not base64".into()))?;
        let (joiner, id) = (e2ee.clone(), share_id.to_owned());
        blocking(move || joiner.finish_join(&id, &bytes)).await?;
        let info = client.info(share_id).await?;
        let snapshot = self.read_verified_snapshot(&client, share_id).await?;
        let manifest = &snapshot.manifest;
        let mut rows = Vec::new();
        let mut local: Vec<(ThreadGid, ThreadId)> = Vec::new();
        for t in &manifest.threads {
            let parent = t
                .parent_gid
                .as_ref()
                .and_then(|p| local.iter().find(|(g, _)| g == p).map(|(_, id)| id.clone()));
            let thread = Thread {
                id: ThreadId::generate(),
                workspace_id: Some(workspace_id.clone()),
                parent_thread_id: parent,
                kind: if t.parent_gid.is_none() {
                    ThreadKind::Top
                } else {
                    thread_kind(t.kind)
                },
                title: Some(t.title.clone()).filter(|s| !s.is_empty()),
                repo_attachments: Vec::new(),
            };
            self.db().create_thread(&thread).await?;
            if let Some(slice) = &t.message_slice {
                let publisher = &manifest.publisher_device_id.0;
                if let Err(e) = self
                    .import_slice(&client, share_id, slice, &thread.id, publisher)
                    .await
                {
                    crate::log::warn(format!(
                        "share {share_id}: pre-share history not imported: {e}"
                    ));
                }
            }
            local.push((t.thread_gid.clone(), thread.id.clone()));
            rows.push(ShareThreadRow {
                share_id: share.clone(),
                thread_gid: t.thread_gid.clone(),
                local_thread_id: Some(thread.id.to_string()),
                parent_gid: t.parent_gid.clone(),
                source_scope_root: manifest.root_thread_gids.contains(&t.thread_gid),
                kind: kind_str(t.kind),
            });
        }
        self.db()
            .insert_share(ShareRow {
                share_id: share.clone(),
                server_id: Some(server_id),
                scope: manifest.scope,
                policy: SharePolicy::default(),
                root_gid: manifest.root_thread_gids.first().cloned(),
                current_snapshot_version: manifest.snapshot_version,
                status: ShareStatus::Ready,
                created_at: Utc::now().to_rfc3339(),
            })
            .await?;
        self.db().upsert_share_threads(rows).await?;
        if info.read_code {
            // History stays usable when code cannot be restored.
            self.run_share_restore(&client, share_id, &snapshot).await?;
        }
        let role = serde_json::to_value(info.role)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "reader".into());
        self.db()
            .collab_setting_or_insert(&role_key(share_id), &role)
            .await?;
        self.init_history_marks(share_id).await?;
        self.ensure_share_stream(share_id, connection.generation);
        self.share_snapshot(share_id).await
    }

    pub(crate) async fn publish_share_snapshot(&self, share_id: &str) -> Result<u64> {
        let (_, client) = self.share_client()?;
        if client.info(share_id).await?.role != ShareRole::Owner {
            return Err(DaemonError::Invalid(
                "only the share owner publishes snapshots".into(),
            ));
        }
        if !self.flush_share_ops(Some(share_id)).await {
            return Err(DaemonError::Collab(
                "earlier membership changes of this share are still being delivered".into(),
            ));
        }
        self.publish_history_snapshot(&client, share_id, false)
            .await
    }
}
