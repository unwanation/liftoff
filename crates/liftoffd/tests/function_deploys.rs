#![cfg(test)]
//! A function deploy over the daemon's own adapters (`FDEP-03`): the `SQLite` store, the wasmtime
//! runtime and the kept files under `paths.data/functions/`, as
//! [`liftoffd::functions::function_ports`] wires them; every other port
//! is the shared fake. The component is the contract function `fn_rust`
//! (`just functions`) from `WASM_MODULES_DIR`. The function listener
//! serves it from the same runtime, as the
//! daemon's start wires it.

use std::sync::Arc;

use liftoff_core::domain::{ComponentHash, DeployStatus, EnvName, ProjectName};
use liftoff_core::fakes::{
    FakeBlobs, FakeGit, FakeHooks, FakeModuleRuntime, FakeModuleSource, FakePorts, FakeResolver,
    FakeSecrets, FixedClock, RecordingDocker, RecordingFiles, RecordingProxy, RecordingQueue,
    RecordingSystemd, Records, SeqRng, World,
};
use liftoff_core::ports::{Ports, Store as _};
use liftoff_core::service::DeployService;
use liftoff_host::FileComponentFiles;
use liftoff_store::SqliteStore;
use liftoff_wasm::testing::component;
use liftoffd::config::AppConfig;
use liftoffd::functions::{DaemonFunctions, function_ports};

/// The fakes with the daemon's store, function runtime and kept files.
struct FunctionPorts {
    fakes: Arc<FakePorts>,
    store: SqliteStore,
    functions: DaemonFunctions,
    components: FileComponentFiles,
}

impl Ports for FunctionPorts {
    type Store = SqliteStore;
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
    type Hooks = FakeHooks;
    type Registry = FakeModuleSource;
    type Modules = FakeModuleRuntime;
    type Functions = DaemonFunctions;
    type Components = FileComponentFiles;
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
        &self.fakes.hooks
    }
    fn registry(&self) -> &Self::Registry {
        &self.fakes.registry
    }
    fn modules(&self) -> &Self::Modules {
        &self.fakes.modules
    }
    fn functions(&self) -> &Self::Functions {
        &self.functions
    }
    fn components(&self) -> &Self::Components {
        &self.components
    }
    fn blobs(&self) -> &Self::Blobs {
        &self.fakes.blobs
    }
}

/// The world's owner and `shop` in a fresh `SQLite` store with the
/// function environment `api`, the daemon's runtime and files over
/// `data`, and the deploy service over them.
async fn daemon(world: &World, data: &std::path::Path) -> DeployService<FunctionPorts> {
    DeployService::new(daemon_ports(world, data).await, Arc::clone(&world.settings))
}

/// The ports of [`daemon`].
async fn daemon_ports(world: &World, data: &std::path::Path) -> Arc<FunctionPorts> {
    let store = SqliteStore::open(data.join("liftoff.db")).unwrap();
    store.insert_user(&world.owner).await.unwrap();
    store.insert_project(&world.shop).await.unwrap();
    let mut api = world.records.function(&world.shop, "api").unwrap();
    api.pin = Some(Records::pin("main", &world.owner).unwrap());
    store.insert_environment(&api).await.unwrap();
    let mut config = AppConfig::default();
    config.paths.data = data.to_path_buf();
    let (functions, components) = function_ports(&config, &store).unwrap();
    let ports = FunctionPorts {
        fakes: Arc::clone(&world.ports),
        store,
        functions,
        components,
    };
    Arc::new(ports)
}

mod deploy {
    use super::*;

    // TC: FDEP-03 (the file by its hash under `paths.data/functions/`,
    // the deploy `succeeded`)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_file_by_hash_under_data_functions_and_succeeds_when_run() {
        let world = World::new().await;
        let data = tempfile::tempdir().unwrap();
        let deploys = daemon(&world, data.path()).await;
        let bytes = component("fn_rust").unwrap();
        let queued = deploys
            .request_function(
                &world.owner,
                &ProjectName::new("shop").unwrap(),
                &EnvName::new("api").unwrap(),
                bytes.clone(),
            )
            .await
            .unwrap();
        let done = deploys.run(queued.deploy_id).await.unwrap();
        let kept = data
            .path()
            .join("functions")
            .join(format!("{}.wasm", ComponentHash::of(&bytes)));
        assert_eq!(
            (
                done.status,
                done.error,
                std::fs::read(&kept).ok() == Some(bytes)
            ),
            (DeployStatus::Succeeded, None, true),
            "{}",
            done.log
        );
    }
}

mod listener {
    use super::*;

    use bytes::Bytes;
    use http::{Request, StatusCode};
    use http_body_util::{BodyExt as _, Full};
    use hyper_util::rt::TokioIo;
    use liftoff_core::domain::{EnvState, Host};
    use liftoffd::listeners::functions::FunctionListener;
    use tokio::net::{TcpListener, TcpStream};

    // TC: FN-11, FN-13 (the deployed component answers through the
    // listener over the daemon's runtime, as `start_functions` wires it)
    #[tokio::test(flavor = "multi_thread")]
    async fn deployed_function_answers_when_asked_through_listener() {
        let world = World::new().await;
        let data = tempfile::tempdir().unwrap();
        let ports = daemon_ports(&world, data.path()).await;
        let deploys = DeployService::new(Arc::clone(&ports), Arc::clone(&world.settings));
        let queued = deploys
            .request_function(
                &world.owner,
                &ProjectName::new("shop").unwrap(),
                &EnvName::new("api").unwrap(),
                component("fn_rust").unwrap(),
            )
            .await
            .unwrap();
        let done = deploys.run(queued.deploy_id).await.unwrap();
        assert_eq!(done.status, DeployStatus::Succeeded, "{}", done.log);
        let api = ports
            .store
            .environment(done.environment_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(api.state, EnvState::Running);
        let domain = world.records.domain(&world.shop, "api.apps.test").unwrap();
        ports.store.insert_domain(&domain).await.unwrap();
        let binding = world
            .records
            .binding(&api, &domain, "web", 80, 20_100)
            .unwrap();
        ports.store.insert_binding(&binding).await.unwrap();
        assert!(
            ports
                .store
                .domain_by_host(&Host::new("api.apps.test").unwrap())
                .await
                .unwrap()
                .is_some()
        );

        let bound = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = bound.local_addr().unwrap();
        let served = FunctionListener::new(
            Arc::clone(&ports),
            Arc::clone(&world.settings),
            Arc::clone(ports.functions.runtime()),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(served.serve(bound, async {
            let _either = stopped.await;
        }));
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let response = sender
            .send_request(
                Request::builder()
                    .uri("/?x=1")
                    .header("host", "api.apps.test")
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        drop(stop);
        let body = String::from_utf8_lossy(&body).into_owned();
        assert_eq!(
            (status, body.contains(r#""path":"/?x=1""#)),
            (StatusCode::OK, true),
            "{body}"
        );
    }
}
