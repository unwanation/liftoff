//! The libsodium sealed box (`crypto_box_seal`) GitHub takes a CI secret
//! in: X25519 with an ephemeral key, XSalsa20-Poly1305, the nonce the
//! `BLAKE2b` of both public keys (`crypto_box`, pure Rust).
//!
//! The ephemeral key's randomness comes from `getrandom` 0.3, which reads
//! `wasi:random/random` on `wasm32-wasip2`, through [`WasiRng`], the
//! `rand_core` 0.6 generator `crypto_box` takes. Built for
//! `wasm32-wasip2` by `just modules` (the plan's live check of,
//! done as a build check): it compiles and links without a C toolchain,
//! and GIT-05 opens what it seals with the repository's private key.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crypto_box::PublicKey;
use rand_core::{CryptoRng, Error as RngError, RngCore};

use crate::bindings::liftoff::module::host::Error;
use crate::github::error;

/// The host's CSPRNG (`wasi:random/random`) as `rand_core` 0.6 wants it.
#[derive(Debug, Default)]
struct WasiRng {
    /// Whether a [`RngCore::fill_bytes`] could not read randomness (it
    /// cannot return an error): the result is then thrown away.
    failed: bool,
}

impl RngCore for WasiRng {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        // `wasi:random/random` does not fail; should it, the box sealed
        // with these bytes is thrown away (`seal` checks `failed`).
        if getrandom::fill(dest).is_err() {
            dest.fill(0);
            self.failed = true;
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), RngError> {
        getrandom::fill(dest).map_err(|_failed| RngError::from(core::num::NonZeroU32::MIN))
    }
}

impl CryptoRng for WasiRng {}

/// `value` sealed for the base64 public key `key`, in base64: GitHub's
/// `encrypted_value`.
pub(crate) fn seal(key: &str, value: &str) -> Result<String, Error> {
    let bytes: [u8; 32] = STANDARD
        .decode(key.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| {
            error(
                "upstream",
                "github.com: the repository's public key is not 32 bytes of base64",
            )
        })?;
    let mut rng = WasiRng::default();
    let sealed = PublicKey::from(bytes)
        .seal(&mut rng, value.as_bytes())
        .map_err(|_failed| error("internal", "sealing the CI secret failed"))?;
    if rng.failed {
        return Err(error(
            "internal",
            "no randomness to seal the CI secret with",
        ));
    }
    Ok(STANDARD.encode(sealed))
}
