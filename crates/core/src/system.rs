//! The real [`Clock`] and [`Rng`]: wall time and operating-system
//! randomness.
//!
//! Moved from `liftoffd` so the daemon and the
//! CLI's local mode stamp and identify their records the same way, as
//! [`crypto`](crate::crypto) seals them the same way: this module and
//! `crypto` are the core's only reads of the operating system (its clock
//! and its random source), and neither does any other I/O.

use crate::domain::{SecretBody, Timestamp};
use crate::ports::{Clock, Rng};
use time::OffsetDateTime;
use ulid::Ulid;

/// The wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        OffsetDateTime::now_utc()
    }
}

/// ULIDs and token secrets from the operating system's random source.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsRng;

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// The largest multiple of 62 that fits a byte: bytes at or above it are
/// dropped so every character is equally likely.
const BASE62_LIMIT: u8 = 248;

/// Milliseconds since the Unix epoch.
fn unix_millis() -> u64 {
    u64::try_from(
        OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .div_euclid(1_000_000),
    )
    .unwrap_or_default()
}

/// Fills `buf` from the operating system: the random bytes of [`OsRng`],
/// and of what else must be unguessable (a generated secret of `lof
/// create`, the CLI's local key).
///
/// # Panics
///
/// There is no way to hand out an identifier or a token without entropy,
/// and the [`Rng`] port cannot fail, so a broken random source stops the
/// process instead of minting predictable secrets.
#[expect(
    clippy::expect_used,
    reason = "no entropy means no safe identifiers or tokens; the port is infallible"
)]
pub fn fill(buf: &mut [u8]) {
    getrandom::fill(buf).expect("the operating system provides random bytes");
}

impl Rng for OsRng {
    fn ulid(&self) -> Ulid {
        let millis = unix_millis();
        let mut random = [0_u8; 16];
        fill(&mut random);
        Ulid::from_parts(millis, u128::from_be_bytes(random))
    }

    fn token_secret(&self) -> SecretBody {
        let mut chars = String::with_capacity(SecretBody::LEN);
        while chars.len() < SecretBody::LEN {
            let mut block = [0_u8; 64];
            fill(&mut block);
            for byte in block.iter().filter(|byte| **byte < BASE62_LIMIT) {
                if chars.len() == SecretBody::LEN {
                    break;
                }
                if let Some(c) = BASE62.get(usize::from(byte % 62)) {
                    chars.push(char::from(*c));
                }
            }
        }
        seal(&chars)
    }
}

#[expect(
    clippy::expect_used,
    reason = "unreachable: the generator only emits 40 base62 characters"
)]
fn seal(chars: &str) -> SecretBody {
    SecretBody::new(chars).expect("generated token secret is 40 base62 characters")
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;

    mod clock {
        use super::*;

        #[test]
        fn is_close_to_now_when_asked() {
            let drift = (SystemClock.now() - OffsetDateTime::now_utc()).abs();
            assert!(drift < Duration::from_secs(1), "drift {drift}");
        }
    }

    mod rng {
        use super::*;

        #[test]
        fn token_secrets_are_valid_and_differ_when_called_twice() {
            let (a, b) = (OsRng.token_secret(), OsRng.token_secret());
            assert_eq!(
                (
                    a.as_str().len(),
                    SecretBody::new(a.as_str()).is_ok(),
                    a == b
                ),
                (40, true, false)
            );
        }

        #[test]
        fn ulids_carry_the_current_time_and_differ_when_called_twice() {
            let (a, b) = (OsRng.ulid(), OsRng.ulid());
            let now_ms = unix_millis();
            assert_eq!(
                (a == b, now_ms.saturating_sub(a.timestamp_ms()) < 1_000),
                (false, true)
            );
        }

        #[test]
        fn token_secrets_use_every_character_class_when_many_are_drawn() {
            let drawn: String = (0..50)
                .map(|_| OsRng.token_secret().as_str().to_owned())
                .collect();
            assert_eq!(
                (
                    drawn.bytes().any(|b| b.is_ascii_digit()),
                    drawn.bytes().any(|b| b.is_ascii_uppercase()),
                    drawn.bytes().any(|b| b.is_ascii_lowercase())
                ),
                (true, true, true)
            );
        }
    }
}
