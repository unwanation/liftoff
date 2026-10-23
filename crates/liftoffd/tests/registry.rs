#![cfg(test)]
//! Installing modules end to end (REG-01, REG-05, REG-06, REG-07): the core's `ModuleService` over the SQLite store in a
//! file, the daemon's module runtime (`liftoffd::modules::DaemonModules`
//! over the wasmtime host and `<data>/modules/`), and the fake registry
//! serving the built `probe` component (`modules/testing/probe`) signed
//! with its test key, which the world's settings pin. It lives with the
//! composition root because it joins two adapters (`liftoff-wasm` and
//! `liftoff-store`).
//!
//! `mod rest` drives the same install through the REST router with the
//! daemon's hooks (`ModuleHooks` over the runtime's modules), as `lof
//! module install` and `lof probe hello` do (the acceptance criterion).
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); the test reads it from `WASM_MODULES_DIR`, which the just recipes
//! set, else from `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use liftoff_core::Error;
use liftoff_core::System;
use liftoff_core::domain::ModuleName;
use liftoff_core::fakes::{FakeRelease, PROBE_MANIFEST, WithModules, World};
use liftoff_core::module::{ModuleCalls as _, ModuleHooks};
use liftoff_core::ports::{ModuleRequest, ModuleRuntime as _, ModuleStore as _, Store as _};
use liftoff_core::service::{InstallRequest, ModuleService};
use liftoff_rest::{AppState, RestConfig, Services, app_router};
use liftoff_store::SqliteStore;
use liftoff_wasm::{Limits, ModuleHost, ModuleSet, TokioTimer, TracingLog};
use liftoffd::modules::DaemonModules;
use serde_json::{Map, Value, json};

type Runtime = DaemonModules<SqliteStore, TokioTimer, TracingLog>;
type DaemonHooks = ModuleHooks<ModuleSet<SqliteStore, TracingLog>>;
type Ports = WithModules<SqliteStore, Runtime>;

fn component(name: &str) -> Vec<u8> {
    let dir = std::env::var_os("WASM_MODULES_DIR").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../modules/target/wasm32-wasip2/release")
        },
        PathBuf::from,
    );
    let path = dir.join(format!("{name}.wasm"));
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; build the modules first with `just modules`",
            path.display()
        )
    })
}

/// The daemon of a test: its data directory, its database file, the
/// world whose fakes and settings it runs on, and the service.
struct Daemon {
    data: tempfile::TempDir,
    world: World,
    ports: Arc<Ports>,
    store: SqliteStore,
}

impl Daemon {
    fn modules(&self) -> ModuleService<Ports> {
        ModuleService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
    }

    fn database(&self) -> PathBuf {
        self.data.path().join("liftoff.db")
    }

    /// `<data>/modules/<name>-<version>.wasm`.
    fn file(&self, name: &str, version: &str) -> PathBuf {
        self.data
            .path()
            .join("modules")
            .join(format!("{name}-{version}.wasm"))
    }

    async fn install(&self, config: Map<String, Value>) -> Result<(), Error> {
        self.modules()
            .install(
                &self.world.owner,
                InstallRequest {
                    name: probe(),
                    version: None,
                    config,
                },
            )
            .await
            .map(drop)
    }
}

fn probe() -> ModuleName {
    ModuleName::new("probe").unwrap()
}

fn greeting() -> Map<String, Value> {
    std::iter::once(("greeting".to_owned(), json!("hi"))).collect()
}

/// The world, its data directory, its SQLite store and the daemon's
/// module runtime in `<data>/modules`, the registry serving the built
/// `probe` as `release` makes it.
async fn parts(
    release: impl FnOnce(FakeRelease) -> FakeRelease,
) -> (World, tempfile::TempDir, SqliteStore, Runtime) {
    let world = World::new().await;
    let data = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(data.path().join("liftoff.db")).unwrap();
    let limits = Limits {
        max_memory_bytes: 64 * 1024 * 1024,
        call_timeout: Duration::from_secs(10),
    };
    let host = ModuleHost::new(store.clone(), TokioTimer, TracingLog, limits).unwrap();
    let runtime = DaemonModules::new(host, data.path().join("modules"));
    let built = FakeRelease::new(PROBE_MANIFEST, component("probe")).unwrap();
    world.ports.registry.publish(release(built)).unwrap();
    (world, data, store, runtime)
}

/// A daemon whose registry serves the built `probe` as `release` makes it.
async fn daemon_with(release: impl FnOnce(FakeRelease) -> FakeRelease) -> Daemon {
    let (world, data, store, runtime) = parts(release).await;
    let ports = Arc::new(WithModules::new(
        Arc::clone(&world.ports),
        store.clone(),
        runtime,
    ));
    Daemon {
        data,
        world,
        ports,
        store,
    }
}

/// A daemon whose registry serves the built `probe` as built and signed.
async fn daemon() -> Daemon {
    daemon_with(|release| release).await
}

fn exists(path: &Path) -> bool {
    path.try_exists().unwrap()
}

mod install {
    use super::*;

    // TC: REG-01 (the refused file is not kept)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_no_file_when_signature_is_by_another_key() {
        let daemon = daemon_with(FakeRelease::signed_by_another_key).await;
        let refused = daemon.install(greeting()).await;
        assert_eq!(
            (
                matches!(
                    &refused,
                    Err(Error::Upstream {
                        system: System::Registry,
                        ..
                    })
                ),
                exists(&daemon.file("probe", "0.1.0"))
            ),
            (true, false),
            "{refused:?}"
        );
    }

    // TC: REG-06 (the component is kept as `<name>-<version>.wasm`)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_component_in_modules_dir_when_installed() {
        let daemon = daemon().await;
        daemon.install(greeting()).await.unwrap();
        assert_eq!(
            std::fs::read(daemon.file("probe", "0.1.0")).ok(),
            Some(component("probe"))
        );
    }

    // TC: REG-06 (`lof module install probe --set
    // greeting=hi` adds `lof probe hello`, which works, without a restart)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_command_route_with_configured_greeting_right_after_install() {
        let daemon = daemon().await;
        daemon.install(greeting()).await.unwrap();
        let answered = daemon
            .ports
            .modules
            .set()
            .handle_route(
                &probe(),
                Some(&daemon.world.owner),
                ModuleRequest {
                    method: "POST".to_owned(),
                    path: "/hello".to_owned(),
                    headers: Vec::new(),
                    body: br#"{"who":"world"}"#.to_vec(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (answered.status, String::from_utf8_lossy(&answered.body)),
            (200, "hi, world".into())
        );
    }

    // TC: REG-06 (of two installs at once one wins, and
    // its file stays)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_file_of_winner_when_two_installs_race() {
        let daemon = daemon().await;
        let (first, second) = tokio::join!(daemon.install(greeting()), daemon.install(greeting()));
        assert_eq!(
            (
                first.is_ok() != second.is_ok(),
                exists(&daemon.file("probe", "0.1.0"))
            ),
            (true, true),
            "{first:?} {second:?}"
        );
    }

    // TC: REG-01 (the directory of kept components is the daemon's own, like the compile cache)
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_components_in_private_dir_when_installed() {
        use std::os::unix::fs::PermissionsExt as _;

        let daemon = daemon().await;
        daemon.install(greeting()).await.unwrap();
        let mode = std::fs::metadata(daemon.data.path().join("modules"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }

    // TC: REG-06 (the command is registered at once)
    #[tokio::test(flavor = "multi_thread")]
    async fn registers_hello_command_right_after_install() {
        let daemon = daemon().await;
        daemon.install(greeting()).await.unwrap();
        assert_eq!(
            daemon
                .ports
                .modules
                .registry()
                .commands()
                .into_iter()
                .map(|(module, command)| (module.to_string(), command.name))
                .collect::<Vec<_>>(),
            vec![("probe".to_owned(), "hello".to_owned())]
        );
    }

    // TC: REG-05 (the secret field is sealed in the `modules.config`
    // column itself)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_secret_out_of_modules_config_column_when_installed() {
        let daemon = daemon().await;
        let mut config = greeting();
        let _none = config.insert("token".to_owned(), json!("s3cret"));
        daemon.install(config).await.unwrap();
        let connection = rusqlite::Connection::open(daemon.database()).unwrap();
        let column: String = connection
            .query_row(
                "SELECT config FROM modules WHERE name = 'probe'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            (column.contains("greeting"), column.contains("s3cret")),
            (true, false),
            "{column}"
        );
    }
}

mod remove {
    use super::*;

    /// How many rows `m_probe_items` has, as the probe's own query sees
    /// it.
    async fn items(daemon: &Daemon) -> Result<String, Error> {
        daemon
            .store
            .module_query(&probe(), "SELECT count(*) AS n FROM m_probe_items", &[])
            .await
    }

    // TC: REG-07 (the table the migration made stays)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_items_table_when_removed_without_purge() {
        let daemon = daemon().await;
        daemon.install(greeting()).await.unwrap();
        daemon
            .modules()
            .remove(&daemon.world.owner, &probe(), false)
            .await
            .unwrap();
        assert_eq!(
            (
                items(&daemon).await.ok(),
                exists(&daemon.file("probe", "0.1.0"))
            ),
            (Some(r#"[{"n":0}]"#.to_owned()), false)
        );
    }

    // TC: REG-07
    #[tokio::test(flavor = "multi_thread")]
    async fn drops_items_table_when_removed_with_purge() {
        let daemon = daemon().await;
        daemon.install(greeting()).await.unwrap();
        daemon
            .modules()
            .remove(&daemon.world.owner, &probe(), true)
            .await
            .unwrap();
        let read = items(&daemon).await;
        assert!(read.is_err(), "{read:?}");
    }
}

mod rest {
    use super::*;

    /// The API over the daemon's wiring: the SQLite store, the module
    /// runtime and `ModuleHooks` over the modules it loads; the owner and
    /// a session of theirs in that store. The data directory (kept alive),
    /// the router and the owner's bearer.
    async fn app() -> (tempfile::TempDir, axum::Router, String) {
        let (world, data, store, runtime) = parts(|release| release).await;
        store.insert_user(&world.owner).await.unwrap();
        let (session, secret) = world.records.session(&world.owner);
        store.insert_session(&session).await.unwrap();
        let hooks: DaemonHooks =
            ModuleHooks::new(Arc::clone(runtime.host().registry()), runtime.set().clone());
        let ports =
            Arc::new(WithModules::new(Arc::clone(&world.ports), store, runtime).with_hooks(hooks));
        let services = Services::new(ports, Arc::clone(&world.settings));
        let app = app_router(AppState::new(), services, &RestConfig::default());
        (data, app, secret.expose().to_owned())
    }

    /// `POST path` with `body` as the owner: the status and the body.
    async fn post(
        app: &axum::Router,
        bearer: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, String) {
        use tower::ServiceExt as _;

        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    // TC: REG-06 (`lof module install probe --set
    // greeting=hi` adds `lof probe hello`, which works, without a restart;
    // through REST, the core's route service and the daemon's hooks)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_module_route_with_configured_greeting_right_after_install_over_rest() {
        let (_data, app, bearer) = app().await;
        let (installed, body) = post(
            &app,
            &bearer,
            "/api/v1/modules",
            r#"{"name":"probe","config":{"greeting":"hi"}}"#,
        )
        .await;
        assert!(installed.is_success(), "{installed} {body}");
        assert_eq!(
            post(&app, &bearer, "/api/v1/m/probe/hello", r#"{"who":"world"}"#).await,
            (StatusCode::OK, "hi, world".to_owned())
        );
    }
}
