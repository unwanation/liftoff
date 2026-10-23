//! [`WithModules`].

use std::sync::Arc;

use super::{
    FakeBlobs, FakeComponentFiles, FakeFunctions, FakeGit, FakeHooks, FakeModuleSource, FakePorts,
    FakeResolver, FakeSecrets, FixedClock, RecordingDocker, RecordingFiles, RecordingProxy,
    RecordingQueue, RecordingSystemd, SeqRng,
};
use crate::ports::{Hooks, ModuleRuntime, Ports, Store};

/// [`FakePorts`] with a real [`Store`] and a real [`ModuleRuntime`] in
/// place of [`InMemoryStore`](super::InMemoryStore) and
/// [`FakeModuleRuntime`](super::FakeModuleRuntime).
///
/// For the end-to-end tests of installing modules: the SQLite store and the daemon's runtime over the wasmtime host
/// and built components, served by the fake registry
/// [`FakeModuleSource`].
///
/// The hooks are the shared [`FakeHooks`] unless
/// [`WithModules::with_hooks`] puts the dispatcher over the runtime's
/// modules in their place (the daemon's wiring). Every other port is the
/// shared fake.
#[derive(Debug, Clone)]
pub struct WithModules<S, M, H = FakeHooks> {
    /// Every other port.
    pub fakes: Arc<FakePorts>,
    /// The store under test.
    pub store: S,
    /// The module runtime under test.
    pub modules: M,
    /// The hooks.
    pub hooks: H,
}

impl<S, M> WithModules<S, M> {
    /// `fakes` with `store` as the store and `modules` as the runtime.
    #[must_use]
    pub fn new(fakes: Arc<FakePorts>, store: S, modules: M) -> Self {
        let hooks = fakes.hooks.clone();
        Self {
            fakes,
            store,
            modules,
            hooks,
        }
    }
}

impl<S, M, H> WithModules<S, M, H> {
    /// The same ports with `hooks` as the hooks.
    #[must_use]
    pub fn with_hooks<G>(self, hooks: G) -> WithModules<S, M, G> {
        WithModules {
            fakes: self.fakes,
            store: self.store,
            modules: self.modules,
            hooks,
        }
    }
}

impl<S: Store + 'static, M: ModuleRuntime + 'static, H: Hooks + 'static> Ports
    for WithModules<S, M, H>
{
    type Store = S;
    type Docker = RecordingDocker;
    type Files = RecordingFiles;
    type Systemd = RecordingSystemd;
    type Proxy = RecordingProxy;
    type Git = FakeGit;
    type Resolver = FakeResolver;
    type Clock = FixedClock;
    type Rng = SeqRng;
    type Secrets = FakeSecrets;
    type Queue = RecordingQueue;
    type Hooks = H;
    type Registry = FakeModuleSource;
    type Modules = M;
    type Functions = FakeFunctions;
    type Components = FakeComponentFiles;
    type Blobs = FakeBlobs;

    fn store(&self) -> &Self::Store {
        &self.store
    }
    fn docker(&self) -> &Self::Docker {
        &self.fakes.docker
    }
    fn files(&self) -> &Self::Files {
        &self.fakes.files
    }
    fn systemd(&self) -> &Self::Systemd {
        &self.fakes.systemd
    }
    fn proxy(&self) -> &Self::Proxy {
        &self.fakes.proxy
    }
    fn git(&self) -> &Self::Git {
        &self.fakes.git
    }
    fn resolver(&self) -> &Self::Resolver {
        &self.fakes.resolver
    }
    fn clock(&self) -> &Self::Clock {
        &self.fakes.clock
    }
    fn rng(&self) -> &Self::Rng {
        &self.fakes.rng
    }
    fn secrets(&self) -> &Self::Secrets {
        &self.fakes.secrets
    }
    fn queue(&self) -> &Self::Queue {
        &self.fakes.queue
    }
    fn hooks(&self) -> &Self::Hooks {
        &self.hooks
    }
    fn registry(&self) -> &Self::Registry {
        &self.fakes.registry
    }
    fn modules(&self) -> &Self::Modules {
        &self.modules
    }
    fn functions(&self) -> &Self::Functions {
        &self.fakes.functions
    }
    fn components(&self) -> &Self::Components {
        &self.fakes.components
    }
    fn blobs(&self) -> &Self::Blobs {
        &self.fakes.blobs
    }
}
