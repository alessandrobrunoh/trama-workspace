//! `config.json` load/seed. Workspaces are upserted by name; repos by local path.

use crate::error::{DaemonError, Result};
use crate::log;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use trama_db::TramaDb;
use trama_domain::agent::{BillingMode, PermissionMode};
use trama_domain::event::{Aggregate, DomainEvent, EventRecord};
use trama_domain::message::Actor;
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership, WorkspaceSettings};
use trama_infra::checkout::CheckoutManager;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileConfig {
    #[serde(default)]
    pub workspaces: Vec<FileWorkspace>,
    /// `"subscription"` (default) hides `ANTHROPIC_API_KEY` /
    /// `ANTHROPIC_AUTH_TOKEN` from Claude Code so it bills the Claude login.
    /// `"api_key"` is the explicit opt-in to API billing.
    #[serde(default)]
    pub billing: BillingMode,
    /// Trama subagents: limits and whether Claude Code's built-in subagent
    /// tool stays available.
    #[serde(default)]
    pub subagents: crate::session::SubagentSettings,
    /// How thread checkouts are copied: `{"mode": "copy_on_write" | "git",
    /// "exclude": ["relative/path", ...]}` (ADR 0012).
    #[serde(default)]
    pub checkouts: trama_infra::checkout::CopyOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileWorkspace {
    pub name: String,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub repos: Vec<FileRepo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRepo {
    pub name: String,
    pub path: PathBuf,
}

pub fn parse_permission_mode(raw: &str) -> Result<PermissionMode> {
    match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "" | "default" => Ok(PermissionMode::Default),
        "accept_edits" | "acceptedits" => Ok(PermissionMode::AcceptEdits),
        "plan" => Ok(PermissionMode::Plan),
        "bypass" | "bypasspermissions" => Ok(PermissionMode::Bypass),
        other => Err(DaemonError::Invalid(format!(
            "unknown permission_mode `{other}` (expected default|accept_edits|plan|bypass)"
        ))),
    }
}

pub fn permission_mode_label(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept_edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypass",
    }
}

pub fn permission_mode_from_settings(settings: &WorkspaceSettings) -> PermissionMode {
    match settings
        .extra
        .get("permission_mode")
        .and_then(|v| v.as_str())
    {
        Some(raw) => parse_permission_mode(raw).unwrap_or(PermissionMode::Default),
        None => PermissionMode::Default,
    }
}

/// Load `config.json`, writing `{"workspaces":[]}` when the file is missing.
pub fn load_or_init_config(path: &Path) -> Result<FileConfig> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        let empty = FileConfig::default();
        let json = serde_json::to_string_pretty(&empty)?;
        std::fs::write(path, json)?;
        log::info(format!("wrote empty config at {}", path.display()));
        return Ok(empty);
    }
    log::info(format!("using config {}", path.display()));
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Idempotent upsert by workspace name and repository path.
/// Rows created in the database and absent from this file are left in place,
/// including when the file is empty, so a restart does not erase UI data.
///
/// Every configured repository is inspected first, with the same rules as
/// `add_repository`: an existing, non-bare, top-level Git working tree. An
/// invalid entry is an error and nothing is seeded from that file — no
/// repository row is written and the source working tree is never touched.
pub async fn seed_from_config(db: &TramaDb, config: &FileConfig) -> Result<()> {
    // Validate everything before writing anything, so a broken config cannot
    // leave half-seeded state behind.
    let mut plans = Vec::with_capacity(config.workspaces.len());
    for file_ws in &config.workspaces {
        let name = file_ws.name.trim();
        if name.is_empty() {
            return Err(DaemonError::Invalid(
                "config workspace name must not be empty".into(),
            ));
        }
        let mode = match file_ws.permission_mode.as_deref() {
            None | Some("") => PermissionMode::Default,
            Some(raw) => parse_permission_mode(raw)?,
        };
        let mut repos = Vec::with_capacity(file_ws.repos.len());
        for repo in &file_ws.repos {
            if repo.path.as_os_str().is_empty() {
                return Err(DaemonError::Invalid(format!(
                    "repo `{}` in workspace `{name}` has an empty path",
                    repo.name
                )));
            }
            let inspected = CheckoutManager::inspect_working_root(&repo.path).map_err(|err| {
                DaemonError::Invalid(format!(
                    "repo `{}` in workspace `{name}` cannot be used: {err}. \
                     Fix or remove the entry in config.json; nothing was seeded \
                     and the path was not modified",
                    repo.name
                ))
            })?;
            let alias = {
                let trimmed = repo.name.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            };
            repos.push((inspected, alias));
        }
        plans.push((name.to_owned(), mode, repos));
    }

    for (name, mode, repos) in plans {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "permission_mode".into(),
            serde_json::Value::String(permission_mode_label(&mode).into()),
        );

        let workspace = if let Some(mut existing) = db.get_workspace_by_name(&name).await? {
            if let serde_json::Value::Object(map) = &existing.settings.extra {
                extra.extend(map.clone());
                extra.insert(
                    "permission_mode".into(),
                    serde_json::Value::String(permission_mode_label(&mode).into()),
                );
            }
            existing.settings.extra = serde_json::Value::Object(extra);
            db.update_workspace_settings(&existing.id, &existing.settings)
                .await?;
            existing
        } else {
            let workspace = Workspace {
                id: trama_domain::ids::WorkspaceId::generate(),
                name: name.clone(),
                settings: WorkspaceSettings {
                    default_model: None,
                    extra: serde_json::Value::Object(extra),
                },
            };
            let record = EventRecord::new(
                Aggregate::Workspace {
                    id: workspace.id.clone(),
                },
                Actor::System,
                DomainEvent::WorkspaceCreated {
                    workspace: workspace.clone(),
                },
                vec![],
            )?;
            // The duplicate-name guard runs inside the transaction, so the
            // workspace row and its event are inserted atomically and two
            // concurrent seeders cannot create the same name twice.
            db.create_workspace_with_event(&workspace, &record).await?;
            workspace
        };

        for (inspected, alias) in repos {
            // Same inspection-derived row as `add_repository`: canonical
            // path, default branch, and origin when present.
            let candidate = Repository::new(
                inspected.origin,
                Some(inspected.path),
                inspected.default_branch,
            );
            db.upsert_repository_membership(
                &candidate,
                &WorkspaceMembership {
                    workspace_id: workspace.id.clone(),
                    repository_id: candidate.id.clone(),
                    alias,
                    default_enabled: true,
                },
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_git(cwd: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Trama Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Trama Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn test_repository(path: &std::path::Path) {
        std::fs::create_dir_all(path).unwrap();
        test_git(path, &["init", "-b", "main"]);
        std::fs::write(path.join("same.txt"), "source\n").unwrap();
        test_git(path, &["add", "same.txt"]);
        test_git(path, &["commit", "-m", "initial"]);
    }

    #[test]
    fn missing_config_is_created_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");
        let loaded = load_or_init_config(&path).unwrap();
        assert!(loaded.workspaces.is_empty());
        assert!(path.exists());
        let again = load_or_init_config(&path).unwrap();
        assert_eq!(loaded, again);
    }

    #[tokio::test]
    async fn seed_is_idempotent_by_workspace_name_and_repo_path() {
        let dir = tempdir().unwrap();
        let backend = dir.path().join("backend");
        let frontend = dir.path().join("frontend");
        test_repository(&backend);
        test_repository(&frontend);

        let config = FileConfig {
            workspaces: vec![FileWorkspace {
                name: "LaFraternita".into(),
                permission_mode: Some("accept_edits".into()),
                repos: vec![
                    FileRepo {
                        name: "backend".into(),
                        path: backend.clone(),
                    },
                    FileRepo {
                        name: "frontend".into(),
                        path: frontend.clone(),
                    },
                ],
            }],
            ..Default::default()
        };
        let db = TramaDb::open_in_memory().unwrap();
        seed_from_config(&db, &config).await.unwrap();
        seed_from_config(&db, &config).await.unwrap();

        let workspaces = db.list_workspaces().await.unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "LaFraternita");
        let mode = permission_mode_from_settings(&workspaces[0].settings);
        assert_eq!(mode, PermissionMode::AcceptEdits);

        let memberships = db.list_memberships(&workspaces[0].id).await.unwrap();
        assert_eq!(memberships.len(), 2);
        assert!(memberships.iter().all(|m| m.default_enabled));

        let repos: Vec<_> = {
            let mut out = Vec::new();
            for m in &memberships {
                out.push(db.get_repository(&m.repository_id).await.unwrap().unwrap());
            }
            out
        };
        let paths: Vec<_> = repos.iter().filter_map(|r| r.local_path.clone()).collect();
        // Seeding stores the canonical path, like `add_repository`.
        let canonical_backend = std::fs::canonicalize(&backend).unwrap();
        let canonical_frontend = std::fs::canonicalize(&frontend).unwrap();
        assert!(paths.contains(&canonical_backend));
        assert!(paths.contains(&canonical_frontend));
        // Inspection-derived metadata is stored, like `add_repository`.
        assert!(
            repos
                .iter()
                .all(|r| r.default_branch.as_deref() == Some("main"))
        );
    }

    #[tokio::test]
    async fn invalid_config_repo_seeds_nothing_and_does_not_touch_the_path() {
        let dir = tempdir().unwrap();
        let valid = dir.path().join("valid");
        test_repository(&valid);
        let not_a_repo = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&not_a_repo).unwrap();
        std::fs::write(not_a_repo.join("keep.txt"), "unchanged\n").unwrap();

        let db = TramaDb::open_in_memory().unwrap();
        let config = FileConfig {
            workspaces: vec![FileWorkspace {
                name: "Broken".into(),
                permission_mode: None,
                repos: vec![
                    FileRepo {
                        name: "valid".into(),
                        path: valid.clone(),
                    },
                    FileRepo {
                        name: "not-a-repo".into(),
                        path: not_a_repo.clone(),
                    },
                ],
            }],
            ..Default::default()
        };
        let err = seed_from_config(&db, &config).await.unwrap_err();
        assert!(err.to_string().contains("not-a-repo"), "{err}");
        assert!(err.to_string().contains("nothing was seeded"), "{err}");
        // No rows: the invalid entry rejects the whole seed before any write.
        assert!(db.list_workspaces().await.unwrap().is_empty());
        assert!(db.list_all_repositories().await.unwrap().is_empty());
        // The original path was not mutated either.
        assert_eq!(
            std::fs::read_to_string(not_a_repo.join("keep.txt")).unwrap(),
            "unchanged\n"
        );
        assert!(!not_a_repo.join(".git").exists());

        // A missing path is rejected the same way.
        let missing_config = FileConfig {
            workspaces: vec![FileWorkspace {
                name: "Missing".into(),
                permission_mode: None,
                repos: vec![FileRepo {
                    name: "gone".into(),
                    path: dir.path().join("gone"),
                }],
            }],
            ..Default::default()
        };
        assert!(seed_from_config(&db, &missing_config).await.is_err());
        assert!(db.list_workspaces().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn seeded_workspace_with_no_repos_is_created() {
        let dir = tempdir().unwrap();
        let db = TramaDb::open_in_memory().unwrap();
        seed_from_config(
            &db,
            &FileConfig {
                workspaces: vec![FileWorkspace {
                    name: "Empty repos".into(),
                    permission_mode: None,
                    repos: vec![],
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let workspaces = db.list_workspaces().await.unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "Empty repos");
        assert_eq!(
            db.list_memberships(&workspaces[0].id).await.unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn empty_reseed_keeps_database_workspaces() {
        let dir = tempdir().unwrap();
        let db = TramaDb::open(&dir.path().join("trama.db")).unwrap();
        let workspace = Workspace::new("From UI");
        db.create_workspace(&workspace).await.unwrap();
        let repo_path = dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        let repo = Repository::new(None, Some(repo_path), None);
        db.create_repository(&repo).await.unwrap();
        db.upsert_membership(&WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: repo.id,
            alias: Some("repo".into()),
            default_enabled: true,
        })
        .await
        .unwrap();

        seed_from_config(&db, &FileConfig::default()).await.unwrap();
        seed_from_config(
            &db,
            &FileConfig {
                workspaces: vec![FileWorkspace {
                    name: "From file".into(),
                    permission_mode: None,
                    repos: vec![],
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let names: Vec<_> = db
            .list_workspaces()
            .await
            .unwrap()
            .into_iter()
            .map(|workspace| workspace.name)
            .collect();
        assert!(names.contains(&"From UI".to_string()));
        assert!(names.contains(&"From file".to_string()));
        assert_eq!(db.list_memberships(&workspace.id).await.unwrap().len(), 1);
    }

    #[test]
    fn checkout_copy_settings_parse_with_defaults() {
        let config: FileConfig = serde_json::from_str(r#"{"workspaces":[]}"#).unwrap();
        assert_eq!(
            config.checkouts.mode,
            trama_infra::checkout::CopyMode::CopyOnWrite
        );
        assert!(config.checkouts.exclude.is_empty());
        let config: FileConfig = serde_json::from_str(
            r#"{"checkouts":{"mode":"git","exclude":["target/debug/incremental",".venv"]}}"#,
        )
        .unwrap();
        assert_eq!(config.checkouts.mode, trama_infra::checkout::CopyMode::Git);
        assert_eq!(
            config.checkouts.exclude,
            vec!["target/debug/incremental".to_owned(), ".venv".to_owned()]
        );
    }

    #[test]
    fn permission_mode_aliases() {
        assert_eq!(
            parse_permission_mode("accept_edits").unwrap(),
            PermissionMode::AcceptEdits
        );
        assert_eq!(
            parse_permission_mode("bypass").unwrap(),
            PermissionMode::Bypass
        );
        assert!(parse_permission_mode("nope").is_err());
    }
}
