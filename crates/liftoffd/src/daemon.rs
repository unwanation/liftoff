//! The production composition root: configuration in, a running daemon out.
//!
//! Everything that touches the real world is built here and nowhere else
//! ([`DaemonPorts`]): the `SQLite` store, the host adapters (docker, compose
//! files, systemd, public DNS), the Caddy admin API (`liftoff-proxy`), the
//! module host and the registry client (`liftoff-wasm`), the forges as the
//! `git.*` modules serve them ([`DaemonGit`]), the function runtime and
//! the kept components (`crate::functions`), the clock, the random
//! source, the master key and the deploy queue. The queue runs over a
//! [`LateRunner`] bound to `DeployService::run` once the ports exist; the
//! daily re-verification of external domains runs next to it over
//! `DomainService`. Both stop with the REST listener, as do the gate
//! listener and the function listener (`crate::listeners::functions`).

use core::fmt;
use core::pin::Pin;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use liftoff_core::Error;
use liftoff_core::crypto::MasterKey;
use liftoff_core::domain::DeployId;
use liftoff_core::module::{ModuleGit, ModuleHooks};
use liftoff_core::ports::Ports;
use liftoff_core::service::{
    ArtifactsService, DeployService, DomainService, GateListener, LinksService, McpAuthService,
    ModuleService, Reconciler, Settings, TunnelsService,
};
use liftoff_host::{DnsResolver, FileComponentFiles, FileProjectFiles, ProcessDocker, SudoSystemd};
use liftoff_proxy::CaddyAdmin;
use liftoff_rest::{
    AppState, Probed, Readiness, Services, app_router, serve_gate_on, serve_router_on,
};
use liftoff_store::{FileBlobs, LinkMode, SqliteStore};
use liftoff_tunnel::{Bandwidth, ServerConfig, TunnelServer};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use liftoff_wasm::{HttpModuleSource, ModuleHost, ModuleSet, TokioTimer, TracingLog};

use crate::config::{AppConfig, FunctionsConfig, TunnelConfig};
use crate::error::AppError;
use crate::functions::{DaemonFunctions, function_ports};
use crate::listeners::functions::{FunctionListener, ListenerLimits};
use crate::listeners::tunnel::TunnelListener;
use crate::mcp_http::{self, McpMount};
use crate::modules::{DaemonModules, LateCore, SYNC_STATUS_EVERY, bind_core, sync_status_every};
use crate::queue::{JobQueue, LateRunner, RunFuture};
use crate::reverify::reverify_daily;
use crate::sweep::{SWEEP_ARTIFACTS_EVERY, sweep_artifacts_every};
use crate::system::{OsRng, SystemClock};
use crate::tunnels::DaemonTunnels;

/// One dependency `/readyz` asks about.
pub trait Probe: fmt::Debug + Send + Sync + 'static {
    /// `Ok` when the dependency answers.
    fn probe(&self) -> impl Future<Output = Result<(), Error>> + Send;
}

impl Probe for ProcessDocker {
    fn probe(&self) -> impl Future<Output = Result<(), Error>> + Send {
        self.version()
    }
}

impl Probe for SqliteStore {
    fn probe(&self) -> impl Future<Output = Result<(), Error>> + Send {
        self.ping()
    }
}

/// `/readyz` and `GET /server`: docker and SQLite, each on its own.
///
/// Docker answers `docker version` and SQLite `SELECT 1`, each asked
/// whatever the other answered (so `lof doctor` shows them apart). The
/// Caddy admin API is asked by the REST adapter through the `Proxy` port.
#[derive(Debug, Clone)]
pub struct HostReadiness<D = ProcessDocker, S = SqliteStore> {
    docker: D,
    store: S,
}

impl<D: Probe, S: Probe> Readiness for HostReadiness<D, S> {
    fn check(&self) -> Pin<Box<dyn Future<Output = Vec<Probed>> + Send + '_>> {
        Box::pin(async {
            vec![
                Probed {
                    name: "docker",
                    result: self.docker.probe().await,
                },
                Probed {
                    name: "sqlite",
                    result: self.store.probe().await,
                },
            ]
        })
    }
}

/// The deploy queue of the daemon.
pub type DaemonQueue = JobQueue<SqliteStore, LateRunner, SystemClock>;

/// Every port over the real adapters.
#[derive(Debug)]
pub struct DaemonPorts {
    store: SqliteStore,
    docker: ProcessDocker,
    files: FileProjectFiles,
    systemd: SudoSystemd,
    proxy: CaddyAdmin,
    git: DaemonGit,
    resolver: DnsResolver,
    clock: SystemClock,
    rng: OsRng,
    secrets: MasterKey,
    queue: DaemonQueue,
    hooks: DaemonHooks,
    registry: HttpModuleSource,
    modules: DaemonRuntime,
    functions: DaemonFunctions,
    components: FileComponentFiles,
    blobs: FileBlobs,
}

/// The core's hooks answered by the installed modules: the dispatcher
/// over the module host's registry and the modules it holds.
pub type DaemonHooks = ModuleHooks<DaemonModuleSet>;

/// The core's `Git` port answered by the installed `git.*` modules, the
/// one serving each connection's host.
pub type DaemonGit = ModuleGit<DaemonModuleSet>;

/// The modules the daemon's module host has loaded.
pub type DaemonModuleSet = ModuleSet<SqliteStore, TracingLog, LateCore>;

impl Ports for DaemonPorts {
    type Store = SqliteStore;
    type Docker = ProcessDocker;
    type Files = FileProjectFiles;
    type Systemd = SudoSystemd;
    type Proxy = CaddyAdmin;
    type Git = DaemonGit;
    type Resolver = DnsResolver;
    type Clock = SystemClock;
    type Rng = OsRng;
    type Secrets = MasterKey;
    type Queue = DaemonQueue;
    type Hooks = DaemonHooks;
    type Registry = HttpModuleSource;
    type Modules = DaemonRuntime;
    type Functions = DaemonFunctions;
    type Components = FileComponentFiles;
    type Blobs = FileBlobs;

    fn store(&self) -> &Self::Store {
        &self.store
    }
    fn docker(&self) -> &Self::Docker {
        &self.docker
    }
    fn files(&self) -> &Self::Files {
        &self.files
    }
    fn systemd(&self) -> &Self::Systemd {
        &self.systemd
    }
    fn proxy(&self) -> &Self::Proxy {
        &self.proxy
    }
    fn git(&self) -> &Self::Git {
        &self.git
    }
    fn resolver(&self) -> &Self::Resolver {
        &self.resolver
    }
    fn clock(&self) -> &Self::Clock {
        &self.clock
    }
    fn rng(&self) -> &Self::Rng {
        &self.rng
    }
    fn secrets(&self) -> &Self::Secrets {
        &self.secrets
    }
    fn queue(&self) -> &Self::Queue {
        &self.queue
    }
    fn hooks(&self) -> &Self::Hooks {
        &self.hooks
    }
    fn registry(&self) -> &Self::Registry {
        &self.registry
    }
    fn modules(&self) -> &Self::Modules {
        &self.modules
    }
    fn functions(&self) -> &Self::Functions {
        &self.functions
    }
    fn components(&self) -> &Self::Components {
        &self.components
    }
    fn blobs(&self) -> &Self::Blobs {
        &self.blobs
    }
}

/// Loads the master key. A bad key is a configuration problem (exit 78),
/// like any other secret file; the error never carries the key.
fn master_key(path: &Path) -> Result<MasterKey, AppError> {
    std::fs::read(path)
        .map_err(Error::internal)
        .and_then(|bytes| MasterKey::from_key_bytes(&bytes))
        .map_err(|source| AppError::MasterKey {
            path: path.to_path_buf(),
            source,
        })
}

fn setup(what: impl Into<String>) -> impl FnOnce(Error) -> AppError {
    let what = what.into();
    move |source| AppError::Setup { what, source }
}

/// Binds `DeployService::run` over `ports` as what the queue runs. The
/// queue lives inside the ports, so the runner holds them weakly: a deploy
/// that starts while the daemon is going away fails instead of keeping the
/// ports alive.
fn bind_runner(runner: &LateRunner, ports: &Arc<DaemonPorts>, settings: &Arc<Settings>) {
    let ports: Weak<DaemonPorts> = Arc::downgrade(ports);
    let settings = Arc::clone(settings);
    let bound = runner.bind(move |id: DeployId| -> RunFuture {
        let ports = ports.upgrade();
        let settings = Arc::clone(&settings);
        Box::pin(async move {
            let ports = ports.ok_or_else(|| Error::unavailable("the daemon is stopping"))?;
            DeployService::new(ports, settings).run(id).await.map(drop)
        })
    });
    if !bound {
        tracing::error!("the deploy runner was bound twice");
    }
}

/// Opens the store, builds the adapters and serves until `stop` is
/// cancelled; then the deploy queue gets `[deploy] wait_timeout` to finish
/// the running deploys.
///
/// # Errors
///
/// Anything that stops the daemon from starting or makes it fail:
/// configuration, secrets, the database, the listener.
#[expect(
    clippy::too_many_lines,
    reason = "the composition root wires every port and listener in one place"
)]
pub async fn serve(config: AppConfig, stop: CancellationToken) -> Result<(), AppError> {
    // Every secret and setting is checked before the database is created,
    // so a configuration error leaves nothing behind.
    let secrets = master_key(&config.paths.master_key)?;
    let settings = Arc::new(config.settings().map_err(AppError::Settings)?);
    let proxy = CaddyAdmin::new(config.proxy.admin).map_err(setup("the Caddy admin client"))?;
    let resolver = DnsResolver::public().map_err(setup("the DNS resolver"))?;

    let store = SqliteStore::open(&config.store.path).map_err(setup(format!(
        "the database at {}",
        config.store.path.display()
    )))?;
    let docker = ProcessDocker::new(config.paths.compose(), &config.docker.config_dir);
    let (modules, hooks, git, registry, core) = module_ports(&config, &store)?;
    let (functions, components) = function_ports(&config, &store)?;
    let artifacts = config.paths.data.join("artifacts");
    let blobs = FileBlobs::open(&store, &artifacts, LinkMode::Detect)
        .await
        .map_err(setup(format!(
            "the artifact files under {}",
            artifacts.display()
        )))?;
    let runner = LateRunner::default();
    let queue = JobQueue::new(
        Arc::new(store.clone()),
        Arc::new(SystemClock),
        runner.clone(),
        config.deploy.parallel,
    );
    let ports = Arc::new(DaemonPorts {
        store: store.clone(),
        docker: docker.clone(),
        files: FileProjectFiles::new(config.paths.compose()),
        systemd: SudoSystemd,
        proxy,
        git,
        resolver,
        clock: SystemClock,
        rng: OsRng,
        secrets,
        queue: queue.clone(),
        hooks,
        registry,
        modules,
        functions,
        components,
        blobs,
    });
    bind_runner(&runner, &ports, &settings);
    bind_core(&core, &ports, &settings);

    let rest = config.api.rest();
    let listener = TcpListener::bind(rest.bind)
        .await
        .map_err(|source| AppError::Bind {
            what: "REST",
            addr: rest.bind,
            source,
        })?;
    let gate = start_gate(config.gate.bind, &ports, &settings, &stop).await?;
    let functions = start_functions(&config.functions, &ports, &settings, &stop).await?;
    let (tunnel, tunnel_server) = start_tunnel(&config.tunnel, &ports, &settings, &stop).await?;
    let module_status = start_modules(&ports, &settings, &stop).await;
    reconcile_at_start(&ports, &settings).await;
    let reverify = tokio::spawn(reverify_daily(
        DomainService::new(Arc::clone(&ports), Arc::clone(&settings)),
        stop.child_token(),
    ));
    let sweeping = tokio::spawn(sweep_artifacts_every(
        ArtifactsService::new(Arc::clone(&ports), Arc::clone(&settings)),
        SWEEP_ARTIFACTS_EVERY,
        stop.child_token(),
    ));

    let state = AppState::new()
        .with_readiness(HostReadiness { docker, store })
        .with_tunnels(tunnel_server.clone());
    let services =
        Services::new(Arc::clone(&ports), settings).with_tunnels(tunnel_server.service().clone());
    let app = mcp_http::mount(
        app_router(state, services, &rest),
        McpAuthService::new(Arc::clone(&ports)),
        &McpMount {
            api_url: loopback_url(rest.bind),
            public_url: config.api.public_url.clone(),
        },
        &stop,
    );
    let served = serve_router_on(listener, app, stop.clone().cancelled_owned()).await;
    stop.cancel();
    let outcome = queue.shutdown(config.deploy.wait_timeout).await;
    tracing::info!(?outcome, "deploy queue stopped");
    joined(reverify.await, "the re-verification task");
    joined(module_status.await, "the module status task");
    joined(sweeping.await, "the artifact sweep task");
    gate_stopped(gate.await);
    functions_stopped(functions.await);
    tunnel_stopped(tunnel.await);
    served?;
    Ok(())
}

/// The URL the transport of `/mcp` calls the REST listener on `bind`
/// with: the address itself, or loopback for a listener on every
/// address.
fn loopback_url(bind: core::net::SocketAddr) -> String {
    let ip = match bind.ip() {
        core::net::IpAddr::V4(ip) if ip.is_unspecified() => {
            core::net::IpAddr::V4(core::net::Ipv4Addr::LOCALHOST)
        }
        core::net::IpAddr::V6(ip) if ip.is_unspecified() => {
            core::net::IpAddr::V6(core::net::Ipv6Addr::LOCALHOST)
        }
        ip => ip,
    };
    format!("http://{}", core::net::SocketAddr::new(ip, bind.port()))
}

/// Logs that the background task `what` failed, when it did.
fn joined(ended: Result<(), tokio::task::JoinError>, what: &str) {
    if let Err(error) = ended {
        tracing::error!(%error, task = what, "a background task failed");
    }
}

/// How often the expired gate codes, host sessions and one-time links are
/// deleted.
const PRUNE_GATE_EVERY: core::time::Duration = core::time::Duration::from_hours(1);

/// Deletes the expired gate codes and host sessions (v1 SSO-13) and the expired one-time links (OTL-01) at start and every [`PRUNE_GATE_EVERY`] until `stop` is
/// cancelled.
async fn prune_gate_hourly(
    gate: GateListener<DaemonPorts>,
    links: LinksService<DaemonPorts>,
    stop: CancellationToken,
) {
    let mut every = tokio::time::interval(PRUNE_GATE_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _instant = every.tick() => {}
        }
        match gate.prune().await {
            Ok((codes, sessions)) => {
                tracing::debug!(codes, sessions, "deleted expired gate records");
            }
            Err(error) => tracing::error!(%error, "deleting expired gate records failed"),
        }
        match links.prune().await {
            Ok(links) => tracing::debug!(links, "deleted expired one-time links"),
            Err(error) => tracing::error!(%error, "deleting expired one-time links failed"),
        }
    }
}

/// Logs how the gate listener's task ended, when it failed.
fn gate_stopped(ended: Result<Result<(), liftoff_rest::ServeError>, tokio::task::JoinError>) {
    match ended {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%error, "the gate listener failed"),
        Err(error) => tracing::error!(%error, "the gate listener task failed"),
    }
}

/// Binds the gate listener on `bind` and serves
/// it, deleting expired gate records hourly, until `stop` is cancelled.
async fn start_gate(
    bind: core::net::SocketAddr,
    ports: &Arc<DaemonPorts>,
    settings: &Arc<Settings>,
    stop: &CancellationToken,
) -> Result<tokio::task::JoinHandle<Result<(), liftoff_rest::ServeError>>, AppError> {
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| AppError::Bind {
            what: "gate",
            addr: bind,
            source,
        })?;
    let gate = GateListener::new(Arc::clone(ports), Arc::clone(settings));
    let links = LinksService::new(Arc::clone(ports), Arc::clone(settings));
    let pruning = tokio::spawn(prune_gate_hourly(gate.clone(), links, stop.child_token()));
    let served = serve_gate_on(listener, gate, stop.clone().cancelled_owned());
    Ok(tokio::spawn(async move {
        let served = served.await;
        if let Err(error) = pruning.await {
            tracing::error!(%error, "the gate pruning task failed");
        }
        served
    }))
}

/// Logs how the function listener's task ended, when it failed.
fn functions_stopped(ended: Result<std::io::Result<()>, tokio::task::JoinError>) {
    match ended {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%error, "the function listener failed"),
        Err(error) => tracing::error!(%error, "the function listener task failed"),
    }
}

/// Binds the function listener on `bind` and serves the function environments' requests from the
/// daemon's runtime until `stop` is cancelled. The current components
/// are activated again by the start-up reconcile; until then a request
/// gets `503`. A request body must arrive, and a request find its turn,
/// within `functions.timeout`.
async fn start_functions(
    config: &FunctionsConfig,
    ports: &Arc<DaemonPorts>,
    settings: &Arc<Settings>,
    stop: &CancellationToken,
) -> Result<tokio::task::JoinHandle<std::io::Result<()>>, AppError> {
    let bind = config.bind;
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| AppError::Bind {
            what: "functions",
            addr: bind,
            source,
        })?;
    let functions = FunctionListener::new(
        Arc::clone(ports),
        Arc::clone(settings),
        Arc::clone(ports.functions.runtime()),
    )
    .with_limits(ListenerLimits {
        body_timeout: config.timeout,
        ..ListenerLimits::default()
    });
    Ok(tokio::spawn(
        functions.serve(listener, stop.clone().cancelled_owned()),
    ))
}

/// The daemon's tunnel server: `[tunnel] max_streams` per
/// connection, `[tunnel] bandwidth` per environment, the idle timeout of
/// three keepalive intervals.
fn tunnel_server(config: &TunnelConfig) -> TunnelServer {
    TunnelServer::new(ServerConfig {
        max_streams: config.max_streams,
        bandwidth: Some(Bandwidth {
            bytes_per_second: config.bandwidth.bytes_per_second,
            burst: config.bandwidth.burst,
        }),
        ..ServerConfig::default()
    })
}

/// Logs how the tunnel listener's task ended, when it failed.
fn tunnel_stopped(ended: Result<std::io::Result<()>, tokio::task::JoinError>) {
    match ended {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%error, "the tunnel listener failed"),
        Err(error) => tracing::error!(%error, "the tunnel listener task failed"),
    }
}

/// Binds the tunnel listener on `[tunnel] bind` and serves the dev addresses through the
/// daemon's tunnel server until `stop` is cancelled; returns its task
/// and what `GET /api/v1/tunnel` hands its connections to.
async fn start_tunnel(
    config: &TunnelConfig,
    ports: &Arc<DaemonPorts>,
    settings: &Arc<Settings>,
    stop: &CancellationToken,
) -> Result<
    (
        tokio::task::JoinHandle<std::io::Result<()>>,
        DaemonTunnels<DaemonPorts>,
    ),
    AppError,
> {
    let server = tunnel_server(config);
    let bind = config.bind;
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|source| AppError::Bind {
            what: "tunnel",
            addr: bind,
            source,
        })?;
    let tunnel = TunnelListener::new(Arc::clone(ports), Arc::clone(settings), server.clone());
    let handoff = DaemonTunnels::new(
        server,
        TunnelsService::new(Arc::clone(ports), Arc::clone(settings)),
    );
    Ok((
        tokio::spawn(tunnel.serve(listener, stop.clone().cancelled_owned())),
        handoff,
    ))
}

/// the installed modules are loaded again before anything
/// asks their hooks (one that fails to load is disabled with its error and
/// the daemon runs without it); then the task that writes what the host
/// disabled into `modules` starts.
async fn start_modules(
    ports: &Arc<DaemonPorts>,
    settings: &Arc<Settings>,
    stop: &CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let modules = ModuleService::new(Arc::clone(ports), Arc::clone(settings));
    if let Err(error) = modules.restore().await {
        tracing::warn!(?error, "restoring the installed modules failed");
    }
    tokio::spawn(sync_status_every(
        modules,
        SYNC_STATUS_EVERY,
        stop.child_token(),
    ))
}

/// What [`module_ports`] builds; the [`LateCore`] is bound by
/// [`bind_core`] once the ports exist.
type ModulePorts = (
    DaemonRuntime,
    DaemonHooks,
    DaemonGit,
    HttpModuleSource,
    LateCore,
);

/// The module runtime of the daemon.
pub type DaemonRuntime = DaemonModules<SqliteStore, TokioTimer, TracingLog, LateCore>;

/// The module runtime over the wasmtime host (compile cache in
/// `paths.data/cache`, components in `paths.data/modules`), the hook
/// dispatcher and the `Git` port over the modules it loads, and the
/// registry client.
fn module_ports(config: &AppConfig, store: &SqliteStore) -> Result<ModulePorts, AppError> {
    let limits = config.modules.limits().map_err(AppError::Settings)?;
    let module_host = |error| AppError::Setup {
        what: "the module host".to_owned(),
        source: Error::internal(error),
    };
    let core = LateCore::default();
    let host = ModuleHost::new(store.clone(), TokioTimer, TracingLog, limits)
        .and_then(|host| host.with_compile_cache(config.paths.data.join("cache")))
        .and_then(|host| host.with_core_calls(core.clone()))
        .map_err(module_host)?;
    let modules = DaemonModules::new(host, config.paths.data.join("modules"));
    let hooks = ModuleHooks::new(Arc::clone(modules.host().registry()), modules.set().clone());
    let git = ModuleGit::new(Arc::clone(modules.host().registry()), modules.set().clone());
    let registry = HttpModuleSource::new(config.modules.registry.clone())
        .map_err(setup("the module registry client"))?;
    Ok((modules, hooks, git, registry, core))
}

/// Before serving: deploys a restart cut short fail and their
/// environments go back to the current deploy; Caddy's configuration
/// lives in memory only, so the whole of it is pushed. A failure is
/// logged and `/readyz` says so.
async fn reconcile_at_start(ports: &Arc<DaemonPorts>, settings: &Arc<Settings>) {
    if let Err(error) = Reconciler::new(Arc::clone(ports), Arc::clone(settings))
        .startup()
        .await
    {
        tracing::warn!(%error, "start-up reconcile failed");
    }
}

/// Where the config file is when none was named: the standard path if it
/// exists, nothing otherwise (defaults and env only).
#[must_use]
pub fn default_config_file() -> Option<PathBuf> {
    let path = PathBuf::from(crate::config::DEFAULT_CONFIG_FILE);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Answers as told and notes that it was asked.
    #[derive(Debug, Clone)]
    struct Fake {
        name: &'static str,
        fails: bool,
        asked: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Probe for Fake {
        fn probe(&self) -> impl Future<Output = Result<(), Error>> + Send {
            self.asked.lock().unwrap().push(self.name);
            core::future::ready(if self.fails {
                Err(Error::unavailable(self.name))
            } else {
                Ok(())
            })
        }
    }

    /// `(name, failure)` of each dependency a readiness check over two
    /// fakes reports, and who was asked.
    async fn check(
        docker_fails: bool,
        store_fails: bool,
    ) -> (Vec<(&'static str, Option<String>)>, Vec<&'static str>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let fake = |name, fails| Fake {
            name,
            fails,
            asked: Arc::clone(&asked),
        };
        let readiness = HostReadiness {
            docker: fake("docker", docker_fails),
            store: fake("store", store_fails),
        };
        let verdict = readiness
            .check()
            .await
            .into_iter()
            .map(|probed| (probed.name, probed.result.err().map(|e| e.to_string())))
            .collect();
        let order = asked.lock().unwrap().clone();
        (verdict, order)
    }

    mod host_readiness {
        use super::*;

        #[tokio::test]
        async fn is_ready_when_docker_and_store_answer() {
            let (verdict, order) = check(false, false).await;
            assert_eq!(
                (verdict, order),
                (
                    vec![("docker", None), ("sqlite", None)],
                    vec!["docker", "store"]
                )
            );
        }

        // TC: CLIX-09 (docker and SQLite apart)
        #[tokio::test]
        async fn still_asks_the_store_when_docker_is_down() {
            let (verdict, order) = check(true, false).await;
            assert_eq!(
                (verdict, order),
                (
                    vec![("docker", Some("docker".to_owned())), ("sqlite", None)],
                    vec!["docker", "store"]
                )
            );
        }

        #[tokio::test]
        async fn fails_only_the_store_when_the_store_is_down() {
            let (verdict, order) = check(false, true).await;
            assert_eq!(
                (verdict, order),
                (
                    vec![("docker", None), ("sqlite", Some("store".to_owned()))],
                    vec!["docker", "store"]
                )
            );
        }
    }

    mod serve {
        use super::*;

        /// A config in a fresh directory, listening on a free port.
        fn config_in(dir: &Path) -> AppConfig {
            let mut config = AppConfig::default();
            config.paths.data = dir.to_path_buf();
            config.paths.master_key = dir.join("master.key");
            config.store.path = dir.join("liftoff.db");
            config.docker.config_dir = dir.join("docker");
            config.api.bind = "127.0.0.1:0".parse().unwrap();
            config.gate.bind = "127.0.0.1:0".parse().unwrap();
            // Free ports for every listener: a daemon of the host may hold
            // the defaults.
            config.functions.bind = "127.0.0.1:0".parse().unwrap();
            config.tunnel.bind = "127.0.0.1:0".parse().unwrap();
            config.api.public_url = "https://liftoff.test".to_owned();
            config.domains.base = "apps.test".to_owned();
            config.domains.public_ip = Some(core::net::Ipv4Addr::new(203, 0, 113, 10));
            config.proxy.acme_email = Some("ops@example.com".to_owned());
            // Never the host's Caddy: nothing listens on the discard port.
            config.proxy.admin = "127.0.0.1:9".parse().unwrap();
            config
        }

        #[tokio::test]
        async fn leaves_no_database_behind_when_the_master_key_is_bad() {
            let dir = tempfile::tempdir().unwrap();
            let config = config_in(dir.path());
            std::fs::write(&config.paths.master_key, "not a key").unwrap();
            let error = serve(config, CancellationToken::new()).await.unwrap_err();
            assert_eq!(error.exit_code(), 78, "{error}");
            assert!(!dir.path().join("liftoff.db").exists());
        }

        // TC: SETUP-05 (a missing setting is exit 78)
        #[tokio::test]
        async fn exits_78_without_a_database_when_the_base_domain_is_missing() {
            let dir = tempfile::tempdir().unwrap();
            let mut config = config_in(dir.path());
            std::fs::write(&config.paths.master_key, "00".repeat(32)).unwrap();
            config.domains.base = String::new();
            let error = serve(config, CancellationToken::new()).await.unwrap_err();
            assert_eq!(
                (error.exit_code(), dir.path().join("liftoff.db").exists()),
                (78, false),
                "{error}"
            );
        }

        #[tokio::test]
        async fn exits_70_naming_the_address_when_the_rest_address_is_taken() {
            let dir = tempfile::tempdir().unwrap();
            let mut config = config_in(dir.path());
            std::fs::write(&config.paths.master_key, "00".repeat(32)).unwrap();
            let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = taken.local_addr().unwrap();
            config.api.bind = addr;
            let error = serve(config, CancellationToken::new()).await.unwrap_err();
            assert_eq!(error.exit_code(), 70, "{error}");
            let text = error.to_string();
            assert!(
                text.contains("REST") && text.contains(&addr.to_string()),
                "{text}"
            );
        }

        #[tokio::test]
        async fn stops_cleanly_when_cancelled() {
            let dir = tempfile::tempdir().unwrap();
            let config = config_in(dir.path());
            std::fs::write(&config.paths.master_key, "00".repeat(32)).unwrap();
            let stop = CancellationToken::new();
            stop.cancel();
            assert!(
                serve(config, stop).await.is_ok(),
                "a cancelled daemon exits 0"
            );
            assert!(
                dir.path().join("liftoff.db").exists(),
                "the store was opened"
            );
        }
    }

    mod master_key {

        #[test]
        fn exits_78_without_the_content_when_the_key_is_malformed() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("master.key");
            std::fs::write(&path, "TOPSECRET not a key").unwrap();
            let error = super::super::master_key(&path).unwrap_err();
            assert_eq!(error.exit_code(), 78);
            assert!(!format!("{error} {error:?}").contains("TOPSECRET"));
        }

        #[test]
        fn exits_78_when_the_key_file_is_missing() {
            let dir = tempfile::tempdir().unwrap();
            let error = super::super::master_key(&dir.path().join("nope")).unwrap_err();
            assert_eq!(error.exit_code(), 78);
            assert!(error.to_string().contains("nope"), "{error}");
        }
    }
}
