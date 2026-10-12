//! Composer backend: `@` mentionables and prompt expansion, `/` skills,
//! model selection, and inline terminals. Temp Git repos and the fake
//! adapter only; nothing reaches a real agent.

use super::context::{fuzzy_score, normalize_model};
use super::skills::{SkillRoots, discover, parse_frontmatter};
use super::terminal::{filter_env, strip_ansi, tail_lines};
use super::*;
use crate::testing::{FakeAdapter, scripted_events};
use base64::Engine as _;
use std::os::unix::fs::symlink;
use std::path::Path;
use trama_domain::comment::{CodeAnchor, CommentTarget};
use trama_domain::ids::RepositoryId;
use trama_protocol::{
    MentionKind, Mentionable, PromptMention, SkillInfo, SkillKind, SkillSource, TerminalExitStatus,
};

fn git(cwd: &Path, args: &[&str]) {
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

/// A committed repository with `files` (path, contents).
fn repo(path: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    for (file, contents) in files {
        let full = path.join(file);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
    git(path, &["add", "."]);
    git(path, &["commit", "-q", "-m", "initial"]);
}

struct Fixture {
    manager: SessionManager,
    fake: FakeAdapter,
    workspace: WorkspaceId,
    dir: tempfile::TempDir,
}

const APP_FILES: &[(&str, &str)] = &[
    ("src/lib.rs", "pub fn lib() {}\n"),
    ("src/main.rs", "fn main() {}\n"),
    ("docs/guide.md", "# Guide\n"),
    (".gitignore", "target/\n"),
];

/// One workspace; one repository per `(alias, files)`.
async fn fixture_with(repos: &[(&str, &[(&str, &str)])], fake: FakeAdapter) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = TramaDb::open(&dir.path().join("trama.db")).unwrap();
    let workspace = Workspace::new("composer");
    db.create_workspace(&workspace).await.unwrap();
    for (alias, files) in repos {
        let source = dir.path().join(format!("source-{alias}"));
        repo(&source, files);
        let repository = Repository::new(None, Some(source), Some("main".into()));
        db.create_repository(&repository).await.unwrap();
        db.add_membership(&WorkspaceMembership {
            workspace_id: workspace.id.clone(),
            repository_id: repository.id,
            alias: Some((*alias).into()),
            default_enabled: true,
        })
        .await
        .unwrap();
    }
    let manager =
        SessionManager::with_checkout_root(db, Arc::new(fake.clone()), dir.path().join("managed"))
            .with_skill_home(None);
    Fixture {
        manager,
        fake,
        workspace: workspace.id,
        dir,
    }
}

fn fast_fake() -> FakeAdapter {
    FakeAdapter::new(scripted_events(), Duration::from_millis(1))
}

async fn fixture() -> Fixture {
    fixture_with(&[("app", APP_FILES)], fast_fake()).await
}

async fn thread(fixture: &Fixture, title: &str) -> ThreadId {
    fixture
        .manager
        .create_thread(fixture.workspace.clone(), title.into())
        .await
        .unwrap()
        .id
}

async fn checkout(fixture: &Fixture, thread_id: &ThreadId) -> (RepositoryId, PathBuf) {
    let ready = fixture.manager.ready_checkouts(thread_id).await.unwrap();
    (ready[0].repository_id.clone(), ready[0].path.clone())
}

async fn subscribed(
    manager: &SessionManager,
    thread_id: &ThreadId,
) -> mpsc::UnboundedReceiver<DaemonMessage> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    manager.subscribe(thread_id, tx).await.unwrap();
    while rx.try_recv().is_ok() {}
    rx
}

/// Wait for a turn to end (`UsageSnapshot`), failing on `Failed`.
async fn wait_turn(rx: &mut mpsc::UnboundedReceiver<DaemonMessage>) {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("turn event")
            .expect("channel open");
        match message {
            DaemonMessage::UsageSnapshot { .. } => return,
            DaemonMessage::AgentEventEnvelope {
                event: AgentEvent::Failed { message },
                ..
            } => panic!("turn failed: {message}"),
            _ => {}
        }
    }
}

async fn run_turn(fixture: &Fixture, thread_id: &ThreadId, text: &str, context: PromptContext) {
    let mut rx = subscribed(&fixture.manager, thread_id).await;
    fixture
        .manager
        .send_prompt_with_context(thread_id.clone(), text.into(), context)
        .await
        .unwrap();
    wait_turn(&mut rx).await;
}

fn prompts(fake: &FakeAdapter) -> Vec<String> {
    fake.prompts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn configs(fake: &FakeAdapter) -> Vec<SessionConfig> {
    fake.start_configs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn mention(kind: MentionKind, value: impl Into<String>) -> PromptMention {
    PromptMention {
        kind,
        value: value.into(),
    }
}

fn labels(items: &[Mentionable]) -> Vec<&str> {
    items.iter().map(|item| item.label.as_str()).collect()
}

// ---- @ mentions -----------------------------------------------------------

#[test]
fn fuzzy_ranking_prefers_basename_and_boundaries() {
    let lib = fuzzy_score("lib", "src/lib.rs").unwrap();
    let deep = fuzzy_score("lib", "vendor/calibration/table.rs").unwrap();
    assert!(lib > deep, "{lib} vs {deep}");
    assert!(fuzzy_score("slr", "src/lib.rs").is_some());
    assert!(fuzzy_score("xyz", "src/lib.rs").is_none());
    assert_eq!(fuzzy_score("", "anything"), Some(0));
    assert!(
        fuzzy_score("LIB", "src/lib.rs").is_some(),
        "case-insensitive"
    );
}

#[tokio::test]
async fn mentionables_list_checkout_files_threads_and_people() {
    let fixture = fixture().await;
    let tid = thread(&fixture, "feature").await;
    let other = thread(&fixture, "Auth refactor").await;
    let (repo_id, path) = checkout(&fixture, &tid).await;
    // Untracked non-ignored files are listed; ignored ones are not.
    std::fs::create_dir_all(path.join("notes")).unwrap();
    std::fs::write(path.join("notes/todo.md"), "x").unwrap();
    std::fs::create_dir_all(path.join("target")).unwrap();
    std::fs::write(path.join("target/debug.log"), "x").unwrap();

    let all = fixture
        .manager
        .list_mentionables(&tid, "", Some(200))
        .await
        .unwrap();
    let names = labels(&all);
    for expected in [
        "src/",
        "src/lib.rs",
        "docs/guide.md",
        "notes/todo.md",
        "Auth refactor",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    assert!(
        !names.iter().any(|label| label.contains("target")),
        "{names:?}"
    );
    assert!(
        !names.contains(&"feature"),
        "the thread itself is not listed"
    );
    let lib = all.iter().find(|item| item.label == "src/lib.rs").unwrap();
    assert_eq!(lib.kind, MentionKind::File);
    assert_eq!(lib.value, format!("{repo_id}:src/lib.rs"));
    assert_eq!(lib.detail.as_deref(), Some("app"));
    let dir = all.iter().find(|item| item.label == "src/").unwrap();
    assert_eq!(dir.kind, MentionKind::Directory);
    assert_eq!(dir.value, format!("{repo_id}:src"));
    let thread_item = all
        .iter()
        .find(|item| item.label == "Auth refactor")
        .unwrap();
    assert_eq!(thread_item.kind, MentionKind::Thread);
    assert_eq!(thread_item.value, other.to_string());
    let people: Vec<_> = all
        .iter()
        .filter(|item| item.kind == MentionKind::Person)
        .collect();
    assert_eq!(people.len(), 1, "only the local user until collab lands");
    assert_eq!(people[0].value, "user");

    let ranked = fixture
        .manager
        .list_mentionables(&tid, "lib", None)
        .await
        .unwrap();
    assert_eq!(ranked[0].label, "src/lib.rs");
    let auth = fixture
        .manager
        .list_mentionables(&tid, "auth", None)
        .await
        .unwrap();
    assert_eq!(auth[0].kind, MentionKind::Thread);
    let bounded = fixture
        .manager
        .list_mentionables(&tid, "", Some(2))
        .await
        .unwrap();
    assert_eq!(bounded.len(), 2);
    assert!(
        fixture
            .manager
            .list_mentionables(&ThreadId::from("missing"), "", None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn multi_repo_mentionables_are_prefixed_by_alias() {
    let fixture = fixture_with(
        &[("app", APP_FILES), ("web", &[("index.html", "<p>\n")])],
        fast_fake(),
    )
    .await;
    let tid = thread(&fixture, "feature").await;
    let items = fixture
        .manager
        .list_mentionables(&tid, "", Some(200))
        .await
        .unwrap();
    let names = labels(&items);
    assert!(names.contains(&"app/src/lib.rs"), "{names:?}");
    assert!(names.contains(&"web/index.html"), "{names:?}");
    let web = items.iter().find(|i| i.label == "web/index.html").unwrap();
    assert_eq!(web.detail.as_deref(), Some("web"));
}

// ---- prompt expansion -----------------------------------------------------

#[tokio::test]
async fn mentions_expand_into_a_referenced_preamble() {
    let fixture = fixture().await;
    let tid = thread(&fixture, "feature").await;
    let other = thread(&fixture, "Auth refactor").await;
    run_turn(&fixture, &other, "start", PromptContext::default()).await;
    let (repo_id, path) = checkout(&fixture, &tid).await;

    let context = PromptContext {
        mentions: vec![
            mention(MentionKind::File, format!("{repo_id}:src/lib.rs")),
            mention(MentionKind::Directory, format!("{repo_id}:src")),
            // Duplicates collapse.
            mention(MentionKind::File, format!("{repo_id}:src/lib.rs")),
            mention(MentionKind::Thread, other.to_string()),
            mention(MentionKind::Person, "user"),
        ],
        terminals: vec![],
    };
    run_turn(&fixture, &tid, "explain", context).await;
    let sent = prompts(&fixture.fake).pop().unwrap();
    let head = format!(
        "Referenced:\n- file: {root}/src/lib.rs\n- directory: {root}/src/\n- thread \"Auth refactor\" (Trama thread {other}). Its latest reply:\n  > Hello from the mock agent.\n  It is a separate Trama thread with its own session and checkouts; you cannot read or message it from here, so ask the user to relay anything you need from it.\n- person: ",
        root = path.display(),
    );
    assert!(sent.starts_with(&head), "{sent}");
    assert!(
        sent.ends_with(" (the user writing this prompt)\n\nexplain"),
        "{sent}"
    );
    // The stored user message is exactly what was sent.
    let stored = fixture
        .manager
        .db
        .list_messages_by_thread(&tid)
        .await
        .unwrap();
    assert_eq!(stored[0].text(), sent);
}

#[tokio::test]
async fn path_mentions_never_leave_the_thread_checkouts() {
    let fixture = fixture().await;
    let tid = thread(&fixture, "feature").await;
    let (repo_id, path) = checkout(&fixture, &tid).await;
    let outside = fixture.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "x").unwrap();
    symlink(&outside, path.join("escape")).unwrap();

    let other_ws = Workspace::new("elsewhere");
    fixture
        .manager
        .db
        .create_workspace(&other_ws)
        .await
        .unwrap();
    let foreign = Thread::new_for_workspace(&other_ws, &[], Some("foreign".into()));
    fixture.manager.db.create_thread(&foreign).await.unwrap();

    let source = fixture.dir.path().join("source-app");
    for bad in [
        mention(
            MentionKind::File,
            format!("{repo_id}:../source-app/src/lib.rs"),
        ),
        mention(
            MentionKind::File,
            format!("{repo_id}:{}", source.join("src/lib.rs").display()),
        ),
        mention(MentionKind::File, format!("{repo_id}:escape/secret.txt")),
        mention(MentionKind::Directory, format!("{repo_id}:escape")),
        mention(MentionKind::File, format!("{repo_id}:missing.rs")),
        mention(MentionKind::File, "other-repo:src/lib.rs"),
        mention(MentionKind::File, "no-separator"),
        mention(MentionKind::Thread, foreign.id.to_string()),
        mention(MentionKind::Person, "somebody"),
    ] {
        let result = fixture
            .manager
            .send_prompt_with_context(
                tid.clone(),
                "hi".into(),
                PromptContext {
                    mentions: vec![bad.clone()],
                    terminals: vec![],
                },
            )
            .await;
        assert!(
            matches!(result, Err(DaemonError::Invalid(_))),
            "{bad:?} -> {result:?}"
        );
    }
    assert!(prompts(&fixture.fake).is_empty());
    assert!(
        fixture
            .manager
            .db
            .list_messages_by_thread(&tid)
            .await
            .unwrap()
            .is_empty()
    );
    // The rejections released the thread.
    run_turn(&fixture, &tid, "ok", PromptContext::default()).await;
    assert_eq!(prompts(&fixture.fake), vec!["ok".to_string()]);
}

#[tokio::test]
async fn slash_commands_go_first_and_unchanged_and_keep_comments_pending() {
    let fixture = fixture().await;
    let tid = thread(&fixture, "feature").await;
    let (repo_id, path) = checkout(&fixture, &tid).await;
    fixture
        .manager
        .create_comment(
            tid.clone(),
            CommentTarget::Code {
                anchor: CodeAnchor {
                    repository_id: repo_id.clone(),
                    path: "src/lib.rs".into(),
                    revision: 0,
                    start_line: 1,
                    end_line: 1,
                    text_hash: None,
                    context_hash: None,
                },
            },
            "rename".into(),
            None,
        )
        .await
        .unwrap();

    run_turn(&fixture, &tid, "/compact", PromptContext::default()).await;
    run_turn(
        &fixture,
        &tid,
        "/review the parser",
        PromptContext {
            mentions: vec![mention(MentionKind::File, format!("{repo_id}:src/lib.rs"))],
            terminals: vec![],
        },
    )
    .await;
    let sent = prompts(&fixture.fake);
    assert_eq!(sent[0], "/compact");
    assert_eq!(
        sent[1],
        format!(
            "/review the parser\n\nReferenced:\n- file: {}/src/lib.rs",
            path.display()
        )
    );
    // The comment is still pending and goes out with the next plain prompt.
    run_turn(&fixture, &tid, "now", PromptContext::default()).await;
    let last = prompts(&fixture.fake).pop().unwrap();
    assert!(last.starts_with("<review-comments>"), "{last}");
    assert!(last.ends_with("\n\nnow"), "{last}");
}

// ---- / skills ---------------------------------------------------------------

#[test]
fn frontmatter_parses_plain_quoted_folded_and_literal_values() {
    assert_eq!(
        parse_frontmatter("---\nname: land\ndescription: Land the branch # note\n---\nbody"),
        super::skills::Frontmatter {
            name: Some("land".into()),
            description: Some("Land the branch".into()),
        }
    );
    let quoted = parse_frontmatter(
        "---\nname: \"deploy\"\ndescription: 'It''s \"safe\"'\nmetadata:\n  trama-action: land\n---\n",
    );
    assert_eq!(quoted.name.as_deref(), Some("deploy"));
    assert_eq!(quoted.description.as_deref(), Some("It's \"safe\""));
    let folded = parse_frontmatter(
        "---\nname: review\ndescription: >\n  Review the diff\n  line by line.\n\nallowed-tools: Read\n---\n",
    );
    assert_eq!(
        folded.description.as_deref(),
        Some("Review the diff line by line.")
    );
    let literal = parse_frontmatter("---\ndescription: |\n  one\n  two\n---\n");
    assert_eq!(literal.description.as_deref(), Some("one two"));
    let continued = parse_frontmatter("---\ndescription: starts here\n  and continues\n---\n");
    assert_eq!(
        continued.description.as_deref(),
        Some("starts here and continues")
    );
    assert_eq!(
        parse_frontmatter("no frontmatter\nname: x"),
        super::skills::Frontmatter::default()
    );
    assert_eq!(
        parse_frontmatter("\u{feff}---\nname: bom\n---\n")
            .name
            .as_deref(),
        Some("bom")
    );
}

fn skill(dir: &Path, name: &str, frontmatter: &str) {
    std::fs::create_dir_all(dir.join(name)).unwrap();
    std::fs::write(dir.join(name).join("SKILL.md"), frontmatter).unwrap();
}

fn find<'a>(items: &'a [SkillInfo], name: &str) -> Option<&'a SkillInfo> {
    items.iter().find(|item| item.name == name)
}

#[test]
fn skill_discovery_covers_project_user_and_plugins_and_refuses_symlinks_out() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let home = dir.path().join("home");
    let outside = dir.path().join("outside");
    skill(
        &checkout.join(".claude/skills"),
        "land",
        "---\nname: land\ndescription: Land it\n---\n",
    );
    skill(
        &checkout.join(".agents/skills"),
        "triage",
        "---\ndescription: Triage issues\n---\n",
    );
    std::fs::create_dir_all(checkout.join(".claude/commands/git")).unwrap();
    std::fs::write(
        checkout.join(".claude/commands/deploy.md"),
        "Deploy the app to staging\n",
    )
    .unwrap();
    std::fs::write(
        checkout.join(".claude/commands/git/sync.md"),
        "---\ndescription: Sync branches\n---\n",
    )
    .unwrap();
    skill(&outside, "evil", "---\nname: evil\ndescription: x\n---\n");
    skill(
        &outside,
        "evil-user",
        "---\nname: evil-user\ndescription: x\n---\n",
    );
    // Project symlink out of the checkout: refused.
    symlink(outside.join("evil"), checkout.join(".claude/skills/evil")).unwrap();
    // User skills: a plain one, one linked from ~/.agents/skills (allowed),
    // one linked elsewhere (refused).
    skill(
        &home.join(".claude/skills"),
        "bro",
        "---\nname: bro\ndescription: Re-explain\n---\n",
    );
    skill(
        &home.join(".agents/skills"),
        "shared",
        "---\nname: shared\ndescription: Shared skill\n---\n",
    );
    symlink(
        home.join(".agents/skills/shared"),
        home.join(".claude/skills/shared"),
    )
    .unwrap();
    symlink(
        outside.join("evil-user"),
        home.join(".claude/skills/evil-user"),
    )
    .unwrap();
    // A project SKILL.md that is itself a symlink out: refused.
    std::fs::create_dir_all(checkout.join(".claude/skills/linked")).unwrap();
    symlink(
        outside.join("evil/SKILL.md"),
        checkout.join(".claude/skills/linked/SKILL.md"),
    )
    .unwrap();
    // A plugin with a manifest name.
    let plugin = home.join(".claude/plugins/cache/market/tools/1.0.0");
    std::fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    std::fs::write(
        plugin.join(".claude-plugin/plugin.json"),
        r#"{"name":"tools"}"#,
    )
    .unwrap();
    skill(
        &plugin.join("skills"),
        "lint",
        "---\nname: lint\ndescription: Lint code\n---\n",
    );
    // A symlinked directory inside the plugin root is not walked.
    symlink(&outside, home.join(".claude/plugins/linked")).unwrap();

    let items = discover(&SkillRoots {
        checkouts: vec![std::fs::canonicalize(&checkout).unwrap()],
        home: Some(home.clone()),
    });
    let land = find(&items, "land").unwrap();
    assert_eq!(
        (land.source, land.kind, land.description.as_deref()),
        (SkillSource::Project, SkillKind::Skill, Some("Land it"))
    );
    assert_eq!(find(&items, "triage").unwrap().source, SkillSource::Project);
    let deploy = find(&items, "deploy").unwrap();
    assert_eq!(deploy.kind, SkillKind::Command);
    assert_eq!(
        deploy.description.as_deref(),
        Some("Deploy the app to staging")
    );
    assert_eq!(
        find(&items, "git:sync").unwrap().description.as_deref(),
        Some("Sync branches")
    );
    assert_eq!(find(&items, "bro").unwrap().source, SkillSource::User);
    assert_eq!(find(&items, "shared").unwrap().source, SkillSource::User);
    let lint = find(&items, "tools:lint").unwrap();
    assert_eq!(lint.source, SkillSource::Plugin);
    for refused in ["evil", "evil-user", "linked"] {
        assert!(find(&items, refused).is_none(), "{refused} must be refused");
    }
    assert!(!items.iter().any(|item| item.name.contains("evil")));
}

#[tokio::test]
async fn list_skills_merges_disk_builtins_and_reported_commands() {
    let mut events = scripted_events();
    events.insert(
        1,
        AgentEvent::CommandsReported {
            slash_commands: vec!["compact".into(), "land".into(), "mcp".into()],
            skills: vec!["land".into(), "remote-skill".into()],
        },
    );
    let fixture = fixture_with(
        &[(
            "app",
            &[(
                ".claude/skills/land/SKILL.md",
                "---\nname: land\ndescription: Land it\n---\n",
            )],
        )],
        FakeAdapter::new(events, Duration::from_millis(1)),
    )
    .await;
    let tid = thread(&fixture, "feature").await;
    let before = fixture.manager.list_skills(&tid).await.unwrap();
    assert!(find(&before, "mcp").is_none());
    assert_eq!(
        find(&before, "compact").unwrap().source,
        SkillSource::Builtin
    );

    run_turn(&fixture, &tid, "hi", PromptContext::default()).await;
    let after = fixture.manager.list_skills(&tid).await.unwrap();
    let land = find(&after, "land").unwrap();
    assert_eq!(
        land.source,
        SkillSource::Project,
        "disk wins over the report"
    );
    assert_eq!(land.description.as_deref(), Some("Land it"));
    let mcp = find(&after, "mcp").unwrap();
    assert_eq!(
        (mcp.source, mcp.kind),
        (SkillSource::Builtin, SkillKind::Command)
    );
    assert_eq!(find(&after, "remote-skill").unwrap().kind, SkillKind::Skill);
    let names: Vec<_> = after.iter().map(|s| s.name.clone()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    // The report is persisted and survives a daemon restart.
    let reported = fixture
        .manager
        .db
        .agent_commands(&tid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reported.skills, vec!["land", "remote-skill"]);
}

// ---- model selection --------------------------------------------------------

#[test]
fn model_ids_are_normalized() {
    assert_eq!(normalize_model(None).unwrap(), None);
    assert_eq!(normalize_model(Some("default".into())).unwrap(), None);
    assert_eq!(normalize_model(Some("  ".into())).unwrap(), None);
    assert_eq!(
        normalize_model(Some(" claude-opus-5-5[1m] ".into())).unwrap(),
        Some("claude-opus-5-5[1m]".into())
    );
    assert!(normalize_model(Some("opus; rm -rf".into())).is_err());
    let ids: Vec<_> = model_choices().into_iter().map(|m| m.id).collect();
    for id in [
        "default",
        "claude-sonnet-5-5",
        "claude-opus-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
    ] {
        assert!(ids.contains(&id.to_string()), "{id}");
    }
}

#[tokio::test]
async fn thread_model_override_applies_on_the_next_turn_with_resume() {
    let fixture = fixture().await;
    let tid = thread(&fixture, "feature").await;
    let mut rx = subscribed(&fixture.manager, &tid).await;
    assert_eq!(
        fixture
            .manager
            .set_thread_model(&tid, Some("claude-opus-5-5".into()))
            .await
            .unwrap()
            .as_deref(),
        Some("claude-opus-5-5")
    );
    match rx.recv().await.unwrap() {
        DaemonMessage::ThreadModelChanged { model, .. } => {
            assert_eq!(model.as_deref(), Some("claude-opus-5-5"))
        }
        other => panic!("{other:?}"),
    }
    let threads = fixture
        .manager
        .list_threads(&fixture.workspace)
        .await
        .unwrap();
    assert_eq!(threads[0].model.as_deref(), Some("claude-opus-5-5"));

    run_turn(&fixture, &tid, "one", PromptContext::default()).await;
    // Same model: the live session is reused.
    run_turn(&fixture, &tid, "two", PromptContext::default()).await;
    assert_eq!(configs(&fixture.fake).len(), 1);
    assert_eq!(
        configs(&fixture.fake)[0].model.as_deref(),
        Some("claude-opus-5-5")
    );

    fixture
        .manager
        .set_thread_model(&tid, Some("claude-haiku-5-5".into()))
        .await
        .unwrap();
    run_turn(&fixture, &tid, "three", PromptContext::default()).await;
    let started = configs(&fixture.fake);
    assert_eq!(started.len(), 2, "the session restarts for the new model");
    assert_eq!(started[1].model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(
        started[1].resume_external_session.as_deref(),
        Some("fake-ext-1"),
        "the conversation resumes"
    );

    fixture
        .manager
        .set_thread_model(&tid, Some("default".into()))
        .await
        .unwrap();
    run_turn(&fixture, &tid, "four", PromptContext::default()).await;
    let started = configs(&fixture.fake);
    assert_eq!(started.len(), 3);
    assert_eq!(
        started[2].model.as_deref(),
        Some(crate::session::DEFAULT_CLAUDE_MODEL),
        "back to the workspace default"
    );
    // Restarts were quiet: no "ended unexpectedly" notice in the transcript.
    let messages = fixture
        .manager
        .db
        .list_messages_by_thread(&tid)
        .await
        .unwrap();
    assert!(
        !messages
            .iter()
            .any(|m| m.text().contains("ended unexpectedly")),
        "{:?}",
        messages.iter().map(|m| m.text()).collect::<Vec<_>>()
    );
    assert_eq!(prompts(&fixture.fake), vec!["one", "two", "three", "four"]);

    // A subthread follows its parent's override unless it has its own.
    fixture
        .manager
        .set_thread_model(&tid, Some("claude-fable-5-1".into()))
        .await
        .unwrap();
    let parent = fixture.manager.db.get_thread(&tid).await.unwrap().unwrap();
    let child =
        Thread::new_subthread(&parent, trama_domain::thread::ThreadKind::Sub, None).unwrap();
    assert_eq!(
        fixture
            .manager
            .thread_model_override(&child)
            .await
            .unwrap()
            .as_deref(),
        Some("claude-fable-5-1")
    );
    assert!(
        fixture
            .manager
            .set_thread_model(&tid, Some("bad model".into()))
            .await
            .is_err()
    );
}

// ---- terminals --------------------------------------------------------------

#[test]
fn ansi_is_stripped_and_tails_are_bounded() {
    let raw =
        b"\x1b[1;31mred\x1b[0m plain\r\nprogress 10%\rprogress 100%\n\x1b]0;title\x07ab\x08c\n";
    assert_eq!(strip_ansi(raw), "red plain\nprogress 100%\nac\n");
    let text = (1..=10)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        tail_lines(&text, 3, 1000),
        ("line 8\nline 9\nline 10".into(), true)
    );
    assert_eq!(tail_lines(&text, 100, 15), ("line 9\nline 10".into(), true));
    assert_eq!(tail_lines("a\nb\n\n", 100, 100), ("a\nb".into(), false));
}

#[test]
fn terminal_env_drops_claude_code_markers() {
    let env = filter_env(vec![
        ("PATH".into(), "/bin".into()),
        ("CLAUDECODE".into(), "1".into()),
        ("CLAUDE_CODE_ENTRYPOINT".into(), "cli".into()),
        ("CLAUDE_CONFIG_DIR".into(), "/x".into()),
        ("TRAMA_APPROVAL_TOKEN".into(), "t".into()),
        ("TERM".into(), "dumb".into()),
    ]);
    let keys: Vec<_> = env
        .iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    assert_eq!(keys, vec!["PATH", "CLAUDE_CONFIG_DIR", "TERM"]);
    assert_eq!(env[2].1, "xterm-256color");
}

/// Output of `terminal_id` until it exits.
async fn until_exit(
    rx: &mut mpsc::UnboundedReceiver<DaemonMessage>,
    terminal_id: &str,
) -> (String, TerminalExitStatus, Vec<u64>) {
    let mut bytes = Vec::new();
    let mut seqs = Vec::new();
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("terminal event")
            .expect("channel open");
        match message {
            DaemonMessage::TerminalOutput {
                terminal_id: id,
                data,
                seq,
                replay,
                ..
            } if id == terminal_id => {
                assert!(!replay);
                seqs.push(seq);
                bytes.extend(
                    base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .unwrap(),
                );
            }
            DaemonMessage::TerminalExited {
                terminal_id: id,
                status,
                ..
            } if id == terminal_id => {
                return (String::from_utf8_lossy(&bytes).into_owned(), status, seqs);
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn terminal_runs_in_the_checkout_and_its_snapshot_attaches_to_a_prompt() {
    let fixture = fixture().await;
    let tid = thread(&fixture, "feature").await;
    let (_, path) = checkout(&fixture, &tid).await;
    let mut rx = subscribed(&fixture.manager, &tid).await;
    let command = "pwd -P; printf 'hello \\033[31mred\\033[0m\\n'; exit 3";
    let terminal = fixture
        .manager
        .open_terminal(&tid, None, Some(command.into()), 80, 24)
        .await
        .unwrap();
    assert_eq!(terminal.cwd, path);
    assert!(matches!(
        rx.recv().await.unwrap(),
        DaemonMessage::TerminalOpened { .. }
    ));
    let (output, status, seqs) = until_exit(&mut rx, &terminal.terminal_id).await;
    assert_eq!(
        status,
        TerminalExitStatus {
            code: Some(3),
            signal: None
        }
    );
    assert!(output.contains(&path.display().to_string()), "{output:?}");
    assert!(
        output.contains("\x1b[31mred"),
        "raw bytes are kept: {output:?}"
    );
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());

    let listed = fixture.manager.list_terminals(&tid);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].exit_status, Some(status));
    assert_eq!(listed[0].last_seq, *seqs.last().unwrap());
    match fixture
        .manager
        .replay_terminal(&terminal.terminal_id)
        .unwrap()
    {
        DaemonMessage::TerminalOutput { data, replay, .. } => {
            assert!(replay);
            let replayed = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&replayed), output);
        }
        other => panic!("{other:?}"),
    }

    run_turn(
        &fixture,
        &tid,
        "why did it fail?",
        PromptContext {
            mentions: vec![],
            terminals: vec![terminal.terminal_id.clone()],
        },
    )
    .await;
    let sent = prompts(&fixture.fake).pop().unwrap();
    assert_eq!(
        sent,
        format!(
            "<terminal-output command=\"pwd -P; printf 'hello \\033[31mred\\033[0m\\n'; exit 3\" cwd=\"{root}\" status=\"exited with code 3\">\n{root}\nhello red\n</terminal-output>\n\nwhy did it fail?",
            root = path.display()
        )
    );

    // Another thread cannot attach this terminal.
    let other = thread(&fixture, "other").await;
    let result = fixture
        .manager
        .send_prompt_with_context(
            other,
            "x".into(),
            PromptContext {
                mentions: vec![],
                terminals: vec![terminal.terminal_id.clone()],
            },
        )
        .await;
    assert!(matches!(result, Err(DaemonError::Invalid(_))), "{result:?}");
    fixture
        .manager
        .close_terminal(&terminal.terminal_id)
        .unwrap();
    assert!(fixture.manager.list_terminals(&tid).is_empty());
    assert!(
        fixture
            .manager
            .close_terminal(&terminal.terminal_id)
            .is_err()
    );
}

#[tokio::test]
async fn terminal_echoes_input_resizes_and_closes() {
    let fixture = fixture().await;
    let tid = thread(&fixture, "feature").await;
    let mut rx = subscribed(&fixture.manager, &tid).await;
    let terminal = fixture
        .manager
        .open_terminal(
            &tid,
            None,
            Some("read line; echo \"got:$line\"; stty size".into()),
            80,
            24,
        )
        .await
        .unwrap();
    fixture
        .manager
        .resize_terminal(&terminal.terminal_id, 100, 30)
        .unwrap();
    fixture
        .manager
        .terminal_input(&terminal.terminal_id, "hi\n".into())
        .await
        .unwrap();
    let (output, status, _) = until_exit(&mut rx, &terminal.terminal_id).await;
    assert_eq!(status.code, Some(0));
    assert!(output.contains("got:hi"), "{output:?}");
    assert!(output.contains("30 100"), "{output:?}");
    assert_eq!(
        (
            fixture.manager.list_terminals(&tid)[0].cols,
            fixture.manager.list_terminals(&tid)[0].rows
        ),
        (100, 30)
    );
    assert!(
        fixture
            .manager
            .terminal_input(&terminal.terminal_id, "late\n".into())
            .await
            .is_err(),
        "an exited terminal takes no input"
    );

    // A background terminal keeps running until it is closed.
    let sleeper = fixture
        .manager
        .open_terminal(&tid, None, Some("sleep 30".into()), 80, 24)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        super::terminal::exit_of(&fixture.manager, &sleeper.terminal_id),
        None
    );
    fixture
        .manager
        .close_terminal(&sleeper.terminal_id)
        .unwrap();
    let (_, status, _) = until_exit(&mut rx, &sleeper.terminal_id).await;
    assert_eq!(status.code, None, "killed: {status:?}");
    assert!(status.signal.is_some());

    // Shutdown kills whatever is left.
    let left = fixture
        .manager
        .open_terminal(&tid, None, Some("sleep 30".into()), 80, 24)
        .await
        .unwrap();
    fixture.manager.shutdown_terminals();
    let (_, status, _) = until_exit(&mut rx, &left.terminal_id).await;
    assert_eq!(status.signal, Some(9));
    assert!(fixture.manager.list_terminals(&tid).is_empty());
    assert!(
        fixture
            .manager
            .open_terminal(&ThreadId::from("missing"), None, None, 80, 24)
            .await
            .is_err()
    );
}
