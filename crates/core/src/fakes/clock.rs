//! [`FixedClock`], [`SeqRng`], [`FakeSecrets`] and [`RecordingQueue`].

use core::fmt::Write as _;
use core::future::ready;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use time::{Duration, OffsetDateTime};
use ulid::Ulid;

use super::log::{Call, CallLog, Faults, lock, scriptable};
use crate::domain::{DeployId, SealedEnv, SecretBody, Timestamp};
use crate::error::Error;
use crate::ports::{Clock, DeployQueue, Rng, Secrets};

/// A clock that only moves when told to.
#[derive(Debug, Clone)]
pub struct FixedClock(Arc<Mutex<Timestamp>>);

impl FixedClock {
    /// 2026-10-13T00:00:00Z, the default start of every test.
    pub const START_UNIX: i64 = 1_791_849_600;

    /// A clock stopped at `at`.
    #[must_use]
    pub fn at(at: Timestamp) -> Self {
        Self(Arc::new(Mutex::new(at)))
    }

    /// Sets the time.
    pub fn set(&self, at: Timestamp) {
        *lock(&self.0) = at;
    }

    /// Moves the time forward (or back, for negative `by`).
    pub fn advance(&self, by: Duration) {
        let mut now = lock(&self.0);
        *now = now.saturating_add(by);
    }
}

impl Default for FixedClock {
    fn default() -> Self {
        Self::at(
            OffsetDateTime::from_unix_timestamp(Self::START_UNIX)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
        )
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        *lock(&self.0)
    }
}

/// Deterministic "randomness": ULIDs and token secrets from a counter.
///
/// ULIDs increase with every call, so ids sort in creation order. Token
/// secrets are the counter zero-padded to 40 digits (valid base62).
#[derive(Debug, Clone, Default)]
pub struct SeqRng(Arc<AtomicU64>);

impl SeqRng {
    /// A generator starting at 1.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed).saturating_add(1)
    }
}

impl Rng for SeqRng {
    fn ulid(&self) -> Ulid {
        let n = self.next();
        Ulid::from_parts(n, u128::from(n))
    }

    fn token_secret(&self) -> SecretBody {
        SecretBody::from_counter(self.next())
    }
}

/// Reversible "encryption" for tests: `sealed:` + hex of the plaintext, so
/// the sealed value never contains the plaintext (DEP-17).
#[derive(Debug, Clone, Default)]
pub struct FakeSecrets {
    faults: Faults,
}

scriptable!(FakeSecrets);

impl FakeSecrets {
    /// A fake that seals and opens everything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

const SEALED_PREFIX: &str = "sealed:";

impl Secrets for FakeSecrets {
    fn encrypt(&self, plaintext: &str) -> Result<SealedEnv, Error> {
        self.faults.check("encrypt")?;
        let mut sealed = String::from(SEALED_PREFIX);
        for byte in plaintext.bytes() {
            let _infallible = write!(sealed, "{byte:02x}");
        }
        Ok(SealedEnv::new(sealed))
    }

    fn decrypt(&self, sealed: &SealedEnv) -> Result<String, Error> {
        self.faults.check("decrypt")?;
        let corrupt = || Error::internal_msg("sealed env is corrupt");
        let hex = sealed
            .as_str()
            .strip_prefix(SEALED_PREFIX)
            .ok_or_else(corrupt)?;
        let (pairs, rest) = hex.as_bytes().as_chunks::<2>();
        if !rest.is_empty() {
            return Err(corrupt());
        }
        let bytes = pairs
            .iter()
            .map(|pair| {
                core::str::from_utf8(pair)
                    .ok()
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .ok_or_else(corrupt)
            })
            .collect::<Result<Vec<u8>, Error>>()?;
        String::from_utf8(bytes).map_err(|_not_utf8| corrupt())
    }

    fn mac(&self, message: &[u8]) -> Result<[u8; 32], Error> {
        self.faults.check("mac")?;
        Ok(crate::crypto::hmac_sha256(FAKE_MAC_KEY, message))
    }
}

/// The installation key of [`FakeSecrets::mac`].
const FAKE_MAC_KEY: &[u8] = b"the fake installation key";

/// Records enqueued deploys instead of running them.
#[derive(Debug, Clone, Default)]
pub struct RecordingQueue {
    log: CallLog,
    faults: Faults,
}

scriptable!(RecordingQueue);

impl RecordingQueue {
    /// A fake with its own call log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake writing to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }

    /// The deploys enqueued so far, oldest first.
    #[must_use]
    pub fn enqueued(&self) -> Vec<DeployId> {
        self.log
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Enqueue(id) => Some(id),
                _ => None,
            })
            .collect()
    }
}

impl DeployQueue for RecordingQueue {
    fn enqueue(&self, deploy: DeployId) -> impl Future<Output = Result<(), Error>> + Send {
        self.log.push(Call::Enqueue(deploy));
        ready(self.faults.check("enqueue"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod fake_secrets {
        use super::*;

        #[test]
        fn round_trips_when_sealed_and_opened() {
            let secrets = FakeSecrets::new();
            let sealed = secrets.encrypt("A=1\nB=\"x y\"\n").unwrap();
            assert_eq!(secrets.decrypt(&sealed).unwrap(), "A=1\nB=\"x y\"\n");
        }

        // TC: v1 DEP-17
        #[test]
        fn hides_plaintext_when_sealed() {
            let sealed = FakeSecrets::new().encrypt("PASSWORD=hunter2").unwrap();
            assert!(!sealed.as_str().contains("hunter2"), "plaintext leaked");
        }

        #[test]
        fn rejects_when_ciphertext_foreign() {
            let foreign = SealedEnv::new("not-sealed".to_owned());
            assert!(FakeSecrets::new().decrypt(&foreign).is_err(), "must fail");
        }
    }

    mod seq_rng {
        use super::*;

        #[test]
        fn increase_when_drawn_in_sequence() {
            let rng = SeqRng::new();
            let (first, second) = (rng.ulid(), rng.ulid());
            assert!(first < second, "ids must sort in creation order");
        }

        #[test]
        fn valid_and_distinct_when_drawn_twice() {
            let rng = SeqRng::new();
            assert_ne!(rng.token_secret(), rng.token_secret());
        }
    }

    mod fixed_clock {
        use super::*;

        #[test]
        fn is_2026_10_13_when_new() {
            let now = FixedClock::default().now();
            assert_eq!(
                (now.year(), u8::from(now.month()), now.day()),
                (2026, 10, 13)
            );
        }

        #[test]
        fn moves_now_when_advanced() {
            let clock = FixedClock::default();
            let before = clock.now();
            clock.advance(Duration::days(1));
            assert_eq!(clock.now() - before, Duration::days(1));
        }
    }
}
