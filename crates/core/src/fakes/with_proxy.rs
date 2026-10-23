//! [`WithProxy`].

use std::sync::Arc;

use super::{
    FakeBlobs, FakeComponentFiles, FakeFunctions, FakeGit, FakeHooks, FakeModuleRuntime,
    FakeModuleSource, FakePorts, FakeResolver, FakeSecrets, FixedClock, InMemoryStore,
    RecordingDocker, RecordingFiles, RecordingQueue, RecordingSystemd, SeqRng,
};
use crate::ports::{Ports, Proxy};

/// [`FakePorts`] with a real [`Proxy`] adapter in place of
/// [`RecordingProxy`](super::RecordingProxy).
///
/// For the adapter's own end-to-end tests through a service (PRX-05:
/// `CaddyAdmin` behind `DomainService`).
///
/// Every other port is the shared fake, so `World`'s store and log keep
/// working.
#[derive(Debug, Clone)]
pub struct WithProxy<X> {
    /// Every other port.
    pub fakes: Arc<FakePorts>,
    /// The proxy under test.
    pub proxy: X,
}

impl<X> WithProxy<X> {
    /// `fakes` with `proxy` as the reverse proxy.
    #[must_use]
    pub const fn new(fakes: Arc<FakePorts>, proxy: X) -> Self {
        Self { fakes, proxy }
    }
}

impl<X: Proxy + 'static> Ports for WithProxy<X> {
    type Store = InMemoryStore;
    type Docker = RecordingDocker;
    type Files = RecordingFiles;
    type Systemd = RecordingSystemd;
    type Proxy = X;
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
        &self.proxy
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
