use std::collections::BTreeMap;

use crate::domain::{ModuleName, ModuleVersion};
use crate::error::Error;
use crate::module::{ModuleManifest, ModuleRegistry};

/// The module host as installation drives it: the components it keeps, the modules it runs and their
/// storage.
///
/// The daemon's runtime is `liftoffd`'s over the wasmtime host of
/// `liftoff-wasm` and `paths.data/modules/<name>-<version>.wasm`; tests
/// use `FakeModuleRuntime`. Every rule of installing (signature, versions,
/// dependencies, configuration, secrets) lives in
/// [`ModuleService`](crate::service::ModuleService); the runtime only
/// does what it is told.
pub trait ModuleRuntime: Send + Sync {
    /// The registry of the modules it runs: their manifests and status,
    /// which the hooks, the module routes and `GET /modules` read.
    fn registry(&self) -> &ModuleRegistry;

    /// The manifest of `component` as the module `name`, read without
    /// registering or keeping anything.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] when the bytes are not a component of the
    /// world or its manifest is refused (MOD-06).
    fn inspect(
        &self,
        name: &ModuleName,
        component: &[u8],
    ) -> impl Future<Output = Result<ModuleManifest, Error>> + Send;

    /// Keeps `component` as the file of `name` at `version`.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when it cannot be written.
    fn save(
        &self,
        name: &ModuleName,
        version: &ModuleVersion,
        component: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// The kept file of `name` at `version`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when there is none; [`Error::Internal`] when it
    /// cannot be read.
    fn read(
        &self,
        name: &ModuleName,
        version: &ModuleVersion,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send;

    /// Deletes the kept file of `name` at `version`; nothing when there is
    /// none.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when it cannot be deleted.
    fn discard(
        &self,
        name: &ModuleName,
        version: &ModuleVersion,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Loads `component` as the module `name` with `config` (secrets
    /// opened: what `config-get` answers), runs `migrate(from)` when
    /// `migrate_from` is given, and registers it in
    /// [`ModuleRuntime::registry`] with its routes, tools, commands and
    /// hooks, without a restart (REG-06). A module of that name already
    /// loaded is replaced.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] when the component does not load;
    /// [`Error::Internal`] when its migration fails. Nothing stays
    /// registered then.
    fn load(
        &self,
        name: &ModuleName,
        component: &[u8],
        config: &BTreeMap<String, String>,
        migrate_from: Option<u32>,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    /// Stops the module `name` and forgets it in the registry; nothing
    /// when it is not loaded.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when it cannot be stopped.
    fn unload(&self, name: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send;

    /// Drops the storage of `name`: its tables (`m_<name>_*`) and its key
    /// space (`remove --purge`, REG-07).
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the store fails.
    fn purge(&self, name: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send;
}
