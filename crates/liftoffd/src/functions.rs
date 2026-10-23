//! The function runtime and the kept components of the daemon.
//!
//! The [`FunctionHost`] of the core is
//! `liftoff-wasm`'s [`FunctionRuntime`] (as a [`FunctionDeployer`]) over
//! the `SQLite` store (its
//! `wasi:keyvalue` writes the `kv` table), the [`ComponentFiles`] are
//! `liftoff-host`'s files under `paths.data/functions/`.
//!
//! [`FunctionHost`]: liftoff_core::ports::FunctionHost
//! [`ComponentFiles`]: liftoff_core::ports::ComponentFiles

use std::sync::Arc;

use liftoff_core::Error;
use liftoff_host::FileComponentFiles;
use liftoff_store::SqliteStore;
use liftoff_wasm::functions::{FunctionDeployer, FunctionLimits, FunctionRuntime};

use crate::config::AppConfig;
use crate::error::AppError;

/// The function runtime of the daemon as deploys drive it; its
/// [`FunctionDeployer::runtime`] serves the requests.
pub type DaemonFunctions = FunctionDeployer<SqliteStore>;

/// What one MiB is in bytes.
const MIB: u64 = 1024 * 1024;

/// The limits of `[functions]`. An outgoing `wasi:http` request
/// may take what the whole request may.
///
/// # Errors
///
/// [`AppError::Settings`] when a limit does not fit this platform.
pub fn limits(config: &AppConfig) -> Result<FunctionLimits, AppError> {
    let functions = &config.functions;
    let too_large = |field: &'static str| {
        move |_: core::num::TryFromIntError| {
            AppError::Settings(Error::invalid(field, "is too large for this platform"))
        }
    };
    let max_memory_bytes = usize::try_from(u64::from(functions.max_memory_mb) * MIB)
        .map_err(too_large("functions.max_memory_mb"))?;
    let instances_per_env = usize::try_from(functions.instances_per_env)
        .map_err(too_large("functions.instances_per_env"))?;
    Ok(FunctionLimits {
        max_memory_bytes,
        timeout: functions.timeout,
        instances_per_env,
        kv_max_bytes: u64::from(functions.kv_max_mb) * MIB,
        outgoing_timeout: functions.timeout,
    })
}

/// The runtime with the limits of `[functions]` over `store`, compiling
/// into the compile cache of `paths.data/cache` (FN-09), and the kept
/// components under `paths.data/functions`.
///
/// # Errors
///
/// [`AppError::Settings`] for a limit that does not fit,
/// [`AppError::Setup`] when the engine or its cache cannot be made.
pub fn function_ports(
    config: &AppConfig,
    store: &SqliteStore,
) -> Result<(DaemonFunctions, FileComponentFiles), AppError> {
    let runtime = FunctionRuntime::new(store.clone(), limits(config)?)
        .and_then(|runtime| runtime.with_compile_cache(config.paths.data.join("cache")))
        .map_err(|error| AppError::Setup {
            what: "the function runtime".to_owned(),
            source: Error::internal(error),
        })?;
    let components = FileComponentFiles::new(config.paths.data.join("functions"));
    Ok((FunctionDeployer::new(Arc::new(runtime)), components))
}
