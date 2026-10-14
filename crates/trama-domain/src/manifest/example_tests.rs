//! Keeps `docs/plan/examples/share-manifest.example.json` in sync with the
//! manifest types: it must parse, verify and validate as a real pair of
//! signed envelopes. Regenerate with
//! `cargo test -p trama-domain regenerate_share_manifest_example -- --ignored`.

use super::*;
use crate::checkout_state::{CheckoutMode, HeadRef, ObjectFormat};
use crate::share::{DeviceId, RepoGid, ShareId, ShareScope, SharedThreadKind, ThreadGid};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::VerifyingKey;
use serde_json::{Value, json};

const EXAMPLE: &str = include_str!("../../../../docs/plan/examples/share-manifest.example.json");
const EXAMPLE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/plan/examples/share-manifest.example.json"
);
/// Public test-only seed. Never use it for a real device key.
const DEMO_SEED: [u8; 32] = [7; 32];
const KEY_ID: &str = "key_alice_demo_1";
const DEVICE: &str = "device_alice_demo";

fn oid(c: char) -> String {
    std::iter::repeat_n(c, 40).collect()
}

fn thread(gid: &str, parent: Option<&str>, kind: SharedThreadKind, title: &str) -> ManifestThread {
    ManifestThread {
        thread_gid: ThreadGid::from(gid),
        parent_gid: parent.map(ThreadGid::from),
        kind,
        title: title.into(),
        message_slice: Some(format!("slice_{gid}")),
        fork_origin: parent.map(|p| ForkOrigin {
            parent_thread_gid: ThreadGid::from(p),
            checkpoint_id: format!("checkpoint_{gid}"),
        }),
    }
}

fn history() -> HistoryManifestV1 {
    HistoryManifestV1 {
        schema_version: MANIFEST_SCHEMA_VERSION,
        share_id: ShareId::from("share_demo_payments_01"),
        snapshot_version: 4,
        scope: ShareScope::Subtree,
        root_thread_gids: vec![ThreadGid::from("thread_main")],
        include_future_descendants: true,
        created_at: "2026-10-13T08:00:00Z".into(),
        publisher_device_id: DeviceId::from(DEVICE),
        history_cursor: "cursor_demo_220".into(),
        threads: vec![
            ManifestThread {
                fork_origin: None,
                ..thread("thread_main", None, SharedThreadKind::Main, "Main Thread")
            },
            thread(
                "thread_auth",
                Some("thread_main"),
                SharedThreadKind::Child,
                "Auth",
            ),
            thread(
                "thread_review",
                Some("thread_auth"),
                SharedThreadKind::Review,
                "Auth Review",
            ),
        ],
    }
}

fn repo(gid: &str, alias: &str) -> ManifestRepository {
    ManifestRepository {
        repo_gid: RepoGid::from(gid),
        alias: alias.into(),
        origin_hint: Some(format!("git@github.com:acme/payments-{alias}.git")),
        origin_identity_hint: Some(format!("acme/payments-{alias}")),
        git_object_format: ObjectFormat::Sha1,
        requires_git_clone_permission: true,
        lfs_policy: LfsPolicy::FetchFromOriginIfRequired,
        submodule_policy: SubmodulePolicy::ExplicitConsentRequired,
    }
}

#[allow(clippy::too_many_arguments)]
fn checkout(
    thread: &str,
    repo: &str,
    mode: CheckoutMode,
    generation: u64,
    c: char,
    head: HeadRef,
    fork: bool,
    objects: &[&str],
) -> ManifestCheckout {
    ManifestCheckout {
        thread_gid: ThreadGid::from(thread),
        repo_gid: RepoGid::from(repo),
        checkout_mode: mode,
        generation,
        base_commit: Some(oid('0')),
        head_commit: oid(c),
        index_tree: oid(c),
        working_tree: oid(c),
        head,
        fork_point: fork.then(|| oid('f')),
        object_index_refs: objects.iter().map(|o| o.to_string()).collect(),
    }
}

fn object(id: &str, n: u8, len: u64) -> ManifestObject {
    ManifestObject {
        content_id: id.into(),
        ciphertext_hash: format!("sha256:{}", format!("{n:02x}").repeat(32)),
        byte_len: len,
        key_epoch: 3,
        parts: vec![format!("chunk_{n:03}")],
        object_key: None,
    }
}

fn content() -> ContentManifestV1 {
    let main = || HeadRef::Branch {
        name: "main".into(),
    };
    ContentManifestV1 {
        schema_version: MANIFEST_SCHEMA_VERSION,
        share_id: ShareId::from("share_demo_payments_01"),
        snapshot_version: 4,
        history_manifest_digest: String::new(),
        created_at: "2026-10-13T08:00:00Z".into(),
        publisher_device_id: DeviceId::from(DEVICE),
        repositories: vec![
            repo("repo_backend", "backend"),
            repo("repo_frontend", "frontend"),
        ],
        checkouts: vec![
            checkout(
                "thread_main",
                "repo_backend",
                CheckoutMode::Owned,
                4,
                'a',
                main(),
                false,
                &["content_backend_001"],
            ),
            checkout(
                "thread_main",
                "repo_frontend",
                CheckoutMode::Owned,
                4,
                'b',
                main(),
                false,
                &["content_frontend_001"],
            ),
            checkout(
                "thread_auth",
                "repo_backend",
                CheckoutMode::Owned,
                2,
                'c',
                HeadRef::Branch {
                    name: "feature-auth".into(),
                },
                true,
                &["content_backend_001", "content_backend_002"],
            ),
            checkout(
                "thread_review",
                "repo_backend",
                CheckoutMode::Alias {
                    owner: ThreadGid::from("thread_auth"),
                },
                1,
                'c',
                HeadRef::Detached,
                true,
                &["content_backend_002"],
            ),
        ],
        object_index: vec![
            object("content_backend_001", 1, 1024),
            object("content_backend_002", 2, 2048),
            object("content_frontend_001", 4, 512),
        ],
        content_policy: ContentPolicySummary {
            ignored_files_exported: false,
            untracked: "review_required".into(),
            excluded_path_classes: vec![
                "secrets".into(),
                "build_cache".into(),
                "machine_local".into(),
            ],
            max_bytes: Some(512 * 1024 * 1024),
        },
    }
}

fn keyring_from(doc: &Value) -> Keyring {
    let key = &doc["signing_key"];
    let bytes: [u8; 32] = STANDARD
        .decode(key["public_key"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let mut ring = Keyring::default();
    ring.insert(
        key["key_id"].as_str().unwrap(),
        DeviceId::from(key["device_id"].as_str().unwrap()),
        VerifyingKey::from_bytes(&bytes).unwrap(),
    );
    ring
}

fn build() -> Value {
    let key = SigningKey::from_bytes(&DEMO_SEED);
    let history_env = seal_history(&history(), KEY_ID, &key).unwrap();
    let content_env = seal_content(content(), &history_env, KEY_ID, &key).unwrap();
    let decoded_content: ContentManifestV1 =
        canonical::from_canonical_bytes(&content_env.payload_bytes).unwrap();
    json!({
        "$comment": "ILLUSTRATIVE EXAMPLE. Signed with a public test-only Ed25519 seed ([7; 32]); never trust this key. OIDs are placeholders. `payload_bytes` is the canonical JSON shown under `decoded`; encryption (MLS) is M4 and not shown. Regenerate with `cargo test -p trama-domain regenerate_share_manifest_example -- --ignored`.",
        "signing_key": {
            "key_id": KEY_ID,
            "device_id": DEVICE,
            "alg": "ed25519",
            "public_key": STANDARD.encode(key.verifying_key().to_bytes()),
        },
        "history_envelope": history_env,
        "content_envelope": content_env,
        "decoded": {
            "history": history(),
            "content": decoded_content,
        }
    })
}

#[test]
#[ignore = "rewrites the docs example"]
fn regenerate_share_manifest_example() {
    let text = serde_json::to_string_pretty(&build()).unwrap() + "\n";
    std::fs::write(EXAMPLE_PATH, text).unwrap();
}

#[test]
fn share_manifest_example_verifies_and_matches_code_types() {
    let doc: Value = serde_json::from_str(EXAMPLE).unwrap();
    let ring = keyring_from(&doc);
    let history_env: SignedEnvelope =
        serde_json::from_value(doc["history_envelope"].clone()).unwrap();
    let content_env: SignedEnvelope =
        serde_json::from_value(doc["content_envelope"].clone()).unwrap();
    let opened_history = open_history(&history_env, &ring, Some(4)).unwrap();
    let opened_content = open_content(&content_env, &ring, &history_env, &opened_history).unwrap();
    let decoded_history: HistoryManifestV1 =
        serde_json::from_value(doc["decoded"]["history"].clone()).unwrap();
    let decoded_content: ContentManifestV1 =
        serde_json::from_value(doc["decoded"]["content"].clone()).unwrap();
    assert_eq!(decoded_history, opened_history);
    assert_eq!(decoded_content, opened_content);
    // No `plain` kind and no single combined manifest.
    assert!(!EXAMPLE.contains("\"plain\""));
    assert!(doc.get("signed_envelope").is_none());
    // The checked-in file is exactly what the code produces.
    assert_eq!(doc, build(), "example is stale; regenerate it");
}
