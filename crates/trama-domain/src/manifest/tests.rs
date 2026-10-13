use super::canonical::{CanonicalError, from_canonical_bytes, to_canonical_bytes};
use super::*;
use crate::checkout_state::{CheckoutMode, HeadRef, ObjectFormat};
use crate::share::{DeviceId, RepoGid, ShareId, ShareScope, SharedThreadKind, ThreadGid};
use serde_json::{Value, json};

const OID: &str = "0123456789abcdef0123456789abcdef01234567";

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn keyring() -> Keyring {
    let mut ring = Keyring::default();
    ring.insert(
        "k_alice",
        DeviceId::from("dev_alice"),
        key(1).verifying_key(),
    );
    ring.insert(
        "k_mallory",
        DeviceId::from("dev_mallory"),
        key(2).verifying_key(),
    );
    ring
}

fn thread(gid: &str, parent: Option<&str>) -> ManifestThread {
    ManifestThread {
        thread_gid: ThreadGid::from(gid),
        parent_gid: parent.map(ThreadGid::from),
        kind: SharedThreadKind::Main,
        title: format!("Thread {gid} \u{e8}\n"),
        message_slice: None,
        fork_origin: None,
    }
}

fn history() -> HistoryManifestV1 {
    HistoryManifestV1 {
        schema_version: 1,
        share_id: ShareId::from("share_1"),
        snapshot_version: 4,
        scope: ShareScope::Subtree,
        root_thread_gids: vec![ThreadGid::from("t_main")],
        include_future_descendants: true,
        created_at: "2026-10-13T08:00:00Z".into(),
        publisher_device_id: DeviceId::from("dev_alice"),
        history_cursor: "cursor_220".into(),
        threads: vec![thread("t_main", None), thread("t_auth", Some("t_main"))],
    }
}

fn content() -> ContentManifestV1 {
    ContentManifestV1 {
        schema_version: 1,
        share_id: ShareId::from("share_1"),
        snapshot_version: 4,
        history_manifest_digest: String::new(),
        created_at: "2026-10-13T08:00:00Z".into(),
        publisher_device_id: DeviceId::from("dev_alice"),
        repositories: vec![ManifestRepository {
            repo_gid: RepoGid::from("repo_backend"),
            alias: "backend".into(),
            origin_hint: Some("git@github.com:acme/payments-backend.git".into()),
            origin_identity_hint: Some("acme/payments-backend".into()),
            git_object_format: ObjectFormat::Sha1,
            requires_git_clone_permission: true,
            lfs_policy: LfsPolicy::FetchFromOriginIfRequired,
            submodule_policy: SubmodulePolicy::ExplicitConsentRequired,
        }],
        checkouts: vec![ManifestCheckout {
            thread_gid: ThreadGid::from("t_auth"),
            repo_gid: RepoGid::from("repo_backend"),
            checkout_mode: CheckoutMode::Owned,
            generation: 2,
            base_commit: Some(OID.into()),
            head_commit: OID.into(),
            index_tree: OID.into(),
            working_tree: OID.into(),
            head: HeadRef::Branch {
                name: "feature-auth".into(),
            },
            fork_point: None,
            object_index_refs: vec!["c_1".into()],
        }],
        object_index: vec![ManifestObject {
            content_id: "c_1".into(),
            ciphertext_hash: "h".into(),
            byte_len: 1024,
            key_epoch: 3,
            parts: vec!["chunk_1".into()],
            object_key: None,
        }],
        content_policy: ContentPolicySummary {
            ignored_files_exported: false,
            untracked: "review_required".into(),
            excluded_path_classes: vec!["secrets".into()],
            max_bytes: None,
        },
    }
}

#[test]
fn canonical_encoding_is_sorted_compact_and_strict() {
    let value = json!({"b": [1, -2, {"z": null, "a": true}], "a": "x\u{1}\"é"});
    let bytes = to_canonical_bytes(&value).unwrap();
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        r#"{"a":"x\u0001\"é","b":[1,-2,{"a":true,"z":null}]}"#
    );
    assert_eq!(from_canonical_bytes::<Value>(&bytes).unwrap(), value);
    for bad in [
        &br#"{"b":1,"a":2}"#[..],
        br#"{"a": 1}"#,
        br#"{"a":1,"a":1}"#,
        br#"{"a":"\u0041"}"#,
    ] {
        assert_eq!(
            from_canonical_bytes::<Value>(bad),
            Err(CanonicalError::NotCanonical)
        );
    }
    assert_eq!(
        to_canonical_bytes(&json!({"a": 1.5})),
        Err(CanonicalError::Float)
    );
    assert!(matches!(
        to_canonical_bytes(&json!({"é": 1})),
        Err(CanonicalError::NonAsciiKey(_))
    ));
}

#[test]
fn history_and_content_roundtrip() {
    let ring = keyring();
    let h_env = seal_history(&history(), "k_alice", &key(1)).unwrap();
    let c_env = seal_content(content(), &h_env, "k_alice", &key(1)).unwrap();
    let h = open_history(&h_env, &ring, Some(4)).unwrap();
    assert_eq!(h, history());
    let c = open_content(&c_env, &ring, &h_env, &h).unwrap();
    assert_eq!(c.history_manifest_digest, payload_digest(&h_env));
    // Envelope JSON survives a wire roundtrip.
    let wire = serde_json::to_string(&c_env).unwrap();
    let back: SignedEnvelope = serde_json::from_str(&wire).unwrap();
    assert_eq!(back, c_env);
}

#[test]
fn signatures_are_deterministic_and_type_bound() {
    let a = seal_history(&history(), "k_alice", &key(1)).unwrap();
    let b = seal_history(&history(), "k_alice", &key(1)).unwrap();
    assert_eq!(a, b);
    let mut retyped = a.clone();
    retyped.payload_type = CONTENT_PAYLOAD_TYPE.into();
    assert!(open_content(&retyped, &keyring(), &a, &history()).is_err());
}

#[test]
fn tampering_and_wrong_keys_are_rejected() {
    let ring = keyring();
    let env = seal_history(&history(), "k_alice", &key(1)).unwrap();
    let mut tampered = env.clone();
    tampered.payload_bytes = to_canonical_bytes(&HistoryManifestV1 {
        snapshot_version: 5,
        ..history()
    })
    .unwrap();
    assert!(matches!(
        open_history(&tampered, &ring, None),
        Err(ManifestError::Envelope(EnvelopeError::BadSignature))
    ));
    let mut unknown = env.clone();
    unknown.key_id = "k_nobody".into();
    assert!(matches!(
        open_history(&unknown, &ring, None),
        Err(ManifestError::Envelope(EnvelopeError::UnknownKey(_)))
    ));
    assert!(open_history(&env, &Keyring::default(), None).is_err());
    // Mallory signs a manifest claiming Alice as publisher.
    let forged = seal_history(&history(), "k_mallory", &key(2)).unwrap();
    assert_eq!(
        open_history(&forged, &ring, None),
        Err(ManifestError::PublisherMismatch)
    );
}

#[test]
fn rollback_and_schema_are_enforced() {
    let ring = keyring();
    let env = seal_history(&history(), "k_alice", &key(1)).unwrap();
    assert_eq!(
        open_history(&env, &ring, Some(5)),
        Err(ManifestError::Rollback {
            pinned: 5,
            found: 4
        })
    );
    let future = HistoryManifestV1 {
        schema_version: 2,
        ..history()
    };
    assert_eq!(future.validate(), Err(ManifestError::UnsupportedSchema(2)));
}

#[test]
fn content_must_be_bound_to_its_history() {
    let ring = keyring();
    let h_env = seal_history(&history(), "k_alice", &key(1)).unwrap();
    let other = HistoryManifestV1 {
        history_cursor: "other".into(),
        ..history()
    };
    let other_env = seal_history(&other, "k_alice", &key(1)).unwrap();
    let c_env = seal_content(content(), &other_env, "k_alice", &key(1)).unwrap();
    assert_eq!(
        open_content(&c_env, &ring, &h_env, &history()),
        Err(ManifestError::Unbound)
    );

    let mut outside = content();
    outside.checkouts[0].thread_gid = ThreadGid::from("t_secret");
    let c_env = seal_content(outside, &h_env, "k_alice", &key(1)).unwrap();
    assert!(matches!(
        open_content(&c_env, &ring, &h_env, &history()),
        Err(ManifestError::Invalid(_))
    ));
}

#[test]
fn history_structure_is_validated() {
    let mut orphan = history();
    orphan.threads.push(thread("t_x", None));
    assert!(orphan.validate().is_err());
    let mut dangling = history();
    dangling.threads.push(thread("t_y", Some("t_missing")));
    assert!(dangling.validate().is_err());
    let mut root_with_parent = history();
    root_with_parent.threads[0].parent_gid = Some(ThreadGid::from("t_auth"));
    assert!(root_with_parent.validate().is_err());
    let only = HistoryManifestV1 {
        scope: ShareScope::ThreadOnly,
        ..history()
    };
    assert!(only.validate().is_err());
}

#[test]
fn content_structure_is_validated() {
    let digest = "d".to_string();
    let base = ContentManifestV1 {
        history_manifest_digest: digest,
        ..content()
    };
    assert_eq!(base.validate(), Ok(()));
    for mutate in [
        (|c: &mut ContentManifestV1| c.repositories[0].origin_hint = Some("/home/a/repo".into()))
            as fn(&mut ContentManifestV1),
        |c| c.repositories[0].origin_hint = Some("C:\\src\\repo".into()),
        |c| c.repositories[0].origin_hint = Some("file:///src".into()),
        |c| c.checkouts[0].head_commit = "abc".into(),
        |c| c.checkouts[0].repo_gid = RepoGid::from("repo_x"),
        |c| c.checkouts[0].object_index_refs.push("c_missing".into()),
        |c| c.object_index[0].parts.clear(),
        |c| c.repositories.push(c.repositories[0].clone()),
    ] {
        let mut bad = base.clone();
        mutate(&mut bad);
        assert!(bad.validate().is_err(), "{bad:?}");
    }
}

#[test]
fn unknown_fields_are_rejected() {
    let mut value = serde_json::to_value(history()).unwrap();
    value["publisher_signature"] = json!("x");
    assert!(serde_json::from_value::<HistoryManifestV1>(value).is_err());
}
