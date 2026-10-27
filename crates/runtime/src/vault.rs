//! Encryption of stored secrets with the runtime's master key (ADR 21).

use anyhow::{Context, bail};
use base64::Engine;
use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

const NONCE: usize = 24;

pub struct Vault {
    cipher: XChaCha20Poly1305,
}

impl Vault {
    /// The key is 32 bytes, base64-encoded, e.g. from `openssl rand -base64 32`.
    pub fn from_base64(key: &str) -> anyhow::Result<Self> {
        let key = base64::engine::general_purpose::STANDARD
            .decode(key.trim())
            .context("the master key isn't valid base64")?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key)
            .map_err(|_| anyhow::anyhow!("the master key must be 32 bytes"))?;
        Ok(Self { cipher })
    }

    /// Returns the nonce followed by the ciphertext.
    pub fn encrypt(&self, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let nonce = XNonce::generate();
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        let mut sealed = nonce.to_vec();
        sealed.extend(ciphertext);
        Ok(sealed)
    }

    pub fn decrypt(&self, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        if sealed.len() < NONCE {
            bail!("sealed secret too short");
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE);
        let nonce = XNonce::try_from(nonce).map_err(|_| anyhow::anyhow!("bad nonce"))?;
        self.cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| anyhow::anyhow!("decryption failed; was the master key changed?"))
    }

    pub fn decrypt_string(&self, sealed: &[u8]) -> anyhow::Result<String> {
        Ok(String::from_utf8(self.decrypt(sealed)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let vault =
            Vault::from_base64(&base64::engine::general_purpose::STANDARD.encode([7u8; 32]))
                .unwrap();
        let sealed = vault.encrypt(b"secret").unwrap();
        assert_ne!(&sealed[NONCE..], b"secret");
        assert_eq!(vault.decrypt(&sealed).unwrap(), b"secret");
        assert!(Vault::from_base64("c2hvcnQ=").is_err());
    }
}
