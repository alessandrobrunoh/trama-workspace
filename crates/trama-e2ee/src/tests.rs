use crate::*;

fn setup() -> (DeviceIdentity, ShareGroup, DeviceIdentity, ShareGroup) {
    let alice = DeviceIdentity::generate("device_alice").unwrap();
    let bob = DeviceIdentity::generate("device_bob").unwrap();
    let mut a = ShareGroup::create(&alice, "share_1").unwrap();
    let (pending, kp) = PendingJoin::new(&bob).unwrap();
    let change = a.add_members(&alice, &[&kp]).unwrap();
    let b = pending.join(&change.welcome.unwrap()).unwrap();
    (alice, a, bob, b)
}

#[test]
fn create_join_and_exchange() {
    let (alice, mut a, bob, mut b) = setup();
    assert_eq!(a.epoch(), 1);
    assert_eq!(b.epoch(), 1);
    assert_eq!(b.members().unwrap(), ["device_alice", "device_bob"]);
    assert_eq!(b.share_id().unwrap(), "share_1");

    let ct = a.encrypt(&alice, b"evt_1", b"hello").unwrap();
    assert!(!ct.windows(5).any(|w| w == b"hello"));
    let got = b.process(&ct, b"evt_1").unwrap();
    assert_eq!(
        got,
        Inbound::Application {
            sender: "device_alice".into(),
            plaintext: b"hello".to_vec()
        }
    );
    let back = b.encrypt(&bob, b"evt_2", b"hi").unwrap();
    assert!(
        matches!(a.process(&back, b"evt_2").unwrap(), Inbound::Application { sender, .. } if sender == "device_bob")
    );
}

#[test]
fn aad_mismatch_and_replay_rejected() {
    let (alice, mut a, _bob, mut b) = setup();
    let ct = a.encrypt(&alice, b"evt_1", b"x").unwrap();
    assert!(matches!(
        b.process(&ct, b"evt_other"),
        Err(E2eeError::AadMismatch)
    ));
    let ct2 = a.encrypt(&alice, b"evt_2", b"y").unwrap();
    b.process(&ct2, b"evt_2").unwrap();
    assert!(b.process(&ct2, b"evt_2").is_err(), "replay must fail");
}

#[test]
fn removed_member_cannot_read_new_epoch() {
    let (alice, mut a, _bob, mut b) = setup();
    let carol = DeviceIdentity::generate("device_carol").unwrap();
    let (pending, kp) = PendingJoin::new(&carol).unwrap();
    let add = a.add_members(&alice, &[&kp]).unwrap();
    let mut c = pending.join(add.welcome.as_ref().unwrap()).unwrap();
    assert_eq!(
        b.process(&add.commit, b"").unwrap(),
        Inbound::Commit {
            epoch: 2,
            still_member: true
        }
    );

    let rm = a.remove_member(&alice, "device_bob").unwrap();
    assert_eq!(rm.epoch, 3);
    assert_eq!(
        c.process(&rm.commit, b"").unwrap(),
        Inbound::Commit {
            epoch: 3,
            still_member: true
        }
    );
    assert_eq!(
        b.process(&rm.commit, b"").unwrap(),
        Inbound::Commit {
            epoch: 3,
            still_member: false
        }
    );

    let ct = a.encrypt(&alice, b"e", b"secret").unwrap();
    assert!(c.process(&ct, b"e").is_ok());
    assert!(b.process(&ct, b"e").is_err());
    assert!(matches!(
        a.remove_member(&alice, "nobody"),
        Err(E2eeError::UnknownMember)
    ));
}

#[test]
fn state_survives_snapshot_and_store() {
    let (alice, mut a, _bob, b) = setup();
    let store = MemoryEpochStore::default();
    store.save("share_1", &b.snapshot()).unwrap();
    drop(b);
    let mut b = ShareGroup::restore("share_1", &store.load("share_1").unwrap().unwrap()).unwrap();
    let ct = a.encrypt(&alice, b"e", b"after restart").unwrap();
    assert!(matches!(
        b.process(&ct, b"e").unwrap(),
        Inbound::Application { .. }
    ));
    assert!(ShareGroup::restore("share_1", b"garbage").is_err());
    assert!(ShareGroup::restore("other", &b.snapshot()).is_err());
}

#[test]
fn pending_join_and_identity_survive_restart() {
    let alice = DeviceIdentity::generate("device_alice").unwrap();
    let bob = DeviceIdentity::generate("device_bob").unwrap();
    let bob = DeviceIdentity::from_secret_bytes(&bob.to_secret_bytes().unwrap()).unwrap();
    let mut a = ShareGroup::create(&alice, "s").unwrap();
    let (pending, kp) = PendingJoin::new(&bob).unwrap();
    let pending = PendingJoin::restore(&pending.snapshot()).unwrap();
    let w = a.add_members(&alice, &[&kp]).unwrap().welcome.unwrap();
    let mut b = pending.join(&w).unwrap();
    let ct = b.encrypt(&bob, b"", b"ok").unwrap();
    assert!(a.process(&ct, b"").is_ok());
}

#[test]
fn malformed_inputs_rejected() {
    let alice = DeviceIdentity::generate("a").unwrap();
    let mut g = ShareGroup::create(&alice, "s").unwrap();
    assert!(matches!(
        g.add_members(&alice, &[b"nope"]),
        Err(E2eeError::Malformed(_))
    ));
    let (p, _) = PendingJoin::new(&alice).unwrap();
    assert!(p.join(b"nope").is_err());
}
