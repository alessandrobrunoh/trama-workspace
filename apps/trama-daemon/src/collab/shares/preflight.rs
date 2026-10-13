//! Share preflight (docs/plan/02 §4, plan 06): perimeter with scouts
//! outside the share refused, Git index conflicts and secrets in every
//! file and Git object to export. Every blocker blocks; nothing is
//! written anywhere.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use trama_domain::ids::ThreadId;
use trama_domain::secrets::SecretScanReport;
use trama_domain::share::{
    FamilyNode, OutOfScopeAlias, PerimeterError, ShareScope, SharedThreadKind, ThreadGid,
    resolve_perimeter,
};
use trama_domain::thread::{Thread, ThreadKind};
use trama_protocol::{ShareBlocker, SharePreflightReport, SharePreflightThread};

use super::super::CollabService;
use super::scan;
use crate::error::{DaemonError, Result};

/// A checkout the share would export.
pub(crate) struct PlannedCheckout {
    pub thread_id: ThreadId,
    pub repository: String,
    pub path: PathBuf,
}

pub(crate) struct Plan {
    pub report: SharePreflightReport,
    /// Consumed by checkout export (content manifests), deferred.
    #[allow(dead_code)]
    pub checkouts: Vec<PlannedCheckout>,
}

fn kind_of(thread: &Thread, scout: bool) -> SharedThreadKind {
    if scout {
        return SharedThreadKind::Scout;
    }
    match thread.kind {
        ThreadKind::Top => SharedThreadKind::Main,
        ThreadKind::Sub => SharedThreadKind::Child,
        ThreadKind::Review => SharedThreadKind::Review,
        ThreadKind::Fork => SharedThreadKind::Fork,
        ThreadKind::Land => SharedThreadKind::Land,
    }
}

fn gid(id: &ThreadId) -> ThreadGid {
    ThreadGid(id.to_string())
}

impl CollabService {
    pub(crate) async fn share_preflight(
        &self,
        thread_id: &ThreadId,
        scope: ShareScope,
    ) -> Result<SharePreflightReport> {
        Ok(self.share_plan(thread_id, scope).await?.report)
    }

    pub(crate) async fn share_plan(&self, thread_id: &ThreadId, scope: ShareScope) -> Result<Plan> {
        if scope == ShareScope::Workspace {
            return Err(DaemonError::Invalid(
                "workspace scope is not supported by the share flow yet".into(),
            ));
        }
        if self.db().get_thread(thread_id).await?.is_none() {
            return Err(DaemonError::NotFound(format!("thread {thread_id}")));
        }
        let root = self
            .family_root(thread_id)
            .await
            .unwrap_or_else(|| thread_id.clone());
        let family = self.family_threads(&root).await?;
        // Checkouts per thread; an attachment whose checkout another
        // thread owns makes this thread a scout of that owner.
        let mut checkouts: HashMap<ThreadId, Vec<PlannedCheckout>> = HashMap::new();
        let mut alias: HashMap<ThreadId, ThreadId> = HashMap::new();
        let mut unavailable = Vec::new();
        for thread in &family {
            let repos = self.db().list_thread_repositories(&thread.id).await?;
            for attachment in &thread.repo_attachments {
                let Some(checkout) = &attachment.checkout_id else {
                    continue;
                };
                let owner = self.db().checkout_owner(checkout).await?;
                if let Some(owner) = owner.filter(|o| o != &thread.id) {
                    alias.insert(thread.id.clone(), owner);
                    continue;
                }
                let name = repos
                    .iter()
                    .find(|r| r.id == attachment.repository_id)
                    .and_then(|r| r.canonical_remote.clone())
                    .unwrap_or_else(|| attachment.repository_id.to_string());
                match self
                    .db()
                    .checkout_location(&thread.id, &attachment.repository_id, checkout)
                    .await
                {
                    Ok(Some(path)) => {
                        checkouts
                            .entry(thread.id.clone())
                            .or_default()
                            .push(PlannedCheckout {
                                thread_id: thread.id.clone(),
                                repository: name,
                                path,
                            })
                    }
                    Ok(None) => unavailable.push(ShareBlocker::CheckoutUnavailable {
                        thread_id: thread.id.clone(),
                        message: format!("{name}: checkout location unknown"),
                    }),
                    Err(e) => unavailable.push(ShareBlocker::CheckoutUnavailable {
                        thread_id: thread.id.clone(),
                        message: format!("{name}: {e}"),
                    }),
                }
            }
        }
        let nodes: Vec<FamilyNode> = family
            .iter()
            .map(|t| FamilyNode {
                gid: gid(&t.id),
                parent: t.parent_thread_id.as_ref().map(gid),
                kind: kind_of(t, alias.contains_key(&t.id)),
                checkout_alias_of: alias.get(&t.id).map(gid),
            })
            .collect();
        let mut report = SharePreflightReport {
            thread_id: thread_id.clone(),
            scope,
            ok: false,
            threads: Vec::new(),
            repositories: Vec::new(),
            scanned_files: 0,
            blockers: Vec::new(),
        };
        let perimeter = match resolve_perimeter(
            &nodes,
            scope,
            &[gid(thread_id)],
            &BTreeSet::new(),
            Some(OutOfScopeAlias::Refuse),
        ) {
            Ok(p) => p,
            Err(PerimeterError::AliasOutsideScope { scout, owner }) => {
                report.blockers.push(ShareBlocker::ScoutOutsideShare {
                    scout: ThreadId::from(scout.0),
                    owner: ThreadId::from(owner.0),
                });
                return Ok(Plan {
                    report,
                    checkouts: Vec::new(),
                });
            }
            Err(other) => {
                report.blockers.push(ShareBlocker::Perimeter {
                    message: format!("{other:?}"),
                });
                return Ok(Plan {
                    report,
                    checkouts: Vec::new(),
                });
            }
        };
        let by_id: HashMap<String, &Thread> =
            family.iter().map(|t| (t.id.to_string(), t)).collect();
        let mut planned = Vec::new();
        for member in &perimeter.threads {
            let thread = by_id[&member.gid.0];
            report.threads.push(SharePreflightThread {
                thread_id: thread.id.clone(),
                title: thread.title.clone().unwrap_or_default(),
                kind: member.kind,
                parent_thread_id: member.parent.as_ref().map(|g| ThreadId::from(g.0.clone())),
                is_scope_root: member.is_scope_root,
            });
            planned.extend(checkouts.remove(&thread.id).unwrap_or_default());
        }
        let inside: BTreeSet<ThreadId> =
            report.threads.iter().map(|t| t.thread_id.clone()).collect();
        report.blockers.extend(unavailable.into_iter().filter(|b| {
            matches!(b, ShareBlocker::CheckoutUnavailable { thread_id, .. } if inside.contains(thread_id))
        }));
        let (blockers, scanned) = inspect(&planned).await?;
        report.blockers.extend(blockers);
        report.scanned_files = scanned;
        report.repositories = planned
            .iter()
            .map(|c| c.repository.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        report.ok = report.blockers.is_empty();
        Ok(Plan {
            report,
            checkouts: planned,
        })
    }
}

/// Conflicts and secrets of every planned checkout (blocking git work).
async fn inspect(checkouts: &[PlannedCheckout]) -> Result<(Vec<ShareBlocker>, u32)> {
    let jobs: Vec<(ThreadId, String, PathBuf)> = checkouts
        .iter()
        .map(|c| (c.thread_id.clone(), c.repository.clone(), c.path.clone()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut blockers = Vec::new();
        let mut report = SecretScanReport::default();
        let mut scanned = 0u32;
        for (thread_id, repository, path) in jobs {
            match trama_infra::git::conflicts::unmerged_paths(&path) {
                Ok(paths) if !paths.is_empty() => blockers.push(ShareBlocker::GitConflicts {
                    thread_id: thread_id.clone(),
                    repository: repository.clone(),
                    paths,
                }),
                Ok(_) => {}
                Err(e) => {
                    blockers.push(ShareBlocker::CheckoutUnavailable {
                        thread_id,
                        message: format!("{repository}: {e}"),
                    });
                    continue;
                }
            }
            match scan::scan_checkout(&path, &mut report) {
                Ok(n) => scanned += n,
                Err(message) => blockers.push(ShareBlocker::CheckoutUnavailable {
                    thread_id,
                    message: format!("{repository}: {message}"),
                }),
            }
        }
        if report.blocks_share() {
            blockers.push(ShareBlocker::Secrets {
                findings: report.findings,
            });
        }
        (blockers, scanned)
    })
    .await
    .map_err(|e| DaemonError::Collab(format!("preflight task: {e}")))
}
