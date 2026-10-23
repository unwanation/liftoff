//! The wasmtime engine of a host: epoch interruption, the epoch ticker,
//! the compile cache and the linker of the world's imports.

use core::time::Duration;
use std::path::Path;
use std::time::Instant;

use liftoff_core::module::CoreCalls;
use liftoff_core::ports::ModuleStore;
use wasmtime::component::{HasSelf, Linker};
use wasmtime::{Cache, CacheConfig, Config, Engine};

use crate::bindings::liftoff::module::host;
use crate::error::ModuleError;
use crate::imports::HostState;
use crate::log::ModuleLog;

/// How often the engine's epoch advances: the granularity of the call
/// timeout, and how often a running call yields to the executor.
pub(crate) const EPOCH_TICK: Duration = Duration::from_millis(10);

/// How much longer than [`Limits::call_timeout`] a call may wait on the
/// host (an import, an HTTP request) before it is dropped; the epoch
/// interrupts wasm code itself at the timeout.
pub(crate) const HOST_WAIT_GRACE: Duration = Duration::from_secs(1);

/// The deadline of a call starting now that may run `call_timeout`; a
/// timeout too long for the clock is a year.
pub(crate) fn deadline(call_timeout: Duration) -> Instant {
    const YEAR: Duration = Duration::from_hours(8760);
    let now = Instant::now();
    now.checked_add(call_timeout)
        .or_else(|| now.checked_add(YEAR))
        .unwrap_or(now)
}

/// The wasmtime engine of a host: epoch interruption on (one thread
/// advances the epoch every [`EPOCH_TICK`] while the engine lives) and,
/// with `cache_dir`, wasmtime's compile cache there.
pub(crate) fn engine(cache_dir: Option<&Path>) -> Result<(Engine, Option<Cache>), ModuleError> {
    let engine_error = |error: &dyn core::fmt::Display| ModuleError::Load {
        module: "*".to_owned(),
        reason: format!("{error:#}"),
    };
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.epoch_interruption(true);
    let cache = match cache_dir {
        None => None,
        Some(dir) => {
            let dir = std::path::absolute(dir).map_err(|error| engine_error(&error))?;
            private_dir(&dir).map_err(|error| engine_error(&error))?;
            let mut cache_config = CacheConfig::new();
            cache_config.with_directory(dir);
            Some(Cache::new(cache_config).map_err(|error| engine_error(&error))?)
        }
    };
    config.cache(cache.clone());
    let engine = Engine::new(&config).map_err(|error| engine_error(&error))?;
    let weak = engine.weak();
    std::thread::Builder::new()
        .name("liftoff-wasm-epoch".to_owned())
        .spawn(move || {
            while let Some(engine) = weak.upgrade() {
                engine.increment_epoch();
                drop(engine);
                #[expect(
                    clippy::disallowed_methods,
                    reason = "a thread of its own, outside the async runtime"
                )]
                std::thread::sleep(EPOCH_TICK);
            }
        })
        .map_err(|error| engine_error(&error))?;
    Ok((engine, cache))
}

/// The linker of the world's imports for stores of `HostState<S, L, C>`.
pub(crate) fn linker<S, L, C>(engine: &Engine) -> Result<Linker<HostState<S, L, C>>, ModuleError>
where
    S: ModuleStore + 'static,
    L: ModuleLog,
    C: CoreCalls,
{
    let mut linker = Linker::new(engine);
    host::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
        .and_then(|()| wasmtime_wasi::p2::add_to_linker_async(&mut linker))
        .and_then(|()| wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker))
        .map_err(|error| ModuleError::Load {
            module: "*".to_owned(),
            reason: format!("{error:#}"),
        })?;
    Ok(linker)
}

/// Makes `dir` the daemon's own: created `0700` when missing, narrowed to
/// `0700` when others may write to it, refused when it is a symlink or
/// belongs to another user.
///
/// For the compile cache, wasmtime loads the native code it finds there
/// by its key, without checking it: whoever can write the directory runs
/// code in the daemon, past the signature and hash checks that guard
/// module files. The cache is part of what the daemon trusts, like its
/// binary. The directory of the kept components (`paths.data/modules/`)
/// gets the same rule, so that only the daemon itself replaces a file
/// between its checks.
///
/// # Errors
///
/// What the file system answered, or why the directory is refused.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

        let refused = |why: String| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("the directory {} {why}", dir.display()),
            )
        };
        if std::fs::symlink_metadata(dir).is_err() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let meta = std::fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(refused("is not a directory".to_owned()));
        }
        // The owner of a file the daemon makes is the daemon's user.
        let probe = dir.join(format!(".owner-{}", std::process::id()));
        let owner = std::fs::File::create(&probe)
            .and_then(|file| file.metadata())
            .map(|probed| probed.uid());
        let _removed = std::fs::remove_file(&probe);
        if owner? != meta.uid() {
            return Err(refused(format!(
                "belongs to another user (uid {})",
                meta.uid()
            )));
        }
        if meta.mode() & 0o077 != 0 {
            tracing::warn!(dir = %dir.display(), mode = format!("{:o}", meta.mode() & 0o777), "a private directory of the daemon was open to other users; it is made 0700");
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}
