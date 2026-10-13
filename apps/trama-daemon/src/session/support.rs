use super::*;

pub(super) const MIGRATION_NOTICE: &str = "Migrated this thread onto isolated checkouts. The transcript is retained, but the agent session is fresh and will not resume the previous session. Send a new message when you want the agent to continue; the earlier prompt was not run automatically.";

pub(super) const MAX_LABEL_LEN: usize = 120;

pub(super) struct MigrationPlan {
    pub(super) source: PathBuf,
    pub(super) attachment: ThreadRepositoryAttachment,
    pub(super) provision: bool,
    pub(super) checkout_path: Option<PathBuf>,
    pub(super) repo_local: bool,
}

pub(super) fn validate_managed_checkout(
    manager: &CheckoutManager,
    source: &std::path::Path,
    path: &std::path::Path,
    base: &str,
    repo_local: bool,
) -> std::result::Result<(), CheckoutError> {
    if repo_local {
        manager.validate_repo_local(source, path, base)
    } else {
        manager.validate(source, path, base)
    }
}

/// Run `.agents/linked` / `.agents/prepare` (see `trama_infra::process::setup`) in a
/// newly created checkout. The outcome is recorded in the checkout and shown
/// by `thread_checkouts`; it never removes the checkout.
pub(crate) fn run_checkout_setup(
    source: &std::path::Path,
    checkout: &std::path::Path,
    thread_id: &ThreadId,
    thread_title: &str,
    repository_alias: Option<&str>,
) {
    use trama_infra::process::setup::{SetupRequest, SetupStatus, timeout_from_env};
    let record = trama_infra::process::setup::run_checkout_setup(&SetupRequest {
        source,
        checkout,
        thread_id: &thread_id.0,
        thread_title,
        repository_alias,
        timeout: timeout_from_env(),
    });
    if record.status == SetupStatus::Failed {
        crate::log::warn(format!(
            "checkout setup failed in {}: {}",
            checkout.display(),
            record.failure_summary().unwrap_or_default()
        ));
    }
}

pub(super) fn prepare_migration(
    checkout_manager: CheckoutManager,
    thread_id: &ThreadId,
    mut plans: Vec<MigrationPlan>,
) -> std::result::Result<Vec<MigrationPlan>, CheckoutError> {
    for plan in &plans {
        if plan.provision {
            CheckoutManager::assert_clean(&plan.source)?;
        } else {
            let base_revision = plan
                .attachment
                .base_revision
                .as_deref()
                .ok_or_else(|| CheckoutError::Invalid(plan.source.clone()))?;
            let path = plan
                .checkout_path
                .as_ref()
                .ok_or_else(|| CheckoutError::Invalid(plan.source.clone()))?;
            validate_managed_checkout(
                &checkout_manager,
                &plan.source,
                path,
                base_revision,
                plan.repo_local,
            )?;
        }
    }
    for plan in &mut plans {
        if !plan.provision {
            continue;
        }
        let checkout_id = plan
            .attachment
            .checkout_id
            .clone()
            .ok_or_else(|| CheckoutError::Invalid(plan.source.clone()))?;
        let provisioned = checkout_manager.provision_repo_local(
            &plan.source,
            thread_id,
            &plan.attachment.repository_id,
            &checkout_id,
        )?;
        plan.checkout_path = Some(provisioned.path);
        plan.attachment.base_revision = Some(provisioned.base_revision);
        plan.attachment.attach_mode = AttachMode::Isolated;
    }
    Ok(plans)
}

pub(super) fn checkout_to_invalid(error: CheckoutError) -> DaemonError {
    match error {
        CheckoutError::Dirty { path } => DaemonError::Invalid(format!(
            "repository has uncommitted or untracked non-ignored files at {}; commit or stash them before continuing. Those changes stay in the original working tree and are not copied or deleted",
            path.display()
        )),
        CheckoutError::Missing { path } => DaemonError::Invalid(format!(
            "path does not exist: {}. Choose an existing local Git working tree, not a remote URL. Nothing was changed",
            path.display()
        )),
        CheckoutError::Nested {
            root,
            source_path: source,
        } => DaemonError::Invalid(format!(
            "managed checkout root {} would live inside the source repository at {}. Move the managed checkout root outside that repository (for example out of a dotfiles repo) so provisioning cannot dirty the source working tree. Nothing was changed",
            root.display(),
            source.display()
        )),
        other => DaemonError::Invalid(other.to_string()),
    }
}

pub(super) fn migration_checkout_error(error: CheckoutError) -> DaemonError {
    match error {
        CheckoutError::Dirty { path } => DaemonError::Invalid(format!(
            "repository has uncommitted or untracked non-ignored files at {}; commit or stash them before migrating. Those changes stay in the original working tree and are not copied or deleted. The thread was not changed",
            path.display()
        )),
        CheckoutError::Missing { path } => DaemonError::Invalid(format!(
            "repository path does not exist: {}. Restore the local Git working tree before migrating. The thread was not changed",
            path.display()
        )),
        CheckoutError::Nested {
            root,
            source_path: source,
        } => DaemonError::Invalid(format!(
            "managed checkout root {} would live inside the source repository at {}. Move the managed checkout root outside that repository (for example out of a dotfiles repo) so migrating cannot dirty the source working tree. The thread was not changed",
            root.display(),
            source.display()
        )),
        other => DaemonError::Invalid(format!(
            "{other}. The thread, its messages, and the original files were left unchanged"
        )),
    }
}

pub(super) fn validate_label(value: &str, what: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(DaemonError::Invalid(format!("{what} must not be empty")));
    }
    if value.chars().count() > MAX_LABEL_LEN {
        return Err(DaemonError::Invalid(format!(
            "{what} must be at most {MAX_LABEL_LEN} characters"
        )));
    }
    if value.chars().any(|ch| ch.is_control()) {
        return Err(DaemonError::Invalid(format!(
            "{what} must not contain control characters"
        )));
    }
    Ok(value.to_owned())
}

pub(super) fn validate_workspace_name(value: &str) -> Result<String> {
    let value = validate_label(value, "workspace name")?;
    if value.chars().any(|ch| ch == '/' || ch == '\\') {
        return Err(DaemonError::Invalid(
            "workspace name must not contain path separators".into(),
        ));
    }
    Ok(value)
}

pub(super) fn repository_snapshot(
    repository: &Repository,
    title: &str,
    default_enabled: bool,
) -> RepositorySnapshot {
    RepositorySnapshot {
        id: repository.id.clone(),
        title: title.to_owned(),
        path: repository.local_path.clone().unwrap_or_default(),
        default_enabled,
    }
}

pub(super) fn usage_report_present(report: &UsageReport) -> bool {
    !report.model.is_empty()
        || report.input_tokens > 0
        || report.output_tokens > 0
        || report.cache_read_tokens > 0
        || report.cache_write_tokens > 0
}

pub(super) fn same_directory(left: &std::path::Path, right: &std::path::Path) -> bool {
    match (dunce::canonicalize(left), dunce::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub(super) fn thread_snapshot(thread: &Thread) -> ThreadSnapshot {
    ThreadSnapshot {
        id: thread.id.clone(),
        title: thread.title.clone().unwrap_or_default(),
        repo_count: thread.repo_attachments.len() as u32,
        parent_thread_id: thread.parent_thread_id.clone(),
        kind: thread.kind,
        review_verdict: None,
        model: None,
        depth: 0,
        subagent: None,
        archived: false,
    }
}

pub(super) fn agent_kind_label(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "claude_code",
        AgentKind::Acp => "acp",
        AgentKind::Codex => "codex",
        AgentKind::Gemini => "gemini",
        AgentKind::Builtin => "builtin",
    }
}
