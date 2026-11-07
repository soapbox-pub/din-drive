//! Drive key derivation, blinded tags, and node content encryption.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use nostr::nips::nip44::v2::{ConversationKey, decrypt_to_bytes, encrypt_to_bytes_with_nonce};
use rand::RngCore;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::Result;

/// The id of every drive's top-level folder.
pub const ROOT: &str = "root";

fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// Everything derived from a drive key (DK).
pub struct DriveKeys {
    dk: Zeroizing<[u8; 32]>,
    content: ConversationKey,
    tag_key: Zeroizing<[u8; 32]>,
    /// `blind("drive")`, the drive's `j` tag.
    pub j: String,
}

impl DriveKeys {
    pub fn new(dk: [u8; 32]) -> Self {
        let content = ConversationKey::new(hmac(&dk, b"blossom-drive/content"));
        let tag_key = Zeroizing::new(hmac(&dk, b"blossom-drive/tags"));
        let j = blind_with(&tag_key, "drive");
        Self {
            dk: Zeroizing::new(dk),
            content,
            tag_key,
            j,
        }
    }

    pub fn generate() -> Self {
        let mut dk = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut dk);
        Self::new(dk)
    }

    pub fn from_hex(hex_key: &str) -> Result<Self> {
        let bytes = hex::decode(hex_key)?;
        let dk: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("drive key must be 32 bytes"))?;
        Ok(Self::new(dk))
    }

    pub fn dk_hex(&self) -> String {
        hex::encode(*self.dk)
    }

    pub fn blind(&self, label: &str) -> String {
        blind_with(&self.tag_key, label)
    }

    /// The `y` tag of a node in folder `parent`.
    pub fn parent_tag(&self, parent: &str) -> String {
        self.blind(&format!("parent:{parent}"))
    }

    /// The `z` tag of a version event of node `id`.
    pub fn node_tag(&self, id: &str) -> String {
        self.blind(&format!("node:{id}"))
    }

    /// NIP-44 v2 encrypt with the content key.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let mut nonce = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let payload = encrypt_to_bytes_with_nonce(&self.content, plaintext.as_bytes(), nonce)?;
        Ok(B64.encode(payload))
    }

    /// NIP-44 v2 decrypt with the content key.
    pub fn decrypt(&self, payload: &str) -> Result<String> {
        let bytes = B64.decode(payload)?;
        let plaintext = decrypt_to_bytes(&self.content, &bytes)?;
        Ok(String::from_utf8(plaintext)?)
    }
}

fn blind_with(tag_key: &[u8; 32], label: &str) -> String {
    hex::encode(&hmac(tag_key, label.as_bytes())[..16])
}

/// A random id of `n` bytes, hex encoded.
pub fn random_id(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_deterministic_and_distinct() {
        let a = DriveKeys::new([7; 32]);
        let b = DriveKeys::new([7; 32]);
        let c = DriveKeys::new([8; 32]);
        assert_eq!(a.j, b.j);
        assert_ne!(a.j, c.j);
        assert_eq!(a.j.len(), 32);
        assert_eq!(a.parent_tag(ROOT), b.parent_tag(ROOT));
        assert_ne!(a.parent_tag("abc"), a.node_tag("abc"));
    }

    #[test]
    fn content_roundtrip() {
        let k = DriveKeys::generate();
        let ct = k.encrypt("{\"t\":\"dir\"}").unwrap();
        assert_eq!(k.decrypt(&ct).unwrap(), "{\"t\":\"dir\"}");
        assert!(DriveKeys::generate().decrypt(&ct).is_err());
    }
}
