//! Encrypted share objects: message slices and Git bundles. Each object
//! gets a fresh 32-byte key (ChaCha20-Poly1305); parts are sealed with a
//! nonce = part index and AAD = `content_id/part`, so parts cannot be
//! reordered or moved between objects. The key only travels inside the
//! MLS-sealed manifests. `ciphertext_hash` (SHA-256 over all parts) lets a
//! reader check the download before decrypting.

use base64::{Engine, engine::general_purpose::STANDARD};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use trama_collab_protocol::vnext::api::{MAX_OBJECT_PART_BYTES, MAX_OBJECT_PARTS, ObjectClass};
use trama_domain::ids::MessageId;

use super::super::transport::ShareClient;
use crate::error::{DaemonError, Result};

const TAG_LEN: usize = 16;
const PLAIN_PART: usize = MAX_OBJECT_PART_BYTES - TAG_LEN;
/// Largest plaintext object (Git bundle) this version exports.
pub(crate) const MAX_OBJECT_BYTES: usize = 256 * 1024 * 1024;

pub(crate) struct SealedObject {
    pub content_id: String,
    pub key: String,
    pub parts: Vec<Vec<u8>>,
    pub ciphertext_hash: String,
    pub byte_len: u64,
}

fn err(what: &str) -> DaemonError {
    DaemonError::Collab(format!("share object: {what}"))
}

fn nonce(part: u32) -> Nonce {
    let mut bytes = [0u8; NONCE_LEN];
    bytes[NONCE_LEN - 4..].copy_from_slice(&part.to_be_bytes());
    Nonce::assume_unique_for_key(bytes)
}

fn aad(content_id: &str, part: u32) -> String {
    format!("trama/object/v1\n{content_id}\n{part}")
}

pub(crate) fn hash_parts(parts: &[Vec<u8>]) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for p in parts {
        ctx.update(p);
    }
    ctx.finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn seal_object(plain: &[u8]) -> Result<SealedObject> {
    if plain.len() > MAX_OBJECT_BYTES {
        return Err(err("too large to export"));
    }
    let mut raw = [0u8; 32];
    SystemRandom::new().fill(&mut raw).map_err(|_| err("rng"))?;
    let key = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &raw).expect("32 bytes"));
    let content_id = format!("obj_{}", MessageId::generate());
    let mut parts = Vec::new();
    let chunks: Vec<&[u8]> = if plain.is_empty() {
        vec![&[][..]]
    } else {
        plain.chunks(PLAIN_PART).collect()
    };
    for (i, chunk) in chunks.into_iter().enumerate() {
        let i = u32::try_from(i).map_err(|_| err("too many parts"))?;
        let mut buf = chunk.to_vec();
        key.seal_in_place_append_tag(
            nonce(i),
            Aad::from(aad(&content_id, i).as_bytes()),
            &mut buf,
        )
        .map_err(|_| err("seal"))?;
        parts.push(buf);
    }
    Ok(SealedObject {
        ciphertext_hash: hash_parts(&parts),
        byte_len: plain.len() as u64,
        content_id,
        key: STANDARD.encode(raw),
        parts,
    })
}

pub(crate) fn open_object(
    content_id: &str,
    key: &str,
    parts: &[Vec<u8>],
    ciphertext_hash: &str,
) -> Result<Vec<u8>> {
    if hash_parts(parts) != ciphertext_hash {
        return Err(err("ciphertext hash mismatch"));
    }
    let raw = STANDARD
        .decode(key)
        .ok()
        .filter(|k| k.len() == 32)
        .ok_or_else(|| err("bad key"))?;
    let key = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &raw).expect("32 bytes"));
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let i = i as u32;
        let mut buf = part.clone();
        let plain = key
            .open_in_place(nonce(i), Aad::from(aad(content_id, i).as_bytes()), &mut buf)
            .map_err(|_| err("part does not authenticate"))?;
        out.extend_from_slice(plain);
    }
    Ok(out)
}

pub(crate) async fn upload(
    client: &ShareClient,
    share_id: &str,
    class: ObjectClass,
    object: &SealedObject,
) -> Result<()> {
    for (i, part) in object.parts.iter().enumerate() {
        client
            .put_part(share_id, &object.content_id, i as u32, class, part.clone())
            .await?;
    }
    Ok(())
}

pub(crate) async fn download(
    client: &ShareClient,
    share_id: &str,
    content_id: &str,
    parts: u32,
) -> Result<Vec<Vec<u8>>> {
    if parts == 0 || parts > MAX_OBJECT_PARTS {
        return Err(err("bad part count"));
    }
    let mut out = Vec::new();
    for i in 0..parts {
        out.push(client.get_part(share_id, content_id, i).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_round_trip_and_bind_parts() {
        let plain: Vec<u8> = (0..(PLAIN_PART + 10)).map(|i| i as u8).collect();
        let o = seal_object(&plain).unwrap();
        assert_eq!(o.parts.len(), 2);
        let back = open_object(&o.content_id, &o.key, &o.parts, &o.ciphertext_hash).unwrap();
        assert_eq!(back, plain);
        let swapped = vec![o.parts[1].clone(), o.parts[0].clone()];
        assert!(open_object(&o.content_id, &o.key, &swapped, &hash_parts(&swapped)).is_err());
        assert!(open_object("obj_other", &o.key, &o.parts, &o.ciphertext_hash).is_err());
        assert!(open_object(&o.content_id, &o.key, &o.parts, "00").is_err());
        let empty = seal_object(b"").unwrap();
        assert!(
            open_object(
                &empty.content_id,
                &empty.key,
                &empty.parts,
                &empty.ciphertext_hash
            )
            .unwrap()
            .is_empty()
        );
    }
}
