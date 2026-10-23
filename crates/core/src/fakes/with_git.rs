//! [`WithGit`].

use std::sync::Arc;

use super::{
    FakeBlobs, FakeComponentFiles, FakeFunctions, FakeHooks, FakeModuleRuntime, FakeModuleSource,
    FakePorts, FakeResolver, FakeSecrets, FixedClock, InMemoryStore, RecordingDocker,
    RecordingFiles, RecordingProxy, RecordingQueue, RecordingSystemd, SeqRng,
};
use crate::ports::{Git, Ports};

/// [`FakePorts`] with another [`Git`] in place of
/// [`FakeGit`](super::FakeGit).
///
/// For tests that run the core services over the forge modules
/// ([`ModuleGit`](crate::module::ModuleGit) over the module host and a
/// fake forge API: DEP-07 and ENV-04 through the
/// `git.forgejo` module).
///
/// Every other port is the shared fake.
#[derive(Debug, Clone)]
pub struct WithGit<G> {
    /// Every other port.
    pub fakes: Arc<FakePorts>,
    /// The forge under test.
    pub git: G,
}

impl<G> WithGit<G> {
    /// `fakes` with `git` as the forge.
    #[must_use]
    pub const fn new(fakes: Arc<FakePorts>, git: G) -> Self {
        Self { fakes, git }
    }
}

impl<G: Git + 'static> Ports for WithGit<G> {
    type Store = InMemoryStore;
    type Docker = RecordingDocker;
    type Files = RecordingFiles;
    type Systemd = RecordingSystemd;
    type Proxy = RecordingProxy;
    type Git = G;
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
    type Hooks = FakeHooks;

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
        &self.git
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
        &self.fakes.hooks
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
