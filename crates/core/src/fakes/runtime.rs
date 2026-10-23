//! [`FakeModuleRuntime`].

use core::future::ready;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::log::{Call, CallLog, Faults, RuntimeCall, lock, scriptable};
use crate::domain::{ModuleName, ModuleVersion};
use crate::error::Error;
use crate::module::{ModuleManifest, ModuleRegistry};
use crate::ports::ModuleRuntime;

#[derive(Debug, Default)]
struct State {
    /// The kept files, by module and version.
    files: BTreeMap<(String, String), Vec<u8>>,
    /// The configuration each loaded module was loaded with.
    configs: BTreeMap<String, BTreeMap<String, String>>,
    /// The manifest the loads of a module register in place of its
    /// component's ([`FakeModuleRuntime::load_as`]).
    load_as: BTreeMap<String, String>,
}

/// The module host in memory, behind the same [`ModuleRuntime`] as the
/// daemon's.
///
/// A component here is its manifest's JSON (what
/// [`FakeRelease::of_manifest`](super::FakeRelease::of_manifest) serves):
/// `inspect` and `load` parse it with [`ModuleManifest::parse`], and
/// `load` registers the module in [`FakeModuleRuntime::registry`] (a
/// [`ModuleRegistry`] of its own, shared by clones). Every call but
/// `read` is logged ([`RuntimeCall`]) and scriptable by its name
/// (`fail_next("load", …)`); the kept files and the configuration each
/// module was loaded with can be inspected.
#[derive(Debug, Clone)]
pub struct FakeModuleRuntime {
    log: CallLog,
    faults: Faults,
    registry: Arc<ModuleRegistry>,
    state: Arc<Mutex<State>>,
}

scriptable!(FakeModuleRuntime);

impl FakeModuleRuntime {
    /// A runtime with nothing loaded, logging to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            faults: Faults::default(),
            registry: Arc::new(ModuleRegistry::new()),
            state: Arc::default(),
        }
    }

    /// The `(module, version)` of every kept file.
    #[must_use]
    pub fn files(&self) -> Vec<(String, String)> {
        lock(&self.state).files.keys().cloned().collect()
    }

    /// The configuration `module` was last loaded with (secrets opened),
    /// if it is loaded.
    #[must_use]
    pub fn config(&self, module: &str) -> Option<BTreeMap<String, String>> {
        lock(&self.state).configs.get(module).cloned()
    }

    /// From now on, loading `module` registers the manifest JSON
    /// `manifest` while `inspect` still reads the component's: a module
    /// whose manifest changes with the configuration it is loaded with.
    pub fn load_as(&self, module: &str, manifest: &str) {
        let _previous = lock(&self.state)
            .load_as
            .insert(module.to_owned(), manifest.to_owned());
    }

    /// The registry, shared (for a dispatcher over the same modules).
    #[must_use]
    pub fn shared_registry(&self) -> Arc<ModuleRegistry> {
        Arc::clone(&self.registry)
    }

    fn record(&self, call: RuntimeCall) {
        self.log.push(Call::Runtime(call));
    }
}

/// The manifest a fake component holds.
fn manifest_of(component: &[u8]) -> Result<ModuleManifest, Error> {
    let text = core::str::from_utf8(component)
        .map_err(|_not_utf8| Error::invalid("component", "not a component of the world"))?;
    Ok(ModuleManifest::parse(text)?)
}

impl ModuleRuntime for FakeModuleRuntime {
    fn registry(&self) -> &ModuleRegistry {
        &self.registry
    }

    fn inspect(
        &self,
        name: &ModuleName,
        component: &[u8],
    ) -> impl Future<Output = Result<ModuleManifest, Error>> + Send {
        self.record(RuntimeCall::Inspect(name.to_string()));
        ready(
            self.faults
                .check("inspect")
                .and_then(|()| manifest_of(component)),
        )
    }

    fn save(
        &self,
        name: &ModuleName,
        version: &ModuleVersion,
        component: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(RuntimeCall::Save {
            module: name.to_string(),
            version: version.to_string(),
        });
        let saved = self.faults.check("save").map(|()| {
            let _previous = lock(&self.state)
                .files
                .insert((name.to_string(), version.to_string()), component.to_vec());
        });
        ready(saved)
    }

    fn read(
        &self,
        name: &ModuleName,
        version: &ModuleVersion,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        let read = self.faults.check("read").and_then(|()| {
            lock(&self.state)
                .files
                .get(&(name.to_string(), version.to_string()))
                .cloned()
                .ok_or_else(|| Error::not_found(format!("the file of {name} {version}")))
        });
        ready(read)
    }

    fn discard(
        &self,
        name: &ModuleName,
        version: &ModuleVersion,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(RuntimeCall::Discard {
            module: name.to_string(),
            version: version.to_string(),
        });
        let discarded = self.faults.check("discard").map(|()| {
            let _removed = lock(&self.state)
                .files
                .remove(&(name.to_string(), version.to_string()));
        });
        ready(discarded)
    }

    fn load(
        &self,
        name: &ModuleName,
        component: &[u8],
        config: &BTreeMap<String, String>,
        migrate_from: Option<u32>,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(RuntimeCall::Load {
            module: name.to_string(),
            migrate_from,
        });
        let replaced = lock(&self.state).load_as.get(name.as_str()).cloned();
        let loaded = self
            .faults
            .check("load")
            .and_then(|()| manifest_of(replaced.as_deref().map_or(component, str::as_bytes)))
            .map(|manifest| {
                self.registry.register(name.clone(), manifest);
                let _previous = lock(&self.state)
                    .configs
                    .insert(name.to_string(), config.clone());
            });
        ready(loaded)
    }

    fn unload(&self, name: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(RuntimeCall::Unload(name.to_string()));
        let unloaded = self.faults.check("unload").map(|()| {
            let _was = self.registry.remove(name);
            let _removed = lock(&self.state).configs.remove(name.as_str());
        });
        ready(unloaded)
    }

    fn purge(&self, name: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(RuntimeCall::Purge(name.to_string()));
        ready(self.faults.check("purge"))
    }
}
