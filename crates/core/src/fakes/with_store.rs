//! [`WithStore`].

use std::sync::Arc;

use super::{
    FakeBlobs, FakeComponentFiles, FakeFunctions, FakeGit, FakeHooks, FakeModuleRuntime,
    FakeModuleSource, FakePorts, FakeResolver, FakeSecrets, FixedClock, RecordingDocker,
    RecordingFiles, RecordingProxy, RecordingQueue, RecordingSystemd, SeqRng,
};
use crate::ports::{Ports, Store};

/// [`FakePorts`] with a real [`Store`] adapter in place of
/// [`InMemoryStore`](super::InMemoryStore).
///
/// For tests that run a service over records an adapter wrote (IMP-04:
/// a deploy of an environment imported from v1 into the SQLite store).
///
/// Every other port is the shared fake.
#[derive(Debug, Clone)]
pub struct WithStore<S> {
    /// Every other port.
    pub fakes: Arc<FakePorts>,
    /// The store under test.
    pub store: S,
}

impl<S> WithStore<S> {
    /// `fakes` with `store` as the store.
    #[must_use]
    pub const fn new(fakes: Arc<FakePorts>, store: S) -> Self {
        Self { fakes, store }
    }
}

impl<S: Store + 'static> Ports for WithStore<S> {
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
    type Registry = FakeModuleSource;
    type Modules = FakeModuleRuntime;
    type Functions = FakeFunctions;
    type Components = FakeComponentFiles;
    type Blobs = FakeBlobs;
    type Hooks = FakeHooks;

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
