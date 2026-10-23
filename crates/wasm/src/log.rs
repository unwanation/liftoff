//! [`ModuleLog`]: where the `log` import of a module goes.

use liftoff_core::domain::ModuleName;

/// The `level` of the WIT `log` import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LogLevel {
    /// `debug`.
    Debug,
    /// `info`.
    Info,
    /// `warn`.
    Warn,
    /// `error`.
    Error,
}

/// Receives every `log` line of every module, tagged with the module.
pub trait ModuleLog: Send + Sync + 'static {
    /// `module` logged `message` at `level`.
    fn log(&self, module: &ModuleName, level: LogLevel, message: &str);
}

/// The daemon's [`ModuleLog`]: a `tracing` event with the module's name.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingLog;

impl ModuleLog for TracingLog {
    fn log(&self, module: &ModuleName, level: LogLevel, message: &str) {
        match level {
            LogLevel::Debug => tracing::debug!(%module, "{message}"),
            LogLevel::Info => tracing::info!(%module, "{message}"),
            LogLevel::Warn => tracing::warn!(%module, "{message}"),
            LogLevel::Error => tracing::error!(%module, "{message}"),
        }
    }
}
