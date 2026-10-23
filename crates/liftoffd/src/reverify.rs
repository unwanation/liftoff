//! The daily re-verification of external domains (DOM-07).
//!
//! Next to the deploy queue the daemon runs one background task that calls
//! `DomainService::reverify_external` once at start and then every
//! [`REVERIFY_EVERY`], until the daemon stops. A failed round is logged and
//! the schedule goes on.

use core::future::Future;
use core::time::Duration;

use liftoff_core::error::Error;
use liftoff_core::ports::Ports;
use liftoff_core::service::DomainService;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// How often external domains are checked again: once a day.
pub const REVERIFY_EVERY: Duration = Duration::from_hours(24);

/// One round of re-verification: in production
/// [`DomainService::reverify_external`].
pub trait Reverify: Send + Sync + 'static {
    /// Checks every external domain once.
    fn reverify(&self) -> impl Future<Output = Result<(), Error>> + Send;
}

impl<P: Ports> Reverify for DomainService<P> {
    async fn reverify(&self) -> Result<(), Error> {
        self.reverify_external().await.map(drop)
    }
}

/// Runs `reverify` at once and then every [`REVERIFY_EVERY`] until `stop`
/// is cancelled; returns when it is.
pub async fn reverify_daily<R: Reverify>(reverify: R, stop: CancellationToken) {
    let mut every = tokio::time::interval(REVERIFY_EVERY);
    // A round that overran (or a suspended host) shifts the schedule
    // instead of firing the missed rounds back to back.
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _instant = every.tick() => {}
        }
        // The first tick completes at once: the round at start.
        tokio::select! {
            () = stop.cancelled() => return,
            result = reverify.reverify() => {
                if let Err(error) = result {
                    tracing::error!(?error, "re-verification of external domains failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Counts its rounds; fails the first `failing` of them.
    #[derive(Debug, Clone, Default)]
    struct Counting {
        rounds: Arc<AtomicUsize>,
        failing: usize,
    }

    impl Reverify for Counting {
        fn reverify(&self) -> impl Future<Output = Result<(), Error>> + Send {
            let before = self.rounds.fetch_add(1, Ordering::SeqCst);
            core::future::ready(if before < self.failing {
                Err(Error::unavailable("resolver down"))
            } else {
                Ok(())
            })
        }
    }

    impl Counting {
        fn rounds(&self) -> usize {
            self.rounds.load(Ordering::SeqCst)
        }
    }

    /// Lets the paused clock run `by` (auto-advancing over the task's
    /// timers) so the task observes it.
    async fn after(by: Duration) {
        tokio::time::sleep(by).await;
    }

    const HOUR: Duration = Duration::from_secs(3_600);

    mod reverify_daily {
        use super::*;

        // TC: DOM-07 (at start)
        #[tokio::test(start_paused = true)]
        async fn runs_once_when_daemon_starts() {
            let counting = Counting::default();
            let stop = CancellationToken::new();
            let task = tokio::spawn(reverify_daily(counting.clone(), stop.clone()));
            after(Duration::from_secs(1)).await;
            let rounds = counting.rounds();
            stop.cancel();
            task.await.unwrap();
            assert_eq!(rounds, 1);
        }

        // TC: DOM-07 (not again within the day)
        #[tokio::test(start_paused = true)]
        async fn does_not_run_again_when_23_hours_passed() {
            let counting = Counting::default();
            let stop = CancellationToken::new();
            let task = tokio::spawn(reverify_daily(counting.clone(), stop.clone()));
            after(23 * HOUR).await;
            let rounds = counting.rounds();
            stop.cancel();
            task.await.unwrap();
            assert_eq!(rounds, 1);
        }

        // TC: DOM-07 (then once every 24 hours)
        #[tokio::test(start_paused = true)]
        async fn runs_once_a_day_when_days_pass() {
            let counting = Counting::default();
            let stop = CancellationToken::new();
            let task = tokio::spawn(reverify_daily(counting.clone(), stop.clone()));
            after(HOUR).await;
            let first_day = counting.rounds();
            after(24 * HOUR).await;
            let second_day = counting.rounds();
            after(24 * HOUR).await;
            let third_day = counting.rounds();
            stop.cancel();
            task.await.unwrap();
            assert_eq!((first_day, second_day, third_day), (1, 2, 3));
        }

        // TC: DOM-07 (a failed round does not end the schedule)
        #[tokio::test(start_paused = true)]
        async fn runs_next_day_when_first_round_failed() {
            let counting = Counting {
                failing: 1,
                ..Counting::default()
            };
            let stop = CancellationToken::new();
            let task = tokio::spawn(reverify_daily(counting.clone(), stop.clone()));
            after(25 * HOUR).await;
            let rounds = counting.rounds();
            stop.cancel();
            task.await.unwrap();
            assert_eq!(rounds, 2);
        }

        #[tokio::test(start_paused = true)]
        async fn returns_when_stopped() {
            let stop = CancellationToken::new();
            let task = tokio::spawn(reverify_daily(Counting::default(), stop.clone()));
            after(HOUR).await;
            stop.cancel();
            let ended = tokio::time::timeout(Duration::from_secs(1), task).await;
            assert!(
                matches!(ended, Ok(Ok(()))),
                "the task ends cleanly once stopped, got {ended:?}"
            );
        }
    }

    mod domain_service {
        use liftoff_core::domain::DomainKind;
        use liftoff_core::fakes::World;
        use liftoff_core::ports::{Clock as _, Store as _};

        use super::*;

        // TC: DOM-07 (the daily round is `DomainService::reverify_external`)
        #[tokio::test]
        async fn marks_record_lost_when_external_record_disappeared() {
            let world = World::new().await;
            let mut domain = world
                .records
                .domain(&world.shop, "www.shop.example")
                .unwrap();
            domain.kind = DomainKind::External;
            domain.verified_at = Some(world.ports.clock.now());
            world.ports.store.insert_domain(&domain).await.unwrap();
            world.domains().reverify().await.unwrap();
            let stored = world.ports.store.domain(domain.id).await.unwrap().unwrap();
            assert!(stored.record_lost_at.is_some(), "{stored:?}");
        }

        // TC: DOM-07 (a failed round is reported to the scheduler)
        #[tokio::test]
        async fn fails_when_store_fails() {
            let world = World::new().await;
            world
                .ports
                .store
                .fail_next("domains", Error::internal_msg("disk gone"));
            let result = world.domains().reverify().await;
            assert!(matches!(result, Err(Error::Internal(_))), "{result:?}");
        }
    }
}
