//! [`LocalPorts`]: the core's ports in local mode, so the CLI runs the same core services as the daemon
//! (`GitService`, `ModuleService`, `TemplateCatalog`, `CreateService` up
//! to `Push`) over local adapters:
//!
//! - the store is [`LocalSecretStore`] (`secrets.db`), the secrets port the
//!   local key;
//! - the forges are the `git.*` modules in the CLI's module host
//!   ([`LocalGit`], the core's `ModuleGit` over the loaded modules), the
//!   module runtime `liftoff-wasm`'s [`HostRuntime`] over that host and
//!   `modules/`, the registry `liftoff-wasm`'s [`HttpModuleSource`];
//! - the clock and the random source are the core's `system` ones, the
//!   hooks `NoHooks` (no module of local mode answers a hook);
//! - what only an installation has (docker, compose files, slices, the
//!   proxy, public DNS, the deploy queue, the function runtime and its
//!   kept components) is [`Offline`]: every call is
//!   [`Error::Unavailable`] with a hint to `lof server connect`. No local
//!   command reaches them: the plan of `create` stops before its server
//!   steps.

use core::future::{Future, ready};
use core::net::{Ipv4Addr, SocketAddr};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::sync::Arc;

use liftoff_core::Error;
use liftoff_core::crypto::MasterKey;
use liftoff_core::domain::{
    ArtifactId, ArtifactPath, BlobHash, ComponentHash, ComposeProject, DeployId, Env,
    EnvironmentId, Host, HostPort, Limits as SliceLimits, Login, ServiceName, SliceName,
    VersionNumber,
};
use liftoff_core::module::{ModuleGit, RegistryKey};
use liftoff_core::ports::{
    Blobs, CommandOutput, ComponentFiles, ComponentsLock, DeployQueue, Docker, FunctionHost,
    NoHooks, Ports, ProjectFile, ProjectFiles, Proxy, Resolver, ServiceStatus, Systemd,
};
use liftoff_core::proxy_config::ProxyConfig;
use liftoff_core::service::{ArtifactSettings, ModuleSettings, ProxySettings, Settings, TlsMode};
use liftoff_core::system::{OsRng, SystemClock};
use liftoff_wasm::{HostRuntime, HttpModuleSource, ModuleSet, TokioTimer, TracingLog};

use super::host::LocalCoreCalls;
use super::secrets::LocalSecretStore;
use super::store::LocalModuleStore;

/// The modules the CLI's module host loaded.
pub type LocalModuleSet = ModuleSet<LocalModuleStore, TracingLog, LocalCoreCalls>;

/// The core's `Git` port in local mode: the `git.*` modules the CLI runs
/// itself (LCL-01).
pub type LocalGit = ModuleGit<LocalModuleSet>;

/// The module runtime of local mode.
pub type LocalRuntime = HostRuntime<LocalModuleStore, TokioTimer, TracingLog, LocalCoreCalls>;

/// What local mode runs without: a call is [`Error::Unavailable`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Offline;

/// The answer of [`Offline`] to `what`.
fn offline<T>(what: &str) -> Result<T, Error> {
    Err(Error::unavailable(format!(
        "{what} runs on a liftoff server, and the CLI runs without one: \
         connect this project with `lof server connect <url>`"
    )))
}

/// The log lines [`Offline`] never streams.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct NoLines;

impl futures_core::Stream for NoLines {
    type Item = Result<String, Error>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

impl Docker for Offline {
    fn login(
        &self,
        _registry: &Host,
        _user: &Login,
        _token: &str,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send {
        ready(offline("docker"))
    }

    fn compose_pull(
        &self,
        _project: &ComposeProject,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send {
        ready(offline("docker compose"))
    }

    fn compose_up(
        &self,
        _project: &ComposeProject,
        _wait_timeout: Duration,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send {
        ready(offline("docker compose"))
    }

    fn compose_down(
        &self,
        _project: &ComposeProject,
        _volumes: bool,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send {
        ready(offline("docker compose"))
    }

    fn compose_ps(
        &self,
        _project: &ComposeProject,
    ) -> impl Future<Output = Result<Vec<ServiceStatus>, Error>> + Send {
        ready(offline("docker compose"))
    }

    fn logs(
        &self,
        _project: &ComposeProject,
        _service: Option<&ServiceName>,
        _tail: Option<u32>,
        _follow: bool,
    ) -> impl Future<
        Output = Result<
            impl futures_core::Stream<Item = Result<String, Error>> + Send + 'static,
            Error,
        >,
    > + Send {
        ready(offline::<NoLines>("docker compose logs"))
    }
}

impl ProjectFiles for Offline {
    fn write(
        &self,
        _project: &ComposeProject,
        _file: ProjectFile,
        _content: &str,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a compose project"))
    }

    fn read(
        &self,
        _project: &ComposeProject,
        _file: ProjectFile,
    ) -> impl Future<Output = Result<Option<String>, Error>> + Send {
        ready(offline("a compose project"))
    }

    fn remove_all(
        &self,
        _project: &ComposeProject,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a compose project"))
    }
}

impl Systemd for Offline {
    fn set_slice_limits(
        &self,
        _slice: &SliceName,
        _limits: SliceLimits,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a slice"))
    }

    fn stop_slice(&self, _slice: &SliceName) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a slice"))
    }
}

impl Proxy for Offline {
    fn apply(&self, _cfg: ProxyConfig) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("the proxy"))
    }

    fn ready(&self) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("the proxy"))
    }
}

impl Resolver for Offline {
    fn cname(&self, _host: &Host) -> impl Future<Output = Result<Vec<Host>, Error>> + Send {
        ready(offline("checking DNS"))
    }

    fn a(&self, _host: &Host) -> impl Future<Output = Result<Vec<Ipv4Addr>, Error>> + Send {
        ready(offline("checking DNS"))
    }
}

impl DeployQueue for Offline {
    fn enqueue(&self, _deploy: DeployId) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a deploy"))
    }
}

impl FunctionHost for Offline {
    fn check(&self, _component: &[u8]) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a function"))
    }

    fn precompile(&self, _component: &[u8]) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a function"))
    }

    fn activate(
        &self,
        _environment: EnvironmentId,
        _component: &[u8],
        _config: Env,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a function"))
    }

    fn is_active(&self, _environment: EnvironmentId) -> impl Future<Output = bool> + Send {
        ready(false)
    }

    fn logs(
        &self,
        _environment: EnvironmentId,
        _tail: Option<u32>,
        _follow: bool,
    ) -> impl Future<
        Output = Result<
            impl futures_core::Stream<Item = Result<String, Error>> + Send + 'static,
            Error,
        >,
    > + Send {
        ready(offline::<NoLines>("a function's logs"))
    }
}

/// The lock of [`Offline`]'s files, which it never writes.
static OFFLINE_FILES: ComponentsLock = ComponentsLock::new();

impl ComponentFiles for Offline {
    fn lock(&self) -> &ComponentsLock {
        &OFFLINE_FILES
    }

    fn save(
        &self,
        _hash: &ComponentHash,
        _component: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a function deploy"))
    }

    fn read(
        &self,
        _hash: &ComponentHash,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        ready(offline("a function deploy"))
    }

    fn list(&self) -> impl Future<Output = Result<Vec<ComponentHash>, Error>> + Send {
        ready(offline("a function deploy"))
    }

    fn remove(&self, _hash: &ComponentHash) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("a function deploy"))
    }
}

impl Blobs for Offline {
    fn put(
        &self,
        _hash: &BlobHash,
        _bytes: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("an artifact"))
    }

    fn release(&self, _hash: &BlobHash) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("an artifact"))
    }

    fn link(
        &self,
        _artifact: ArtifactId,
        _version: VersionNumber,
        _name: &ArtifactPath,
        _hash: &BlobHash,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("an artifact"))
    }

    fn read(
        &self,
        _artifact: ArtifactId,
        _version: VersionNumber,
        _name: &ArtifactPath,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        ready(offline("an artifact"))
    }

    fn remove_version(
        &self,
        _artifact: ArtifactId,
        _version: VersionNumber,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("an artifact"))
    }

    fn remove_artifact(
        &self,
        _artifact: ArtifactId,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        ready(offline("an artifact"))
    }
}

/// The ports of local mode: the store and key of the config home, the CLI's module host, the registry client; `Offline` for what only a server has.
pub struct LocalPorts {
    pub(crate) store: LocalSecretStore,
    pub(crate) secrets: Arc<MasterKey>,
    pub(crate) git: LocalGit,
    pub(crate) registry: HttpModuleSource,
    pub(crate) modules: LocalRuntime,
}

impl core::fmt::Debug for LocalPorts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalPorts").finish_non_exhaustive()
    }
}

impl Ports for LocalPorts {
    type Store = LocalSecretStore;
    type Docker = Offline;
    type Files = Offline;
    type Systemd = Offline;
    type Proxy = Offline;
    type Git = LocalGit;
    type Resolver = Offline;
    type Clock = SystemClock;
    type Rng = OsRng;
    type Secrets = MasterKey;
    type Queue = Offline;
    type Hooks = NoHooks;
    type Registry = HttpModuleSource;
    type Modules = LocalRuntime;
    type Functions = Offline;
    type Components = Offline;
    type Blobs = Offline;

    fn store(&self) -> &Self::Store {
        &self.store
    }
    fn docker(&self) -> &Self::Docker {
        &Offline
    }
    fn files(&self) -> &Self::Files {
        &Offline
    }
    fn systemd(&self) -> &Self::Systemd {
        &Offline
    }
    fn proxy(&self) -> &Self::Proxy {
        &Offline
    }
    fn git(&self) -> &Self::Git {
        &self.git
    }
    fn resolver(&self) -> &Self::Resolver {
        &Offline
    }
    fn clock(&self) -> &Self::Clock {
        &SystemClock
    }
    fn rng(&self) -> &Self::Rng {
        &OsRng
    }
    fn secrets(&self) -> &Self::Secrets {
        &self.secrets
    }
    fn queue(&self) -> &Self::Queue {
        &Offline
    }
    fn hooks(&self) -> &Self::Hooks {
        &NoHooks
    }
    fn registry(&self) -> &Self::Registry {
        &self.registry
    }
    fn modules(&self) -> &Self::Modules {
        &self.modules
    }
    fn functions(&self) -> &Self::Functions {
        &Offline
    }
    fn components(&self) -> &Self::Components {
        &Offline
    }
    fn blobs(&self) -> &Self::Blobs {
        &Offline
    }
}

/// The base domain of local mode: a project's domain is the server's,
/// so a local plan names `<name>.liftoff.localhost` (RFC 6761: it never
/// leaves the machine) and the plan stops before `AddDomain`.
pub const LOCAL_BASE: &str = "liftoff.localhost";

/// The settings the core services of local mode read: no installation
/// stands behind them, so the values a server would configure are local
/// stand-ins ([`LOCAL_BASE`], loopback addresses) that no local command
/// sends anywhere; the module settings are real: `registry_key` is what
/// local installs check releases against, and the CLI's version what a
/// release's `liftoffd_min` is compared with.
///
/// # Errors
///
/// [`Error::Internal`] when a stand-in does not validate.
pub(super) fn settings(registry_key: RegistryKey) -> Result<Settings, Error> {
    let base = Host::new(LOCAL_BASE)?;
    Ok(Settings {
        base: base.clone(),
        public_ip: Ipv4Addr::LOCALHOST,
        reserved: Vec::new(),
        port_range: HostPort::new(HostPort::MIN)?..=HostPort::new(HostPort::MAX)?,
        wait_timeout: Duration::from_mins(2),
        deploy_history: 20,
        // Local mode opens no tunnels.
        tunnel_offline_after: Duration::from_secs(30),
        proxy: ProxySettings {
            api_host: base,
            api_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 7000)),
            tls: TlsMode::Edge { port: 7002 },
            gate_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 7001)),
            functions_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 7003)),
            tunnel_bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 7004)),
        },
        modules: ModuleSettings {
            registry_key,
            liftoffd_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        templates: Vec::new(),
        // Local mode publishes no artifacts.
        artifacts: ArtifactSettings::default(),
    })
}
