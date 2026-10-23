//! Ciphertext produced by the [`Secrets`](crate::ports::Secrets) port.

use core::fmt;

/// An env file or a secret value, encrypted by the `Secrets` port.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedEnv(String);

impl SealedEnv {
    /// Wraps ciphertext produced by the `Secrets` port.
    #[must_use]
    pub const fn new(ciphertext: String) -> Self {
        Self(ciphertext)
    }

    /// The ciphertext.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SealedEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SealedEnv({} bytes)", self.0.len())
    }
}
