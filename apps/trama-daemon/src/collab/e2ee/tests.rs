use std::sync::Arc;

use trama_collab_protocol::vnext::cipher::{PayloadCipher, PayloadKind};
use trama_db::TramaDb;
use trama_e2ee::ShareGroup;

use super::*;

fn e2ee(db: &TramaDb, key: u8, device: &str) -> E2ee {
    let key = Arc::new(AtRestKey::from_bytes([key; 32], KeySource::File));
    E2ee::with_key(db.clone(), key, device).unwrap()
}

#[test]
fn sealed_blobs_bind_their_name_and_key() {
    let a = AtRestKey::from_bytes([1; 32], KeySource::File);
    let b = AtRestKey::from_bytes([2; 32], KeySource::File);
    let sealed = a.seal("x", b"secret").unwrap();
    assert_eq!(a.open("x", &sealed).unwrap(), b"secret");
    assert!(a.open("y", &sealed).is_err());
    assert!(b.open("x", &sealed).is_err());
}

#[test]
fn file_fallback_key_is_stable() {
    let dir = tempfile::tempdir().unwrap();
    // Pre-create the file so the keychain is never touched in tests.
    std::fs::write(dir.path().join(at_rest::KEY_FILE), base64_key()).unwrap();
    let k1 = AtRestKey::load_or_create(dir.path()).unwrap();
    let k2 = AtRestKey::load_or_create(dir.path()).unwrap();
    assert_eq!(k1.source, KeySource::File);
    let sealed = k1.seal("n", b"v").unwrap();
    assert_eq!(k2.open("n", &sealed).unwrap(), b"v");
}

fn base64_key() -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    STANDARD.encode([9u8; 32])
}

#[test]
fn identity_and_groups_survive_reopen_and_join_works() {
    let owner_db = TramaDb::open_in_memory().unwrap();
    let joiner_db = TramaDb::open_in_memory().unwrap();
    let owner = e2ee(&owner_db, 1, "dev_a");
    let joiner = e2ee(&joiner_db, 2, "dev_b");
    owner
        .cipher
        .insert(ShareGroup::create(&owner.identity, "sh1").unwrap())
        .unwrap();
    let kp = joiner.begin_join("sh1").unwrap();
    let change = owner
        .cipher
        .with_group("sh1", |g, id| g.add_members(id, &[&kp]))
        .unwrap();
    let epoch = joiner
        .finish_join("sh1", change.welcome.as_ref().unwrap())
        .unwrap();
    assert_eq!(epoch, change.epoch);

    let sealed = owner
        .cipher
        .seal("sh1", epoch, PayloadKind::Event, b"hello")
        .unwrap();
    // Reopen the joiner from its database: identity and group persist.
    drop(joiner);
    let joiner = e2ee(&joiner_db, 2, "ignored");
    assert_eq!(joiner.device_id(), "dev_b");
    assert!(joiner.has_group("sh1"));
    let plain = joiner
        .cipher
        .open("sh1", epoch, PayloadKind::Event, &sealed)
        .unwrap();
    assert_eq!(plain, b"hello");
    // A wrong at-rest key cannot read the stored state.
    assert!(
        E2ee::with_key(
            joiner_db,
            Arc::new(AtRestKey::from_bytes([3; 32], KeySource::File)),
            "dev_b"
        )
        .is_err()
    );
}
