//! The runtime of functions.
//!
//! A function is a component of the world `wasi:http/proxy@0.2`
//! (`wit/function/`: plus `wasi:config/store` and `wasi:keyvalue`).
//! [`FunctionRuntime`] precompiles it ([`FunctionRuntime::precompile`]:
//! its size, the export `wasi:http/incoming-handler` and its `wasi:`-only
//! imports are checked before it compiles, FN-01; the compiled
//! code is cached by the component's hash and the wasmtime version under
//! the cache directory, FN-09), activates it for an environment
//! ([`FunctionRuntime::activate`]: one instance and one `GET /` with
//! [`WARMUP_HEADER`], whose answer is ignored and whose trap is an error
//! that leaves the environment's earlier component active, FN-10) and
//! hands it requests ([`FunctionRuntime::invoke`]) through a pool of at
//! most [`FunctionLimits::instances_per_env`] instances per environment
//! (FN-07):
//!
//! - `wasi:config` answers the environment's resolved secrets, the [`Env`]
//!   the function was activated with (FN-02);
//! - `wasi:keyvalue` (`store`, `atomics`, `batch`) reads and writes the
//!   environment's `kv` rows through the [`Store`] port, so environments
//!   never see each other's keys (FN-03); a write past
//!   [`FunctionLimits::kv_max_bytes`] is the function's `wasi:keyvalue`
//!   error, not the daemon's (FN-04);
//! - a request running past [`FunctionLimits::timeout`] (one deadline from
//!   its arrival for waiting for an instance, making a fresh one and
//!   serving it) is interrupted (epoch interruption) and answers
//!   [`FunctionError::Timeout`], 504; its instance is dropped and the next
//!   request runs in a fresh one (FN-05, FN-07); the warm-up waits for an
//!   instance within its own deadline the same way;
//! - a client's [`WARMUP_HEADER`] is removed before the function sees the
//!   request: only the daemon's warm-up carries one (FN-10);
//! - the `wasi:keyvalue` rules (key length, `functions.kv_max_mb`) are the
//!   core's ([`liftoff_core::service::check_kv_write`]);
//! - the daemon's own failures (its engine, its compile cache, a compile
//!   that did not finish) are [`FunctionError::HostFailed`] with a fixed
//!   reason; the cause goes to the log;
//! - the memories of an instance together past
//!   [`FunctionLimits::max_memory_bytes`] trap: 500, the pool keeps
//!   serving (FN-06); so does a table past the module host's cap, and an
//!   instance cannot make more core instances, tables or memories than a
//!   module's;
//! - outgoing `wasi:http` goes through the host with
//!   [`FunctionLimits::outgoing_timeout`] and no allow-list of hosts; like
//!   a module's, it refuses loopback and private addresses unless
//!   [`FunctionRuntime::allowing_private_networks`] (FN-08);
//! - a request's response body is read up to [`MAX_RESPONSE_BYTES`]; a
//!   larger one is a trap;
//! - no `store-query`, no preopened directories, no sockets, no
//!   environment variables; stdout and stderr are the environment's log
//!   in memory, its last [`KEPT_LINES`] lines ([`FunctionRuntime::logs`],
//!   `lof logs` of a function).

mod config;
mod http;
mod keyvalue;
mod logs;
mod names;
mod pool;

use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use liftoff_core::Error;
use liftoff_core::domain::{ComponentHash, Env, EnvironmentId};
use liftoff_core::ports::{FunctionHost, Store};
use tokio::sync::Semaphore;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Cache, Engine};
use wasmtime_wasi_http::p2::bindings::ProxyPre;

use self::pool::{FnState, HostFault, PastLimit, Pool, Recipe};
use crate::CompileStats;
use crate::engine::engine;

pub use self::http::MAX_RESPONSE_BYTES;
pub use self::logs::{KEPT_LINES, MAX_LINE_BYTES};

/// The largest component [`FunctionRuntime::precompile`] takes: the 50 MB
/// of `POST /deploys`, so a caller that skips that limit
/// cannot make the runtime copy or compile more.
pub const MAX_COMPONENT_BYTES: usize = liftoff_core::domain::MAX_COMPONENT_BYTES;

/// [`MAX_COMPONENT_BYTES`] in MiB.
const MAX_COMPONENT_MIB: usize = 50;

/// The header of the warm-up request (`GET /`; FN-10).
pub const WARMUP_HEADER: &str = "x-liftoff-warmup";

/// What one environment's function may use (`[functions]` of the daemon's configuration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionLimits {
    /// The most bytes the linear memories of one instance may hold
    /// together (`functions.max_memory_mb`).
    pub max_memory_bytes: usize,
    /// How long one request may take from its arrival, waiting for an
    /// instance included (`functions.timeout`).
    pub timeout: Duration,
    /// How many instances of an environment run requests at once
    /// (`functions.instances_per_env`); one more request waits, at most
    /// until its [`FunctionLimits::timeout`].
    pub instances_per_env: usize,
    /// How many bytes an environment's keys and values may take together
    /// (`functions.kv_max_mb`).
    pub kv_max_bytes: u64,
    /// How long an outgoing `wasi:http` request may take.
    pub outgoing_timeout: Duration,
}

/// Why the runtime refused a component or a request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FunctionError {
    /// The bytes are not a component of the world `wasi:http/proxy@0.2`
    /// (no export `wasi:http/incoming-handler`, an import the runtime does
    /// not link) or do not compile (FN-01).
    #[error("the function does not load: {reason}")]
    Load {
        /// What went wrong, for the person who deploys it.
        reason: String,
    },
    /// The warm-up request trapped or timed out: the component is not
    /// activated (FN-10).
    #[error("the function's warm-up failed: {reason}")]
    Warmup {
        /// What went wrong.
        reason: String,
    },
    /// No component is active for the environment.
    #[error("no function is active for the environment")]
    NotActive,
    /// The request ran past [`FunctionLimits::timeout`] (FN-05).
    #[error("the function timed out")]
    Timeout,
    /// The request trapped: a panic of the function, its memory cap
    /// (FN-06). The trap is logged in full; this carries its short reason.
    #[error("the function trapped: {reason}")]
    Trap {
        /// The trap code or the host's own message.
        reason: String,
    },
    /// The daemon failed under the request (the store behind
    /// `wasi:keyvalue`) or under itself (its engine, its compile cache);
    /// the cause is logged, never carried here.
    #[error("the host of the function failed: {reason}")]
    HostFailed {
        /// What went wrong.
        reason: String,
    },
}

impl FunctionError {
    /// The HTTP status the function listener answers for it: 504 for a
    /// timeout, 503 when nothing is active, 500 for a trap, a failed
    /// warm-up or the host's failure, 400 for a component that does not
    /// load.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::Load { .. } => 400,
            Self::NotActive => 503,
            Self::Timeout => 504,
            Self::Warmup { .. } | Self::Trap { .. } | Self::HostFailed { .. } => 500,
        }
    }
}

/// The reason of a [`FunctionError::Trap`] whose cause is the host's own
/// error rather than a trap code or a limit: the cause goes to the log.
const HOST_CAUSE: &str = "the host stopped the function; the daemon's log has the cause";

/// A failure of wasmtime during a request of `environment`'s function: an
/// epoch interruption past its deadline is a [`FunctionError::Timeout`]
/// (FN-05), a failed store a [`FunctionError::HostFailed`], a trap code
/// or a passed limit a trap with that reason (FN-06). Any other error
/// (wasmtime's or a host import's, such as a full resource table or too
/// many core instances) is a trap whose cause is logged, never answered.
fn failure(environment: EnvironmentId, error: &wasmtime::Error) -> FunctionError {
    if let Some(fault) = error.downcast_ref::<HostFault>() {
        return FunctionError::HostFailed {
            reason: fault.to_string(),
        };
    }
    if let Some(limit) = error.downcast_ref::<PastLimit>() {
        tracing::warn!(%environment, %limit, "a function passed a limit");
        return FunctionError::Trap {
            reason: limit.to_string(),
        };
    }
    match error.downcast_ref::<wasmtime::Trap>() {
        Some(wasmtime::Trap::Interrupt) => {
            tracing::warn!(%environment, "a function request ran past functions.timeout");
            FunctionError::Timeout
        }
        Some(trap) => {
            tracing::warn!(%environment, error = ?error, "a function trapped");
            FunctionError::Trap {
                reason: trap.to_string(),
            }
        }
        None => {
            tracing::error!(%environment, error = ?error, "a function failed in its host");
            FunctionError::Trap {
                reason: HOST_CAUSE.to_owned(),
            }
        }
    }
}

/// The reason of a [`FunctionError::HostFailed`] whose cause is the
/// daemon's own (its engine, its cache directory, a compile that did not
/// finish): the cause goes to the log.
const INTERNAL_CAUSE: &str = "the daemon failed; its log has the cause";

/// The daemon's own failure at `what`: logged in full, answered as a
/// [`FunctionError::HostFailed`] that names neither it nor its paths.
fn internal(what: &str, error: &dyn core::fmt::Display) -> FunctionError {
    tracing::error!(error = %format!("{error:#}"), "{what} failed");
    FunctionError::HostFailed {
        reason: INTERNAL_CAUSE.to_owned(),
    }
}

/// Checks what [`FunctionRuntime::precompile`] checks first, without
/// compiling anything.
///
/// `component` is no larger than [`MAX_COMPONENT_BYTES`],
/// is a component (not a core module), exports
/// `wasi:http/incoming-handler@0.2` and imports `wasi:` interfaces only
/// (FN-01). `POST /deploys` runs it before it keeps the file (FDEP-02),
/// `lof deploy` before it uploads (FDEP-08).
///
/// # Errors
///
/// [`FunctionError::Load`] with a readable reason.
pub fn check(component: &[u8]) -> Result<(), FunctionError> {
    if component.len() > MAX_COMPONENT_BYTES {
        return Err(FunctionError::Load {
            reason: format!("it is larger than {MAX_COMPONENT_MIB} MiB"),
        });
    }
    names::check(component)
}

/// A component precompiled by [`FunctionRuntime::precompile`]: checked
/// and compiled by that runtime's engine.
#[derive(Clone)]
pub struct Compiled {
    hash: ComponentHash,
    component: Component,
}

impl core::fmt::Debug for Compiled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Compiled")
            .field("hash", &self.hash)
            .finish_non_exhaustive()
    }
}

impl Compiled {
    /// The SHA-256 of the component's bytes (`deploys.component_hash`).
    #[must_use]
    pub const fn hash(&self) -> &ComponentHash {
        &self.hash
    }
}

/// What the runtime keeps for one environment: the permits of its
/// instances (FN-07), the lock of its `wasi:keyvalue` writes, and its
/// active function.
struct Slot<S: 'static> {
    permits: Arc<Semaphore>,
    kv_writes: Arc<tokio::sync::Mutex<()>>,
    /// What its functions wrote, across the components it activates.
    log: Arc<logs::FunctionLog>,
    active: Mutex<Option<Arc<Pool<S>>>>,
}

/// The runtime of functions over the store `S` (`wasi:keyvalue` → `kv`).
pub struct FunctionRuntime<S: 'static> {
    store: Arc<S>,
    limits: FunctionLimits,
    engine: Engine,
    linker: Linker<FnState<S>>,
    cache_dir: Option<PathBuf>,
    cache: Option<Cache>,
    private_networks: bool,
    /// Components compiled or taken from the compile cache so far.
    loads: AtomicUsize,
    environments: Mutex<HashMap<EnvironmentId, Arc<Slot<S>>>>,
}

impl<S: 'static> core::fmt::Debug for FunctionRuntime<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FunctionRuntime")
            .field("limits", &self.limits)
            .field("cache_dir", &self.cache_dir)
            .field("private_networks", &self.private_networks)
            .finish_non_exhaustive()
    }
}

/// The engine of a runtime, its compile cache and the linker of the
/// function world.
type Machinery<S> = (Engine, Option<Cache>, Linker<FnState<S>>);

/// The engine of a runtime (with its compile cache in `cache_dir`) and
/// the linker of the function world.
fn engine_and_linker<S: Store + 'static>(
    cache_dir: Option<&std::path::Path>,
) -> Result<Machinery<S>, FunctionError> {
    let (engine, cache) =
        engine(cache_dir).map_err(|error| internal("the engine of functions", &error))?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .and_then(|()| wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker))
        .and_then(|()| {
            config::bindings::wasi::config::store::add_to_linker::<_, HasSelf<_>>(
                &mut linker,
                |state| state,
            )
        })
        .and_then(|()| {
            keyvalue::bindings::wasi::keyvalue::store::add_to_linker::<_, HasSelf<_>>(
                &mut linker,
                |state| state,
            )
        })
        .and_then(|()| {
            keyvalue::bindings::wasi::keyvalue::atomics::add_to_linker::<_, HasSelf<_>>(
                &mut linker,
                |state| state,
            )
        })
        .and_then(|()| {
            keyvalue::bindings::wasi::keyvalue::batch::add_to_linker::<_, HasSelf<_>>(
                &mut linker,
                |state| state,
            )
        })
        .map_err(|error| internal("the linker of functions", &error))?;
    Ok((engine, cache, linker))
}

/// `config` as `wasi:config` answers it: key → value.
fn config_of(config: &Env) -> BTreeMap<String, String> {
    config
        .vars()
        .iter()
        .map(|(key, value)| (key.to_string(), value.expose().to_owned()))
        .collect()
}

/// The warm-up request (FN-10).
fn warmup_request() -> Result<::http::Request<Bytes>, FunctionError> {
    ::http::Request::get("/")
        .header(::http::header::HOST, "localhost")
        .header(WARMUP_HEADER, "1")
        .body(Bytes::new())
        .map_err(|error| internal("the warm-up request", &error))
}

impl<S: Store + 'static> FunctionRuntime<S> {
    /// A runtime with `limits` whose functions keep their keys in `store`;
    /// outgoing `wasi:http` reaches public addresses only until
    /// [`FunctionRuntime::allowing_private_networks`].
    ///
    /// # Errors
    ///
    /// [`FunctionError::HostFailed`] when the engine cannot be made.
    pub fn new(store: S, limits: FunctionLimits) -> Result<Self, FunctionError> {
        let (engine, cache, linker) = engine_and_linker(None)?;
        Ok(Self {
            store: Arc::new(store),
            limits,
            engine,
            linker,
            cache_dir: None,
            cache,
            private_networks: false,
            loads: AtomicUsize::new(0),
            environments: Mutex::new(HashMap::new()),
        })
    }

    /// Keeps compiled components in wasmtime's compile cache in `dir`
    /// (`paths.data/cache/`), keyed by the component's hash and the
    /// wasmtime version, made the daemon's own (`0700`) like the modules'
    /// (FN-09). Call it before anything is precompiled: the runtime gets
    /// a new engine.
    ///
    /// # Errors
    ///
    /// [`FunctionError::HostFailed`] when the directory is refused or the
    /// engine cannot be made; the cause goes to the log.
    pub fn with_compile_cache(self, dir: impl Into<PathBuf>) -> Result<Self, FunctionError> {
        let dir = dir.into();
        let (engine, cache, linker) = engine_and_linker(Some(&dir))?;
        Ok(Self {
            engine,
            linker,
            cache,
            cache_dir: Some(dir),
            loads: AtomicUsize::new(0),
            environments: Mutex::new(HashMap::new()),
            ..self
        })
    }

    /// Lets outgoing `wasi:http` reach loopback and private addresses
    /// (tests; the daemon never does).
    #[must_use]
    pub fn allowing_private_networks(self) -> Self {
        Self {
            private_networks: true,
            ..self
        }
    }

    /// The store the functions' keys live in.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The limits functions run with.
    #[must_use]
    pub const fn limits(&self) -> FunctionLimits {
        self.limits
    }

    /// Components compiled and components taken from the compile cache so
    /// far (FN-09); without a cache every precompile compiles.
    #[must_use]
    pub fn compile_stats(&self) -> CompileStats {
        let loads = self.loads.load(Ordering::SeqCst);
        let cached = self.cache.as_ref().map_or(0, Cache::cache_hits);
        CompileStats {
            compiled: loads.saturating_sub(cached),
            cached,
        }
    }

    /// Checks that `component` is a component of the world
    /// `wasi:http/proxy@0.2` with the export `wasi:http/incoming-handler`
    /// whose imports the runtime links, and compiles it (on a blocking
    /// thread), or takes it from the compile cache by its hash; a second
    /// call with the same bytes compiles nothing. Its size, its export
    /// and the names of its imports are checked before it compiles
    /// ([`MAX_COMPONENT_BYTES`]; only `wasi:` interfaces), so a refused
    /// component writes nothing to the compile cache.
    ///
    /// # Errors
    ///
    /// [`FunctionError::Load`] with a readable reason (FN-01);
    /// [`FunctionError::HostFailed`] when the compile does not finish.
    pub async fn precompile(&self, component: &[u8]) -> Result<Compiled, FunctionError> {
        // Before anything compiles, so a refused component leaves nothing
        // in the compile cache.
        check(component)?;
        let hash = ComponentHash::of(component);
        let engine = self.engine.clone();
        let bytes = component.to_vec();
        let compiled = tokio::task::spawn_blocking(move || Component::new(&engine, &bytes))
            .await
            .map_err(|joined| internal("the compile of a function", &joined))?
            .map_err(|error| FunctionError::Load {
                reason: format!("it is not a WebAssembly component that compiles: {error:#}"),
            })?;
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.pre(&compiled)?;
        Ok(Compiled {
            hash,
            component: compiled,
        })
    }

    /// The instance-pre of `component`: its imports linked.
    fn pre(&self, component: &Component) -> Result<ProxyPre<FnState<S>>, FunctionError> {
        if !Engine::same(component.engine(), &self.engine) {
            return Err(FunctionError::Load {
                reason: "it was precompiled by another runtime".to_owned(),
            });
        }
        let pre = self
            .linker
            .instantiate_pre(component)
            .map_err(|error| FunctionError::Load {
                reason: format!("it imports what a function cannot use: {error:#}"),
            })?;
        ProxyPre::new(pre).map_err(|error| FunctionError::Load {
            reason: format!("it does not export wasi:http/incoming-handler@0.2: {error:#}"),
        })
    }

    /// The slot of `environment`, made when it has none.
    fn slot(&self, environment: EnvironmentId) -> Arc<Slot<S>> {
        let mut environments = self
            .environments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(environments.entry(environment).or_insert_with(|| {
            Arc::new(Slot {
                permits: Arc::new(Semaphore::new(self.limits.instances_per_env.max(1))),
                kv_writes: Arc::new(tokio::sync::Mutex::new(())),
                log: Arc::default(),
                active: Mutex::new(None),
            })
        }))
    }

    /// Makes `compiled` the function of `environment` with `config` as its
    /// `wasi:config`: one instance answers one `GET /` with
    /// [`WARMUP_HEADER`] (its answer ignored, whatever its status) and
    /// then serves the pool. An active component of the environment is
    /// replaced only when the warm-up succeeds; requests already running
    /// in it finish there.
    ///
    /// # Errors
    ///
    /// [`FunctionError::Load`] when `compiled` comes from another runtime;
    /// [`FunctionError::Warmup`] when the warm-up traps, times out or the
    /// host fails under it; the earlier component, if any, stays active
    /// (FN-10).
    pub async fn activate(
        &self,
        environment: EnvironmentId,
        compiled: &Compiled,
        config: Env,
    ) -> Result<(), FunctionError> {
        let pre = self.pre(&compiled.component)?;
        let slot = self.slot(environment);
        let recipe = Recipe {
            pre,
            hash: compiled.hash.clone(),
            environment,
            store: Arc::clone(&self.store),
            config: Arc::new(config_of(&config)),
            limits: self.limits,
            private_networks: self.private_networks,
            kv_writes: Arc::clone(&slot.kv_writes),
            log: Arc::clone(&slot.log),
        };
        let failed = |error: FunctionError| {
            tracing::warn!(%environment, hash = %compiled.hash, %error, "a function's warm-up failed; it is not activated");
            FunctionError::Warmup {
                reason: error.to_string(),
            }
        };
        let warm = {
            // Waiting for a busy instance counts toward the warm-up's time.
            let until = recipe.deadline();
            let _permit = pool::acquire(&slot.permits, environment, until)
                .await
                .map_err(failed)?;
            let mut instance = recipe.instantiate(until).await.map_err(failed)?;
            let _ignored = recipe
                .serve(&mut instance, warmup_request()?, until)
                .await
                .map_err(failed)?;
            instance
        };
        let pool = Arc::new(Pool::new(recipe, Arc::clone(&slot.permits), warm));
        let replaced = slot
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(pool);
        tracing::info!(%environment, hash = %compiled.hash, replaced = ?replaced.map(|old| old.recipe.hash.clone()), "a function is active");
        Ok(())
    }

    /// Hands `request` to the active function of `environment` and returns
    /// its response. The request waits while all
    /// [`FunctionLimits::instances_per_env`] instances are busy, and the
    /// wait counts toward its [`FunctionLimits::timeout`]. A
    /// [`WARMUP_HEADER`] of the request is removed: only the daemon's
    /// warm-up carries one.
    ///
    /// # Errors
    ///
    /// [`FunctionError::NotActive`], [`FunctionError::Timeout`] (FN-05, or
    /// still waiting for an instance at its deadline),
    /// [`FunctionError::Trap`] (FN-06) or [`FunctionError::HostFailed`].
    pub async fn invoke(
        &self,
        environment: EnvironmentId,
        request: ::http::Request<Bytes>,
    ) -> Result<::http::Response<Bytes>, FunctionError> {
        let slot = self
            .environments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&environment)
            .map(Arc::clone);
        let pool = slot
            .and_then(|slot| {
                slot.active
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .map(Arc::clone)
            })
            .ok_or(FunctionError::NotActive)?;
        let mut request = request;
        // Only the daemon's warm-up carries it (FN-10): a client's would
        // pass for one.
        request.headers_mut().remove(WARMUP_HEADER);
        pool.invoke(request).await
    }

    /// Whether a component is active for `environment`.
    #[must_use]
    pub fn is_active(&self, environment: EnvironmentId) -> bool {
        self.environments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&environment)
            .is_some_and(|slot| {
                slot.active
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some()
            })
    }

    /// What the functions of `environment` wrote to their stdout and
    /// stderr, one item per line, oldest first: the last [`KEPT_LINES`]
    /// (the last `tail` of them when given), then, with `follow`, every
    /// new line as it comes.
    pub fn logs(
        &self,
        environment: EnvironmentId,
        tail: Option<u32>,
        follow: bool,
    ) -> impl futures_util::Stream<Item = Result<String, Error>> + Send + 'static + use<S> {
        self.slot(environment).log.lines(tail, follow)
    }
}

/// What a function deploy is told of a refusal of the runtime: the
/// component's own faults are [`Error::Validation`] on `component`,
/// readable by the person who deploys it; the daemon's are internal with
/// the runtime's fixed reason (the cause is in the log already).
fn deploy_error(error: &FunctionError) -> Error {
    match error {
        FunctionError::Load { .. } | FunctionError::Warmup { .. } => {
            Error::invalid("component", error.to_string())
        }
        FunctionError::NotActive
        | FunctionError::Timeout
        | FunctionError::Trap { .. }
        | FunctionError::HostFailed { .. } => Error::internal_msg(error.to_string()),
    }
}

/// A [`FunctionRuntime`] as the core's [`FunctionHost`]: what a deploy
/// drives.
///
/// `check` reads the names without compiling, `precompile` fills the
/// compile cache, `activate` warms the component up and switches the
/// environment to it. The runtime stays shared with whoever serves the
/// environments' requests ([`FunctionDeployer::runtime`]).
pub struct FunctionDeployer<S: 'static> {
    runtime: Arc<FunctionRuntime<S>>,
}

impl<S: 'static> core::fmt::Debug for FunctionDeployer<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FunctionDeployer")
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl<S: 'static> FunctionDeployer<S> {
    /// Deploys into `runtime`.
    #[must_use]
    pub const fn new(runtime: Arc<FunctionRuntime<S>>) -> Self {
        Self { runtime }
    }

    /// The runtime deploys go into.
    #[must_use]
    pub const fn runtime(&self) -> &Arc<FunctionRuntime<S>> {
        &self.runtime
    }
}

impl<S: Store + 'static> FunctionHost for FunctionDeployer<S> {
    fn check(&self, component: &[u8]) -> impl Future<Output = Result<(), Error>> + Send {
        core::future::ready(check(component).map_err(|error| match error {
            FunctionError::Load { reason } => Error::invalid(
                "component",
                format!("it is not a wasi:http component: {reason}"),
            ),
            other => deploy_error(&other),
        }))
    }

    async fn precompile(&self, component: &[u8]) -> Result<(), Error> {
        self.runtime
            .precompile(component)
            .await
            .map(drop)
            .map_err(|error| deploy_error(&error))
    }

    async fn activate(
        &self,
        environment: EnvironmentId,
        component: &[u8],
        config: Env,
    ) -> Result<(), Error> {
        // Precompiled by the deploy a moment ago: the compile cache has it,
        // so this loads rather than compiles (FN-09).
        let compiled = self
            .runtime
            .precompile(component)
            .await
            .map_err(|error| deploy_error(&error))?;
        self.runtime
            .activate(environment, &compiled, config)
            .await
            .map_err(|error| deploy_error(&error))
    }

    fn is_active(&self, environment: EnvironmentId) -> impl Future<Output = bool> + Send {
        core::future::ready(self.runtime.is_active(environment))
    }

    fn logs(
        &self,
        environment: EnvironmentId,
        tail: Option<u32>,
        follow: bool,
    ) -> impl Future<
        Output = Result<
            impl futures_util::Stream<Item = Result<String, Error>> + Send + 'static,
            Error,
        >,
    > + Send {
        core::future::ready(Ok(self.runtime.logs(environment, tail, follow)))
    }
}
