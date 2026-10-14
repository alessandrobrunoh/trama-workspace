use super::*;
use trama_domain::agent::AgentEvent;
use trama_domain::ids::ThreadId;
use trama_domain::message::Message;

fn roundtrip_eq<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value);
    let again = serde_json::to_string(&back).unwrap();
    assert_eq!(again, json);
}

fn roundtrip_json<T>(value: &T)
where
    T: Serialize + DeserializeOwned,
{
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(
        serde_json::to_value(value).unwrap(),
        serde_json::to_value(&back).unwrap()
    );
    let line = encode_json_line(value).unwrap();
    assert_eq!(*line.last().unwrap(), b'\n');
    assert_eq!(line.iter().filter(|b| **b == b'\n').count(), 1);
    let decoded: T = decode_json_line(std::str::from_utf8(&line).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(value).unwrap(),
        serde_json::to_value(&decoded).unwrap()
    );
}

fn workspace_id() -> WorkspaceId {
    WorkspaceId::from("01hworkspace000000000000001")
}

fn thread_id() -> ThreadId {
    ThreadId::from("01hthread0000000000000000001")
}

fn snapshot_ws() -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        id: workspace_id(),
        title: "LaFraternita".into(),
        repo_count: 2,
    }
}

fn snapshot_th() -> ThreadSnapshot {
    ThreadSnapshot {
        id: thread_id(),
        title: "Fix auth".into(),
        repo_count: 1,
        parent_thread_id: None,
        kind: ThreadKind::Top,
        review_verdict: None,
        model: None,
        depth: 0,
        subagent: None,
        archived: false,
    }
}

fn snapshot_repo() -> RepositorySnapshot {
    RepositorySnapshot {
        id: RepositoryId::from("01hrepo000000000000000000001"),
        title: "app".into(),
        path: std::path::PathBuf::from("/src/app"),
        default_enabled: true,
    }
}

#[test]
fn client_message_roundtrip_every_kind() {
    let tid = thread_id();
    let wid = workspace_id();
    let kinds = [
        ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "trama-desktop".into(),
        },
        ClientMessage::ListWorkspaces,
        ClientMessage::ListThreads {
            workspace_id: wid.clone(),
        },
        ClientMessage::CreateThread {
            workspace_id: wid.clone(),
            title: "New thread".into(),
        },
        ClientMessage::SubscribeThread {
            thread_id: tid.clone(),
        },
        ClientMessage::UnsubscribeThread {
            thread_id: tid.clone(),
        },
        ClientMessage::SendPrompt {
            thread_id: tid.clone(),
            text: "hello".into(),
            mentions: Vec::new(),
            terminals: Vec::new(),
        },
        ClientMessage::Interrupt {
            thread_id: tid.clone(),
        },
        ClientMessage::RespondApproval {
            thread_id: tid.clone(),
            request_id: "req-1".into(),
            decision: ApprovalDecision::AllowOnce,
        },
        ClientMessage::CreateWorkspace {
            name: "LaFraternita".into(),
        },
        ClientMessage::AddRepository {
            workspace_id: wid,
            path: std::path::PathBuf::from("/src/app"),
            alias: "app".into(),
        },
        ClientMessage::ListRepositories {
            workspace_id: workspace_id(),
        },
        ClientMessage::MigrateLegacyThread { thread_id: tid },
        ClientMessage::ApprovalHook {
            token: "tok".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
        },
        ClientMessage::Shutdown,
    ];
    for msg in &kinds {
        roundtrip_eq(msg);
        roundtrip_json(msg);
    }
    assert_eq!(kinds.len(), 15);
}

#[test]
fn daemon_message_roundtrip_every_kind() {
    let tid = thread_id();
    let message = Message::new_user(tid.clone(), "alice", "hi");
    let kinds = [
        DaemonMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: "0.1.0".into(),
            accepted: true,
        },
        DaemonMessage::Workspaces(vec![snapshot_ws()]),
        DaemonMessage::Threads(vec![snapshot_th()]),
        DaemonMessage::ThreadCreated(snapshot_th()),
        DaemonMessage::MessageAppended {
            thread_id: tid.clone(),
            message,
        },
        DaemonMessage::AgentEventEnvelope {
            thread_id: tid.clone(),
            event: AgentEvent::TextDelta {
                text: "chunk".into(),
            },
        },
        DaemonMessage::UsageSnapshot {
            thread_id: tid,
            totals: UsageReport {
                model: "mock".into(),
                input_tokens: 3,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 1,
            },
        },
        DaemonMessage::Error {
            code: "not_found".into(),
            message: "thread missing".into(),
        },
        DaemonMessage::WorkspaceCreated(snapshot_ws()),
        DaemonMessage::Repositories {
            workspace_id: workspace_id(),
            repositories: vec![snapshot_repo()],
        },
        DaemonMessage::RepositoryAdded {
            workspace_id: workspace_id(),
            repository: snapshot_repo(),
        },
        DaemonMessage::ThreadMigrated(snapshot_th()),
        DaemonMessage::ApprovalHookDecision {
            decision: ApprovalDecision::Deny,
            message: Some("denied".into()),
            interrupt: false,
        },
    ];
    for msg in &kinds {
        roundtrip_json(msg);
    }
    assert_eq!(kinds.len(), 13);
}

#[test]
fn unknown_tags_are_denied() {
    let client = serde_json::from_str::<ClientMessage>(r#"{"type":"launch_nukes","payload":{}}"#);
    assert!(client.is_err(), "unknown client tags must be denied");
    let daemon =
        serde_json::from_str::<DaemonMessage>(r#"{"type":"secret_backdoor","payload":null}"#);
    assert!(daemon.is_err(), "unknown daemon tags must be denied");
    let client_err = client.unwrap_err().to_string();
    assert!(
        client_err.contains("unknown variant") || client_err.contains("launch_nukes"),
        "{client_err}"
    );
}

#[test]
fn hello_json_shape_is_adjacently_tagged() {
    let json = serde_json::to_string(&ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        client_name: "ui".into(),
    })
    .unwrap();
    assert_eq!(
        json,
        format!(
            r#"{{"type":"hello","payload":{{"protocol_version":{PROTOCOL_VERSION},"client_name":"ui"}}}}"#
        )
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::ListWorkspaces).unwrap(),
        r#"{"type":"list_workspaces"}"#
    );
    assert_eq!(
        serde_json::to_string(&DaemonMessage::Workspaces(vec![])).unwrap(),
        r#"{"type":"workspaces","payload":[]}"#
    );
}

#[test]
fn onboarding_and_migration_tags_keep_existing_shape() {
    let workspace = workspace_id();
    let repo = snapshot_repo();
    assert_eq!(
        serde_json::to_string(&ClientMessage::CreateWorkspace {
            name: "Demo".into(),
        })
        .unwrap(),
        r#"{"type":"create_workspace","payload":{"name":"Demo"}}"#
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::AddRepository {
            workspace_id: workspace.clone(),
            path: std::path::PathBuf::from("/src/app"),
            alias: "app".into(),
        })
        .unwrap(),
        format!(
            r#"{{"type":"add_repository","payload":{{"workspace_id":"{workspace}","path":"/src/app","alias":"app"}}}}"#
        )
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::ListRepositories {
            workspace_id: workspace.clone(),
        })
        .unwrap(),
        format!(r#"{{"type":"list_repositories","payload":{{"workspace_id":"{workspace}"}}}}"#)
    );
    assert_eq!(
        serde_json::to_string(&ClientMessage::MigrateLegacyThread {
            thread_id: thread_id(),
        })
        .unwrap(),
        format!(
            r#"{{"type":"migrate_legacy_thread","payload":{{"thread_id":"{}"}}}}"#,
            thread_id()
        )
    );
    assert_eq!(
        serde_json::to_string(&DaemonMessage::WorkspaceCreated(snapshot_ws())).unwrap(),
        format!(
            r#"{{"type":"workspace_created","payload":{{"id":"{workspace}","title":"LaFraternita","repo_count":2}}}}"#
        )
    );
    assert_eq!(
        serde_json::to_string(&DaemonMessage::Repositories {
            workspace_id: workspace.clone(),
            repositories: vec![repo.clone()],
        })
        .unwrap(),
        format!(
            r#"{{"type":"repositories","payload":{{"workspace_id":"{workspace}","repositories":[{{"id":"{}","title":"app","path":"/src/app","default_enabled":true}}]}}}}"#,
            repo.id
        )
    );
    assert_eq!(
        serde_json::to_string(&DaemonMessage::RepositoryAdded {
            workspace_id: workspace.clone(),
            repository: repo.clone(),
        })
        .unwrap(),
        format!(
            r#"{{"type":"repository_added","payload":{{"workspace_id":"{workspace}","repository":{{"id":"{}","title":"app","path":"/src/app","default_enabled":true}}}}}}"#,
            repo.id
        )
    );
    let thread = thread_id();
    assert_eq!(
        serde_json::to_string(&DaemonMessage::ThreadMigrated(snapshot_th())).unwrap(),
        format!(
            r#"{{"type":"thread_migrated","payload":{{"id":"{thread}","title":"Fix auth","repo_count":1,"parent_thread_id":null,"kind":"top","review_verdict":null,"model":null,"depth":0,"subagent":null,"archived":false}}}}"#
        )
    );
    roundtrip_eq(&repo);
}

#[test]
fn approval_channel_json_shape() {
    assert_eq!(
        serde_json::to_string(&ClientMessage::ApprovalHook {
            token: "tok".into(),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
        })
        .unwrap(),
        r#"{"type":"approval_hook","payload":{"token":"tok","tool_name":"Bash","tool_input":{"command":"ls"}}}"#
    );
    assert_eq!(
        serde_json::to_string(&DaemonMessage::ApprovalHookDecision {
            decision: ApprovalDecision::AllowSession,
            message: None,
            interrupt: false,
        })
        .unwrap(),
        r#"{"type":"approval_hook_decision","payload":{"decision":"allow_session","message":null,"interrupt":false}}"#
    );
    let envelope = DaemonMessage::AgentEventEnvelope {
        thread_id: thread_id(),
        event: AgentEvent::ApprovalRequested {
            request_id: "apr-1".into(),
            tool_name: "Bash".into(),
            input_json: serde_json::json!({"command": "ls"}),
        },
    };
    roundtrip_json(&envelope);
}

#[test]
fn platform_app_dir_follows_os_conventions() {
    let env = |key: &str| -> Option<std::ffi::OsString> {
        match key {
            "HOME" => Some("/home/someone".into()),
            "APPDATA" => Some("C:/Users/someone/AppData/Roaming".into()),
            _ => None,
        }
    };
    let expected = if cfg!(target_os = "macos") {
        "/home/someone/Library/Application Support/TramaWorkspace"
    } else if cfg!(windows) {
        "C:/Users/someone/AppData/Roaming/TramaWorkspace"
    } else {
        "/home/someone/.local/share/TramaWorkspace"
    };
    assert_eq!(platform_app_dir(env), Some(PathBuf::from(expected)));
    if !cfg!(target_os = "macos") && !cfg!(windows) {
        let xdg = |key: &str| -> Option<std::ffi::OsString> {
            match key {
                "XDG_DATA_HOME" => Some("/data".into()),
                "HOME" => Some("/home/someone".into()),
                _ => None,
            }
        };
        assert_eq!(
            platform_app_dir(xdg),
            Some(PathBuf::from("/data/TramaWorkspace"))
        );
    }
}

#[test]
fn app_dir_prefers_trama_data_dir_over_home() {
    // Single test touching these variables, so no cross-test races.
    let saved = (std::env::var_os(DATA_DIR_ENV), std::env::var_os("HOME"));
    unsafe {
        std::env::set_var("HOME", "/Users/someone");
        std::env::remove_var(DATA_DIR_ENV);
    }
    assert_eq!(
        default_app_dir(),
        platform_app_dir(|key| std::env::var_os(key).filter(|value| !value.is_empty()))
    );
    unsafe { std::env::set_var(DATA_DIR_ENV, "/tmp/trama-test") };
    assert_eq!(default_app_dir(), Some(PathBuf::from("/tmp/trama-test")));
    unsafe {
        match saved.0 {
            Some(value) => std::env::set_var(DATA_DIR_ENV, value),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
        match saved.1 {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}

#[test]
fn protocol_version_is_current() {
    assert_eq!(PROTOCOL_VERSION, 18);
}

#[test]
fn disk_usage_and_archive_contract_shape() {
    let request: ClientMessage =
        serde_json::from_str(r#"{"type":"get_disk_usage","payload":{}}"#).unwrap();
    assert_eq!(request, ClientMessage::GetDiskUsage { workspace_id: None });
    let archive: ClientMessage = serde_json::from_str(
        r#"{"type":"archive_thread","payload":{"thread_id":"t1","delete_checkouts":true}}"#,
    )
    .unwrap();
    assert_eq!(
        archive,
        ClientMessage::ArchiveThread {
            thread_id: ThreadId::from("t1"),
            delete_checkouts: true,
            force: false,
        }
    );
    for message in [
        ClientMessage::UnarchiveThread {
            thread_id: thread_id(),
        },
        ClientMessage::DeleteThreadCheckouts {
            thread_id: thread_id(),
            force: true,
        },
        ClientMessage::GetDiskUsage {
            workspace_id: Some(workspace_id()),
        },
    ] {
        roundtrip_eq(&message);
    }
    let removal = CheckoutRemoval {
        repository_id: RepositoryId::from("repo-1"),
        title: "app".into(),
        path: PathBuf::from("/src/app/.trama/worktrees/t/c"),
        changed_files: vec!["src/lib.rs".into()],
        changed_file_count: 1,
        commits_not_in_source: 2,
        freed_bytes: Some(4096),
    };
    assert!(removal.has_unsaved_work());
    let usage = DaemonMessage::DiskUsage {
        workspace_id: None,
        items: vec![DiskUsageItem {
            thread_id: thread_id(),
            thread_title: "Fix auth".into(),
            repository_id: RepositoryId::from("repo-1"),
            repository_title: "app".into(),
            path: PathBuf::from("/src/app/.trama/worktrees/t/c"),
            apparent_bytes: 20 << 30,
            private_bytes: 1 << 20,
            private_estimated: false,
            last_active: None,
            archived: false,
        }],
    };
    let json = serde_json::to_value(&usage).unwrap();
    assert_eq!(json["type"], "disk_usage");
    assert_eq!(json["payload"]["items"][0]["private_bytes"], 1 << 20);
    roundtrip_json(&usage);
    let mut archived = snapshot_th();
    archived.archived = true;
    for message in [
        DaemonMessage::ThreadArchived {
            thread: archived.clone(),
            removed: vec![removal.clone()],
        },
        DaemonMessage::ThreadUnarchived(snapshot_th()),
        DaemonMessage::ThreadCheckoutsDeleted {
            thread_id: thread_id(),
            removed: vec![],
        },
        DaemonMessage::CheckoutRemovalRefused {
            thread_id: thread_id(),
            checkouts: vec![removal],
        },
    ] {
        roundtrip_json(&message);
    }
    // Older snapshots without `archived` still decode.
    let mut value = serde_json::to_value(snapshot_th()).unwrap();
    value.as_object_mut().unwrap().remove("archived");
    assert!(
        !serde_json::from_value::<ThreadSnapshot>(value)
            .unwrap()
            .archived
    );
    assert_eq!(
        serde_json::to_value(CheckoutStatus::Removed).unwrap(),
        "removed"
    );
}

#[test]
fn turn_activity_contract_shape() {
    let at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let stored = DaemonMessage::TurnActivity {
        thread_id: thread_id(),
        message_id: Some(MessageId::from("m1")),
        items: vec![
            ActivityItem::Thinking {
                text: "plan".into(),
                truncated: false,
            },
            ActivityItem::ToolCall {
                tool_use_id: "toolu_1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
                output: Some(serde_json::json!("a")),
                is_error: false,
                finished: true,
                started_at: at,
                finished_at: Some(at),
                truncated: false,
            },
        ],
    };
    roundtrip_json(&stored);
    let json = serde_json::to_value(&stored).unwrap();
    assert_eq!(json["type"], "turn_activity");
    assert_eq!(json["payload"]["message_id"], "m1");
    assert_eq!(json["payload"]["items"][0]["kind"], "thinking");
    assert_eq!(json["payload"]["items"][1]["kind"], "tool_call");
    assert_eq!(
        json["payload"]["items"][1]["started_at"],
        "2026-01-01T00:00:00Z"
    );
    let in_flight: DaemonMessage = serde_json::from_value(serde_json::json!({
        "type": "turn_activity",
        "payload": {"thread_id": thread_id().to_string(), "message_id": null, "items": []}
    }))
    .unwrap();
    assert!(matches!(
        in_flight,
        DaemonMessage::TurnActivity {
            message_id: None,
            ..
        }
    ));
}

#[test]
fn composer_contract_shapes() {
    let tid = thread_id();
    // Older clients omit the attachments; empty ones are not serialized.
    let plain: ClientMessage = serde_json::from_value(serde_json::json!({
        "type":"send_prompt","payload":{"thread_id": tid.to_string(), "text":"hi"}}))
    .unwrap();
    assert_eq!(
        plain,
        ClientMessage::SendPrompt {
            thread_id: tid.clone(),
            text: "hi".into(),
            mentions: vec![],
            terminals: vec![],
        }
    );
    assert_eq!(
        serde_json::to_value(&plain).unwrap(),
        serde_json::json!({"type":"send_prompt","payload":{"thread_id": tid.to_string(), "text":"hi"}})
    );
    let send = ClientMessage::SendPrompt {
        thread_id: tid.clone(),
        text: "look".into(),
        mentions: vec![PromptMention {
            kind: MentionKind::File,
            value: "repo1:src/lib.rs".into(),
        }],
        terminals: vec!["term-1".into()],
    };
    assert_eq!(
        serde_json::to_value(&send).unwrap()["payload"],
        serde_json::json!({"thread_id": tid.to_string(), "text":"look",
            "mentions":[{"kind":"file","value":"repo1:src/lib.rs"}],
            "terminals":["term-1"]})
    );
    let client = [
        send,
        ClientMessage::ListMentionables {
            thread_id: tid.clone(),
            query: "lib".into(),
            limit: Some(10),
        },
        ClientMessage::ListSkills {
            thread_id: tid.clone(),
        },
        ClientMessage::SetThreadModel {
            thread_id: tid.clone(),
            model: Some("claude-opus-5-5".into()),
        },
        ClientMessage::ListModels,
        ClientMessage::OpenTerminal {
            thread_id: tid.clone(),
            repository_id: None,
            command: Some("cargo test".into()),
            cols: 80,
            rows: 24,
        },
        ClientMessage::TerminalInput {
            terminal_id: "term-1".into(),
            data: "ls\r".into(),
        },
        ClientMessage::ResizeTerminal {
            terminal_id: "term-1".into(),
            cols: 120,
            rows: 40,
        },
        ClientMessage::CloseTerminal {
            terminal_id: "term-1".into(),
        },
        ClientMessage::ListTerminals {
            thread_id: tid.clone(),
        },
        ClientMessage::ReplayTerminal {
            terminal_id: "term-1".into(),
        },
    ];
    for msg in &client {
        roundtrip_eq(msg);
        roundtrip_json(msg);
    }
    let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
        "type":"list_mentionables","payload":{"thread_id": tid.to_string()}}))
    .unwrap();
    assert_eq!(
        minimal,
        ClientMessage::ListMentionables {
            thread_id: tid.clone(),
            query: String::new(),
            limit: None
        }
    );
    let open: ClientMessage = serde_json::from_value(serde_json::json!({
        "type":"open_terminal","payload":{"thread_id": tid.to_string(),"cols":80,"rows":24}}))
    .unwrap();
    assert!(matches!(
        open,
        ClientMessage::OpenTerminal {
            command: None,
            repository_id: None,
            ..
        }
    ));
    assert_eq!(
        serde_json::to_string(&ClientMessage::ListModels).unwrap(),
        r#"{"type":"list_models"}"#
    );

    let terminal = TerminalSnapshot {
        terminal_id: "term-1".into(),
        thread_id: tid.clone(),
        repository_id: RepositoryId::from("repo1"),
        cwd: PathBuf::from("/src/app/.trama/worktrees/t/c"),
        command: None,
        cols: 80,
        rows: 24,
        last_seq: 3,
        exit_status: Some(TerminalExitStatus {
            code: Some(0),
            signal: None,
        }),
    };
    let daemon = [
        DaemonMessage::Mentionables {
            thread_id: tid.clone(),
            query: "lib".into(),
            items: vec![Mentionable {
                kind: MentionKind::Directory,
                label: "src/".into(),
                detail: Some("app".into()),
                value: "repo1:src".into(),
            }],
        },
        DaemonMessage::Skills {
            thread_id: tid.clone(),
            items: vec![SkillInfo {
                name: "land".into(),
                description: Some("Land the branch".into()),
                source: SkillSource::Project,
                kind: SkillKind::Skill,
            }],
        },
        DaemonMessage::ThreadModelChanged {
            thread_id: tid.clone(),
            model: None,
        },
        DaemonMessage::Models {
            items: vec![ModelInfo {
                id: "default".into(),
                label: "Default".into(),
            }],
        },
        DaemonMessage::TerminalOpened {
            thread_id: tid.clone(),
            terminal: terminal.clone(),
        },
        DaemonMessage::TerminalOutput {
            thread_id: tid.clone(),
            terminal_id: "term-1".into(),
            data: "aGk=".into(),
            seq: 1,
            replay: false,
        },
        DaemonMessage::TerminalExited {
            thread_id: tid.clone(),
            terminal_id: "term-1".into(),
            status: TerminalExitStatus {
                code: None,
                signal: Some(9),
            },
        },
        DaemonMessage::Terminals {
            thread_id: tid.clone(),
            terminals: vec![terminal],
        },
    ];
    for msg in &daemon {
        roundtrip_json(msg);
    }
    assert_eq!(
        serde_json::to_value(&daemon[0]).unwrap(),
        serde_json::json!({"type":"mentionables","payload":{"thread_id": tid.to_string(),
            "query":"lib","items":[{"kind":"directory","label":"src/","detail":"app","value":"repo1:src"}]}})
    );
    assert_eq!(
        serde_json::to_value(&daemon[1]).unwrap()["payload"]["items"][0],
        serde_json::json!({"name":"land","description":"Land the branch","source":"project","kind":"skill"})
    );
    assert_eq!(
        serde_json::to_value(&daemon[5]).unwrap(),
        serde_json::json!({"type":"terminal_output","payload":{"thread_id": tid.to_string(),
            "terminal_id":"term-1","data":"aGk=","seq":1,"replay":false}})
    );
    assert_eq!(
        serde_json::to_value(&daemon[6]).unwrap()["payload"]["status"],
        serde_json::json!({"code":null,"signal":9})
    );
    let snapshot = serde_json::to_value(&daemon[4]).unwrap();
    assert_eq!(snapshot["payload"]["terminal"]["exit_status"]["code"], 0);
    assert_eq!(snapshot["payload"]["terminal"]["last_seq"], 3);
}

#[test]
fn collab_contract_shapes() {
    let tid = thread_id();
    let client = [
        ClientMessage::ConnectCollab {
            server_url: "http://127.0.0.1:8787".into(),
            username: "alice".into(),
            password: "secret-password".into(),
            runner_token: None,
        },
        ClientMessage::DisconnectCollab { forget: true },
        ClientMessage::GetCollabStatus,
        ClientMessage::ShareThread {
            thread_id: tid.clone(),
            access: Some(CollabAccess::Link),
            invite: vec!["bob".into()],
        },
        ClientMessage::GetSharedThread {
            thread_id: tid.clone(),
        },
        ClientMessage::RemoveSharedThreadMember {
            thread_id: tid.clone(),
            username: "bob".into(),
        },
        ClientMessage::JoinSharedThread {
            workspace_id: workspace_id(),
            collab_thread_id: "ct1".into(),
            link_token: Some("tok".into()),
        },
        ClientMessage::ListSharedThreads,
        ClientMessage::UnlinkSharedThread {
            thread_id: tid.clone(),
        },
        ClientMessage::SetCollabDraft {
            thread_id: tid.clone(),
            text: "typing".into(),
        },
        ClientMessage::SetCollabPresence {
            thread_id: tid.clone(),
            state: CollabPresenceState::Idle,
        },
    ];
    for msg in &client {
        roundtrip_eq(msg);
        roundtrip_json(msg);
    }
    assert_eq!(
        serde_json::to_value(&client[3]).unwrap(),
        serde_json::json!({"type":"share_thread","payload":{
            "thread_id": tid.to_string(), "access":"link", "invite":["bob"]}})
    );
    // Optional fields may be omitted.
    let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
        "type":"connect_collab","payload":{"server_url":"http://127.0.0.1:1",
        "username":"a","password":"p"}}))
    .unwrap();
    assert!(matches!(
        minimal,
        ClientMessage::ConnectCollab {
            runner_token: None,
            ..
        }
    ));
    assert_eq!(
        serde_json::from_value::<ClientMessage>(
            serde_json::json!({"type":"disconnect_collab","payload":{}})
        )
        .unwrap(),
        ClientMessage::DisconnectCollab { forget: false }
    );

    let shared = SharedThreadSnapshot {
        thread_id: tid.clone(),
        collab_thread_id: "ct1".into(),
        title: "Shared".into(),
        owner: "alice".into(),
        access: CollabAccess::Invited,
        members: vec![CollabMember {
            username: "bob".into(),
            via: CollabMemberVia::Invite,
        }],
        link_token: None,
        pending_publish: 0,
    };
    let daemon = [
        DaemonMessage::CollabStatus(CollabStatusSnapshot {
            server_url: Some("http://127.0.0.1:8787".into()),
            username: Some("alice".into()),
            signed_in: true,
            runner: CollabRunnerState::Online,
            linked_threads: 1,
            last_error: None,
        }),
        DaemonMessage::SharedThread(shared.clone()),
        DaemonMessage::SharedThreadJoined {
            thread: snapshot_th(),
            shared,
        },
        DaemonMessage::SharedThreads(vec![SharedThreadListing {
            collab_thread_id: "ct1".into(),
            title: "Shared".into(),
            owner: "alice".into(),
            access: CollabAccess::Link,
            via: CollabMemberVia::Link,
            last_seq: 4,
            local_thread_id: None,
        }]),
        DaemonMessage::SharedThreadUnlinked {
            thread_id: tid.clone(),
        },
        DaemonMessage::CollabTurnQueued {
            thread_id: tid.clone(),
            command_id: "trd.x.y".into(),
        },
        DaemonMessage::CollabEvent {
            thread_id: tid.clone(),
            event: CollabEventEnvelope {
                thread_id: "ct1".into(),
                seq: 3,
                kind: CollabEventKind::CommentCreated,
                author: Some("bob".into()),
                command_id: Some("c".into()),
                created_at: 1,
                payload: serde_json::json!({"body":"nit"}),
            },
        },
        DaemonMessage::CollabAgentEvent {
            thread_id: tid.clone(),
            author: "bob".into(),
            command_id: "c".into(),
            event: AgentEvent::TextDelta { text: "hi".into() },
        },
        DaemonMessage::CollabDraft {
            thread_id: tid.clone(),
            username: "bob".into(),
            text: "dra".into(),
        },
        DaemonMessage::CollabPresence {
            thread_id: tid.clone(),
            username: "bob".into(),
            state: CollabPresenceState::Online,
        },
        DaemonMessage::CollabAccessRevoked {
            thread_id: tid.clone(),
        },
    ];
    for msg in &daemon {
        roundtrip_json(msg);
    }
    let json = serde_json::to_value(&daemon[0]).unwrap();
    assert_eq!(json["type"], "collab_status");
    assert_eq!(json["payload"]["runner"], "online");
    assert_eq!(
        serde_json::to_value(&daemon[6]).unwrap()["payload"]["event"]["kind"],
        "comment_created"
    );
}

#[test]
fn subthread_contract_shapes() {
    let parent = thread_id();
    let sub = ThreadId::from("01hsub000000000000000000001");
    let client = [
        ClientMessage::CreateSubthread {
            parent_thread_id: parent.clone(),
            kind: SubthreadKind::Review,
            title: Some("Review".into()),
            initial_prompt: None,
        },
        ClientMessage::ListSubthreads {
            parent_thread_id: parent.clone(),
        },
        ClientMessage::GetChanges {
            thread_id: sub.clone(),
            base: ChangeBase::LastTurn,
        },
        ClientMessage::PullChanges {
            subthread_id: sub.clone(),
        },
        ClientMessage::SubmitReviewVerdict {
            review_thread_id: sub.clone(),
            verdict: ReviewVerdict::RequestChanges {
                summary: "add tests".into(),
            },
        },
    ];
    for msg in &client {
        roundtrip_eq(msg);
        roundtrip_json(msg);
    }
    assert_eq!(
        serde_json::to_value(&client[0]).unwrap(),
        serde_json::json!({"type":"create_subthread","payload":{
            "parent_thread_id": parent.to_string(), "kind":"review",
            "title":"Review", "initial_prompt": null}})
    );
    // Optional fields and the default base may be omitted.
    let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
        "type":"get_changes","payload":{"thread_id": sub.to_string()}}))
    .unwrap();
    assert_eq!(
        minimal,
        ClientMessage::GetChanges {
            thread_id: sub.clone(),
            base: ChangeBase::ThreadBase
        }
    );
    assert_eq!(
        serde_json::to_value(&client[4]).unwrap()["payload"]["verdict"],
        serde_json::json!({"verdict":"request_changes","summary":"add tests"})
    );

    let child = ThreadSnapshot {
        id: sub.clone(),
        title: "Review".into(),
        repo_count: 1,
        parent_thread_id: Some(parent.clone()),
        kind: ThreadKind::Review,
        review_verdict: Some(ReviewVerdict::Approve),
        model: Some("claude-opus-5-5".into()),
        depth: 1,
        subagent: None,
        archived: false,
    };
    let daemon = [
        DaemonMessage::SubthreadCreated(child.clone()),
        DaemonMessage::Subthreads {
            parent_thread_id: parent.clone(),
            threads: vec![child.clone()],
        },
        DaemonMessage::Changes {
            thread_id: sub.clone(),
            base: ChangeBase::ThreadBase,
            repos: vec![RepoChanges {
                repository_id: RepositoryId::from("repo"),
                title: "app".into(),
                base_revision: "abc".into(),
                diff: "diff --git a/x b/x\n".into(),
                truncated: false,
                files: vec![FileChange {
                    path: "x".into(),
                    old_path: None,
                    status: FileChangeStatus::Modified,
                    additions: Some(1),
                    deletions: Some(0),
                }],
            }],
        },
        DaemonMessage::ChangesPulled {
            subthread_id: sub.clone(),
            parent_thread_id: parent.clone(),
            repos: vec![RepoPullResult {
                repository_id: RepositoryId::from("repo"),
                files: vec![PullFileResult {
                    path: "x".into(),
                    change: FileChangeStatus::Modified,
                    outcome: PullFileOutcome::Conflict {
                        markers_written: true,
                        reason: "both".into(),
                    },
                }],
            }],
        },
        DaemonMessage::ReviewVerdictPosted {
            review_thread_id: sub.clone(),
            parent_thread_id: parent.clone(),
            verdict: ReviewVerdict::Approve,
        },
        DaemonMessage::LandResult {
            land_thread_id: sub,
            parent_thread_id: parent,
            outcome: LandOutcome::Landed,
            summary: "merged".into(),
        },
    ];
    for msg in &daemon {
        roundtrip_json(msg);
    }
    let json = serde_json::to_value(&daemon[0]).unwrap();
    assert_eq!(json["type"], "subthread_created");
    assert_eq!(json["payload"]["kind"], "review");
    assert_eq!(json["payload"]["review_verdict"]["verdict"], "approve");
    // Snapshots from older peers without the new fields decode as top-level.
    let legacy: ThreadSnapshot = serde_json::from_value(serde_json::json!({
        "id": "t", "title": "old", "repo_count": 1}))
    .unwrap();
    assert_eq!(legacy.kind, ThreadKind::Top);
    assert!(legacy.parent_thread_id.is_none());
    assert_eq!(legacy.depth, 0);
    assert!(legacy.subagent.is_none());
}

#[test]
fn subagent_and_fork_contract_shapes() {
    let parent = thread_id();
    let child = ThreadId::from("01hchild00000000000000000001");
    let fork = ClientMessage::ForkThread {
        thread_id: parent.clone(),
        at_message_id: Some(MessageId::from("m7")),
        title: Some("Try B".into()),
        initial_prompt: None,
        kind: SubthreadKind::Plain,
    };
    roundtrip_eq(&fork);
    roundtrip_json(&fork);
    assert_eq!(
        serde_json::to_value(&fork).unwrap(),
        serde_json::json!({"type":"fork_thread","payload":{
            "thread_id": parent.to_string(), "at_message_id":"m7",
            "title":"Try B","initial_prompt":null,"kind":"plain"}})
    );
    // Only the thread id is required.
    let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
        "type":"fork_thread","payload":{"thread_id": parent.to_string()}}))
    .unwrap();
    assert_eq!(
        minimal,
        ClientMessage::ForkThread {
            thread_id: parent.clone(),
            at_message_id: None,
            title: None,
            initial_prompt: None,
            kind: SubthreadKind::Plain,
        }
    );
    let call = ClientMessage::McpToolCall {
        token: "tok".into(),
        tool: "spawn_subagent".into(),
        arguments: serde_json::json!({"title":"t","prompt":"p"}),
        tool_use_id: Some("toolu_1".into()),
    };
    roundtrip_eq(&call);
    assert_eq!(
        serde_json::to_value(&call).unwrap(),
        serde_json::json!({"type":"mcp_tool_call","payload":{"token":"tok",
            "tool":"spawn_subagent","arguments":{"title":"t","prompt":"p"},
            "tool_use_id":"toolu_1"}})
    );
    let result = DaemonMessage::McpToolResult {
        is_error: false,
        text: "ok".into(),
    };
    roundtrip_json(&result);
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        serde_json::json!({"type":"mcp_tool_result","payload":{"is_error":false,"text":"ok"}})
    );
    let status = DaemonMessage::SubagentStatusChanged {
        thread_id: child.clone(),
        parent_thread_id: parent.clone(),
        status: SubagentStatus::WaitingToSync,
        detail: None,
    };
    roundtrip_json(&status);
    assert_eq!(
        serde_json::to_value(&status).unwrap()["payload"]["status"],
        "waiting_to_sync"
    );
    let snapshot = ThreadSnapshot {
        id: child.clone(),
        title: "Worker".into(),
        repo_count: 1,
        parent_thread_id: Some(parent.clone()),
        kind: ThreadKind::Sub,
        review_verdict: None,
        model: None,
        depth: 3,
        subagent: Some(SubagentSnapshot {
            spawned_by: SpawnedBy {
                parent_thread_id: parent.clone(),
                tool_use_id: Some("toolu_1".into()),
                profile: SubagentProfile::Worker,
            },
            status: SubagentStatus::Running,
            detail: None,
        }),
        archived: false,
    };
    roundtrip_eq(&snapshot);
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["depth"], 3);
    assert_eq!(
        json["subagent"],
        serde_json::json!({"spawned_by":{"parent_thread_id": parent.to_string(),
            "tool_use_id":"toolu_1","profile":"worker"},"status":"running","detail":null})
    );
    let mut message = Message::new_user(child.clone(), "agent", "hi");
    message.annotation = Some(MessageAnnotation::AgentMessageReceived {
        from_thread_id: parent.clone(),
        from_title: "Main".into(),
    });
    let appended = DaemonMessage::MessageAppended {
        thread_id: child,
        message,
    };
    roundtrip_json(&appended);
    assert_eq!(
        serde_json::to_value(&appended).unwrap()["payload"]["message"]["annotation"]["kind"],
        "agent_message_received"
    );
}

#[test]
fn comment_contract_shapes() {
    let tid = thread_id();
    let cid = CommentId::from("01hcomment00000000000000001");
    let code = CommentTarget::Code {
        anchor: CodeAnchor {
            repository_id: RepositoryId::from("repo"),
            path: PathBuf::from("src/lib.rs"),
            revision: 0,
            start_line: 3,
            end_line: 4,
            text_hash: None,
            context_hash: None,
        },
    };
    let client = [
        ClientMessage::CreateComment {
            thread_id: tid.clone(),
            target: code.clone(),
            body: "rename".into(),
            parent_comment_id: None,
        },
        ClientMessage::CreateComment {
            thread_id: tid.clone(),
            target: CommentTarget::MessageRange {
                message_id: MessageId::from("m1"),
                start_byte: 0,
                end_byte: 5,
            },
            body: "why?".into(),
            parent_comment_id: Some(cid.clone()),
        },
        ClientMessage::ListComments {
            thread_id: tid.clone(),
        },
        ClientMessage::ResolveComment {
            comment_id: cid.clone(),
        },
        ClientMessage::ReopenComment {
            comment_id: cid.clone(),
        },
        ClientMessage::DeleteComment {
            comment_id: cid.clone(),
        },
        ClientMessage::SendComments {
            thread_id: tid.clone(),
        },
    ];
    for msg in &client {
        roundtrip_eq(msg);
        roundtrip_json(msg);
    }
    assert_eq!(
        serde_json::to_value(&client[0]).unwrap(),
        serde_json::json!({"type":"create_comment","payload":{
            "thread_id": tid.to_string(),
            "target": {"target":"code","anchor":{
                "repository_id":"repo","path":"src/lib.rs","revision":0,
                "start_line":3,"end_line":4,"text_hash":null,"context_hash":null}},
            "body":"rename","parent_comment_id":null}})
    );
    assert_eq!(
        serde_json::to_value(&client[6]).unwrap(),
        serde_json::json!({"type":"send_comments","payload":{"thread_id": tid.to_string()}})
    );
    // A UI may omit the parent, the revision and the hashes.
    let minimal: ClientMessage = serde_json::from_value(serde_json::json!({
        "type":"create_comment","payload":{
            "thread_id": tid.to_string(),
            "target":{"target":"diff_hunk","repository_id":"repo","path":"a.rs",
                "base_revision":"abc","hunk_header":"@@ -1,2 +1,3 @@"},
            "body":"hm"}}))
    .unwrap();
    assert!(matches!(
        minimal,
        ClientMessage::CreateComment {
            parent_comment_id: None,
            target: CommentTarget::DiffHunk { .. },
            ..
        }
    ));
    let lean: ClientMessage = serde_json::from_value(serde_json::json!({
        "type":"create_comment","payload":{
            "thread_id": tid.to_string(),
            "target":{"target":"code","anchor":{"repository_id":"repo",
                "path":"src/lib.rs","start_line":1,"end_line":1}},
            "body":"hm"}}))
    .unwrap();
    assert!(matches!(lean, ClientMessage::CreateComment { .. }));

    let created_at = "2026-10-12T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let delivery = CommentDelivery {
        message_id: MessageId::from("m2"),
        turn: 3,
        delivered_at: created_at,
        answer_message_id: Some(MessageId::from("m3")),
    };
    let snapshot = CommentSnapshot {
        id: cid.clone(),
        thread_id: tid.clone(),
        parent_comment_id: None,
        author: Actor::Human { id: "user".into() },
        target: code,
        body: "rename".into(),
        excerpt: Some("let a = 1;\nlet b = 2;".into()),
        created_at,
        state: CommentState::Delivered(delivery.clone()),
        anchor_status: Some(AnchorStatus::Resolved { line: 5 }),
    };
    let json = serde_json::to_value(DaemonMessage::CommentUpdated(snapshot.clone())).unwrap();
    assert_eq!(json["type"], "comment_updated");
    assert_eq!(
        json["payload"]["state"],
        serde_json::json!({"kind":"delivered","message_id":"m2","turn":3,
            "delivered_at":"2026-10-12T10:00:00Z","answer_message_id":"m3"})
    );
    assert_eq!(
        json["payload"]["anchor_status"],
        serde_json::json!({"resolved":{"line":5}})
    );
    assert_eq!(
        json["payload"]["author"],
        serde_json::json!({"kind":"human","id":"user"})
    );
    let mut pending = snapshot.clone();
    pending.state = CommentState::Pending;
    pending.anchor_status = Some(AnchorStatus::Orphaned);
    let json = serde_json::to_value(&pending).unwrap();
    assert_eq!(json["state"], serde_json::json!({"kind":"pending"}));
    assert_eq!(json["anchor_status"], "orphaned");
    let mut resolved = snapshot.clone();
    resolved.state = CommentState::Resolved {
        resolved_at: created_at,
        delivery: Some(delivery),
    };
    assert_eq!(
        serde_json::to_value(&resolved).unwrap()["state"]["kind"],
        "resolved"
    );
    for state in [&snapshot, &pending, &resolved] {
        roundtrip_eq(state);
    }
    let daemon = [
        DaemonMessage::CommentCreated(pending),
        DaemonMessage::CommentUpdated(resolved.clone()),
        DaemonMessage::Comments {
            thread_id: tid.clone(),
            comments: vec![snapshot, resolved],
        },
        DaemonMessage::CommentDeleted {
            thread_id: tid.clone(),
            comment_ids: vec![cid.clone()],
        },
    ];
    for msg in &daemon {
        roundtrip_json(msg);
    }
    assert_eq!(
        serde_json::to_value(&daemon[3]).unwrap(),
        serde_json::json!({"type":"comment_deleted","payload":{
            "thread_id": tid.to_string(), "comment_ids":[cid.to_string()]}})
    );
}

#[test]
fn thread_checkout_discovery_contract_uses_snake_case_status() {
    let message = DaemonMessage::ThreadCheckouts {
        thread_id: ThreadId::from("thread-1"),
        checkouts: vec![CheckoutSnapshot {
            repository_id: RepositoryId::from("repo-1"),
            title: "backend".into(),
            source_path: PathBuf::from("/src/backend"),
            checkout_path: Some(PathBuf::from("/src/backend/.trama/worktrees/t/c")),
            status: CheckoutStatus::Ready,
            detail: None,
        }],
    };
    let json = serde_json::to_value(&message).unwrap();
    assert_eq!(json["type"], "thread_checkouts");
    assert_eq!(json["payload"]["checkouts"][0]["status"], "ready");
    let decoded = serde_json::from_value::<DaemonMessage>(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), json);
}
