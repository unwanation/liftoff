//! [`HostRuntime`]: the core's [`ModuleRuntime`] port over a
//! [`ModuleHost`] and a directory of components. It lived in `liftoffd` until the CLI's local mode needed the
//! same; every rule of installing stays in the
//! core's `ModuleService`, the runtime only does what it is told.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use liftoff_core::Error;
use liftoff_core::domain::{ModuleName, ModuleVersion};
use liftoff_core::module::{CoreCalls, ModuleManifest, ModuleRegistry, NoCoreCalls};
use liftoff_core::ports::{ModuleRuntime, ModuleStore};

use crate::engine::private_dir;
use crate::error::ModuleError;
use crate::host::ModuleHost;
use crate::log::ModuleLog;
use crate::scheduler::Timer;
use crate::set::ModuleSet;

/// The module runtime over a [`ModuleHost`] and a directory.
///
/// Modules run in the host, the loaded ones are held in a [`ModuleSet`]
/// (which the core's hook dispatcher and `ModuleGit` call), and
/// components are kept as `<dir>/<name>-<version>.wasm` (`dir` is the
/// daemon's `paths.data/modules/`, the CLI's `~/.config/liftoff/modules/`).
pub struct HostRuntime<S: 'static, T, L: 'static, C: 'static = NoCoreCalls> {
    host: ModuleHost<S, T, L, C>,
    set: ModuleSet<S, L, C>,
    dir: PathBuf,
}

impl<S: 'static, T, L: 'static, C> core::fmt::Debug for HostRuntime<S, T, L, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostRuntime")
            .field("host", &self.host)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl<S: ModuleStore + 'static, T: Timer, L: ModuleLog, C: CoreCalls> HostRuntime<S, T, L, C> {
    /// The runtime over `host`, keeping components in `dir`.
    #[must_use]
    pub fn new(host: ModuleHost<S, T, L, C>, dir: impl Into<PathBuf>) -> Self {
        Self {
            host,
            set: ModuleSet::new(),
            dir: dir.into(),
        }
    }

    /// The module host.
    #[must_use]
    pub const fn host(&self) -> &ModuleHost<S, T, L, C> {
        &self.host
    }

    /// The loaded modules, for the hook dispatcher and module routes.
    #[must_use]
    pub const fn set(&self) -> &ModuleSet<S, L, C> {
        &self.set
    }

    /// Where the components are kept.
    #[must_use]
    pub const fn dir(&self) -> &PathBuf {
        &self.dir
    }
}

impl<S: 'static, T, L: 'static, C> HostRuntime<S, T, L, C> {
    /// `<dir>/<name>-<version>.wasm`. Both are validated domain types
    /// whose text has no `/` and no `..` segment, so the file stays in
    /// `dir`.
    fn file(&self, name: &ModuleName, version: &ModuleVersion) -> PathBuf {
        self.dir.join(format!("{name}-{version}.wasm"))
    }
}

/// A component the host does not load is the caller's bad input; any
/// other failure maps as the host's does.
fn refused(error: ModuleError) -> Error {
    match error {
        ModuleError::Load { reason, .. } => Error::invalid("component", reason),
        other => other.into(),
    }
}

/// An I/O failure on the file of a module, for the log only.
fn io_failure(path: &Path, error: &std::io::Error) -> Error {
    Error::internal_msg(format!("{}: {error}", path.display()))
}

impl<S: ModuleStore + 'static, T: Timer, L: ModuleLog, C: CoreCalls> ModuleRuntime
    for HostRuntime<S, T, L, C>
{
    fn registry(&self) -> &ModuleRegistry {
        self.host.registry()
    }

    async fn inspect(&self, name: &ModuleName, component: &[u8]) -> Result<ModuleManifest, Error> {
        self.host
            .inspect(name.clone(), component)
            .await
            .map_err(refused)
    }

    /// Writes a temporary file next to it and renames it into place, so
    /// a crash never leaves half a component under the module's name. The
    /// directory is the runtime's own (`0700`, its owner, no symlink:
    /// [`private_dir`]); the core checks the file against
    /// the SHA-256 in its row before every later load.
    async fn save(
        &self,
        name: &ModuleName,
        version: &ModuleVersion,
        component: &[u8],
    ) -> Result<(), Error> {
        let path = self.file(name, version);
        let partial = path.with_extension("wasm.partial");
        let dir = self.dir.clone();
        let bytes = component.to_vec();
        tokio::task::spawn_blocking(move || {
            private_dir(&dir).map_err(|error| io_failure(&dir, &error))?;
            std::fs::write(&partial, &bytes).map_err(|error| io_failure(&partial, &error))?;
            std::fs::rename(&partial, &path).map_err(|error| io_failure(&path, &error))
        })
        .await
        .map_err(Error::internal)?
    }

    async fn read(&self, name: &ModuleName, version: &ModuleVersion) -> Result<Vec<u8>, Error> {
        let path = self.file(name, version);
        let what = format!("the file of {name} {version}");
        tokio::task::spawn_blocking(move || match std::fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::not_found(what))
            }
            Err(error) => Err(io_failure(&path, &error)),
        })
        .await
        .map_err(Error::internal)?
    }

    async fn discard(&self, name: &ModuleName, version: &ModuleVersion) -> Result<(), Error> {
        let path = self.file(name, version);
        tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_failure(&path, &error)),
        })
        .await
        .map_err(Error::internal)?
    }

    /// Loads it in the host (which registers it), runs its migration when
    /// asked, and puts it in the set the hooks and routes call. A failed
    /// migration takes the fresh registration back.
    async fn load(
        &self,
        name: &ModuleName,
        component: &[u8],
        config: &BTreeMap<String, String>,
        migrate_from: Option<u32>,
    ) -> Result<(), Error> {
        let module = self
            .host
            .load(name.clone(), component, config.clone())
            .await
            .map_err(refused)?;
        if let Some(from) = migrate_from
            && let Err(error) = module.migrate(from).await
        {
            tracing::error!(module = %name, ?error, "a module's migration failed");
            let _registered = self.host.registry().remove(name);
            return Err(error.into());
        }
        self.set.insert(module);
        Ok(())
    }

    fn unload(&self, name: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        let _held = self.set.remove(name);
        let _registered = self.host.registry().remove(name);
        core::future::ready(Ok(()))
    }

    async fn purge(&self, name: &ModuleName) -> Result<(), Error> {
        self.host.store().module_purge(name).await
    }
}
