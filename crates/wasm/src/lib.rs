//! The wasmtime host of liftoff modules.
//!
//! A module is a WebAssembly component of the WIT world
//! `liftoff:module/liftoff-module` (the repository's `wit/`). The host
//! loads it ([`ModuleHost::load`]), gives it the `host` imports and calls
//! its exports through a [`Module`]:
//!
//! - `store-get`, `store-set`, `store-query` go to the
//!   [`ModuleStore`](liftoff_core::ports::ModuleStore) port under the
//!   module's name: its own key space and its own tables only (MOD-01,
//!   MOD-02); a refused statement is an error to the module, never a trap
//!   or a panic of the daemon;
//! - `schedule(name, every-secs)` makes the module's scheduler, one tokio
//!   task per module, call `tick(name)` every period on a [`Timer`], one
//!   call of a name at a time (MOD-03); a host
//!   [`without_scheduler`](ModuleHost::without_scheduler) (the CLI's)
//!   keeps the request and never ticks;
//! - `log` goes to a [`ModuleLog`] tagged with the module, its control
//!   characters escaped (no forged lines), at most [`MAX_LOG_BYTES`] per
//!   line and [`MAX_LOG_LINES_PER_CALL`] lines per call;
//! - `config-get` reads the [`ModuleConfig`] the module was loaded with;
//! - `core-call` goes to the host's
//!   [`CoreCalls`](liftoff_core::module::CoreCalls)
//!   ([`ModuleHost::with_core_calls`]; the daemon's is the core's
//!   [`CoreBridge`](liftoff_core::module::CoreBridge)) as the actor of
//!   the call in progress ([`Module::handle_tool_as`]; none for ticks
//!   and migrations), and the core's refusal reaches the module as its
//!   error (MOD-11);
//! - `wasi:http/outgoing-handler` sends a request only when its host and
//!   port are in the manifest's `http_allow` (an entry without a port
//!   allows the scheme's default one); any other is refused
//!   (`HTTP-request-denied`) before a connection is made (MOD-07). Unless
//!   [`ModuleHost::allowing_private_networks`], a host that is or resolves
//!   to a loopback, private, link-local, shared or unspecified address is
//!   refused too (`destination-IP-prohibited`): the daemon's REST and the
//!   Caddy admin API listen on loopback. The name is resolved
//!   once and the request connects to the checked addresses (liftoff's own
//!   client, not `wasmtime-wasi-http`'s, which would resolve it again: a
//!   rebinding DNS server could then race the check). At most
//!   [`MAX_HTTP_REQUESTS_IN_FLIGHT`] requests of an instance are open at
//!   once;
//! - a module's `core-call` runs inside its call: a hook the core raises
//!   from there (`authorize`) skips that module ([`Module::on_hook`]
//!   answers `Continue` without entering it) instead of waiting on the
//!   lock its own call holds;
//! - the rest of WASI 0.2 (clocks, random, stdio, which `std` imports) is
//!   linked without preopened directories, sockets or name lookup;
//! - [`Limits::max_memory_bytes`] caps the module's memory (one linear
//!   memory, a few tables and core instances; MOD-10); keys, values,
//!   schedules and the handles the host keeps for the module have fixed
//!   caps ([`MAX_KEY_BYTES`], [`MAX_VALUE_BYTES`], [`MAX_SCHEDULES`],
//!   [`MAX_HOST_RESOURCES`], [`MAX_HTTP_FIELDS_BYTES`]); [`Limits::call_timeout`] bounds every call
//!   through epoch interruption: the engine's epoch advances every 10 ms,
//!   a running call yields to the executor at each tick and is
//!   interrupted past its deadline ([`ModuleError::Timeout`], MOD-08);
//!   a call waiting on the host is dropped a second later. A call whose
//!   future is dropped takes its instance with it: the next call runs in
//!   a fresh one and nothing counts;
//! - a trap (a panic of the module, a cap), a timeout or a failure of the
//!   module store under the call fails that call only: the next call runs
//!   in a fresh instance. The trap is logged in full; [`ModuleError::Trap`]
//!   carries its short reason only (the trap code or the host's own
//!   message), never wasmtime's chain. The host reports every outcome to
//!   its [`ModuleRegistry`](liftoff_core::module::ModuleRegistry)
//!   ([`ModuleHost::registry`]), the core's, which owns the rule: three
//!   module failures in a row (traps, timeouts, a fresh instance that does
//!   not instantiate; a store failure, [`ModuleError::HostFailed`], is the
//!   daemon's and does not count) disable the module ([`Module::status`],
//!   [`ModuleError::Disabled`], which the core sees as `ModuleRequired`,
//!   424; MOD-09). Every export goes through the same path: `tick`,
//!   `migrate`, `handle-tool`, `handle-route` ([`Module::handle_route`])
//!   and `on-hook` ([`Module::on_hook`]);
//! - [`ModuleHost::load`] compiles on a blocking thread, reads
//!   `manifest()` through the core's strict
//!   [`ModuleManifest::parse`](liftoff_core::module::ModuleManifest::parse)
//!   (MOD-06), registers the module and, with
//!   [`ModuleHost::with_compile_cache`], keeps the
//!   compiled component in wasmtime's compile cache, keyed by the
//!   component's hash and the wasmtime version (MOD-12;
//!   [`ModuleHost::compile_stats`]). wasmtime cannot load a precompiled
//!   component without `unsafe`, which the workspace forbids, so the
//!   cache is wasmtime's own (`Config::cache`), not a file of ours. The
//!   cache directory is made the daemon's own (`0700`, refused when it is
//!   a symlink or another user's): wasmtime runs the code it finds there.
//!
//! [`functions`] is the runtime of functions: components of `wasi:http/proxy@0.2` with `wasi:config` and
//! `wasi:keyvalue`, a pool of instances per environment.
//!
//! [`ModuleSet`] holds the loaded modules for the core's hook dispatcher
//! ([`ModuleHooks`](liftoff_core::module::ModuleHooks))
//! and translates its calls to the world: hooks to `on-hook` with a JSON
//! payload, `authenticate` and `render_page` to `handle-route` below
//! `/.hook/`, module routes to `handle-route` with the actor, tool calls
//! (the core's [`ModuleGit`](liftoff_core::module::ModuleGit) forge calls,) to `handle-tool`.
//!
//! [`HttpModuleSource`] is the `ModuleSource` port over the module
//! registry's HTTPS: where the components the host loads come
//! from. It lived in `liftoff-remote` until that crate went away and belongs here, with the loading of modules, so the CLI's
//! local mode can reuse it.
//!
//! [`HostRuntime`] is the core's `ModuleRuntime` over a host and a
//! directory of components: the daemon's and the CLI's local mode's.
//!
//! The contract every host built on [`ModuleHost`] passes lives in
//! `testing` (feature `test-support`), with the macro
//! `module_host_contract_tests!` that runs it.
//!
//! Adapters never depend on each other: this crate depends on
//! `liftoff-core` only, and wasmtime stays out of the core.

#[expect(
    clippy::missing_asserts_for_indexing,
    clippy::error_impl_error,
    reason = "code wasmtime's bindgen! generates"
)]
mod bindings;
mod engine;
mod error;
pub mod functions;
mod host;
mod http;
mod imports;
mod limits;
mod log;
mod module;
mod registry;
mod runtime;
mod scheduler;
mod send;
mod set;
#[cfg(feature = "test-support")]
pub mod testing;

pub use engine::private_dir;
pub use error::ModuleError;
pub use host::{CompileStats, ModuleConfig, ModuleHost};
pub use liftoff_core::module::ModuleStatus;
pub use limits::{
    Limits, MAX_HOST_RESOURCES, MAX_HTTP_FIELDS_BYTES, MAX_HTTP_REQUESTS_IN_FLIGHT, MAX_KEY_BYTES,
    MAX_LOG_BYTES, MAX_LOG_LINES_PER_CALL, MAX_SCHEDULE_NAME_BYTES, MAX_SCHEDULES, MAX_VALUE_BYTES,
    MIN_PERIOD_SECS,
};
pub use log::{LogLevel, ModuleLog, TracingLog};
pub use module::{Module, RouteRequest, RouteResponse};
pub use registry::HttpModuleSource;
pub use runtime::HostRuntime;
pub use scheduler::{Timer, TokioTimer};
pub use set::ModuleSet;
