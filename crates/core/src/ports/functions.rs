use futures_core::Stream;

use crate::domain::{ComponentHash, Env, EnvironmentId};
use crate::error::Error;

/// The runtime of functions as a deploy drives it: it checks a component, precompiles it and makes it the
/// function of an environment.
///
/// The daemon's host is the wasmtime runtime of `liftoff-wasm`
/// (`FunctionRuntime` implements it); tests use `FakeFunctions`. Every rule of a
/// deploy (when to check, what to keep, what a failure means) lives in
/// [`DeployService`](crate::service::DeployService); the host only does
/// what it is told.
pub trait FunctionHost: Send + Sync {
    /// Checks, without compiling or writing anything, that `component` is
    /// a component of the world `wasi:http/proxy@0.2` with the export
    /// `wasi:http/incoming-handler` whose imports a function may use, no
    /// larger than 50 MiB (FDEP-02).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on field `component` with a readable reason.
    fn check(&self, component: &[u8]) -> impl Future<Output = Result<(), Error>> + Send;

    /// Compiles `component` ahead of its activation into the compile cache
    /// (by its hash and the wasmtime version, FN-09); a component already
    /// compiled compiles nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on field `component` when it does not load;
    /// [`Error::Internal`] when the host fails.
    fn precompile(&self, component: &[u8]) -> impl Future<Output = Result<(), Error>> + Send;

    /// Makes `component` the function of `environment` with `config` as
    /// its `wasi:config`: one instance answers the warm-up (`GET /` with
    /// `X-Liftoff-Warmup`), then the environment's requests go to it (the
    /// route switches). A failed warm-up activates nothing: the earlier
    /// component, if any, keeps serving (FDEP-04).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on field `component` when it does not load or
    /// its warm-up traps or times out (the reason is the deploy's error);
    /// [`Error::Internal`] when the host fails.
    fn activate(
        &self,
        environment: EnvironmentId,
        component: &[u8],
        config: Env,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Whether a component is active for `environment` (requests to it
    /// are answered). A read: nothing is loaded or changed.
    fn is_active(&self, environment: EnvironmentId) -> impl Future<Output = bool> + Send;

    /// What the function of `environment` wrote to its stdout and stderr,
    /// one item per line, oldest first, as far as the host keeps it (`lof logs` of a function): the last `tail`
    /// lines only when given; `follow` keeps streaming new lines.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the host fails.
    fn logs(
        &self,
        environment: EnvironmentId,
        tail: Option<u32>,
        follow: bool,
    ) -> impl Future<
        Output = Result<impl Stream<Item = Result<String, Error>> + Send + 'static, Error>,
    > + Send;
}

/// The kept `.wasm` files of function deploys,
/// `paths.data/functions/<hash>.wasm`: one file
/// per [`ComponentHash`], shared by every deploy of that component.
///
/// The core does no I/O, so a deploy keeps, reads and deletes them
/// through this port; the daemon's adapter is `liftoff-host`'s
/// `FileComponentFiles`, tests use `FakeComponentFiles`.
pub trait ComponentFiles: Send + Sync {
    /// The lock every write and every cleanup of these files takes
    /// (FDEP-06), shared by every clone of the adapter.
    fn lock(&self) -> &ComponentsLock;

    /// Keeps `component` as the file of `hash` (its SHA-256); a file
    /// already kept stays as it is.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when it cannot be written.
    fn save(
        &self,
        hash: &ComponentHash,
        component: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// The kept file of `hash`; `None` when there is none.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when it cannot be read.
    fn read(
        &self,
        hash: &ComponentHash,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send;

    /// The hashes of every kept file, in no particular order.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the directory cannot be read.
    fn list(&self) -> impl Future<Output = Result<Vec<ComponentHash>, Error>> + Send;

    /// Deletes the kept file of `hash`; nothing when there is none.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when it cannot be deleted.
    fn remove(&self, hash: &ComponentHash) -> impl Future<Output = Result<(), Error>> + Send;
}

/// The lock of the kept component files (FDEP-06).
///
/// A request holds it from storing its deploys to keeping their file, a
/// redeploy from reading the deploy it copies to storing the copy, a
/// cleanup from listing the files to removing the last one. A file can
/// already be kept when a new deploy refers to it again (`save` then
/// writes nothing), so only excluding the cleanup in between keeps a
/// cleanup from removing the file of a deploy that is about to run.
#[derive(Debug, Default)]
pub struct ComponentsLock(futures_util::lock::Mutex<()>);

impl ComponentsLock {
    /// An unheld lock.
    #[must_use]
    pub const fn new() -> Self {
        Self(futures_util::lock::Mutex::new(()))
    }

    /// Waits until no one else holds the lock; it is held until the
    /// returned guard drops.
    pub async fn hold(&self) -> ComponentsHold<'_> {
        ComponentsHold(self.0.lock().await)
    }
}

/// A held [`ComponentsLock`]; dropping it lets the next holder in.
#[derive(Debug)]
#[must_use = "the lock is released as soon as the guard drops"]
pub struct ComponentsHold<'lock>(
    #[expect(dead_code, reason = "held for its drop")] futures_util::lock::MutexGuard<'lock, ()>,
);
