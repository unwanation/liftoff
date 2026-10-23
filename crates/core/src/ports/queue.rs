use crate::domain::DeployId;
use crate::error::Error;

/// Where `DeployService` hands queued deploys for execution.
///
/// `liftoffd` implements it: an mpsc channel, a mutex per environment
/// (deploys of one environment run strictly one after another, in creation
/// order) and a global semaphore of `deploy.parallel` permits. The queue
/// calls `DeployService::run(id)` for each deploy; `run` knows nothing about
/// the queue.
///
/// # Errors
///
/// [`Error::Unavailable`] when the queue is shutting down.
pub trait DeployQueue: Send + Sync {
    /// Schedules a deploy whose record is already `queued` in the store.
    fn enqueue(&self, deploy: DeployId) -> impl Future<Output = Result<(), Error>> + Send;
}
