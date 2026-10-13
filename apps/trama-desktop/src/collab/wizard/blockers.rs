//! Preflight report rendering: every blocker, in plain language.

use gpui_kit::assets::IconName;
use gpui_kit::component::v_flex;
use gpui_kit::*;
use trama_domain::ids::ThreadId;
use trama_protocol::{SecretRule, ShareBlocker, SharePreflightReport};

use super::widgets::{blocker, note};
use crate::app::theme::{self, Palette};

pub fn secret_rule_label(rule: SecretRule) -> &'static str {
    match rule {
        SecretRule::DotEnvFile => "Environment file",
        SecretRule::PrivateKeyFile => "Private key file",
        SecretRule::CredentialFile => "Credentials file",
        SecretRule::PrivateKeyBlock => "Private key",
        SecretRule::AwsAccessKeyId => "AWS access key ID",
        SecretRule::AwsSecretAccessKey => "AWS secret access key",
        SecretRule::GcpServiceAccount => "GCP service account key",
        SecretRule::GcpApiKey => "Google API key",
        SecretRule::GithubToken => "GitHub token",
        SecretRule::SlackToken => "Slack token",
        SecretRule::HighEntropyString => "Possible secret (random-looking string)",
        SecretRule::TooLargeToScan => "Too large to scan for secrets",
    }
}

fn title_of(report: &SharePreflightReport, id: &ThreadId) -> String {
    report
        .threads
        .iter()
        .find(|t| &t.thread_id == id)
        .map(|t| format!("\u{201c}{}\u{201d}", t.title))
        .unwrap_or_else(|| "a thread outside the share".into())
}

/// `(title, detail)` lines for one blocker; secrets expand to one line per
/// finding, with the scanner's redacted hint (never the secret).
pub fn blocker_lines(
    report: &SharePreflightReport,
    b: &ShareBlocker,
) -> Vec<(String, Option<String>)> {
    match b {
        ShareBlocker::NotConnected => {
            vec![("Sign in to a collaboration server first.".into(), None)]
        }
        ShareBlocker::ServerUnsupported { missing } => vec![(
            "This server does not support shares yet.".into(),
            Some(format!("Missing: {}", missing.join(", "))),
        )],
        ShareBlocker::GitConflicts {
            thread_id,
            repository,
            paths,
        } => vec![(
            format!(
                "Unresolved merge conflicts in {repository} ({})",
                title_of(report, thread_id)
            ),
            Some(format!("Resolve and stage: {}", paths.join(", "))),
        )],
        ShareBlocker::Secrets { findings } => findings
            .iter()
            .map(|f| {
                let at = match f.line {
                    Some(line) => format!("{}:{line}", f.path),
                    None => f.path.clone(),
                };
                let detail = if f.hint.is_empty() {
                    at
                } else {
                    format!("{at} · {}", f.hint)
                };
                (secret_rule_label(f.rule).to_owned(), Some(detail))
            })
            .collect(),
        ShareBlocker::ScoutOutsideShare { scout, owner } => vec![(
            format!(
                "Scout {} works in a checkout outside the share",
                title_of(report, scout)
            ),
            Some(format!(
                "It uses the checkout of {}. Widen the scope to include it, or close the scout.",
                title_of(report, owner)
            )),
        )],
        ShareBlocker::Perimeter { message } => vec![(
            "The share scope could not be resolved.".into(),
            Some(message.clone()),
        )],
        ShareBlocker::CheckoutUnavailable { thread_id, message } => vec![(
            format!("Checkout of {} is unavailable", title_of(report, thread_id)),
            Some(message.clone()),
        )],
    }
}

pub fn report_view(p: &Palette, report: &SharePreflightReport) -> Div {
    let summary = format!(
        "{} thread(s), {} repository(ies), {} file(s) scanned",
        report.threads.len(),
        report.repositories.len(),
        report.scanned_files
    );
    let body = v_flex()
        .gap(theme::sp(2.))
        .child(note(p, IconName::Info, p.text_muted, summary));
    if report.ok && report.blockers.is_empty() {
        return body.child(note(
            p,
            IconName::Check,
            p.success,
            "No blockers found.".into(),
        ));
    }
    let lines: Vec<_> = report
        .blockers
        .iter()
        .flat_map(|b| blocker_lines(report, b))
        .collect();
    body.child(note(
        p,
        IconName::TriangleAlert,
        p.danger,
        format!(
            "{} problem(s) must be fixed before sharing. There is no override.",
            lines.len()
        ),
    ))
    .children(lines.into_iter().map(|(t, d)| blocker(p, t, d)))
}
