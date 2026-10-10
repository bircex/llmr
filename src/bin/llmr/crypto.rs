//! Credentials at rest.
//!
//! Every stored secret is sealed with XChaCha20-Poly1305 under the master key, with a fresh
//! random 192-bit nonce per value. The nonce is long enough that choosing it at random is
//! safe for any number of values, which is the reason for the X variant: there is no counter
//! to keep, so no counter to lose.
//!
//! The master key comes from `LLMR_MASTER_KEY`, or from the file `LLMR_MASTER_KEY_FILE` names,
//! and never touches the database. A copy of the database alone opens nothing. Losing the key
//! loses every stored credential, by design.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::Path;
use std::time::Duration;

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

    /// The key kept in the file at `path`, made and kept there first when there is none.
    ///
    /// This is what lets a stack start with no key configured by hand: the file sits on a
    /// volume, is written once and is read on every later start. It is created with
    /// `create_new`, so of several processes starting together exactly one writes and the
    /// rest read what it wrote, and it is never replaced. On Unix only its owner can read it.
    ///
    /// # Errors
    ///
    /// When the file cannot be created or read, or holds something that is not a key.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create the directory {}: {e}", dir.display()))?;
        }

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(mut file) => file
                .write_all(Self::generate().as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }

        // The process that created the file may still be writing it: a key is whole once it
        // decodes, so give it a moment rather than reading half of one as a wrong key.
        let mut last = String::new();
        for _ in 0..50 {
            let text = fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            match Self::from_base64(text.trim()) {
                Ok(key) => return Ok(key),
                Err(e) => last = e,
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(format!("{}: {last}", path.display()))
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

    /// A path in a directory of its own under the system temp dir, removed by the caller.
    fn scratch(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir()
            .join(format!(
                "llmr-key-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
            .join(name)
    }

    fn sealed_by(key: &MasterKey) -> Vec<u8> {
        key.seal(b"sk-test").unwrap()
    }

    #[test]
    fn a_missing_key_file_is_made_and_read_back_on_the_next_start() {
        let path = scratch("master_key");
        let first = MasterKey::load_or_create(&path).unwrap();
        let second = MasterKey::load_or_create(&path).unwrap();

        // The same key: what one start sealed, the next one opens.
        assert_eq!(second.open(&sealed_by(&first)).unwrap(), b"sk-test");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn processes_starting_together_agree_on_one_key() {
        let path = scratch("master_key");
        let keys: Vec<_> = (0..16)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || MasterKey::load_or_create(&path).unwrap())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();

        let sealed = sealed_by(&keys[0]);
        for key in &keys {
            assert_eq!(key.open(&sealed).unwrap(), b"sk-test");
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let path = scratch("master_key");
        MasterKey::load_or_create(&path).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_key_file_holding_something_else_is_an_error_not_a_new_key() {
        let path = scratch("master_key");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not a key").unwrap();

        assert!(MasterKey::load_or_create(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not a key");
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
