//! [`MasterKey`]: the [`Secrets`] port as XChaCha20-Poly1305 under the
//! server's master key (secrets and credentials at rest).
//!
//! Moved from `liftoff-remote` (v1 `FileSecrets`) so the daemon and the
//! CLI's local store (`~/.config/liftoff/key`) seal the same
//! way. A sealed value is `base64(nonce || ciphertext)` with a fresh random
//! 24-byte nonce. Reading the key file is the caller's job: the core does
//! no I/O.

use core::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, Generate, KeyInit, Nonce};
use sha2::{Digest as _, Sha256};

use crate::domain::SealedEnv;
use crate::error::Error;
use crate::ports::Secrets;

/// XChaCha20-Poly1305 under a 32-byte key.
pub struct MasterKey {
    key: [u8; 32],
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKey").finish_non_exhaustive()
    }
}

impl MasterKey {
    /// From key file contents: 32 raw bytes, or 64 hex digits (surrounding
    /// whitespace ignored).
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the content is neither.
    pub fn from_key_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if let Ok(key) = <[u8; 32]>::try_from(bytes) {
            return Ok(Self { key });
        }
        let digits = core::str::from_utf8(bytes)
            .map_err(|_utf8| bad_key())?
            .trim()
            .as_bytes();
        if digits.len() != 64 || !digits.iter().all(u8::is_ascii_hexdigit) {
            return Err(bad_key());
        }
        let mut key = [0_u8; 32];
        for (out, pair) in key.iter_mut().zip(digits.as_chunks::<2>().0) {
            let pair = core::str::from_utf8(pair).map_err(|_utf8| bad_key())?;
            *out = u8::from_str_radix(pair, 16).map_err(|_digit| bad_key())?;
        }
        Ok(Self { key })
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&self.key.into())
    }
}

fn bad_key() -> Error {
    Error::internal_msg("master key must be 32 raw bytes or 64 hex digits")
}

/// The input that derives the MAC key of [`Secrets::mac`] from the
/// master key, so the cipher key itself never keys a MAC.
const MAC_KEY_DERIVATION: &[u8] = b"liftoff mac key v1";

/// The block size of SHA-256 (RFC 2104: `B`).
const SHA256_BLOCK: usize = 64;

/// HMAC-SHA256 of `message` under `key` (RFC 2104).
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0_u8; SHA256_BLOCK];
    if key.len() > SHA256_BLOCK {
        let digest = Sha256::digest(key);
        for (out, byte) in block.iter_mut().zip(digest.iter()) {
            *out = *byte;
        }
    } else {
        for (out, byte) in block.iter_mut().zip(key) {
            *out = *byte;
        }
    }
    let pad = |with: u8| block.map(|byte| byte ^ with);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    let outer = Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize();
    let mut mac = [0_u8; 32];
    for (out, byte) in mac.iter_mut().zip(outer.iter()) {
        *out = *byte;
    }
    mac
}

/// Length of the `XChaCha20` nonce that prefixes every sealed value.
const NONCE_LEN: usize = 24;

impl Secrets for MasterKey {
    fn encrypt(&self, plaintext: &str) -> Result<SealedEnv, Error> {
        let nonce = Nonce::<XChaCha20Poly1305>::try_generate()
            .map_err(|_rng| Error::internal_msg("no randomness for a nonce"))?;
        let ciphertext = self
            .cipher()
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_aead| Error::internal_msg("env encryption failed"))?;
        let mut sealed = nonce.to_vec();
        sealed.extend_from_slice(&ciphertext);
        Ok(SealedEnv::new(STANDARD.encode(sealed)))
    }

    fn decrypt(&self, sealed: &SealedEnv) -> Result<String, Error> {
        let too_short = || Error::internal_msg("sealed env is too short");
        let raw = STANDARD
            .decode(sealed.as_str())
            .map_err(|_base64| Error::internal_msg("sealed env is not base64"))?;
        let (nonce, ciphertext) = raw.split_at_checked(NONCE_LEN).ok_or_else(too_short)?;
        let nonce = <[u8; NONCE_LEN]>::try_from(nonce).map_err(|_len| too_short())?;
        let plaintext = self
            .cipher()
            .decrypt(&nonce.into(), ciphertext)
            .map_err(|_aead| {
                Error::internal_msg("sealed env is corrupt or sealed with another key")
            })?;
        String::from_utf8(plaintext).map_err(Error::internal)
    }

    fn mac(&self, message: &[u8]) -> Result<[u8; 32], Error> {
        let key = hmac_sha256(&self.key, MAC_KEY_DERIVATION);
        Ok(hmac_sha256(&key, message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(fill: u8) -> MasterKey {
        MasterKey::from_key_bytes(&[fill; 32]).unwrap()
    }

    /// Base64 characters that encode the 24-byte nonce: 24 bytes are
    /// exactly 32 characters, so character 32 starts the ciphertext.
    const NONCE_CHARS: usize = 32;

    /// `sealed` with the character at `index` swapped for another base64
    /// digit. `index` must name a character that carries six data bits (not
    /// the last one before padding), so the result stays valid base64 and
    /// decoding reaches the AEAD check.
    fn flipped(sealed: &SealedEnv, index: usize) -> SealedEnv {
        let mut chars: Vec<char> = sealed.as_str().chars().collect();
        chars[index] = if chars[index] == 'A' { 'B' } else { 'A' };
        let text: String = chars.into_iter().collect();
        assert!(
            STANDARD.decode(&text).is_ok(),
            "tamper broke base64: {text}"
        );
        SealedEnv::new(text)
    }

    mod master_key {
        use super::*;

        // TC: SEC-07
        #[test]
        fn returns_plaintext_when_round_tripped() {
            let key = key(7);
            let sealed = key.encrypt("postgres://db/shop").unwrap();
            assert_eq!(key.decrypt(&sealed).unwrap(), "postgres://db/shop");
        }

        // TC: SEC-07
        #[test]
        fn hides_plaintext_when_sealed() {
            let sealed = key(7).encrypt("hunter2-plaintext").unwrap();
            assert!(
                !sealed.as_str().contains("hunter2"),
                "plaintext leaked: {}",
                sealed.as_str()
            );
        }

        // TC: SEC-07
        #[test]
        fn differs_when_same_value_sealed_twice() {
            let key = key(7);
            let (first, second) = (key.encrypt("same").unwrap(), key.encrypt("same").unwrap());
            assert_ne!(
                first.as_str().get(..NONCE_CHARS),
                second.as_str().get(..NONCE_CHARS),
                "a nonce was reused"
            );
        }

        // TC: SEC-07
        #[test]
        fn internal_when_opened_with_another_key() {
            let sealed = key(7).encrypt("secret").unwrap();
            assert!(matches!(key(8).decrypt(&sealed), Err(Error::Internal(_))));
        }

        // TC: SEC-07
        #[test]
        fn internal_when_ciphertext_tampered() {
            let key = key(7);
            let sealed = key.encrypt("secret").unwrap();
            assert!(matches!(
                key.decrypt(&flipped(&sealed, NONCE_CHARS)),
                Err(Error::Internal(_))
            ));
        }

        // TC: SEC-07
        #[test]
        fn internal_when_nonce_tampered() {
            let key = key(7);
            let sealed = key.encrypt("secret").unwrap();
            assert!(matches!(
                key.decrypt(&flipped(&sealed, 0)),
                Err(Error::Internal(_))
            ));
        }

        // TC: SEC-07
        #[test]
        fn internal_when_not_base64() {
            assert!(matches!(
                key(7).decrypt(&SealedEnv::new("%%%".to_owned())),
                Err(Error::Internal(_))
            ));
        }

        // TC: SEC-07
        #[test]
        fn internal_when_shorter_than_nonce() {
            // "AAAAAAAAAAAAAA==" is base64 of 10 zero bytes.
            assert!(matches!(
                key(7).decrypt(&SealedEnv::new("AAAAAAAAAAAAAA==".to_owned())),
                Err(Error::Internal(_))
            ));
        }

        // TC: SEC-07
        #[test]
        fn round_trips_when_empty() {
            let key = key(7);
            let sealed = key.encrypt("").unwrap();
            assert_eq!(key.decrypt(&sealed).unwrap(), "");
        }

        // TC: SEC-07
        #[test]
        fn redacts_key_when_debug_formatted() {
            assert_eq!(format!("{:?}", key(0xab)), "MasterKey { .. }");
        }
    }

    mod from_key_bytes {
        use super::*;

        // TC: SEC-07
        #[test]
        fn opens_raw_key_values_when_given_as_hex_with_newline() {
            let hex = format!("{}\n", "ab".repeat(32));
            let from_hex = MasterKey::from_key_bytes(hex.as_bytes()).unwrap();
            let sealed = key(0xab).encrypt("x").unwrap();
            assert_eq!(from_hex.decrypt(&sealed).unwrap(), "x");
        }

        // TC: SEC-07
        #[test]
        fn internal_when_31_bytes() {
            assert!(matches!(
                MasterKey::from_key_bytes(&[1; 31]),
                Err(Error::Internal(_))
            ));
        }

        // TC: SEC-07
        #[test]
        fn internal_when_empty() {
            assert!(matches!(
                MasterKey::from_key_bytes(b""),
                Err(Error::Internal(_))
            ));
        }

        // TC: SEC-07
        #[test]
        fn internal_when_64_chars_not_hex() {
            assert!(matches!(
                MasterKey::from_key_bytes("zz".repeat(32).as_bytes()),
                Err(Error::Internal(_))
            ));
        }
    }

    mod hmac {
        use super::*;

        fn hex(bytes: &[u8]) -> String {
            use core::fmt::Write as _;
            bytes.iter().fold(String::new(), |mut hex, byte| {
                let _infallible = write!(hex, "{byte:02x}");
                hex
            })
        }

        // RFC 4231, test case 2
        #[test]
        fn matches_rfc4231_when_key_is_short() {
            assert_eq!(
                hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
            );
        }

        // RFC 4231, test case 6
        #[test]
        fn matches_rfc4231_when_key_is_longer_than_block() {
            assert_eq!(
                hex(&hmac_sha256(
                    &[0xaa; 131],
                    b"Test Using Larger Than Block-Size Key - Hash Key First"
                )),
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
            );
        }

        // TC: MCPH-04 (the MAC of an `mcp` session is the installation's)
        #[test]
        fn differs_when_master_key_differs() {
            assert_ne!(key(1).mac(b"at").unwrap(), key(2).mac(b"at").unwrap());
        }

        // TC: MCPH-04 (the same token finds the same session)
        #[test]
        fn repeats_when_message_and_key_repeat() {
            assert_eq!(key(1).mac(b"at").unwrap(), key(1).mac(b"at").unwrap());
        }

        // The cipher key itself keys no MAC.
        #[test]
        fn differs_from_hmac_under_master_key_when_computed() {
            assert_ne!(key(1).mac(b"at").unwrap(), hmac_sha256(&[1; 32], b"at"));
        }
    }
}
