use ulid::Ulid;

use crate::domain::{SealedEnv, SecretBody, Timestamp};
use crate::error::Error;

/// The wall clock.
pub trait Clock: Send + Sync {
    /// Now.
    fn now(&self) -> Timestamp;
}

/// Randomness: identifiers and token secrets.
pub trait Rng: Send + Sync {
    /// A fresh ULID (time-ordered in production).
    fn ulid(&self) -> Ulid;
    /// 40 random base62 characters for a deploy token or a session.
    fn token_secret(&self) -> SecretBody;
}

/// Encryption of deploy env files with the master key (XChaCha20-Poly1305
/// in production).
///
/// # Errors
///
/// [`Error::Internal`] when the ciphertext is corrupt or was sealed with
/// another key.
pub trait Secrets: Send + Sync {
    /// Seals `plaintext`.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when encryption fails.
    fn encrypt(&self, plaintext: &str) -> Result<SealedEnv, Error>;
    /// Opens a sealed env.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the ciphertext is corrupt or was sealed with
    /// another key.
    fn decrypt(&self, sealed: &SealedEnv) -> Result<String, Error>;
    /// A message authentication code of `message` under a key of this
    /// installation (HMAC-SHA256 under a key derived from the master key
    /// in production): the same for the same message, and nobody without
    /// the key computes it (the secret of an `mcp` session, derived from its access token).
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the key cannot be used.
    fn mac(&self, message: &[u8]) -> Result<[u8; 32], Error>;
}
