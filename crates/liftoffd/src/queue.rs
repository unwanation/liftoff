//! The deploy queue (test case DEP-11, v1 QUE-01..QUE-04).
//!
//! Implements [`DeployQueue`] for the daemon: deploys of one environment
//! run one after another in the order they were created (their number),
//! deploys of different environments run in parallel but never more than
//! `deploy.parallel` at once. A deploy that panics or returns an error is
//! logged, failed if it was left unfinished, and the queue goes on.
//!
//! The queue knows nothing about what a deploy does: it calls a [`Runner`]
//! (`DeployService::run` once the daemon wires them) with the deploy id.
//! Which environment a deploy belongs to is read from the store at enqueue
//! time.
//!
//! Shape: one worker task per environment that has pending deploys. The
//! worker takes a permit of the global semaphore, pops the next deploy of
//! its environment, runs it in its own task (so a panic is isolated) and
//! gives the permit back. Because a worker only ever runs one deploy at a
//! time and pops the lowest number first, the per-environment order and
//! exclusion hold without a separate mutex.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use liftoff_core::domain::{DeployId, DeployNumber, DeployStatus, EnvironmentId};
use liftoff_core::error::Error;
use liftoff_core::ports::{Clock, DeployQueue, Store};
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};

/// Executes one deploy.
pub trait Runner: Send + Sync + 'static {
    /// Runs the deploy to its end. An error is logged by the queue.
    fn run(&self, deploy: DeployId) -> impl Future<Output = Result<(), Error>> + Send;
}

impl<F, Fut> Runner for F
where
    F: Fn(DeployId) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), Error>> + Send + 'static,
{
    fn run(&self, deploy: DeployId) -> impl Future<Output = Result<(), Error>> + Send {
        self(deploy)
    }
}

/// A deploy in flight, type-erased.
pub type RunFuture = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;

type RunFn = Box<dyn Fn(DeployId) -> RunFuture + Send + Sync>;

/// A [`Runner`] whose body is bound after the queue exists.
///
/// The queue is one of the ports of the services (`Ports::queue`) and the
/// service that runs a deploy needs those ports, so neither can be built
/// first. The erased function breaks the cycle in the types: the queue is
/// built over an unbound `LateRunner`, then the composition root binds
/// `DeployService::run` once the ports exist.
#[derive(Clone, Default)]
pub struct LateRunner(Arc<OnceLock<RunFn>>);

impl fmt::Debug for LateRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LateRunner")
            .field("bound", &self.0.get().is_some())
            .finish()
    }
}

impl LateRunner {
    /// Sets what a deploy runs. Returns `false` (and keeps the first body)
    /// when it was bound before.
    pub fn bind(&self, run: impl Fn(DeployId) -> RunFuture + Send + Sync + 'static) -> bool {
        self.0.set(Box::new(run)).is_ok()
    }
}

impl Runner for LateRunner {
    fn run(&self, deploy: DeployId) -> impl Future<Output = Result<(), Error>> + Send {
        self.0.get().map_or_else(
            || -> RunFuture {
                Box::pin(core::future::ready(Err(Error::unavailable(
                    "the deploy runner is not wired yet",
                ))))
            },
            |run| run(deploy),
        )
    }
}

/// How [`JobQueue::shutdown`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Every running deploy finished within the grace period.
    Drained,
    /// The grace period ran out; the deploys still running were cut off.
    /// Their records stay `running` until the next start reconciles them.
    TimedOut,
}

#[derive(Default)]
struct State {
    /// Pending deploys per environment, by number: creation order no matter in
    /// which order the enqueue calls get here. An environment has an entry
    /// exactly while its worker is alive.
    pending: HashMap<EnvironmentId, BTreeMap<DeployNumber, DeployId>>,
    closed: bool,
    workers: JoinSet<()>,
}

struct Shared<S, R, C> {
    store: Arc<S>,
    clock: Arc<C>,
    runner: R,
    permits: Semaphore,
    state: Mutex<State>,
}

impl<S, R, C> Shared<S, R, C> {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// See the [module documentation](self). Cheap to clone.
pub struct JobQueue<S, R, C> {
    shared: Arc<Shared<S, R, C>>,
}

impl<S, R, C> fmt::Debug for JobQueue<S, R, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobQueue").finish_non_exhaustive()
    }
}

impl<S, R, C> Clone for JobQueue<S, R, C> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<S: Store + 'static, R: Runner, C: Clock + 'static> JobQueue<S, R, C> {
    /// A queue running at most `parallel` deploys at once (at least one,
    /// `[deploy] parallel`).
    /// `clock` stamps the deploys the queue has to fail itself.
    pub fn new(store: Arc<S>, clock: Arc<C>, runner: R, parallel: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                store,
                clock,
                runner,
                permits: Semaphore::new(parallel.max(1)),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Stops accepting deploys, drops the ones that have not started and
    /// waits up to `grace` for the running ones to finish. Deploys that
    /// were dropped stay `queued` in the store; the reconciler fails them
    /// on the next start.
    pub async fn shutdown(&self, grace: Duration) -> ShutdownOutcome {
        let mut workers = {
            let mut state = self.shared.state();
            state.closed = true;
            for queue in state.pending.values_mut() {
                queue.clear();
            }
            core::mem::take(&mut state.workers)
        };
        let drained = async { while workers.join_next().await.is_some() {} };
        if tokio::time::timeout(grace, drained).await.is_ok() {
            ShutdownOutcome::Drained
        } else {
            workers.abort_all();
            ShutdownOutcome::TimedOut
        }
    }

    async fn schedule(&self, id: DeployId) -> Result<(), Error> {
        let deploy = self
            .shared
            .store
            .deploy(id)
            .await?
            .ok_or_else(|| Error::not_found("deploy"))?;
        self.register(deploy.environment_id, deploy.number, id)
    }

    /// Adds `id` to its environment's pending set and starts the worker if the
    /// environment had none. Never awaits, so the lock is short.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the whole body is the critical section: check closed, push, spawn"
    )]
    fn register(
        &self,
        environment: EnvironmentId,
        number: DeployNumber,
        id: DeployId,
    ) -> Result<(), Error> {
        let mut state = self.shared.state();
        let state = &mut *state;
        if state.closed {
            return Err(closed());
        }
        // Finished workers are reaped here so the set does not grow.
        while state.workers.try_join_next().is_some() {}
        match state.pending.entry(environment) {
            Entry::Occupied(mut queue) => {
                let _previous = queue.get_mut().insert(number, id);
            }
            Entry::Vacant(slot) => {
                let _queue = slot.insert(BTreeMap::from([(number, id)]));
                let shared = Arc::clone(&self.shared);
                let _abort_handle = state.workers.spawn(work(shared, environment));
            }
        }
        Ok(())
    }
}

fn closed() -> Error {
    Error::unavailable("the deploy queue is shutting down")
}

impl<S: Store + 'static, R: Runner, C: Clock + 'static> DeployQueue for JobQueue<S, R, C> {
    fn enqueue(&self, deploy: DeployId) -> impl Future<Output = Result<(), Error>> + Send {
        self.schedule(deploy)
    }
}

/// Aborts the deploy task when the worker is cut off.
struct AbortOnDrop(JoinHandle<Result<(), Error>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The worker of one environment: runs its pending deploys in order, then
/// removes the environment's entry and ends.
async fn work<S: Store + 'static, R: Runner, C: Clock + 'static>(
    shared: Arc<Shared<S, R, C>>,
    environment: EnvironmentId,
) {
    loop {
        let Ok(permit) = shared.permits.acquire().await else {
            return;
        };
        let next = {
            let mut state = shared.state();
            let next = if state.closed {
                None
            } else {
                state
                    .pending
                    .get_mut(&environment)
                    .and_then(BTreeMap::pop_first)
                    .map(|(_, id)| id)
            };
            if next.is_none() {
                let _gone = state.pending.remove(&environment);
            }
            next
        };
        let Some(id) = next else {
            return;
        };
        run_isolated(&shared, id).await;
        drop(permit);
    }
}

/// Runs one deploy in its own task so a panic cannot take the worker down.
/// A deploy that ends without a terminal status (a panic, a cancelled task,
/// an error before the runner recorded one) is failed here: a record stuck
/// `running` would refuse every stop, start and domain change of its environment
/// until the next restart.
async fn run_isolated<S: Store + 'static, R: Runner, C: Clock + 'static>(
    shared: &Arc<Shared<S, R, C>>,
    id: DeployId,
) {
    let job = Arc::clone(shared);
    let mut task = AbortOnDrop(tokio::spawn(async move { job.runner.run(id).await }));
    match (&mut task.0).await {
        Ok(Ok(())) => return,
        Ok(Err(error)) => tracing::error!(deploy = %id, ?error, "deploy ended with an error"),
        Err(join) if join.is_panic() => tracing::error!(deploy = %id, "deploy panicked"),
        Err(join) => tracing::warn!(deploy = %id, error = %join, "deploy task was cancelled"),
    }
    if let Err(error) = fail_if_unfinished(shared, id).await {
        tracing::error!(deploy = %id, ?error, "could not fail an unfinished deploy");
    }
}

/// The `error` of a deploy that crashed inside the daemon. Generic on
/// purpose: the cause goes to the log, not to API callers.
const CRASHED: &str = "deploy crashed";

async fn fail_if_unfinished<S: Store + 'static, R: Runner, C: Clock + 'static>(
    shared: &Shared<S, R, C>,
    id: DeployId,
) -> Result<(), Error> {
    let Some(mut deploy) = shared.store.deploy(id).await? else {
        return Ok(());
    };
    if deploy.status.is_terminal() {
        return Ok(());
    }
    deploy.status = DeployStatus::Failed;
    deploy.error = Some(CRASHED.to_owned());
    deploy.finished_at = Some(shared.clock.now());
    shared.store.update_deploy(&deploy).await
}

#[cfg(test)]
mod tests;
