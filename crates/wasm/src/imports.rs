//! The `host` imports of a module and the state of its
//! wasmtime store.

use core::future::ready;
use std::sync::Arc;

use std::time::Instant;

use liftoff_core::Error;
use liftoff_core::domain::{ModuleName, User};
use liftoff_core::module::{Caller, CoreCalls};
use liftoff_core::ports::ModuleStore;
use wasmtime::StoreLimits;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::WasiCtx;
use wasmtime_wasi_http::WasiHttpCtx;

use crate::bindings::liftoff::module::host;
use crate::host::ModuleConfig;
use crate::http::AllowedHttp;
use crate::limits::{MAX_KEY_BYTES, MAX_LOG_BYTES, MAX_LOG_LINES_PER_CALL, MAX_VALUE_BYTES};
use crate::log::{LogLevel, ModuleLog};
use crate::scheduler::Schedules;

/// What the store of one module instance holds: who it is, the ports its
/// imports reach and the WASI contexts.
pub(crate) struct HostState<S, L, C> {
    pub(crate) module: ModuleName,
    pub(crate) store: Arc<S>,
    pub(crate) log: Arc<L>,
    /// Where `core-call` goes.
    pub(crate) core: Arc<C>,
    /// The actor of the call in progress (MOD-11); `None` between calls
    /// and for ticks and migrations.
    pub(crate) actor: Option<User>,
    /// The manifest's `replaces`, which `core-call` hands to the core
    /// ([`Caller::replaces`]); empty until the manifest is read.
    pub(crate) replaces: Arc<[String]>,
    pub(crate) config: ModuleConfig,
    pub(crate) schedules: Arc<Schedules>,
    pub(crate) wasi: WasiCtx,
    pub(crate) http: WasiHttpCtx,
    pub(crate) http_hooks: AllowedHttp,
    pub(crate) table: ResourceTable,
    pub(crate) limits: StoreLimits,
    /// The `log` lines of the current call so far
    /// ([`MAX_LOG_LINES_PER_CALL`]); reset when a call begins.
    pub(crate) logged: usize,
    /// When the call in progress is interrupted (MOD-08).
    pub(crate) deadline: Instant,
}

/// A module's `log` line as the daemon writes it: control characters
/// (line breaks among them) escaped, so a module cannot forge lines of
/// the daemon's log, and cut at [`MAX_LOG_BYTES`].
fn log_line(message: &str) -> String {
    let mut line = String::new();
    for char in message.chars() {
        let start = line.len();
        if char.is_control() {
            line.extend(char.escape_default());
        } else {
            line.push(char);
        }
        if line.len() > MAX_LOG_BYTES {
            line.truncate(start);
            line.push('…');
            break;
        }
    }
    line
}

/// The error a module gets for `error` of a port: the code names the
/// variant, the message is the one safe to show; an internal failure is
/// logged and reaches the module as `internal` without its source.
fn module_error(module: &ModuleName, error: Error) -> host::Error {
    let (code, message) = match error {
        Error::Validation(invalid) => ("validation", invalid.to_string()),
        Error::Unauthenticated(why) => ("unauthenticated", why),
        Error::Forbidden(why) => ("forbidden", why),
        Error::NotFound(what) => ("not_found", format!("{what} not found")),
        Error::Conflict(why) => ("conflict", why),
        Error::Gone(why) => ("gone", why),
        Error::Unavailable(why) => ("unavailable", why),
        Error::ModuleRequired(name) => ("module_required", name),
        // The external system's text is safe to show: `lof
        // protect` answers with Caddy's (v1 SSO-02).
        upstream @ Error::Upstream { .. } => ("upstream", upstream.to_string()),
        other => {
            tracing::error!(%module, error = %other, source = ?core::error::Error::source(&other), "a host import of a module failed");
            ("internal", "internal error".to_owned())
        }
    };
    host::Error {
        code: code.to_owned(),
        message,
    }
}

/// A failure of the daemon under a module call: not the module's fault,
/// so it does not count toward disabling the module (MOD-09).
#[derive(Debug, thiserror::Error)]
#[error("the module store failed")]
pub(crate) struct HostFault;

/// A failure of the store behind `store-get`/`store-set`, which have no
/// error in the WIT: the call traps with a [`HostFault`], the daemon goes
/// on and the next call gets a fresh instance.
fn store_trap(module: &ModuleName, error: &Error) -> wasmtime::Error {
    tracing::error!(%module, %error, "the module store failed");
    wasmtime::Error::new(HostFault)
}

/// Traps a `store-get`/`store-set` whose key or value is over its limit
/// ([`MAX_KEY_BYTES`], [`MAX_VALUE_BYTES`]).
fn check_sizes(module: &ModuleName, key: &str, value: &[u8]) -> wasmtime::Result<()> {
    if key.len() > MAX_KEY_BYTES {
        tracing::warn!(%module, bytes = key.len(), max = MAX_KEY_BYTES, "a store key is too long");
        return Err(wasmtime::Error::msg(format!(
            "store key longer than {MAX_KEY_BYTES} bytes"
        )));
    }
    if value.len() > MAX_VALUE_BYTES {
        tracing::warn!(%module, bytes = value.len(), max = MAX_VALUE_BYTES, "a store value is too large");
        return Err(wasmtime::Error::msg(format!(
            "store value larger than {MAX_VALUE_BYTES} bytes"
        )));
    }
    Ok(())
}

impl<S: ModuleStore + 'static, L: ModuleLog, C: CoreCalls> host::Host for HostState<S, L, C> {
    async fn store_get(&mut self, key: String) -> wasmtime::Result<Option<Vec<u8>>> {
        check_sizes(&self.module, &key, &[])?;
        self.store
            .module_get(&self.module, &key)
            .await
            .map_err(|error| store_trap(&self.module, &error))
    }

    async fn store_set(&mut self, key: String, value: Vec<u8>) -> wasmtime::Result<()> {
        check_sizes(&self.module, &key, &value)?;
        self.store
            .module_set(&self.module, &key, &value)
            .await
            .map_err(|error| store_trap(&self.module, &error))
    }

    async fn store_query(
        &mut self,
        sql: String,
        params: Vec<String>,
    ) -> wasmtime::Result<Result<String, host::Error>> {
        Ok(self
            .store
            .module_query(&self.module, &sql, &params)
            .await
            .map_err(|error| module_error(&self.module, error)))
    }

    fn config_get(
        &mut self,
        key: String,
    ) -> impl Future<Output = wasmtime::Result<Option<String>>> + Send {
        ready(Ok(self.config.get(&key).cloned()))
    }

    async fn core_call(
        &mut self,
        service: String,
        method: String,
        args: String,
    ) -> wasmtime::Result<Result<String, host::Error>> {
        Ok(self
            .core
            .call(
                Caller {
                    module: &self.module,
                    replaces: &self.replaces,
                    actor: self.actor.as_ref(),
                },
                &service,
                &method,
                &args,
            )
            .await
            .map_err(|error| module_error(&self.module, error)))
    }

    fn schedule(
        &mut self,
        name: String,
        every_secs: u32,
    ) -> impl Future<Output = wasmtime::Result<()>> + Send {
        self.schedules.request(&self.module, name, every_secs);
        ready(Ok(()))
    }

    fn log(
        &mut self,
        level: host::Level,
        msg: String,
    ) -> impl Future<Output = wasmtime::Result<()>> + Send {
        let level = match level {
            host::Level::Debug => LogLevel::Debug,
            host::Level::Info => LogLevel::Info,
            host::Level::Warn => LogLevel::Warn,
            host::Level::Error => LogLevel::Error,
        };
        self.logged = self.logged.saturating_add(1);
        if self.logged <= MAX_LOG_LINES_PER_CALL {
            self.log.log(&self.module, level, &log_line(&msg));
        } else if self.logged == MAX_LOG_LINES_PER_CALL.saturating_add(1) {
            tracing::warn!(module = %self.module, max = MAX_LOG_LINES_PER_CALL, "a module call logged too many lines; the rest are dropped");
        }
        ready(Ok(()))
    }
}
