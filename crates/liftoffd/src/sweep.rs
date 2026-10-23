//! The sweep of expired artifacts (ART-08).
//!
//! Next to the deploy queue the daemon runs one background task that calls
//! [`ArtifactsService::sweep`] at start and then every
//! [`SWEEP_ARTIFACTS_EVERY`], until the daemon stops: the private
//! artifacts whose TTL ran out are deleted with their files. A failed
//! round is logged and the schedule goes on; an expired artifact is `404`
//! for everybody until it is swept.

use core::future::Future;
use core::time::Duration;

use liftoff_core::error::Error;
use liftoff_core::ports::Ports;
use liftoff_core::service::ArtifactsService;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

/// How often expired artifacts are swept: once a minute.
pub const SWEEP_ARTIFACTS_EVERY: Duration = Duration::from_secs(60);

/// One round of the sweep: in production [`ArtifactsService::sweep`].
pub trait Sweep: Send + Sync + 'static {
    /// Deletes every expired artifact once.
    fn sweep_once(&self) -> impl Future<Output = Result<(), Error>> + Send;
}

impl<P: Ports> Sweep for ArtifactsService<P> {
    async fn sweep_once(&self) -> Result<(), Error> {
        self.sweep().await.map(drop)
    }
}

/// Runs `sweep` at once and then every `period` until `stop` is
/// cancelled; returns when it is.
pub async fn sweep_artifacts_every<S: Sweep>(sweep: S, period: Duration, stop: CancellationToken) {
    let mut every = tokio::time::interval(period);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _instant = every.tick() => {}
        }
        if let Err(error) = sweep.sweep_once().await {
            tracing::error!(%error, "sweeping expired artifacts failed");
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

    impl Sweep for Counting {
        fn sweep_once(&self) -> impl Future<Output = Result<(), Error>> + Send {
            let before = self.rounds.fetch_add(1, Ordering::SeqCst);
            core::future::ready(if before < self.failing {
                Err(Error::internal_msg("disk full"))
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

    const SECOND: Duration = Duration::from_secs(1);

    mod sweep_artifacts_every {
        use super::*;

        // TC: ART-08 (at start)
        #[tokio::test(start_paused = true)]
        async fn sweeps_once_when_daemon_starts() {
            let counting = Counting::default();
            let stop = CancellationToken::new();
            let task = tokio::spawn(sweep_artifacts_every(
                counting.clone(),
                SWEEP_ARTIFACTS_EVERY,
                stop.clone(),
            ));
            after(SECOND).await;
            let rounds = counting.rounds();
            stop.cancel();
            task.await.unwrap();
            assert_eq!(rounds, 1);
        }

        // TC: ART-08 (then once a minute)
        #[tokio::test(start_paused = true)]
        async fn sweeps_once_a_minute_when_minutes_pass() {
            let counting = Counting::default();
            let stop = CancellationToken::new();
            let task = tokio::spawn(sweep_artifacts_every(
                counting.clone(),
                SWEEP_ARTIFACTS_EVERY,
                stop.clone(),
            ));
            after(SECOND).await;
            let at_start = counting.rounds();
            after(Duration::from_secs(57)).await;
            let within_the_minute = counting.rounds();
            after(3 * SECOND).await;
            let after_a_minute = counting.rounds();
            after(SWEEP_ARTIFACTS_EVERY).await;
            let after_two = counting.rounds();
            stop.cancel();
            task.await.unwrap();
            assert_eq!(
                (at_start, within_the_minute, after_a_minute, after_two),
                (1, 1, 2, 3)
            );
        }

        // TC: ART-08 (a failed round does not end the schedule)
        #[tokio::test(start_paused = true)]
        async fn sweeps_next_minute_when_first_round_failed() {
            let counting = Counting {
                failing: 1,
                ..Counting::default()
            };
            let stop = CancellationToken::new();
            let task = tokio::spawn(sweep_artifacts_every(
                counting.clone(),
                SWEEP_ARTIFACTS_EVERY,
                stop.clone(),
            ));
            after(SWEEP_ARTIFACTS_EVERY + SECOND).await;
            let rounds = counting.rounds();
            stop.cancel();
            task.await.unwrap();
            assert_eq!(rounds, 2);
        }

        #[tokio::test(start_paused = true)]
        async fn returns_when_stopped() {
            let stop = CancellationToken::new();
            let task = tokio::spawn(sweep_artifacts_every(
                Counting::default(),
                SWEEP_ARTIFACTS_EVERY,
                stop.clone(),
            ));
            after(SECOND).await;
            stop.cancel();
            let ended = tokio::time::timeout(SECOND, task).await;
            assert!(
                matches!(ended, Ok(Ok(()))),
                "the task ends cleanly once stopped, got {ended:?}"
            );
        }
    }

    mod artifacts_service {
        use liftoff_core::fakes::World;
        use liftoff_core::ports::Store as _;
        use liftoff_core::service::ArtifactContent;

        use super::*;

        // TC: ART-08 (the round is `ArtifactsService::sweep`)
        #[tokio::test]
        async fn deletes_expired_artifact_when_swept() {
            let world = World::new().await;
            let artifacts = world.artifacts();
            let id = artifacts
                .publish(
                    &world.owner,
                    ArtifactContent {
                        html: b"<h1>v1</h1>".to_vec(),
                        ..ArtifactContent::default()
                    },
                )
                .await
                .unwrap()
                .artifact
                .id;
            world.ports.clock.advance(time::Duration::hours(5));
            artifacts.sweep_once().await.unwrap();
            assert_eq!(world.ports.store.artifact(id).await.unwrap(), None);
        }
    }
}
