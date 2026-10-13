//! One share = one MLS group. Create, join via Welcome, change membership,
//! encrypt/decrypt share event payloads, and snapshot epoch state.

use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;

use crate::identity::{identity_of, parse_key_package};
use crate::provider::Provider;
use crate::{CIPHERSUITE, DeviceIdentity, E2eeError, Result};

/// Output of a membership change made locally. `commit` must be delivered to
/// the existing members, `welcome` to the added ones.
#[derive(Debug)]
pub struct MembershipChange {
    pub commit: Vec<u8>,
    pub welcome: Option<Vec<u8>>,
    /// Epoch after the change (the new `key_epoch` of share events).
    pub epoch: u64,
}

/// A processed inbound MLS message.
#[derive(Debug, PartialEq, Eq)]
pub enum Inbound {
    /// A decrypted share event payload, authenticated as `sender`.
    Application { sender: String, plaintext: Vec<u8> },
    /// A commit was merged; the group is now at `epoch`. `still_member` is
    /// false when the commit removed this device.
    Commit { epoch: u64, still_member: bool },
}

/// A device waiting for a Welcome. Holds the private half of the key package
/// it published; persist it with [`PendingJoin::snapshot`] while waiting.
pub struct PendingJoin {
    provider: Provider,
}

impl PendingJoin {
    /// Creates key package material. Returns the pending state and the
    /// public key package bytes to hand to the share owner.
    pub fn new(identity: &DeviceIdentity) -> Result<(Self, Vec<u8>)> {
        let provider = Provider::default();
        let kp = identity.key_package(&provider)?;
        Ok((Self { provider }, kp))
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.provider.snapshot()
    }

    pub fn restore(state: &[u8]) -> Result<Self> {
        Ok(Self {
            provider: Provider::restore(state)?,
        })
    }

    /// Joins the group from a Welcome (the ratchet tree travels in-band as a
    /// Welcome extension).
    pub fn join(self, welcome: &[u8]) -> Result<ShareGroup> {
        let msg = MlsMessageIn::tls_deserialize_exact(welcome)
            .map_err(|_| E2eeError::Malformed("welcome"))?;
        let MlsMessageBodyIn::Welcome(welcome) = msg.extract() else {
            return Err(E2eeError::Malformed("welcome"));
        };
        let config = MlsGroupJoinConfig::builder()
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .use_ratchet_tree_extension(true)
            .build();
        let group = StagedWelcome::new_from_welcome(&self.provider, &config, welcome, None)
            .map_err(E2eeError::mls)?
            .into_group(&self.provider)
            .map_err(E2eeError::mls)?;
        // Our key package pins the suite; re-check defensively anyway.
        if group.ciphersuite() != CIPHERSUITE {
            return Err(E2eeError::Ciphersuite);
        }
        Ok(ShareGroup {
            provider: self.provider,
            group,
        })
    }
}

pub struct ShareGroup {
    provider: Provider,
    group: MlsGroup,
}

impl ShareGroup {
    /// Creates the group for `share_id` with the owner device as sole member.
    pub fn create(identity: &DeviceIdentity, share_id: &str) -> Result<Self> {
        let provider = Provider::default();
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .use_ratchet_tree_extension(true)
            .build();
        let group = MlsGroup::new_with_group_id(
            &provider,
            &identity.signer,
            &config,
            GroupId::from_slice(share_id.as_bytes()),
            identity.credential.clone(),
        )
        .map_err(E2eeError::mls)?;
        Ok(Self { provider, group })
    }

    pub fn share_id(&self) -> Result<String> {
        String::from_utf8(self.group.group_id().as_slice().to_vec())
            .map_err(|_| E2eeError::Malformed("group id"))
    }

    /// Current epoch: the `key_epoch` to stamp on share events.
    pub fn epoch(&self) -> u64 {
        self.group.epoch().as_u64()
    }

    pub fn is_active(&self) -> bool {
        self.group.is_active()
    }

    /// Device ids of the current members, sorted.
    pub fn members(&self) -> Result<Vec<String>> {
        let mut ids = self
            .group
            .members()
            .map(|m| identity_of(&m.credential))
            .collect::<Result<Vec<_>>>()?;
        ids.sort();
        Ok(ids)
    }

    /// Adds devices from their key packages and merges the commit locally.
    ///
    /// Conservative v1: the commit is merged immediately; with a server that
    /// orders commits, a rejected commit means the caller restores the
    /// snapshot taken before this call.
    pub fn add_members(
        &mut self,
        identity: &DeviceIdentity,
        key_packages: &[&[u8]],
    ) -> Result<MembershipChange> {
        let kps = key_packages
            .iter()
            .map(|b| parse_key_package(&self.provider, b))
            .collect::<Result<Vec<_>>>()?;
        let (commit, welcome, _) = self
            .group
            .add_members(&self.provider, &identity.signer, &kps)
            .map_err(E2eeError::mls)?;
        self.finish(commit, Some(welcome))
    }

    /// Removes every leaf whose credential is `device_id`.
    pub fn remove_member(
        &mut self,
        identity: &DeviceIdentity,
        device_id: &str,
    ) -> Result<MembershipChange> {
        let mut leaves = Vec::new();
        for m in self.group.members() {
            if identity_of(&m.credential)? == device_id {
                leaves.push(m.index);
            }
        }
        if leaves.is_empty() {
            return Err(E2eeError::UnknownMember);
        }
        let (commit, welcome, _) = self
            .group
            .remove_members(&self.provider, &identity.signer, &leaves)
            .map_err(E2eeError::mls)?;
        self.finish(commit, welcome)
    }

    fn finish(
        &mut self,
        commit: MlsMessageOut,
        welcome: Option<MlsMessageOut>,
    ) -> Result<MembershipChange> {
        self.group
            .merge_pending_commit(&self.provider)
            .map_err(E2eeError::mls)?;
        Ok(MembershipChange {
            commit: encode(&commit)?,
            welcome: welcome.as_ref().map(encode).transpose()?,
            epoch: self.epoch(),
        })
    }

    /// Encrypts a share event payload. `aad` must be the canonical encoding
    /// of the event's plaintext routing metadata, so the server cannot move
    /// a ciphertext to another event.
    pub fn encrypt(
        &mut self,
        identity: &DeviceIdentity,
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>> {
        self.group.set_aad(aad.to_vec());
        let out = self
            .group
            .create_message(&self.provider, &identity.signer, plaintext);
        self.group.set_aad(Vec::new());
        encode(&out.map_err(E2eeError::mls)?)
    }

    /// Processes an inbound message. Application messages must carry exactly
    /// `expected_aad`; commits are merged (they carry no event metadata).
    pub fn process(&mut self, message: &[u8], expected_aad: &[u8]) -> Result<Inbound> {
        let msg = MlsMessageIn::tls_deserialize_exact(message)
            .map_err(|_| E2eeError::Malformed("mls message"))?;
        let protocol = msg
            .try_into_protocol_message()
            .map_err(|_| E2eeError::Unexpected("not a protocol message"))?;
        let processed = self
            .group
            .process_message(&self.provider, protocol)
            .map_err(E2eeError::mls)?;
        let sender = identity_of(processed.credential())?;
        let aad_ok = processed.aad() == expected_aad;
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                if !aad_ok {
                    return Err(E2eeError::AadMismatch);
                }
                Ok(Inbound::Application {
                    sender,
                    plaintext: app.into_bytes(),
                })
            }
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                self.group
                    .merge_staged_commit(&self.provider, *staged)
                    .map_err(E2eeError::mls)?;
                Ok(Inbound::Commit {
                    epoch: self.epoch(),
                    still_member: self.group.is_active(),
                })
            }
            // Standalone proposals are not used in v1: only commits by value.
            _ => Err(E2eeError::Unexpected("standalone proposal")),
        }
    }

    /// Secret snapshot of this group's state; see [`crate::EpochStateStore`].
    pub fn snapshot(&self) -> Vec<u8> {
        self.provider.snapshot()
    }

    pub fn restore(share_id: &str, state: &[u8]) -> Result<Self> {
        let provider = Provider::restore(state)?;
        let group = MlsGroup::load(
            provider.storage(),
            &GroupId::from_slice(share_id.as_bytes()),
        )
        .map_err(E2eeError::mls)?
        .ok_or_else(|| E2eeError::State("group not in snapshot".into()))?;
        Ok(Self { provider, group })
    }
}

fn encode(msg: &MlsMessageOut) -> Result<Vec<u8>> {
    msg.tls_serialize_detached().map_err(E2eeError::mls)
}
