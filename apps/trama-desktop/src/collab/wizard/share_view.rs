//! GPUI view of the Share wizard. Commands are emitted as events for the
//! owner (workspace/collab store) to dispatch to the daemon.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::secrets::SecretRule;
use trama_domain::share::{Capability, CodeAccessMode, ShareRole, ShareScope, ShareStatus};

use super::model::{PreflightReport, WizardCommand};
use super::share_flow::{INVITEE_ROLES, ShareFlow, ShareStep};
use super::widgets::{blocker, choice, note, section};
use crate::app::theme::{self, Palette};

pub struct ShareWizard {
    pub flow: Option<ShareFlow>,
}

impl EventEmitter<WizardCommand> for ShareWizard {}

impl ShareWizard {
    pub fn new() -> Self {
        Self { flow: None }
    }

    /// Runs `f` on the flow and emits whatever commands it produced.
    pub fn update_flow(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut ShareFlow, &mut Vec<WizardCommand>),
    ) {
        let Some(flow) = self.flow.as_mut() else {
            return;
        };
        let mut out = Vec::new();
        f(flow, &mut out);
        for command in out {
            cx.emit(command);
        }
        cx.notify();
    }
}

pub fn scope_text(scope: ShareScope) -> (&'static str, &'static str) {
    match scope {
        ShareScope::ThreadOnly => ("This thread only", "No parent, siblings or children."),
        ShareScope::Subtree => (
            "Thread and descendants",
            "Every child thread, at any depth.",
        ),
        ShareScope::Workspace => (
            "Workspace roots",
            "The selected roots of the workspace and their descendants.",
        ),
    }
}

pub fn role_text(role: ShareRole) -> (&'static str, &'static str) {
    match role {
        ShareRole::Owner => ("Owner", "Full control."),
        ShareRole::Editor => ("Editor", "Reads, runs turns, comments and reviews."),
        ShareRole::Reviewer => ("Reviewer", "Reads, comments and reviews."),
        ShareRole::Reader => ("Reader", "Reads the conversation history only."),
    }
}

pub fn capability_label(cap: Capability) -> &'static str {
    match cap {
        Capability::ReadHistory => "Read history",
        Capability::ReadCode => "Receive code",
        Capability::PublishTurn => "Run turns",
        Capability::Comment => "Comment",
        Capability::Review => "Review",
        Capability::Export => "Export",
        Capability::ManageShare => "Manage share",
    }
}

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
        SecretRule::TooLargeToScan => "Too large to scan",
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The checks summary; blockers are listed in full, grouped by kind.
pub fn report_view(p: &Palette, report: &PreflightReport) -> Div {
    let summary = format!(
        "{} thread(s), {} repo(s), {}",
        report.threads,
        report.repos,
        human_bytes(report.bytes)
    );
    let mut body =
        v_flex()
            .gap(theme::sp(2.))
            .child(note(p, IconName::Info, p.text_muted, summary));
    if !report.blocks() {
        return body.child(note(
            p,
            IconName::Check,
            p.success,
            "No blockers found.".into(),
        ));
    }
    body = body.child(note(
        p,
        IconName::TriangleAlert,
        p.danger,
        format!(
            "{} blocker(s) must be fixed before sharing.",
            report.blocker_count()
        ),
    ));
    for c in &report.conflicts {
        body = body.child(blocker(
            p,
            format!("Unresolved merge conflicts in {}", c.repo),
            Some(c.paths.join(", ")),
        ));
    }
    for s in &report.secrets {
        let at = match s.line {
            Some(line) => format!("{}/{}:{line}", s.repo, s.path),
            None => format!("{}/{}", s.repo, s.path),
        };
        let hint = (!s.hint.is_empty())
            .then(|| format!("{at} · {}", s.hint))
            .unwrap_or(at);
        body = body.child(blocker(p, secret_rule_label(s.rule).into(), Some(hint)));
    }
    for s in &report.scouts_outside {
        body = body.child(blocker(
            p,
            format!(
                "Scout \u{201c}{}\u{201d} uses a checkout outside the share",
                s.thread_title
            ),
            Some(format!(
                "Its parent \u{201c}{}\u{201d} is not shared. Widen the scope or close the scout.",
                s.parent_title
            )),
        ));
    }
    body
}

impl Render for ShareWizard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let Some(flow) = self.flow.clone() else {
            return div().into_any_element();
        };
        let body: AnyElement = match &flow.step {
            ShareStep::Scope => self.scope_step(&p, &flow, cx).into_any_element(),
            ShareStep::Access => self.access_step(&p, &flow, cx).into_any_element(),
            ShareStep::Checking => note(
                &p,
                IconName::RefreshCw,
                p.text_muted,
                "Checking conflicts, secrets and scouts…".into(),
            )
            .into_any_element(),
            ShareStep::Checked(report) => {
                section(&p, "Checks", report_view(&p, report)).into_any_element()
            }
            ShareStep::Creating => note(
                &p,
                IconName::Share,
                p.text_muted,
                "Capturing and encrypting the snapshot…".into(),
            )
            .into_any_element(),
            ShareStep::Created(created) => {
                let text = match (created.status, &created.link) {
                    (ShareStatus::Ready, Some(link)) => format!("Ready. Link: {link}"),
                    (ShareStatus::Ready, None) => "Ready.".into(),
                    (status, _) => {
                        format!("{status:?}: the link appears once every snapshot is uploaded.")
                    }
                };
                note(&p, IconName::Link, p.success, text).into_any_element()
            }
            ShareStep::Failed(message) => {
                note(&p, IconName::TriangleAlert, p.danger, message.clone()).into_any_element()
            }
        };
        let blocked = matches!(&flow.step, ShareStep::Checked(r) if r.blocks());
        let busy = matches!(flow.step, ShareStep::Checking | ShareStep::Creating);
        v_flex()
            .gap(theme::sp(4.))
            .child(
                h_flex()
                    .gap(theme::sp(1.5))
                    .items_center()
                    .text_size(theme::text_sm())
                    .text_color(p.text_muted)
                    .child(Icon::new(IconName::Share).with_size(px(13.)))
                    .child(div().min_w_0().truncate().child(flow.title.clone())),
            )
            .child(body)
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .justify_end()
                    .when(busy, |this| {
                        this.child(Spinner::new().small().color(p.accent))
                    })
                    .when(flow.can_go_back(), |this| {
                        this.child(
                            Button::new("share-wizard-back")
                                .small()
                                .label("Back")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_flow(cx, |f, _| f.back())
                                })),
                        )
                    })
                    .when(blocked, |this| {
                        this.child(
                            Button::new("share-wizard-recheck")
                                .small()
                                .label("Re-run checks")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_flow(cx, |f, out| {
                                        f.run_checks(out);
                                    })
                                })),
                        )
                    })
                    .when_some(flow.primary_label(), |this, label| {
                        this.child(
                            Button::new("share-wizard-next")
                                .primary()
                                .small()
                                .label(label)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.update_flow(cx, |f, out| {
                                        f.next(out);
                                    })
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl ShareWizard {
    fn scope_step(&self, p: &Palette, flow: &ShareFlow, cx: &mut Context<Self>) -> Div {
        let mut options = v_flex().gap(theme::sp(1.5));
        for scope in [
            ShareScope::ThreadOnly,
            ShareScope::Subtree,
            ShareScope::Workspace,
        ] {
            let (label, detail) = scope_text(scope);
            options = options.child(
                choice(
                    p,
                    SharedString::from(format!("scope-{}", scope.as_str())),
                    flow.scope == scope,
                    label,
                    detail,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.update_flow(cx, |f, _| f.set_scope(scope))
                })),
            );
        }
        let future = flow.include_future_descendants;
        section(p, "What to share", options).when(flow.scope.includes_descendants(), |this| {
            this.child(
                choice(
                    p,
                    "scope-future",
                    future,
                    "Include future descendants",
                    "New child threads join the share automatically.",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.update_flow(cx, |f, _| f.set_future_descendants(!future))
                })),
            )
        })
    }

    fn access_step(&self, p: &Palette, flow: &ShareFlow, cx: &mut Context<Self>) -> Div {
        let mut roles = v_flex().gap(theme::sp(1.5));
        for role in INVITEE_ROLES {
            let (label, detail) = role_text(role);
            roles =
                roles.child(
                    choice(
                        p,
                        SharedString::from(format!("role-{label}")),
                        flow.role == role,
                        label,
                        detail,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_flow(cx, |f, _| f.set_role(role))
                    })),
                );
        }
        let mut caps = h_flex().flex_wrap().gap(theme::sp(1.5));
        for cap in flow.preset() {
            let on = flow.has(cap);
            caps = caps.child(
                Button::new(SharedString::from(format!("cap-{cap:?}")))
                    .small()
                    .when(on, |b| b.primary())
                    .label(capability_label(cap))
                    .disabled(cap == Capability::ReadHistory)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_flow(cx, |f, _| f.toggle(cap))
                    })),
            );
        }
        let mut code = v_flex().gap(theme::sp(1.5));
        for (mode, label, detail) in [
            (
                CodeAccessMode::GitAuthorizationRequired,
                "Verify GitHub access",
                "Code keys are sent only after GitHub confirms access to every repository.",
            ),
            (
                CodeAccessMode::ShareGrantsCodeAccess,
                "Share grants code",
                "Invitees receive the code even without repository access.",
            ),
        ] {
            code = code.child(
                choice(
                    p,
                    SharedString::from(format!("code-{label}")),
                    flow.code_access == mode,
                    label,
                    detail,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.update_flow(cx, |f, _| f.code_access = mode)
                })),
            );
        }
        v_flex()
            .gap(theme::sp(4.))
            .child(section(p, "Invitee role", roles))
            .child(section(p, "Capabilities", caps))
            .when(flow.has(Capability::ReadCode), |this| {
                this.child(section(p, "Code access", code))
            })
    }
}
