//! Credentials at rest.
//!
//! Every stored secret is sealed with XChaCha20-Poly1305 under the master key, with a fresh
//! random 192-bit nonce per value. The nonce is long enough that choosing it at random is
//! safe for any number of values, which is the reason for the X variant: there is no counter
//! to keep, so no counter to lose.
//!
//! The master key comes from `LLMR_MASTER_KEY` and never touches the database. A copy of the
//! database alone opens nothing. Losing the key loses every stored credential, by design.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// Bytes in a nonce, stored in front of each ciphertext.
const NONCE: usize = 24;

/// The key every credential is sealed under.
pub struct MasterKey(XChaCha20Poly1305);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

impl MasterKey {
    /// Reads a key written as base64, the form `llmr keygen` prints.
    ///
    /// # Errors
    ///
    /// A message saying what is wrong with it, never the key itself.
    pub fn from_base64(text: &str) -> Result<Self, String> {
        let bytes = STANDARD
            .decode(text.trim())
            .map_err(|_| "LLMR_MASTER_KEY is not base64".to_string())?;
        if bytes.len() != 32 {
            return Err(format!(
                "LLMR_MASTER_KEY must be 32 bytes, base64 encoded; this one is {} bytes",
                bytes.len()
            ));
        }
        XChaCha20Poly1305::new_from_slice(&bytes)
            .map(MasterKey)
            .map_err(|_| "LLMR_MASTER_KEY could not be used as a key".to_string())
    }

    /// A new random key, base64 encoded.
    pub fn generate() -> String {
        STANDARD.encode(XChaCha20Poly1305::generate_key(&mut OsRng))
    }

    /// Nonce and ciphertext, in one value.
    ///
    /// # Errors
    ///
    /// Only if the cipher refuses, which for this construction means an input past its
    /// size limit.
    pub fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let sealed = self
            .0
            .encrypt(&nonce, plain)
            .map_err(|_| "a credential could not be encrypted".to_string())?;
        let mut out = Vec::with_capacity(NONCE + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// The plaintext of a value [`MasterKey::seal`] produced.
    ///
    /// # Errors
    ///
    /// When the value was sealed under another key or has been altered. The two are not told
    /// apart, which is the point of an authenticated cipher.
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        if sealed.len() < NONCE {
            return Err("a stored credential is truncated".into());
        }
        let (nonce, body) = sealed.split_at(NONCE);
        self.0
            .decrypt(XNonce::from_slice(nonce), body)
            .map_err(|_| {
                "a stored credential could not be decrypted: the master key is not the one it \
                 was stored under, or the database was altered"
                    .to_string()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> MasterKey {
        MasterKey::from_base64(&MasterKey::generate()).unwrap()
    }

    #[test]
    fn a_sealed_value_opens_with_the_same_key() {
        let key = key();
        let sealed = key.seal(b"sk-secret").unwrap();
        assert_eq!(key.open(&sealed).unwrap(), b"sk-secret");
    }

    #[test]
    fn the_same_value_seals_differently_every_time() {
        // A fresh nonce each time, so two providers with the same key are not visibly equal.
        let key = key();
        assert_ne!(key.seal(b"x").unwrap(), key.seal(b"x").unwrap());
    }

    #[test]
    fn another_key_or_a_flipped_bit_opens_nothing() {
        let sealed = key().seal(b"sk-secret").unwrap();
        assert!(key().open(&sealed).is_err());

        let key = key();
        let mut altered = key.seal(b"sk-secret").unwrap();
        let last = altered.len() - 1;
        altered[last] ^= 1;
        assert!(key.open(&altered).is_err());
    }

    #[test]
    fn a_key_of_the_wrong_size_is_refused_without_echoing_it() {
        let error = MasterKey::from_base64(&STANDARD.encode([7u8; 16])).unwrap_err();
        assert!(error.contains("16 bytes"), "{error}");
        assert!(MasterKey::from_base64("not base64!").is_err());
    }
}
