//! [`Module`]: a loaded module, its current instance behind a lock and
//! the calls into its exports.

use core::time::Duration;
use std::sync::Arc;
use std::time::Instant;

use liftoff_core::domain::{ModuleName, User};
use liftoff_core::module::{
    CallOutcome, CoreCalls, HookName, HttpAllow, ModuleManifest, ModuleRegistry, ModuleStatus,
    NoCoreCalls,
};
use liftoff_core::ports::{HookResult, ModuleStore};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use wasmtime::component::{Component, ResourceTable};
use wasmtime::{Store, StoreLimitsBuilder, UpdateDeadline};
use wasmtime_wasi::{WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::bindings::LiftoffModule;
use crate::bindings::exports::liftoff::module::module as export;
use crate::engine::{HOST_WAIT_GRACE, deadline};
use crate::error::{ModuleError, failure, load_error};
use crate::host::{ModuleConfig, Shared};
use crate::http::{AllowedHttp, Resolver};
use crate::imports::HostState;
use crate::limits::{
    MAX_HOST_RESOURCES, MAX_HTTP_FIELDS_BYTES, MAX_INSTANCES, MAX_TABLE_ELEMENTS, MAX_TABLES,
};
use crate::log::ModuleLog;
use crate::scheduler::Schedules;

/// A request to a route of a module (`handle-route`), below
/// `/api/v1/m/<module>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRequest {
    /// The HTTP method.
    pub method: String,
    /// The path below the module's prefix.
    pub path: String,
    /// The headers, in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// What a module's route answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteResponse {
    /// The HTTP status.
    pub status: u16,
    /// The headers, in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// What it takes to make an instance of one module: the first at load,
/// a fresh one after a trap.
pub(crate) struct Recipe<S: 'static, L: 'static, C: 'static> {
    pub(crate) shared: Arc<Shared<S, L, C>>,
    pub(crate) component: Component,
    pub(crate) name: ModuleName,
    pub(crate) config: ModuleConfig,
    pub(crate) schedules: Arc<Schedules>,
    /// The manifest's `http_allow` (MOD-07).
    pub(crate) http_allow: Arc<[HttpAllow]>,
    /// The manifest's `replaces`, for `core-call`.
    pub(crate) replaces: Arc<[String]>,
    /// Whether `wasi:http` may reach private addresses.
    pub(crate) private_networks: bool,
    /// How `wasi:http` resolves a host.
    pub(crate) resolver: Resolver,
}

impl<S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls> Recipe<S, L, C> {
    /// The `wasi:http` hooks of an instance (MOD-07).
    pub(crate) fn http_hooks(&self) -> AllowedHttp {
        AllowedHttp::new(
            &self.name,
            Arc::clone(&self.http_allow),
            self.private_networks,
            self.resolver.clone(),
        )
    }

    /// A new instance in a store of its own.
    pub(crate) async fn instantiate(&self) -> Result<Instance<S, L, C>, ModuleError> {
        let shared = &self.shared;
        // No preopened directory, no sockets, no name lookup: a module
        // reaches the outside through `wasi:http` only.
        let wasi = WasiCtxBuilder::new()
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .build();
        // What the module makes the host hold for it lives outside its
        // linear memory: its handles (streams, pollables, `wasi:http`
        // fields and requests) and the bytes of each set of fields are
        // capped, so it cannot grow the daemon (MOD-10).
        let mut table = ResourceTable::new();
        table.set_max_capacity(MAX_HOST_RESOURCES);
        let mut http = WasiHttpCtx::new();
        http.set_field_size_limit(MAX_HTTP_FIELDS_BYTES);
        let state = HostState {
            module: self.name.clone(),
            store: Arc::clone(&shared.store),
            log: Arc::clone(&shared.log),
            core: Arc::clone(&shared.core),
            actor: None,
            replaces: Arc::clone(&self.replaces),
            config: self.config.clone(),
            schedules: Arc::clone(&self.schedules),
            wasi,
            http,
            http_hooks: self.http_hooks(),
            table,
            // One linear memory: the cap is the module's, not each
            // memory's.
            limits: StoreLimitsBuilder::new()
                .memory_size(shared.limits.max_memory_bytes)
                .memories(1)
                .instances(MAX_INSTANCES)
                .tables(MAX_TABLES)
                .table_elements(MAX_TABLE_ELEMENTS)
                .build(),
            logged: 0,
            deadline: deadline(shared.limits.call_timeout),
        };
        let mut store = Store::new(&shared.engine, state);
        store.limiter(|state| &mut state.limits);
        // Every epoch tick a running call yields to the executor; past
        // its deadline it is interrupted (MOD-08).
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|context| {
            Ok(if Instant::now() >= context.data().deadline {
                UpdateDeadline::Interrupt
            } else {
                UpdateDeadline::Yield(1)
            })
        });
        let instantiated =
            LiftoffModule::instantiate_async(&mut store, &self.component, &shared.linker).await;
        let bindings = instantiated.map_err(|error| match failure(&self.name, &error) {
            timeout @ ModuleError::Timeout { .. } => timeout,
            _ => load_error(&self.name, &error),
        })?;
        Ok(Instance { store, bindings })
    }
}

/// The instance of one module and the wasmtime store it lives in.
pub(crate) struct Instance<S: 'static, L: 'static, C: 'static> {
    pub(crate) store: Store<HostState<S, L, C>>,
    pub(crate) bindings: LiftoffModule,
}

impl<S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls> Instance<S, L, C> {
    /// Calls `manifest()` and parses it (MOD-06).
    pub(crate) async fn manifest(
        &mut self,
        name: &ModuleName,
        call_timeout: Duration,
    ) -> Result<ModuleManifest, ModuleError> {
        self.store.data_mut().deadline = deadline(call_timeout);
        let called = tokio::time::timeout(
            call_timeout.saturating_add(HOST_WAIT_GRACE),
            self.bindings
                .liftoff_module_module()
                .call_manifest(&mut self.store),
        )
        .await;
        let json = match called {
            Ok(Ok(json)) => json,
            Ok(Err(error)) => return Err(failure(name, &error)),
            Err(_elapsed) => {
                return Err(ModuleError::Timeout {
                    module: name.to_string(),
                });
            }
        };
        ModuleManifest::parse(&json).map_err(|refused| {
            tracing::warn!(module = %name, %refused, "a module's manifest is refused");
            ModuleError::Load {
                module: name.to_string(),
                reason: refused.to_string(),
            }
        })
    }
}

/// The current instance of a module, behind its lock. A trap leaves an
/// instance that cannot be entered again, so it is dropped and the next
/// call makes a fresh one (the module's keys, tables and schedules stay).
pub(crate) struct Slot<S: 'static, L: 'static, C: 'static> {
    pub(crate) recipe: Recipe<S, L, C>,
    pub(crate) instance: Option<Instance<S, L, C>>,
    /// Where the outcome of every call is counted (MOD-09).
    pub(crate) registry: Arc<ModuleRegistry>,
}

/// The outcome of one call: what wasmtime answered, or that the call
/// waited on the host past its deadline.
type Called<R> = Result<wasmtime::Result<R>, tokio::time::error::Elapsed>;

impl<S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls> Slot<S, L, C> {
    /// How long a call may wait in all before it is dropped.
    fn host_wait(&self) -> Duration {
        self.recipe
            .shared
            .limits
            .call_timeout
            .saturating_add(HOST_WAIT_GRACE)
    }

    /// Counts how a call ended in the registry (MOD-09): an answer, a
    /// failure of the module, or a failure of the daemon under it.
    fn record(&self, failed: Option<&ModuleError>) {
        let outcome = match failed {
            None => CallOutcome::Answered,
            Some(ModuleError::HostFailed { .. }) => CallOutcome::HostFailed,
            Some(error) => CallOutcome::Failed(error.to_string()),
        };
        let _status = self.registry.record(&self.recipe.name, outcome);
    }

    /// The instance for a new call as `actor`, made anew when the last
    /// one failed; refused while the module is disabled or not
    /// registered. A fresh instance that does not instantiate counts as a
    /// failed call.
    ///
    /// The instance leaves the slot for the duration of the call and
    /// comes back in [`Slot::after`]: a call whose future is dropped
    /// midway (its caller went away) drops its instance with it, so the
    /// next call runs in a fresh one instead of a guest stopped in the
    /// middle, and the dropped call counts for nothing (MOD-09).
    async fn enter(&mut self, actor: Option<User>) -> Result<Instance<S, L, C>, ModuleError> {
        let status = self.registry.status(&self.recipe.name);
        if !status.as_ref().is_some_and(|status| status.enabled) {
            return Err(ModuleError::Disabled {
                module: self.recipe.name.to_string(),
                error: status
                    .and_then(|status| status.error)
                    .unwrap_or_else(|| "the module is not registered".to_owned()),
            });
        }
        let mut instance = match self.instance.take() {
            Some(instance) => instance,
            None => match self.recipe.instantiate().await {
                Ok(instance) => instance,
                Err(error) => {
                    self.record(Some(&error));
                    return Err(error);
                }
            },
        };
        let state = instance.store.data_mut();
        state.logged = 0;
        state.actor = actor;
        state.deadline = deadline(self.recipe.shared.limits.call_timeout);
        Ok(instance)
    }

    /// Ends a call of `instance`: puts it back in the slot without the
    /// call's actor when the call answered, drops it after a trap or a
    /// timeout, and counts the outcome (MOD-09).
    fn after<R>(
        &mut self,
        mut instance: Instance<S, L, C>,
        called: Called<R>,
    ) -> Result<R, ModuleError> {
        let name = &self.recipe.name;
        let outcome = match called {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(error)) => Err(failure(name, &error)),
            Err(_elapsed) => {
                tracing::error!(module = %name, "a module call waited on the host past its call timeout");
                Err(ModuleError::Timeout {
                    module: name.to_string(),
                })
            }
        };
        if outcome.is_ok() {
            instance.store.data_mut().actor = None;
            self.instance = Some(instance);
        }
        self.record(outcome.as_ref().err());
        outcome
    }

    /// Calls `tick(name)`.
    pub(crate) async fn tick(&mut self, name: &str) -> Result<(), ModuleError> {
        let wait = self.host_wait();
        let mut instance = self.enter(None).await?;
        let Instance { store, bindings } = &mut instance;
        let ticked = tokio::time::timeout(
            wait,
            bindings.liftoff_module_module().call_tick(store, name),
        )
        .await;
        self.after(instance, ticked)
    }

    /// Calls `migrate(from)`.
    async fn migrate(&mut self, from: u32) -> Result<(), ModuleError> {
        let wait = self.host_wait();
        let mut instance = self.enter(None).await?;
        let Instance { store, bindings } = &mut instance;
        let migrated = tokio::time::timeout(
            wait,
            bindings.liftoff_module_module().call_migrate(store, from),
        )
        .await;
        self.after(instance, migrated)?.map_err(ModuleError::from)
    }

    /// Calls `handle-tool(tool, args)` as `actor`.
    async fn handle_tool(
        &mut self,
        actor: Option<User>,
        tool: &str,
        args: &str,
    ) -> Result<String, ModuleError> {
        let wait = self.host_wait();
        let mut instance = self.enter(actor).await?;
        let Instance { store, bindings } = &mut instance;
        let answered = tokio::time::timeout(
            wait,
            bindings
                .liftoff_module_module()
                .call_handle_tool(store, tool, args),
        )
        .await;
        self.after(instance, answered)?.map_err(ModuleError::from)
    }

    /// Calls `handle-route(request)` as `actor`.
    async fn handle_route(
        &mut self,
        actor: Option<User>,
        request: RouteRequest,
    ) -> Result<RouteResponse, ModuleError> {
        let wait = self.host_wait();
        let request = export::Request {
            method: request.method,
            path: request.path,
            headers: request.headers,
            body: request.body,
            actor: actor.as_ref().map(|actor| actor.id.to_string()),
        };
        let mut instance = self.enter(actor).await?;
        let Instance { store, bindings } = &mut instance;
        let answered = tokio::time::timeout(
            wait,
            bindings
                .liftoff_module_module()
                .call_handle_route(store, &request),
        )
        .await;
        let response = self.after(instance, answered)?;
        Ok(RouteResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }

    /// Calls `on-hook(hook, payload)` as `actor`.
    async fn on_hook(
        &mut self,
        actor: Option<User>,
        hook: HookName,
        payload: &str,
    ) -> Result<HookResult, ModuleError> {
        let wait = self.host_wait();
        let mut instance = self.enter(actor).await?;
        let Instance { store, bindings } = &mut instance;
        let answered = tokio::time::timeout(
            wait,
            bindings
                .liftoff_module_module()
                .call_on_hook(store, hook.as_str(), payload),
        )
        .await;
        Ok(match self.after(instance, answered)?? {
            export::HookResult::Allow => HookResult::Allow,
            export::HookResult::Deny(reason) => HookResult::Deny(reason),
            export::HookResult::Continue => HookResult::Continue,
        })
    }
}

tokio::task_local! {
    /// The modules whose calls are in progress in this task, outermost
    /// first: a module's `core-call` runs inside its call, so whatever the
    /// core raises from there (a hook the dispatcher hands to modules)
    /// runs with this module on the chain.
    static CALLING: Vec<ModuleName>;
}

/// Whether a call of `module` is in progress in this task: a call into it
/// now would wait on the lock its own call holds.
fn is_calling(module: &ModuleName) -> bool {
    CALLING
        .try_with(|chain| chain.contains(module))
        .unwrap_or(false)
}

/// Runs `call`, a call of `module`, with `module` on this task's chain of
/// calls in progress ([`is_calling`]).
pub(crate) async fn calling<F: Future>(module: &ModuleName, call: F) -> F::Output {
    let mut chain = CALLING.try_with(Clone::clone).unwrap_or_default();
    chain.push(module.clone());
    CALLING.scope(chain, call).await
}

/// What the clones of a [`Module`] share.
pub(crate) struct Inner<S: 'static, L: 'static, C: 'static> {
    pub(crate) name: ModuleName,
    pub(crate) manifest: ModuleManifest,
    pub(crate) registry: Arc<ModuleRegistry>,
    pub(crate) slot: Arc<Mutex<Slot<S, L, C>>>,
    /// Its scheduler task; none on a host without a scheduler
    /// ([`ModuleHost::without_scheduler`](crate::ModuleHost::without_scheduler)).
    pub(crate) scheduler: Option<JoinHandle<()>>,
}

impl<S: 'static, L: 'static, C: 'static> Drop for Inner<S, L, C> {
    fn drop(&mut self) {
        if let Some(scheduler) = &self.scheduler {
            scheduler.abort();
        }
    }
}

/// A loaded module whose imports reach storage `S`, log `L` and the core
/// services `C`.
///
/// Clones share the instance; calls into it, its ticks included, run one
/// at a time, each within [`Limits::call_timeout`](crate::Limits::call_timeout). After a trap or a
/// timeout the next call runs in a fresh instance; after three failures
/// in a row the host's registry disables the module and it refuses calls
/// (MOD-09).
pub struct Module<S: 'static, L: 'static, C: 'static = NoCoreCalls> {
    pub(crate) inner: Arc<Inner<S, L, C>>,
}

impl<S: 'static, L: 'static, C> Clone for Module<S, L, C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S: 'static, L: 'static, C> core::fmt::Debug for Module<S, L, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Module")
            .field("name", &self.inner.name)
            .finish_non_exhaustive()
    }
}

impl<S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls> Module<S, L, C> {
    /// The name the module was loaded as.
    #[must_use]
    pub fn name(&self) -> &ModuleName {
        &self.inner.name
    }

    /// The manifest the module declared, read and checked at load
    /// (MOD-06).
    #[must_use]
    pub fn manifest(&self) -> &ModuleManifest {
        &self.inner.manifest
    }

    /// Whether the module takes calls, as the host's registry has it
    /// (MOD-09); disabled once it is no longer registered.
    #[must_use]
    pub fn status(&self) -> ModuleStatus {
        self.inner
            .registry
            .status(&self.inner.name)
            .unwrap_or_else(|| ModuleStatus {
                enabled: false,
                error: Some("the module is not registered".to_owned()),
            })
    }

    /// Calls the export `migrate(from)`, without an actor.
    ///
    /// # Errors
    ///
    /// [`ModuleError::Module`] with the module's error;
    /// [`ModuleError::Trap`] when it traps; [`ModuleError::Timeout`] past
    /// the call timeout; [`ModuleError::HostFailed`] when the module store
    /// fails under it; [`ModuleError::Disabled`] once it is disabled;
    /// [`ModuleError::Load`] when a fresh instance after a trap does not
    /// instantiate.
    pub async fn migrate(&self, from: u32) -> Result<(), ModuleError> {
        self.reentry()?;
        let slot = &self.inner.slot;
        calling(&self.inner.name, async {
            slot.lock().await.migrate(from).await
        })
        .await
    }

    /// Refuses a call of this module from inside its own call in
    /// progress (a `core-call` of it that reached it again), which would
    /// wait on the lock that call holds until it timed out.
    fn reentry(&self) -> Result<(), ModuleError> {
        if is_calling(&self.inner.name) {
            tracing::warn!(module = %self.inner.name, "a module is called from inside its own call; refused");
            return Err(ModuleError::Reentered {
                module: self.inner.name.to_string(),
            });
        }
        Ok(())
    }

    /// Calls the export `handle-tool(name, args)` without an actor (its
    /// `core-call`s are refused by the core); its JSON answer.
    ///
    /// # Errors
    ///
    /// As [`Module::migrate`].
    pub async fn handle_tool(&self, tool: &str, args: &str) -> Result<String, ModuleError> {
        self.reentry()?;
        let slot = &self.inner.slot;
        calling(&self.inner.name, async {
            slot.lock().await.handle_tool(None, tool, args).await
        })
        .await
    }

    /// Calls the export `handle-tool(name, args)` as `actor`: its
    /// `core-call`s act as `actor` (MOD-11).
    ///
    /// # Errors
    ///
    /// As [`Module::migrate`].
    pub async fn handle_tool_as(
        &self,
        actor: &User,
        tool: &str,
        args: &str,
    ) -> Result<String, ModuleError> {
        self.reentry()?;
        let slot = &self.inner.slot;
        calling(&self.inner.name, async {
            slot.lock()
                .await
                .handle_tool(Some(actor.clone()), tool, args)
                .await
        })
        .await
    }

    /// Calls the export `handle-route(request)` as `actor` (none for a
    /// public route): the request's `actor` is the actor's ID and its
    /// `core-call`s act as the actor.
    ///
    /// # Errors
    ///
    /// As [`Module::migrate`], but for [`ModuleError::Module`]: the route
    /// answers its errors as responses.
    pub async fn handle_route(
        &self,
        actor: Option<&User>,
        request: RouteRequest,
    ) -> Result<RouteResponse, ModuleError> {
        self.reentry()?;
        let slot = &self.inner.slot;
        calling(&self.inner.name, async {
            slot.lock()
                .await
                .handle_route(actor.cloned(), request)
                .await
        })
        .await
    }

    /// Calls the export `on-hook(hook, payload)` as `actor`; the
    /// module's decision. A hook that traps counts toward disabling the
    /// module like any other call.
    ///
    /// A hook raised from inside a call of this module in progress (its
    /// own `core-call`, directly or through another module's hook) does
    /// not enter it: the module has no opinion on what it does itself
    /// ([`HookResult::Continue`]), and the call it is in does not wait on
    /// itself.
    ///
    /// # Errors
    ///
    /// As [`Module::migrate`].
    pub async fn on_hook(
        &self,
        actor: Option<&User>,
        hook: HookName,
        payload: &str,
    ) -> Result<HookResult, ModuleError> {
        if is_calling(&self.inner.name) {
            tracing::debug!(module = %self.inner.name, hook = hook.as_str(), "a hook raised by the module's own call skips it");
            return Ok(HookResult::Continue);
        }
        let slot = &self.inner.slot;
        calling(&self.inner.name, async {
            slot.lock()
                .await
                .on_hook(actor.cloned(), hook, payload)
                .await
        })
        .await
    }
}

impl<S: Send + Sync + 'static, L: Send + Sync + 'static, C: Send + Sync + 'static> WasiView
    for HostState<S, L, C>
{
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl<S: Send + Sync + 'static, L: Send + Sync + 'static, C: Send + Sync + 'static> WasiHttpView
    for HostState<S, L, C>
{
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

#[cfg(test)]
mod tests {
    use liftoff_core::fakes::InMemoryStore;

    use super::*;
    use crate::testing::{component, limits};
    use crate::{ModuleHost, TokioTimer, TracingLog};

    /// The smallest component: it compiles, but has none of the world's
    /// exports, so it never instantiates as a module.
    const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\x00\x01\x00";

    mod enter {
        use super::*;

        /// `trap`, loaded, whose fresh instances from now on fail to
        /// instantiate.
        async fn unrestartable() -> Module<InMemoryStore, TracingLog> {
            let host =
                ModuleHost::new(InMemoryStore::new(), TokioTimer, TracingLog, limits()).unwrap();
            let module = host
                .load(
                    ModuleName::new("trap").unwrap(),
                    &component("trap").unwrap(),
                    ModuleConfig::new(),
                )
                .await
                .unwrap();
            {
                let mut slot = module.inner.slot.lock().await;
                slot.recipe.component =
                    Component::new(&host.shared.engine, EMPTY_COMPONENT).unwrap();
                slot.instance = None;
            }
            module
        }

        // TC: MOD-09 (a module that does not load is disabled)
        #[tokio::test(flavor = "multi_thread")]
        async fn disables_module_when_fresh_instance_fails_three_times() {
            let module = unrestartable().await;
            let mut failed = Vec::new();
            for _ in 0..3 {
                failed.push(matches!(
                    module.handle_tool("ok", "").await,
                    Err(ModuleError::Load { .. })
                ));
            }
            let refused = module.handle_tool("ok", "").await;
            assert_eq!(
                (
                    failed,
                    module.status().enabled,
                    matches!(refused, Err(ModuleError::Disabled { .. }))
                ),
                (vec![true, true, true], false, true)
            );
        }

        // TC: MOD-09
        #[tokio::test(flavor = "multi_thread")]
        async fn keeps_module_enabled_when_fresh_instance_fails_twice() {
            let module = unrestartable().await;
            for _ in 0..2 {
                let _failed = module.handle_tool("ok", "").await;
            }
            assert!(module.status().enabled);
        }
    }
}
