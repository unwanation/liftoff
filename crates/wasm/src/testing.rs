//! The contract of the module host (feature `test-support`).
//!
//! It holds for every host built on [`ModuleHost`]: the daemon's host over its store here, the CLI's host
//! over its own.
//!
//! [`module_host_contract_tests!`](crate::module_host_contract_tests)
//! turns every check of [`contract`] into its own test over a fresh
//! [`ModuleStore`] per host:
//!
//! ```ignore
//! // in a crate's tests:
//! liftoff_wasm::module_host_contract_tests!(InMemoryStore::new);
//! ```
//!
//! The tests are grouped by unit of work, one module each: `manifest`
//! (MOD-06), `http` (MOD-07), `timeout` (MOD-08), `traps` (MOD-09,
//! MOD-10), `exports` (routes and hooks, MOD-09), `core_call` and
//! `core_call_reentry` (MOD-11), `compile_cache` and `compile_cache_dir`
//! (MOD-12).
//!
//! The checks load the contract components of `modules/testing/`
//! (`trap`, `loop`, `badhost`, `badmanifest`, `allimports` and `probe`),
//! which `just modules` builds (part of `just test` and `just ci`):
//! [`component`] reads them from the directory in `WASM_MODULES_DIR`,
//! which the just recipes set, else from the repository's
//! `modules/target/wasm32-wasip2/release/`.
//!
//! The helpers are public so a host test can add checks of its own:
//! [`Listener`] (a loopback HTTP server counting its connections),
//! [`within`] (a call on a thread of its own, bounded in time),
//! [`RecordingCore`] (a hand-written [`CoreCalls`] fake) and
//! [`FailingStore`] (a store whose key space fails).

pub mod contract;

use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use liftoff_core::Error;
use liftoff_core::domain::{ModuleName, UserId};
use liftoff_core::module::{Caller, CoreCalls};
use liftoff_core::ports::ModuleStore;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::{Limits, Module, ModuleConfig, ModuleHost, TokioTimer, TracingLog};

/// What a check returns: `Err` when the host or a fixture failed to set
/// up; a broken rule panics with the rule.
pub type Checked = Result<(), Failure>;

/// A failure to set a check up.
pub type Failure = Box<dyn core::error::Error + Send + Sync>;

/// A module host of the contract over the store `S` and core `C`.
pub type Host<S, C = liftoff_core::module::NoCoreCalls> = ModuleHost<S, TokioTimer, TracingLog, C>;

/// A module of a [`Host`].
pub type Loaded<S, C = liftoff_core::module::NoCoreCalls> = Module<S, TracingLog, C>;

/// The memory cap of the contract's hosts.
pub const MEMORY_CAP: usize = 64 * 1024 * 1024;

/// The limits of the contract's hosts: [`MEMORY_CAP`] and a call timeout
/// of 10 s.
#[must_use]
pub const fn limits() -> Limits {
    Limits {
        max_memory_bytes: MEMORY_CAP,
        call_timeout: Duration::from_secs(10),
    }
}

/// Where the built components are: `WASM_MODULES_DIR`, else the
/// repository's `modules/target/wasm32-wasip2/release/`.
#[must_use]
pub fn components_dir() -> PathBuf {
    std::env::var_os("WASM_MODULES_DIR").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../modules/target/wasm32-wasip2/release")
        },
        PathBuf::from,
    )
}

/// The bytes of the built component `name`.
///
/// # Errors
///
/// When it is not built (run `just modules`).
pub fn component(name: &str) -> Result<Vec<u8>, Failure> {
    let path = components_dir().join(format!("{name}.wasm"));
    std::fs::read(&path).map_err(|error| {
        format!(
            "{}: {error}; build the modules first with `just modules`",
            path.display()
        )
        .into()
    })
}

/// The contract component `name` loaded under its own name.
///
/// # Errors
///
/// When it is not built or does not load.
pub async fn load<S: ModuleStore + 'static, C: CoreCalls>(
    host: &Host<S, C>,
    name: &str,
) -> Result<Loaded<S, C>, Failure> {
    Ok(host
        .load(
            ModuleName::new(name)?,
            &component(name)?,
            ModuleConfig::new(),
        )
        .await?)
}

/// Runs `work` on a thread of its own (with its own runtime) and waits
/// at most `limit` for its result: a call that never returns fails the
/// check instead of hanging it. The thread is left behind then.
///
/// # Errors
///
/// When the runtime does not start or `work` does not finish in time.
pub fn within<R: Send + 'static>(
    limit: Duration,
    work: impl AsyncFnOnce() -> R + Send + 'static,
) -> Result<R, Failure> {
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ran = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map(|runtime| runtime.block_on(work()));
        let _gone = sent.send(ran);
    });
    match received.recv_timeout(limit) {
        Ok(ran) => Ok(ran?),
        Err(_late) => Err(format!("the call did not return within {}ms", limit.as_millis()).into()),
    }
}

/// A loopback HTTP server answering `200 ok` to anything, counting the
/// connections it accepted.
#[derive(Debug)]
pub struct Listener {
    /// Its port on `127.0.0.1`.
    pub port: u16,
    accepted: Arc<AtomicUsize>,
}

impl Listener {
    /// Starts it on a free port of `127.0.0.1`.
    ///
    /// # Errors
    ///
    /// When it cannot bind.
    pub async fn start() -> Result<Self, Failure> {
        Self::serving(true).await
    }

    /// Starts one that accepts connections and never answers: it holds
    /// them open and reads nothing.
    ///
    /// # Errors
    ///
    /// When it cannot bind.
    pub async fn silent() -> Result<Self, Failure> {
        Self::serving(false).await
    }

    /// Starts it, answering requests or not.
    async fn serving(answers: bool) -> Result<Self, Failure> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&accepted);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                if answers {
                    tokio::spawn(answer(stream));
                } else {
                    tokio::spawn(async move {
                        let _held = stream;
                        core::future::pending::<()>().await;
                    });
                }
            }
        });
        Ok(Self { port, accepted })
    }

    /// How many connections it accepted so far.
    #[must_use]
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// Reads a request head from `stream` and answers `200 ok`.
async fn answer(mut stream: tokio::net::TcpStream) {
    let mut seen: Vec<u8> = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !seen.windows(4).any(|end: &[u8]| end == b"\r\n\r\n") {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => seen.extend(buffer.iter().take(read)),
        }
    }
    let _sent = stream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
        .await;
}

/// A `core-call` seen by [`RecordingCore`]: actor, service, method, args.
pub type CoreCallSeen = (Option<UserId>, String, String, String);

/// A [`CoreCalls`] that records every call and answers what the check
/// scripted (`Ok("[]")` by default).
#[derive(Debug, Clone, Default)]
pub struct RecordingCore {
    calls: Arc<Mutex<Vec<CoreCallSeen>>>,
    answer: Arc<Mutex<Option<Result<String, Error>>>>,
}

/// Locks `mutex`, whatever a panic left in it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl RecordingCore {
    /// Every later call answers `answer`.
    pub fn answer(&self, answer: Result<String, Error>) {
        *lock(&self.answer) = Some(answer);
    }

    /// The calls so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<CoreCallSeen> {
        lock(&self.calls).clone()
    }
}

impl CoreCalls for RecordingCore {
    fn call(
        &self,
        caller: Caller<'_>,
        service: &str,
        method: &str,
        args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        lock(&self.calls).push((
            caller.actor.map(|actor| actor.id),
            service.to_owned(),
            method.to_owned(),
            args.to_owned(),
        ));
        let answer = lock(&self.answer)
            .clone()
            .unwrap_or_else(|| Ok("[]".to_owned()));
        core::future::ready(answer)
    }
}

/// A [`ModuleStore`] whose key space (`store-get`, `store-set`) and purge
/// always fail with [`Error::Internal`], as a broken database would;
/// `store-query` goes to the store it wraps.
#[derive(Debug, Clone, Default)]
pub struct FailingStore<S>(S);

impl<S> FailingStore<S> {
    /// Wraps `store`.
    pub const fn new(store: S) -> Self {
        Self(store)
    }
}

impl<S: ModuleStore> ModuleStore for FailingStore<S> {
    fn module_get(
        &self,
        _module: &ModuleName,
        _key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        core::future::ready(Err(Error::internal_msg("the store is broken")))
    }

    fn module_set(
        &self,
        _module: &ModuleName,
        _key: &str,
        _value: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        core::future::ready(Err(Error::internal_msg("the store is broken")))
    }

    fn module_query(
        &self,
        module: &ModuleName,
        sql: &str,
        params: &[String],
    ) -> impl Future<Output = Result<String, Error>> + Send {
        self.0.module_query(module, sql, params)
    }

    fn module_purge(&self, _module: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        core::future::ready(Err(Error::internal_msg("the store is broken")))
    }
}

/// Generates one test per check of the module host [`contract`].
///
/// One module per unit of work: `manifest`, `http`, `timeout`, `traps`,
/// `exports`, `core_call`, `core_call_reentry`, `compile_cache`,
/// `compile_cache_dir`.
///
/// Each runs over hosts whose stores come from `$new_store` (a closure or
/// fn path returning a [`ModuleStore`]). Invoke it inside a test module; the
/// calling crate needs `tokio` with the `macros` and `rt-multi-thread`
/// features as a dev-dependency.
#[macro_export]
macro_rules! module_host_contract_tests {
    ($new_store:expr) => {
        /// A fresh store for one check.
        fn contract_store() -> impl $crate::testing::ContractStore {
            ($new_store)()
        }

        $crate::module_host_contract_tests!(
            @async manifest;
            reads_declared_fields_when_allimports_loaded,
            reads_minimal_manifest_when_probe_loaded,
            refuses_load_when_manifest_has_unknown_field,
            registers_nothing_when_manifest_is_refused,
            registers_module_enabled_when_loaded,
        );
        $crate::module_host_contract_tests!(
            @async http;
            reaches_host_when_it_is_in_http_allow,
            refuses_request_when_host_is_not_in_http_allow,
            names_denial_when_host_is_not_in_http_allow,
            refuses_request_when_port_is_not_in_http_allow,
            refuses_request_when_entry_has_no_port_and_port_is_not_default,
            refuses_request_when_allowed_host_is_private,
            refuses_request_when_allowed_name_resolves_to_private_address,
        );
        $crate::module_host_contract_tests!(
            @sync timeout;
            interrupts_call_when_it_runs_past_call_timeout,
            interrupts_call_when_host_request_hangs,
            reports_internal_when_call_timed_out,
            answers_next_call_when_previous_call_timed_out,
            disables_module_when_three_calls_in_a_row_time_out,
            answers_next_call_when_previous_call_was_dropped,
        );
        $crate::module_host_contract_tests!(
            @async traps;
            disables_module_when_three_calls_in_a_row_trap,
            refuses_call_when_module_is_disabled,
            keeps_module_enabled_when_answered_call_breaks_trap_run,
            keeps_module_enabled_when_two_calls_trap,
            reports_module_required_when_module_is_disabled,
            reports_disabled_status_in_registry_when_three_calls_trap,
            keeps_module_enabled_when_store_fails_three_times,
            reports_internal_trap_when_module_grows_past_memory_cap,
            traps_call_when_module_holds_too_many_host_resources,
        );
        $crate::module_host_contract_tests!(
            @async exports;
            answers_route_with_call_actor_when_module_handles_it,
            disables_module_when_three_routes_in_a_row_trap,
            answers_hook_result_when_module_hooks,
            disables_module_when_three_hooks_in_a_row_trap,
        );
        $crate::module_host_contract_tests!(
            @async core_call;
            passes_call_actor_when_module_calls_core,
            answers_core_result_when_core_answers,
            hands_refusal_to_module_when_core_refuses,
            passes_no_actor_when_call_has_none,
            forgets_actor_when_next_call_has_none,
            asks_authorize_with_call_actor_when_bridge_lists_projects,
            refuses_method_when_bridge_has_no_such_method,
        );
        $crate::module_host_contract_tests!(
            @sync core_call_reentry;
            skips_module_when_its_own_core_call_raises_hook,
            refuses_tool_when_module_own_core_call_calls_it,
        );
        $crate::module_host_contract_tests!(
            @async compile_cache;
            compiles_component_when_cache_is_empty,
            skips_compiling_when_second_host_loads_same_component,
            compiles_again_when_component_differs,
            answers_calls_when_component_comes_from_cache,
            keeps_compiled_component_in_cache_dir_when_loaded,
        );
        $crate::module_host_contract_tests!(
            @sync compile_cache_dir;
            makes_cache_dir_private_when_it_is_missing,
            narrows_cache_dir_when_others_may_write_to_it,
            refuses_cache_when_dir_is_a_symlink,
        );
    };
    (@async $unit:ident; $($check:ident),+ $(,)?) => {
        mod $unit {
            $(
                #[::tokio::test(flavor = "multi_thread")]
                async fn $check() -> $crate::testing::Checked {
                    $crate::testing::contract::$check(&super::contract_store).await
                }
            )+
        }
    };
    (@sync $unit:ident; $($check:ident),+ $(,)?) => {
        mod $unit {
            $(
                #[test]
                fn $check() -> $crate::testing::Checked {
                    $crate::testing::contract::$check(&super::contract_store)
                }
            )+
        }
    };
}

/// What [`module_host_contract_tests!`](crate::module_host_contract_tests)
/// needs of a store: a [`ModuleStore`] that lives as long as the host.
pub trait ContractStore: ModuleStore + 'static {}

impl<S: ModuleStore + 'static> ContractStore for S {}
