//! [`WithHooks`].

use std::sync::Arc;

use super::{
    FakeBlobs, FakeComponentFiles, FakeFunctions, FakeGit, FakeModuleRuntime, FakeModuleSource,
    FakePorts, FakeResolver, FakeSecrets, FixedClock, InMemoryStore, RecordingDocker,
    RecordingFiles, RecordingProxy, RecordingQueue, RecordingSystemd, SeqRng,
};
use crate::ports::{Hooks, Ports};

/// [`FakePorts`] with other [`Hooks`] in place of
/// [`FakeHooks`](super::FakeHooks).
///
/// For tests that run services or the REST adapter over the module
/// dispatcher ([`ModuleHooks`](crate::module::ModuleHooks) over
/// [`FakeModules`](super::FakeModules) or the module host).
///
/// Every other port is the shared fake.
#[derive(Debug, Clone)]
pub struct WithHooks<H> {
    /// Every other port.
    pub fakes: Arc<FakePorts>,
    /// The hooks under test.
    pub hooks: H,
}

impl<H> WithHooks<H> {
    /// `fakes` with `hooks` as the hooks.
    #[must_use]
    pub const fn new(fakes: Arc<FakePorts>, hooks: H) -> Self {
        Self { fakes, hooks }
    }
}

impl<H: Hooks + 'static> Ports for WithHooks<H> {
    type Store = InMemoryStore;
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
    type Registry = FakeModuleSource;
    type Modules = FakeModuleRuntime;
    type Functions = FakeFunctions;
    type Components = FakeComponentFiles;
    type Blobs = FakeBlobs;
    type Hooks = H;

    fn store(&self) -> &Self::Store {
        &self.fakes.store
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
        &self.fakes.modules
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
