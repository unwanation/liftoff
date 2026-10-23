//! The bandwidth of one environment's tunnel: a token bucket
//! both directions of its connection draw from.
//!
//! A message takes its bytes at once and may leave the bucket in debt;
//! the taker then waits until the debt is paid off at the configured
//! rate, so a message larger than the burst still passes, only later.

use core::time::Duration;
use std::sync::{Mutex, PoisonError};

use tokio::time::Instant;

/// Nanoseconds in a second.
const NANOS: u128 = 1_000_000_000;

/// The bandwidth of one environment: `bytes_per_second` on average, up to
/// `burst` bytes at once after a pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bandwidth {
    /// The bytes per second both directions of the connection share.
    pub bytes_per_second: u64,
    /// The bytes that may pass at once after the connection was idle.
    pub burst: u64,
}

impl Default for Bandwidth {
    /// 10 MiB a second with bursts of 1 MiB.
    fn default() -> Self {
        Self {
            bytes_per_second: 10 * 1024 * 1024,
            burst: 1024 * 1024,
        }
    }
}

/// The token bucket of one environment.
#[derive(Debug)]
pub(crate) struct Bucket {
    limit: Bandwidth,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// Bytes that may pass now; negative while in debt.
    tokens: i128,
    /// Up to when the tokens were counted.
    counted: Instant,
}

impl Bucket {
    /// A full bucket.
    pub(crate) fn new(limit: Bandwidth) -> Self {
        Self {
            limit,
            state: Mutex::new(State {
                tokens: i128::from(limit.burst),
                counted: Instant::now(),
            }),
        }
    }

    /// Takes `bytes` and waits as long as the bucket is in debt.
    pub(crate) async fn take(&self, bytes: usize) {
        let wait = self.charge(bytes);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Takes `bytes` and answers how long the debt takes to pay off.
    #[expect(
        clippy::integer_division,
        reason = "whole bytes earned; the fraction stays in `counted`"
    )]
    fn charge(&self, bytes: usize) -> Duration {
        let rate = u128::from(self.limit.bytes_per_second.max(1));
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.counted).as_nanos();
        let earned = elapsed.saturating_mul(rate) / NANOS;
        if earned > 0 {
            // Counts only the time the earned bytes took, so that frequent
            // small takes lose no fraction of a byte.
            let spent = earned.saturating_mul(NANOS).div_ceil(rate);
            let spent = Duration::from_nanos(u64::try_from(spent).unwrap_or(u64::MAX));
            state.counted = state.counted.checked_add(spent).unwrap_or(now).min(now);
            let earned = i128::try_from(earned).unwrap_or(i128::MAX);
            state.tokens = state
                .tokens
                .saturating_add(earned)
                .min(i128::from(self.limit.burst));
        }
        let bytes = i128::try_from(bytes).unwrap_or(i128::MAX);
        state.tokens = state.tokens.saturating_sub(bytes);
        let debt = u128::try_from(state.tokens.saturating_neg()).unwrap_or(0);
        drop(state);
        let nanos = debt.saturating_mul(NANOS).div_ceil(rate);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket() -> Bucket {
        Bucket::new(Bandwidth {
            bytes_per_second: 1000,
            burst: 500,
        })
    }

    mod take {
        use super::*;

        // TC: TUN-03 (the bandwidth of an environment)
        #[tokio::test(start_paused = true)]
        async fn passes_at_once_when_the_bytes_fit_the_burst() {
            let bucket = bucket();
            let started = Instant::now();

            bucket.take(500).await;

            assert_eq!(started.elapsed(), Duration::ZERO);
        }

        // TC: TUN-03
        #[tokio::test(start_paused = true)]
        async fn waits_for_the_rate_when_the_bytes_exceed_the_burst() {
            let bucket = bucket();
            let started = Instant::now();

            bucket.take(500).await;
            bucket.take(1000).await;

            assert_eq!(started.elapsed(), Duration::from_secs(1));
        }

        // TC: TUN-03
        #[tokio::test(start_paused = true)]
        async fn passes_a_message_larger_than_the_burst_when_it_waits() {
            let bucket = bucket();
            let started = Instant::now();

            bucket.take(2500).await;

            assert_eq!(started.elapsed(), Duration::from_secs(2));
        }

        // TC: TUN-03
        #[tokio::test(start_paused = true)]
        async fn refills_up_to_the_burst_only_when_the_connection_was_idle() {
            let bucket = bucket();
            bucket.take(500).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
            let started = Instant::now();

            bucket.take(1000).await;

            assert_eq!(started.elapsed(), Duration::from_millis(500));
        }
    }
}
