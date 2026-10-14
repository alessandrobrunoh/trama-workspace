//! Persisted share status (`GetShareStatus`): non-fatal warnings such as
//! "published without code", the joiner's checkout restore state, and
//! `RetryShareRestore`.

use std::collections::{HashMap, HashSet};

use trama_protocol::{ClientMessage, ShareRestoreState, ShareStatusReport, ShareWarning};

/// What the UI shows for one share.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusView {
    pub warnings: Vec<ShareWarning>,
    /// `Some` when the restore failed (the reason, possibly empty).
    pub restore_failed: Option<String>,
    pub restored: bool,
    pub no_content: bool,
    pub retrying: bool,
    /// Last `RetryShareRestore` request failure.
    pub retry_error: Option<String>,
}

#[derive(Debug, Default)]
pub struct ShareStatuses {
    reports: HashMap<String, ShareStatusReport>,
    retrying: HashSet<String>,
    retry_errors: HashMap<String, String>,
}

/// Human text for a warning code; unknown codes show the daemon message.
pub fn warning_text(w: &ShareWarning) -> String {
    match w.code.as_str() {
        "content_skipped" => format!("Published without code: {}", w.message),
        _ => w.message.clone(),
    }
}

impl ShareStatuses {
    pub fn request(share_id: &str) -> ClientMessage {
        ClientMessage::GetShareStatus {
            share_id: share_id.to_owned(),
        }
    }

    pub fn apply(&mut self, report: &ShareStatusReport) {
        self.retrying.remove(&report.share_id);
        self.reports.insert(report.share_id.clone(), report.clone());
    }

    /// "Retry restore": only after a failure, one at a time.
    pub fn retry(&mut self, share_id: &str) -> Option<ClientMessage> {
        let failed = self
            .reports
            .get(share_id)
            .is_some_and(|r| r.restore == ShareRestoreState::Failed);
        if !failed || !self.retrying.insert(share_id.to_owned()) {
            return None;
        }
        self.retry_errors.remove(share_id);
        Some(ClientMessage::RetryShareRestore {
            share_id: share_id.to_owned(),
        })
    }

    pub fn fail_retry(&mut self, share_id: Option<&str>, message: String) -> bool {
        let ids: Vec<String> = self
            .retrying
            .iter()
            .filter(|id| share_id.is_none_or(|s| s == id.as_str()))
            .cloned()
            .collect();
        for id in &ids {
            self.retrying.remove(id);
            self.retry_errors.insert(id.clone(), message.clone());
        }
        !ids.is_empty()
    }

    pub fn view(&self, share_id: &str) -> StatusView {
        let Some(r) = self.reports.get(share_id) else {
            return StatusView {
                retrying: self.retrying.contains(share_id),
                retry_error: self.retry_errors.get(share_id).cloned(),
                ..Default::default()
            };
        };
        StatusView {
            warnings: r.warnings.clone(),
            restore_failed: (r.restore == ShareRestoreState::Failed)
                .then(|| r.restore_error.clone().unwrap_or_default()),
            restored: r.restore == ShareRestoreState::Restored,
            no_content: r.restore == ShareRestoreState::NoContent,
            retrying: self.retrying.contains(share_id),
            retry_error: self.retry_errors.get(share_id).cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(restore: ShareRestoreState) -> ShareStatusReport {
        ShareStatusReport {
            share_id: "s1".into(),
            restore,
            restore_error: Some("dirty index".into()),
            checkouts: Vec::new(),
            update_available: None,
            warnings: vec![ShareWarning {
                code: "content_skipped".into(),
                message: "secrets found".into(),
                at: "2026-10-13T10:00:00Z".into(),
            }],
        }
    }

    #[test]
    fn retry_only_after_a_failure_and_once_at_a_time() {
        let mut s = ShareStatuses::default();
        assert!(s.retry("s1").is_none());
        s.apply(&report(ShareRestoreState::Restored));
        assert!(s.retry("s1").is_none());
        s.apply(&report(ShareRestoreState::Failed));
        let v = s.view("s1");
        assert_eq!(v.restore_failed.as_deref(), Some("dirty index"));
        assert_eq!(
            warning_text(&v.warnings[0]),
            "Published without code: secrets found"
        );
        assert!(matches!(
            s.retry("s1"),
            Some(ClientMessage::RetryShareRestore { .. })
        ));
        assert!(s.retry("s1").is_none());
        assert!(s.view("s1").retrying);
        assert!(s.fail_retry(Some("s1"), "offline".into()));
        let v = s.view("s1");
        assert!(!v.retrying);
        assert_eq!(v.retry_error.as_deref(), Some("offline"));
        assert!(s.retry("s1").is_some());
        s.apply(&report(ShareRestoreState::Restored));
        let v = s.view("s1");
        assert!(v.restored && !v.retrying && v.restore_failed.is_none());
    }
}
