//! The instances of one environment's function: the state of their
//! wasmtime stores, how one is made, and the pool of at most
//! `instances_per_env` that serves requests (FN-05…FN-07).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use liftoff_core::domain::{ComponentHash, EnvironmentId};
use liftoff_core::ports::Store;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wasmtime::component::ResourceTable;
use wasmtime::{ResourceLimiter, UpdateDeadline};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::p2::bindings::{Proxy, ProxyPre};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use super::keyvalue::Kv;
use super::logs::{FunctionLog, FunctionOutput};
use super::{FunctionError, FunctionLimits};
use crate::engine::deadline;
use crate::http::AllowedHttp;
use crate::limits::{
    MAX_HOST_RESOURCES, MAX_HTTP_FIELDS_BYTES, MAX_INSTANCES, MAX_TABLE_ELEMENTS, MAX_TABLES,
};

/// A failure of the daemon under a request (the store behind
/// `wasi:keyvalue`): the request traps with it, and the runtime answers
/// [`FunctionError::HostFailed`], not a trap of the function.
#[derive(Debug, thiserror::Error)]
#[error("the store behind wasi:keyvalue failed")]
pub(super) struct HostFault;

/// How many linear memories one function instance may create. A component
/// of `wasm32-wasip2` has one; the cap only keeps the address space the
/// daemon reserves for an instance small. What the memories may hold
/// together is [`FunctionLimits::max_memory_bytes`].
pub(super) const MAX_MEMORIES: usize = 4;

/// A limit of [`FunctionLimits`] or of the runtime that an instance's
/// growth would pass: the growth traps with it (FN-06), and the caller
/// gets its message.
#[derive(Debug, thiserror::Error)]
pub(super) enum PastLimit {
    /// The memories of the instance together.
    #[error("its memory would pass functions.max_memory_mb ({max} bytes)")]
    Memory {
        /// `functions.max_memory_mb`, in bytes.
        max: usize,
    },
    /// One table.
    #[error("a table would pass {MAX_TABLE_ELEMENTS} elements")]
    Table,
}

/// The resource limiter of a function instance: the sizes of all its
/// linear memories together stay under `functions.max_memory_mb`, a table
/// under [`MAX_TABLE_ELEMENTS`] elements, and its core instances, tables
/// and memories under [`MAX_INSTANCES`], [`MAX_TABLES`] and
/// [`MAX_MEMORIES`] (functions are as untrusted as modules).
/// Growing past a limit traps; so does a growth the limiter allowed that
/// then failed.
pub(super) struct FnLimiter {
    max_memory_bytes: usize,
    /// The bytes the instance's memories hold together.
    memory_bytes: usize,
}

impl FnLimiter {
    pub(super) const fn new(max_memory_bytes: usize) -> Self {
        Self {
            max_memory_bytes,
            memory_bytes: 0,
        }
    }
}

impl ResourceLimiter for FnLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|maximum| desired > maximum) {
            return Ok(false);
        }
        let total = self
            .memory_bytes
            .saturating_sub(current)
            .saturating_add(desired);
        if total > self.max_memory_bytes {
            return Err(wasmtime::Error::new(PastLimit::Memory {
                max: self.max_memory_bytes,
            }));
        }
        self.memory_bytes = total;
        Ok(true)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        Err(error.context("a memory of the function could not grow"))
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|maximum| desired > maximum) {
            return Ok(false);
        }
        if desired > MAX_TABLE_ELEMENTS {
            return Err(wasmtime::Error::new(PastLimit::Table));
        }
        Ok(true)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        Err(error.context("a table of the function could not grow"))
    }

    fn instances(&self) -> usize {
        MAX_INSTANCES
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }

    fn memories(&self) -> usize {
        MAX_MEMORIES
    }
}

/// What the store of one function instance holds.
pub(crate) struct FnState<S> {
    pub(super) wasi: WasiCtx,
    pub(super) http: WasiHttpCtx,
    pub(super) hooks: AllowedHttp,
    pub(super) table: ResourceTable,
    pub(super) limits: FnLimiter,
    /// When the request in progress is interrupted (FN-05).
    pub(super) deadline: Instant,
    /// `wasi:config`: the environment's resolved secrets (FN-02).
    pub(super) config: Arc<BTreeMap<String, String>>,
    /// `wasi:keyvalue` (FN-03, FN-04).
    pub(super) kv: Kv<S>,
}

impl<S: Send + Sync + 'static> WasiView for FnState<S> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl<S: Send + Sync + 'static> WasiHttpView for FnState<S> {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// A function instance and the wasmtime store it lives in.
pub(super) struct Instance<S: 'static> {
    pub(super) store: wasmtime::Store<FnState<S>>,
    pub(super) proxy: Proxy,
}

/// What it takes to make an instance of one environment's function.
pub(super) struct Recipe<S: 'static> {
    pub(super) pre: ProxyPre<FnState<S>>,
    pub(super) hash: ComponentHash,
    pub(super) environment: EnvironmentId,
    pub(super) store: Arc<S>,
    pub(super) config: Arc<BTreeMap<String, String>>,
    pub(super) limits: FunctionLimits,
    pub(super) private_networks: bool,
    /// Held by every `wasi:keyvalue` write of the environment.
    pub(super) kv_writes: Arc<tokio::sync::Mutex<()>>,
    /// Where the instances' stdout and stderr go.
    pub(super) log: Arc<FunctionLog>,
}

impl<S: Store + 'static> Recipe<S> {
    /// The deadline of a request starting now: one `functions.timeout`
    /// for making its instance, when it needs a fresh one, and serving it
    /// together (FN-05).
    pub(super) fn deadline(&self) -> Instant {
        deadline(self.limits.timeout)
    }

    /// A new instance in a store of its own, made by `until` (the
    /// deadline of the request it is made for).
    pub(super) async fn instantiate(&self, until: Instant) -> Result<Instance<S>, FunctionError> {
        // No preopened directory, no sockets, no name lookup, no
        // environment variables: a function reaches the outside through
        // `wasi:http` and its secrets through `wasi:config` only.
        // Its stdout and stderr are its environment's log.
        let wasi = WasiCtxBuilder::new()
            .stdout(FunctionOutput(Arc::clone(&self.log)))
            .stderr(FunctionOutput(Arc::clone(&self.log)))
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .build();
        let mut table = ResourceTable::new();
        table.set_max_capacity(MAX_HOST_RESOURCES);
        let mut http = WasiHttpCtx::new();
        http.set_field_size_limit(MAX_HTTP_FIELDS_BYTES);
        let state = FnState {
            wasi,
            http,
            hooks: AllowedHttp::any_host(
                format!("function of environment {}", self.environment),
                self.private_networks,
                self.limits.outgoing_timeout,
            ),
            table,
            // The memories together are capped; growing past the cap
            // traps (FN-06).
            limits: FnLimiter::new(self.limits.max_memory_bytes),
            deadline: until,
            config: Arc::clone(&self.config),
            kv: Kv {
                store: Arc::clone(&self.store),
                environment: self.environment,
                max_bytes: self.limits.kv_max_bytes,
                writes: Arc::clone(&self.kv_writes),
            },
        };
        let mut store = wasmtime::Store::new(self.pre.engine(), state);
        store.limiter(|state| &mut state.limits);
        // Every epoch tick a running request yields to the executor; past
        // its deadline it is interrupted (FN-05).
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|context| {
            Ok(if Instant::now() >= context.data().deadline {
                UpdateDeadline::Interrupt
            } else {
                UpdateDeadline::Yield(1)
            })
        });
        let instantiated = tokio::time::timeout_at(until.into(), self.pre.instantiate_async(&mut store))
            .await
            .map_err(|_elapsed| {
                tracing::warn!(environment = %self.environment, "a function instance was not made within functions.timeout");
                FunctionError::Timeout
            })?;
        let proxy = instantiated.map_err(|error| super::failure(self.environment, &error))?;
        Ok(Instance { store, proxy })
    }

    /// Serves `request` in `instance` by `until`, the request's deadline
    /// ([`Recipe::deadline`]). The instance may serve the next request
    /// only when this one answered: a trap or a timeout leaves a guest
    /// that must not run again.
    pub(super) async fn serve(
        &self,
        instance: &mut Instance<S>,
        request: http::Request<Bytes>,
        until: Instant,
    ) -> Result<http::Response<Bytes>, FunctionError> {
        instance.store.data_mut().deadline = until;
        tokio::time::timeout_at(
            until.into(),
            super::http::exchange(self.environment, instance, request),
        )
        .await
        .unwrap_or_else(|_elapsed| {
            tracing::warn!(environment = %self.environment, "a function request ran past functions.timeout");
            Err(FunctionError::Timeout)
        })
    }
}

/// One of the environment's `permits`, taken by `until` (the deadline of
/// the request or warm-up it is for): past it the wait is a
/// [`FunctionError::Timeout`].
pub(super) async fn acquire(
    permits: &Arc<Semaphore>,
    environment: EnvironmentId,
    until: Instant,
) -> Result<OwnedSemaphorePermit, FunctionError> {
    tokio::time::timeout_at(until.into(), Arc::clone(permits).acquire_owned())
        .await
        .map_err(|_elapsed| {
            tracing::warn!(%environment, "a function request waited for an instance past functions.timeout");
            FunctionError::Timeout
        })?
        .map_err(|_closed| FunctionError::NotActive)
}

/// The active function of one environment: its recipe and its idle
/// instances. At most `instances_per_env` of its instances run at once
/// (the environment's permits); one more request waits for a permit.
pub(super) struct Pool<S: 'static> {
    pub(super) recipe: Recipe<S>,
    pub(super) permits: Arc<Semaphore>,
    idle: Mutex<Vec<Instance<S>>>,
}

impl<S: Store + 'static> Pool<S> {
    /// The pool of `recipe` over the environment's `permits`, with the
    /// instance that answered the warm-up.
    pub(super) fn new(recipe: Recipe<S>, permits: Arc<Semaphore>, warm: Instance<S>) -> Self {
        Self {
            recipe,
            permits,
            idle: Mutex::new(vec![warm]),
        }
    }

    /// Hands `request` to an idle instance, or a fresh one, once a permit
    /// is free (FN-07); a request still waiting at its deadline answers
    /// [`FunctionError::Timeout`]. An instance that trapped or timed out is dropped
    /// (FN-05, FN-06); the next request gets a fresh one.
    pub(super) async fn invoke(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, FunctionError> {
        // The request's time starts when it arrives: waiting for an
        // instance counts toward it, so no request waits without end.
        let until = self.recipe.deadline();
        let _permit = acquire(&self.permits, self.recipe.environment, until).await?;
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let mut instance = match idle {
            Some(instance) => instance,
            None => self.recipe.instantiate(until).await?,
        };
        let answered = self.recipe.serve(&mut instance, request, until).await;
        if answered.is_ok() {
            self.idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(instance);
        }
        answered
    }
}
