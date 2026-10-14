use super::*;

fn store() -> (tempfile::TempDir, ProviderStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = ProviderStore::open(dir.path());
    (dir, store)
}

fn ollama() -> ProviderConfig {
    ProviderConfig {
        id: String::new(),
        kind: ProviderKind::OpenAiCompatible,
        name: "Ollama (local)".into(),
        base_url: Some("http://localhost:11434/v1".into()),
        default_model: Some("llama3.1".into()),
        models: Vec::new(),
    }
}

#[test]
fn claude_code_is_builtin_default() {
    let (_dir, store) = store();
    let list = store.snapshots();
    assert_eq!(list.len(), 1);
    assert!(list[0].builtin && list[0].is_default);
    assert!(store.remove(CLAUDE_CODE_PROVIDER_ID).is_err());
    assert!(store.route(None).unwrap().is_none());
    assert!(store.route(Some("claude-opus-5-5")).unwrap().is_none());
}

#[test]
fn save_derives_id_and_keeps_key_out_of_config() {
    let (dir, store) = store();
    let id = store
        .save(
            ProviderConfig {
                id: String::new(),
                kind: ProviderKind::OpenAi,
                name: "OpenAI".into(),
                base_url: None,
                default_model: Some("gpt-4o".into()),
                models: Vec::new(),
            },
            ApiKeyUpdate::Set("sk-very-secret".into()),
        )
        .unwrap();
    assert_eq!(id, "openai");
    let config = std::fs::read_to_string(dir.path().join(PROVIDERS_FILE)).unwrap();
    assert!(!config.contains("sk-very-secret"));
    let snapshot = store
        .snapshots()
        .into_iter()
        .find(|p| p.config.id == id)
        .unwrap();
    assert!(snapshot.has_api_key);
    // Reopen: persisted.
    let reopened = ProviderStore::open(dir.path());
    assert_eq!(reopened.snapshots().len(), 2);
    store.remove(&id).unwrap();
    assert!(!dir.path().join(keys::KEY_FILE).exists());
}

#[test]
fn default_provider_routes_threads_without_override() {
    let (_dir, store) = store();
    let id = store.save(ollama(), ApiKeyUpdate::Keep).unwrap();
    assert_eq!(id, "ollama-local");
    store.set_default(&id).unwrap();
    assert_eq!(
        store.default_model().as_deref(),
        Some("ollama-local::llama3.1")
    );
    let route = store.route(None).unwrap().unwrap();
    assert_eq!(route.model, "llama3.1");
    // An explicit Claude model still goes to Claude Code.
    assert!(store.route(Some("claude-opus-5-5")).unwrap().is_none());
    let choices = store.model_choices();
    assert_eq!(choices[0].id, "ollama-local::llama3.1");
    store.remove(&id).unwrap();
    assert!(store.snapshots()[0].is_default);
    assert!(store.route(Some("ollama-local::llama3.1")).is_err());
}

#[test]
fn rejects_invalid_providers() {
    let (_dir, store) = store();
    let mut no_url = ollama();
    no_url.base_url = None;
    assert!(store.save(no_url, ApiKeyUpdate::Keep).is_err());
    let mut remote_http = ollama();
    remote_http.base_url = Some("http://example.com/v1".into());
    assert!(
        store
            .save(remote_http, ApiKeyUpdate::Set("k".into()))
            .is_err()
    );
    let mut claude = ollama();
    claude.kind = ProviderKind::ClaudeCode;
    assert!(store.save(claude, ApiKeyUpdate::Keep).is_err());
}
