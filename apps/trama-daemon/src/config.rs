//! `config.json` load/seed. Workspaces are upserted by name; repos by local path.

use crate::error::{DaemonError, Result};
use crate::log;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use trama_db::TramaDb;
use trama_domain::agent::PermissionMode;
use trama_domain::event::{Aggregate, DomainEvent, EventRecord, EventStore};
use trama_domain::message::Actor;
use trama_domain::workspace::{Repository, Workspace, WorkspaceMembership, WorkspaceSettings};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileConfig {
    #[serde(default)]
    pub workspaces: Vec<FileWorkspace>,
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

/// Idempotent: workspace by name, repository by local path, membership upsert.
pub async fn seed_from_config(db: &TramaDb, config: &FileConfig) -> Result<()> {
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
        let mut extra = serde_json::Map::new();
        extra.insert(
            "permission_mode".into(),
            serde_json::Value::String(permission_mode_label(&mode).into()),
        );

        let workspace = if let Some(mut existing) = db.get_workspace_by_name(name).await? {
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
                name: name.to_owned(),
                settings: WorkspaceSettings {
                    default_model: None,
                    extra: serde_json::Value::Object(extra),
                },
            };
            db.create_workspace(&workspace).await?;
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
            EventStore::append(db, vec![record]).await?;
            workspace
        };

        for repo in &file_ws.repos {
            if repo.path.as_os_str().is_empty() {
                return Err(DaemonError::Invalid(format!(
                    "repo `{}` in workspace `{name}` has an empty path",
                    repo.name
                )));
            }
            let repository =
                if let Some(existing) = db.get_repository_by_local_path(&repo.path).await? {
                    existing
                } else {
                    let repository = Repository::new(None, Some(repo.path.clone()), None);
                    db.create_repository(&repository).await?;
                    repository
                };
            let alias = {
                let trimmed = repo.name.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            };
            db.upsert_membership(&WorkspaceMembership {
                workspace_id: workspace.id.clone(),
                repository_id: repository.id,
                alias,
                default_enabled: true,
            })
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        std::fs::create_dir_all(&backend).unwrap();
        std::fs::create_dir_all(&frontend).unwrap();

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
        assert!(paths.contains(&backend));
        assert!(paths.contains(&frontend));
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
