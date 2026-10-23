//! The CLI's module host: the
//! `liftoff-wasm` host over [`LocalModuleStore`], the same as the daemon's
//! but without a scheduler (a module's `schedule` is kept and never
//! ticks: a module of local mode lives for one command) and with
//! [`LocalCoreCalls`], which serves no `core-call`: there is no
//! installation behind the CLI (LCL-07).

use core::future::ready;
use core::time::Duration;
use std::path::Path;

use liftoff_core::Error;
use liftoff_core::module::{Caller, CoreBridge, CoreCalls};
use liftoff_wasm::{Limits, ModuleError, ModuleHost, Timer, TokioTimer, TracingLog};

use super::ports::LocalPorts;
use super::store::LocalModuleStore;

/// The module host of local mode. `T` is the timer a scheduler would sleep
/// on; the CLI host has none, so nothing ever sleeps on it.
pub type CliHost<T = TokioTimer> = ModuleHost<LocalModuleStore, T, TracingLog, LocalCoreCalls>;

/// `core-call` in local mode: there is no installation behind the CLI.
///
/// A method of the core's table ([`CoreBridge::METHODS`]) is refused with
/// `unavailable` and a hint to `lof server connect`: the ones that change
/// an installation (`auth.create_session`,
/// `environments.set_protected`, `tenancy.assign`, `tenancy.adopt`) have
/// none to change, and the ones that read one (`projects.*`,
/// `secrets.resolve`, `tenancy.*`, `system.api_url`) none to read. A
/// method outside the table is `forbidden`, as the daemon's bridge
/// refuses it (MOD-11).
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalCoreCalls;

impl CoreCalls for LocalCoreCalls {
    fn call(
        &self,
        caller: Caller<'_>,
        service: &str,
        method: &str,
        _args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        let name = format!("{service}.{method}");
        let known = CoreBridge::<LocalPorts>::METHODS.contains(&name.as_str());
        ready(Err(if known {
            Error::unavailable(format!(
                "core-call {name} of {} needs a liftoff server, and the CLI runs without one: \
                 connect it with `lof server connect <url>`",
                caller.module
            ))
        } else {
            Error::forbidden(format!("core-call {name} is not a method of the core"))
        }))
    }
}

/// The CLI's module host over `store`, each call within `limits`: no
/// scheduler (`timer` is never slept on), and [`LocalCoreCalls`].
///
/// # Errors
///
/// [`ModuleError::Load`] when the wasmtime engine cannot be built.
pub fn cli_host<T: Timer>(
    store: LocalModuleStore,
    timer: T,
    limits: Limits,
) -> Result<CliHost<T>, ModuleError> {
    ModuleHost::new(store, timer, TracingLog, limits)?
        .without_scheduler()
        .with_core_calls(LocalCoreCalls)
}

/// The limits of a call into a module of local mode, as the daemon's
/// defaults.
pub const MODULE_LIMITS: Limits = Limits {
    max_memory_bytes: 64 * 1024 * 1024,
    call_timeout: Duration::from_secs(10),
};

/// The host a run of local mode uses: [`cli_host`] over `store` within
/// [`MODULE_LIMITS`], keeping compiled components in `cache` (MOD-12).
///
/// # Errors
///
/// [`ModuleError::Load`] when the engine or the cache cannot be set up.
pub fn local_host(store: LocalModuleStore, cache: &Path) -> Result<CliHost, ModuleError> {
    cli_host(store, TokioTimer, MODULE_LIMITS)?.with_compile_cache(cache)
}
