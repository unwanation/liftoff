#![cfg(test)]
//! The module `web` end to end (WEB-01…05; the `web` row, 5.2): the built `web` component (`modules/web`)
//! installed and removed by the core's `ModuleService` from the fake
//! registry over the SQLite store in a file and the daemon's module
//! runtime (`DaemonModules` over the wasmtime host, as `tests/mcp_http.rs`),
//! the core's `ModuleHooks` over the modules it loads, and the REST router
//! over those ports, driven in memory. The router is built once, before
//! any module is installed: installing and removing `web` takes effect
//! without a restart.
//!
//! The panel `liftoff-web` is at [`PANEL`]; a browser is the test, sending
//! `Origin` as browsers do. The browser session is the one a one-time
//! link opens (`GET /api/v1/auth/links/{token}`), to the private function
//! `api` of `shop` at [`HOST`]. It lives with the composition root
//! because it joins three adapters (`liftoff-wasm`, `liftoff-store`,
//! `liftoff-rest`).
//!
//! The component is built by `just modules` (part of `just test` and
//! `just ci`); the test reads it from `WASM_MODULES_DIR`, which the just
//! recipes set, else from `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::TokioIo;
use liftoff_core::Error;
use liftoff_core::domain::{
    EnvState, Environment, EnvironmentId, LinkTarget, ModuleName, UserRole, Visibility,
};
use liftoff_core::fakes::{FakeRelease, WithModules, World};
use liftoff_core::module::ModuleHooks;
use liftoff_core::ports::{Ports as _, Store as _};
use liftoff_core::service::{InstallRequest, LinksService, ModuleService};
use liftoff_rest::{AppState, RestConfig, Services, app_router, pages};
use liftoff_store::SqliteStore;
use liftoff_tunnel::{ServerConfig, TunnelServer};
use liftoff_wasm::functions::FunctionError;
use liftoff_wasm::testing::component;
use liftoff_wasm::{Limits, ModuleHost, ModuleSet, TokioTimer, TracingLog};
use liftoffd::listeners::functions::{FunctionListener, Functions};
use liftoffd::listeners::tunnel::TunnelListener;
use liftoffd::modules::DaemonModules;
use serde_json::{Map, Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tower::ServiceExt as _;

type Runtime = DaemonModules<SqliteStore, TokioTimer, TracingLog>;
type DaemonHooks = ModuleHooks<ModuleSet<SqliteStore, TracingLog>>;
type Ports = WithModules<SqliteStore, Runtime, DaemonHooks>;

/// The origin of the panel `liftoff-web`, the module's `origin`.
const PANEL: &str = "https://panel.test";

/// An origin that is not the panel's.
const FOREIGN: &str = "https://evil.test";

/// The address of the private function `api` of `shop`.
const HOST: &str = "api.apps.test";

/// The address of the function `idle` of `shop`, which does not run.
const IDLE_HOST: &str = "idle.apps.test";

/// The address of the tunnel `dev` of `shop`, whose `lof dev` is not
/// connected.
const TUNNEL_HOST: &str = "dev.apps.test";

/// The address of the protected environment `admin` of `shop`.
const PROTECTED_HOST: &str = "admin.apps.test";

/// Where every link of the world points (its API host is `liftoff.test`).
const LINK_ORIGIN: &str = "https://liftoff.test";

/// The cookie of the session on an application's host.
const HOST_COOKIE: &str = "__Host-liftoff_gate";

/// The cookie of the `browser` session on the API host.
const API_COOKIE: &str = "__Host-liftoff_session";

// ---- the daemon -------------------------------------------------------------

fn name(module: &str) -> ModuleName {
    ModuleName::new(module).unwrap()
}

fn config(pairs: Value) -> Map<String, Value> {
    match pairs {
        Value::Object(map) => map,
        other => panic!("a configuration is an object, not {other}"),
    }
}

/// The registry's release of the built `module`, as
/// `modules/registry/release.sh` writes its index entry.
async fn release(runtime: &Runtime, module: &str) -> FakeRelease {
    let bytes = component(module).unwrap();
    let manifest = runtime.host().inspect(name(module), &bytes).await.unwrap();
    let index = json!({
        "name": manifest.name.as_str(),
        "version": manifest.version,
        "requires": manifest.requires.iter().map(ModuleName::as_str).collect::<Vec<_>>(),
        "config_schema": manifest.config_schema,
    });
    FakeRelease::new(&index.to_string(), bytes).unwrap()
}

/// The daemon of a test.
struct Daemon {
    _data: tempfile::TempDir,
    world: World,
    ports: Arc<Ports>,
    app: Router,
    /// `owner`'s `cli` session in the SQLite store.
    owner_token: String,
    /// The private function `api` of `shop` at [`HOST`].
    api: Environment,
}

impl Daemon {
    fn modules(&self) -> ModuleService<Ports> {
        ModuleService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
    }

    /// Installs `web` as `owner` for [`PANEL`], taking the pages over
    /// when `take_pages`.
    async fn install_web(&self, take_pages: bool) -> Result<(), Error> {
        self.modules()
            .install(
                &self.world.owner,
                InstallRequest {
                    name: name("web"),
                    version: None,
                    config: config(json!({"origin": PANEL, "take_pages": take_pages})),
                },
            )
            .await
            .map(drop)
    }

    /// Installs `web` for [`PANEL`] with its default configuration (no
    /// `take_pages`).
    async fn install_web_default(&self) {
        self.modules()
            .install(
                &self.world.owner,
                InstallRequest {
                    name: name("web"),
                    version: None,
                    config: config(json!({"origin": PANEL})),
                },
            )
            .await
            .unwrap();
    }

    /// Installs `oidc` as `owner` for a provider it never reaches here
    /// (its login page is HTML whatever the provider).
    async fn install_oidc(&self) {
        self.modules()
            .install(
                &self.world.owner,
                InstallRequest {
                    name: name("oidc"),
                    version: None,
                    config: config(
                        json!({"issuer": "https://id.example.test", "client_id": "liftoff"}),
                    ),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("install oidc: {error:?}"));
    }

    /// `bob`, who may open none of `shop`'s addresses, signed in on
    /// `host`: the cookie of his session there.
    async fn stranger_on(&self, host: &str) -> String {
        let bob = self.world.records.user("bob", UserRole::User).unwrap();
        self.ports.store().insert_user(&bob).await.unwrap();
        let (session, _api_cookie) = self.world.records.browser_session(&bob);
        let (gate, secret) = self.world.records.host_session(&session, host).unwrap();
        self.ports.store().insert_session(&session).await.unwrap();
        self.ports.store().insert_gate_session(&gate).await.unwrap();
        format!("{HOST_COOKIE}={}", secret.expose())
    }

    /// The function and tunnel listeners over the daemon's ports, on
    /// ephemeral loopback ports.
    async fn listeners(&self) -> Listeners {
        let functions = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let functions_addr = functions.local_addr().unwrap();
        let (stop_functions, functions_stopped) = oneshot::channel::<()>();
        tokio::spawn(
            FunctionListener::new(
                Arc::clone(&self.ports),
                Arc::clone(&self.world.settings),
                Arc::new(Hello),
            )
            .serve(functions, async {
                let _either: Result<(), oneshot::error::RecvError> = functions_stopped.await;
            }),
        );
        let tunnel = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tunnel_addr = tunnel.local_addr().unwrap();
        let (stop_tunnel, tunnel_stopped) = oneshot::channel::<()>();
        tokio::spawn(
            TunnelListener::new(
                Arc::clone(&self.ports),
                Arc::clone(&self.world.settings),
                TunnelServer::new(ServerConfig::default()),
            )
            .serve(tunnel, async {
                let _either: Result<(), oneshot::error::RecvError> = tunnel_stopped.await;
            }),
        );
        Listeners {
            functions: functions_addr,
            tunnel: tunnel_addr,
            _stop: vec![stop_functions, stop_tunnel],
        }
    }

    async fn remove_web(&self) {
        self.modules()
            .remove(&self.world.owner, &name("web"), false)
            .await
            .unwrap();
    }

    /// A fresh one-time link of `owner` to `api`: its path on the API
    /// host.
    async fn link(&self) -> String {
        let url = LinksService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
            .issue(&self.world.owner, &LinkTarget::Environment(self.api.id))
            .await
            .unwrap()
            .url;
        url.strip_prefix(LINK_ORIGIN).unwrap_or(&url).to_owned()
    }

    /// Opens a fresh link as a browser does: the answer, with the API
    /// host's cookie.
    async fn open_link(&self) -> Answer {
        let path = self.link().await;
        self.send("GET", &path, &[]).await
    }

    /// Sends `method path` with these headers and no body.
    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> Answer {
        let mut request = Request::builder().method(method).uri(path);
        for (header, value) in headers {
            request = request.header(*header, *value);
        }
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            self.app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Answer {
            status,
            headers,
            body: body.to_vec(),
        }
    }

    /// `GET /api/v1/me` as `owner` (bearer) from `origin`.
    async fn me_from(&self, origin: &str) -> Answer {
        let bearer = format!("Bearer {}", self.owner_token);
        self.send(
            "GET",
            "/api/v1/me",
            &[("origin", origin), ("authorization", &bearer)],
        )
        .await
    }

    /// The credentialed preflight of `GET /api/v1/me` from `origin`, asking
    /// for `authorization` and `content-type`.
    async fn preflight_from(&self, origin: &str) -> Answer {
        self.send(
            "OPTIONS",
            "/api/v1/me",
            &[
                ("origin", origin),
                ("access-control-request-method", "GET"),
                (
                    "access-control-request-headers",
                    "authorization, content-type",
                ),
            ],
        )
        .await
    }
}

/// A daemon over a fresh data directory with the fake registry serving
/// the built `web`, nothing installed, and `shop`'s private function
/// `api` at [`HOST`] in its store.
async fn daemon() -> Daemon {
    let world = World::new().await;
    let data = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(data.path().join("liftoff.db")).unwrap();
    store.insert_user(&world.owner).await.unwrap();
    let (session, secret) = world.records.session(&world.owner);
    store.insert_session(&session).await.unwrap();
    store.insert_project(&world.shop).await.unwrap();
    let api = Environment {
        visibility: Visibility::Private,
        state: EnvState::Running,
        ..world.records.function(&world.shop, "api").unwrap()
    };
    store.insert_environment(&api).await.unwrap();
    let domain = world.records.domain(&world.shop, HOST).unwrap();
    store.insert_domain(&domain).await.unwrap();
    let binding = world
        .records
        .binding(&api, &domain, "web", 80, 20_100)
        .unwrap();
    store.insert_binding(&binding).await.unwrap();
    let idle = Environment {
        visibility: Visibility::Shared,
        state: EnvState::Stopped,
        ..world.records.function(&world.shop, "idle").unwrap()
    };
    let tunnel = Environment {
        visibility: Visibility::Shared,
        state: EnvState::Offline,
        ..world
            .records
            .tunnel(&world.shop, "dev", &world.owner)
            .unwrap()
    };
    let protected = Environment {
        protected: true,
        ..world.records.environment(&world.shop, "admin").unwrap()
    };
    for (environment, host, port) in [
        (&idle, IDLE_HOST, 20_101),
        (&tunnel, TUNNEL_HOST, 20_102),
        (&protected, PROTECTED_HOST, 20_103),
    ] {
        store.insert_environment(environment).await.unwrap();
        let domain = world.records.domain(&world.shop, host).unwrap();
        store.insert_domain(&domain).await.unwrap();
        let binding = world
            .records
            .binding(environment, &domain, "web", 80, port)
            .unwrap();
        store.insert_binding(&binding).await.unwrap();
    }
    let limits = Limits {
        max_memory_bytes: 64 * 1024 * 1024,
        call_timeout: Duration::from_secs(10),
    };
    let host = ModuleHost::new(store.clone(), TokioTimer, TracingLog, limits).unwrap();
    let runtime = DaemonModules::new(host, data.path().join("modules"));
    for module in ["web", "oidc"] {
        world
            .ports
            .registry
            .publish(release(&runtime, module).await)
            .unwrap();
    }
    let hooks: DaemonHooks =
        ModuleHooks::new(Arc::clone(runtime.host().registry()), runtime.set().clone());
    let ports =
        Arc::new(WithModules::new(Arc::clone(&world.ports), store, runtime).with_hooks(hooks));
    let services = Services::new(Arc::clone(&ports), Arc::clone(&world.settings));
    let app = app_router(AppState::new(), services, &RestConfig::default());
    Daemon {
        _data: data,
        world,
        ports,
        app,
        owner_token: secret.expose().to_owned(),
        api,
    }
}

/// A daemon with `web` installed for [`PANEL`], taking the pages over
/// when `take_pages`.
async fn daemon_with_web(take_pages: bool) -> Daemon {
    let daemon = daemon().await;
    daemon
        .install_web(take_pages)
        .await
        .unwrap_or_else(|error| panic!("install web: {error:?}"));
    daemon
}

/// The fake function: `200 hello`.
struct Hello;

impl Functions for Hello {
    fn invoke(
        &self,
        _environment: EnvironmentId,
        _request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<Bytes>, FunctionError>> + Send {
        core::future::ready(Ok(Response::new(Bytes::from_static(b"hello"))))
    }
}

/// The daemon's function and tunnel listeners on loopback.
struct Listeners {
    functions: SocketAddr,
    tunnel: SocketAddr,
    _stop: Vec<oneshot::Sender<()>>,
}

impl Listeners {
    /// `GET path` on `host` through the listener at `addr`, as Caddy
    /// asks it, with `headers`.
    async fn get(addr: SocketAddr, host: &str, path: &str, headers: &[(&str, &str)]) -> Answer {
        let mut request = Request::builder()
            .method("GET")
            .uri(path)
            .header("host", host)
            .header("accept", "text/html");
        for (header, value) in headers {
            request = request.header(*header, *value);
        }
        let request = request.body(Full::new(Bytes::new())).unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            let stream = TcpStream::connect(addr).await.unwrap();
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .unwrap();
            tokio::spawn(connection);
            let response = sender.send_request(request).await.unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            Answer {
                status,
                headers,
                body: body.to_vec(),
            }
        })
        .await
        .expect("the listener answers in time")
    }
}

// ---- HTTP -------------------------------------------------------------------

/// An answer of the router.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Every value of `name`.
    fn values(&self, name: &str) -> Vec<String> {
        self.headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect()
    }

    fn header(&self, name: &str) -> Option<String> {
        self.values(name).into_iter().next()
    }

    /// The comma-separated items of every `name` header, lower case.
    fn items(&self, name: &str) -> Vec<String> {
        let mut items: Vec<String> = self
            .values(name)
            .iter()
            .flat_map(|value| value.split(','))
            .map(|item| item.trim().to_ascii_lowercase())
            .filter(|item| !item.is_empty())
            .collect();
        items.sort();
        items
    }

    /// The CORS headers of the answer, `(Allow-Origin,
    /// Allow-Credentials)`.
    fn cors(&self) -> (Option<String>, Option<String>) {
        (
            self.header("access-control-allow-origin"),
            self.header("access-control-allow-credentials"),
        )
    }

    /// The attributes of the `Set-Cookie` of [`API_COOKIE`], sorted.
    fn session_cookie(&self) -> Option<Vec<String>> {
        self.values("set-cookie").into_iter().find_map(|line| {
            let mut parts = line.split(';').map(str::trim);
            let _value = parts.next()?.strip_prefix(API_COOKIE)?.strip_prefix('=')?;
            let mut attributes: Vec<String> = parts.map(str::to_owned).collect();
            attributes.sort();
            Some(attributes)
        })
    }

    /// The value of [`API_COOKIE`] the answer sets.
    fn session_value(&self) -> Option<String> {
        self.values("set-cookie").into_iter().find_map(|line| {
            let first = line.split(';').next()?.trim().to_owned();
            first
                .strip_prefix(API_COOKIE)?
                .strip_prefix('=')
                .map(str::to_owned)
        })
    }

    /// Whether the answer is the daemon's own HTML page (its CSP is
    /// [`pages::CSP`]).
    fn is_daemon_page(&self) -> bool {
        self.header("content-type")
            .is_some_and(|kind| kind.starts_with("text/html"))
            && self.values("content-security-policy") == [pages::CSP]
    }
}

/// The attributes of the browser cookie on the API host with
/// `SameSite=<same_site>` (`HttpOnly; Secure`, a `__Host-` cookie has `Path=/`; it lives as long as the `browser` session, 24 hours), sorted.
fn cookie_attributes(same_site: &str) -> Vec<String> {
    let mut attributes = vec![
        "HttpOnly".to_owned(),
        "Max-Age=86400".to_owned(),
        "Path=/".to_owned(),
        format!("SameSite={same_site}"),
        "Secure".to_owned(),
    ];
    attributes.sort();
    attributes
}

/// Percent-decodes a query component (`+` is a space).
fn decode(text: &str) -> String {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'%' => {
                let hex: Vec<u8> = bytes.by_ref().take(2).collect();
                out.push(u8::from_str_radix(core::str::from_utf8(&hex).unwrap(), 16).unwrap());
            }
            b'+' => out.push(b' '),
            other => out.push(other),
        }
    }
    String::from_utf8(out).unwrap()
}

/// `location` split into what precedes its query and its parameters,
/// decoded and sorted.
fn split_location(location: &str) -> (String, Vec<(String, String)>) {
    let (base, query) = location.split_once('?').unwrap_or((location, ""));
    let mut params: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect();
    params.sort();
    (base.to_owned(), params)
}

// ---- WEB-01: CORS for the panel's origin alone --------------------------------

mod cors {
    use super::*;

    // TC: WEB-01 (the panel's origin, with credentials)
    #[tokio::test]
    async fn allows_panel_origin_with_credentials_when_web_installed() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.me_from(PANEL).await;
        assert_eq!(
            (answer.status, answer.cors()),
            (
                StatusCode::OK,
                (Some(PANEL.to_owned()), Some("true".to_owned()))
            ),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-01 (the answer depends on `Origin`: caches must key on it)
    #[tokio::test]
    async fn varies_on_origin_when_panel_asks() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.me_from(PANEL).await;
        assert!(
            answer.items("vary").contains(&"origin".to_owned()),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-01 (another origin: the request is answered, without CORS)
    #[tokio::test]
    async fn no_cors_headers_when_origin_is_foreign() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.me_from(FOREIGN).await;
        assert_eq!(
            (answer.status, answer.cors()),
            (StatusCode::OK, (None, None)),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-01 (no module, no panel: no CORS for anyone)
    #[tokio::test]
    async fn no_cors_headers_when_web_not_installed() {
        let daemon = daemon().await;
        let answer = daemon.me_from(PANEL).await;
        assert_eq!(answer.cors(), (None, None), "{:?}", answer.headers);
    }

    // TC: WEB-01 (an installation takes effect without a restart: the
    // router was built before it)
    #[tokio::test]
    async fn allows_panel_origin_when_web_installed_after_start() {
        let daemon = daemon().await;
        let _before = daemon.me_from(PANEL).await;
        daemon.install_web(false).await.unwrap();
        let answer = daemon.me_from(PANEL).await;
        assert_eq!(
            answer.cors(),
            (Some(PANEL.to_owned()), Some("true".to_owned())),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-01 (CORS is `/api/v1`'s alone: never the root pages)
    #[tokio::test]
    async fn no_cors_headers_on_login_page_when_panel_asks() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.send("GET", "/login", &[("origin", PANEL)]).await;
        assert_eq!(answer.cors(), (None, None), "{:?}", answer.headers);
    }

    // TC: WEB-01 (CORS never applies to the viewer of artifacts, `/a/`)
    #[tokio::test]
    async fn no_cors_headers_on_artifact_viewer_when_panel_asks() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon
            .send("GET", "/a/AbCdEfGhIjKl", &[("origin", PANEL)])
            .await;
        assert_eq!(answer.cors(), (None, None), "{:?}", answer.headers);
    }

    // TC: WEB-01 (criterion: the panel's `GET /me` with the
    // cookie of the browser session)
    #[tokio::test]
    async fn me_answers_cookie_session_when_panel_asks() {
        let daemon = daemon_with_web(false).await;
        let opened = daemon.open_link().await;
        let cookie = format!("{API_COOKIE}={}", opened.session_value().unwrap());
        let answer = daemon
            .send(
                "GET",
                "/api/v1/me",
                &[("origin", PANEL), ("cookie", &cookie)],
            )
            .await;
        let body: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
        assert_eq!(
            (answer.status, body["login"].as_str(), answer.cors().0),
            (StatusCode::OK, Some("owner"), Some(PANEL.to_owned())),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-01 (with `SameSite=None` the browser sends the cookie on
    // every cross-site request: another origin's request is no session)
    #[tokio::test]
    async fn refuses_cookie_session_when_origin_is_foreign() {
        let daemon = daemon_with_web(false).await;
        let opened = daemon.open_link().await;
        let cookie = format!("{API_COOKIE}={}", opened.session_value().unwrap());
        let answer = daemon
            .send(
                "GET",
                "/api/v1/me",
                &[("origin", FOREIGN), ("cookie", &cookie)],
            )
            .await;
        assert_eq!(
            (answer.status, answer.cors()),
            (StatusCode::UNAUTHORIZED, (None, None)),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }
}

mod cookie_session {
    use super::*;

    /// The cookie of a fresh `browser` session of `owner`.
    async fn browser_cookie(daemon: &Daemon) -> String {
        let opened = daemon.open_link().await;
        format!("{API_COOKIE}={}", opened.session_value().unwrap())
    }

    // TC: WEB-01 (no `Origin` is no panel: the cookie alone is no
    // session)
    #[tokio::test]
    async fn refuses_cookie_session_when_origin_is_absent() {
        let daemon = daemon_with_web(false).await;
        let cookie = browser_cookie(&daemon).await;
        let answer = daemon
            .send("GET", "/api/v1/me", &[("cookie", &cookie)])
            .await;
        assert_eq!(
            answer.status,
            StatusCode::UNAUTHORIZED,
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-01 (an `Authorization` header is the credential: a bad
    // bearer is not rescued by the cookie)
    #[tokio::test]
    async fn refuses_bad_bearer_when_cookie_from_panel_is_valid() {
        let daemon = daemon_with_web(false).await;
        let cookie = browser_cookie(&daemon).await;
        let answer = daemon
            .send(
                "GET",
                "/api/v1/me",
                &[
                    ("origin", PANEL),
                    ("cookie", &cookie),
                    (
                        "authorization",
                        "Bearer lfs_0000000000000000000000000000000000000000000",
                    ),
                ],
            )
            .await;
        assert_eq!(
            answer.status,
            StatusCode::UNAUTHORIZED,
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-01 (`POST /deploys` has its own layer: the cookie from
    // another origin is no session there either)
    #[tokio::test]
    async fn refuses_cookie_on_deploys_when_origin_is_foreign() {
        let daemon = daemon_with_web(false).await;
        let cookie = browser_cookie(&daemon).await;
        let from_panel = daemon
            .send(
                "POST",
                "/api/v1/deploys",
                &[("origin", PANEL), ("cookie", &cookie)],
            )
            .await;
        let foreign = daemon
            .send(
                "POST",
                "/api/v1/deploys",
                &[("origin", FOREIGN), ("cookie", &cookie)],
            )
            .await;
        assert_eq!(
            (
                from_panel.status == StatusCode::UNAUTHORIZED,
                foreign.status
            ),
            (false, StatusCode::UNAUTHORIZED),
            "{} {}",
            from_panel.text(),
            foreign.text()
        );
    }
}

// ---- the module's configuration ------------------------------------------------

mod configuration {
    use super::*;

    // TC: WEB-01 (a cleartext panel would get credentialed CORS and the
    // `SameSite=None` cookie: refused at install)
    #[tokio::test]
    async fn refuses_install_when_origin_is_cleartext() {
        let daemon = daemon().await;
        let installed = daemon
            .modules()
            .install(
                &daemon.world.owner,
                InstallRequest {
                    name: name("web"),
                    version: None,
                    config: config(json!({"origin": "http://panel.test"})),
                },
            )
            .await;
        assert!(
            matches!(installed, Err(Error::Validation(_))),
            "{installed:?}"
        );
    }
}

// ---- WEB-02: the preflight with credentials ---------------------------------

mod preflight {
    use super::*;

    // TC: WEB-02 (answered without a session, credentials allowed)
    #[tokio::test]
    async fn allows_panel_origin_with_credentials_when_preflight_from_panel() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.preflight_from(PANEL).await;
        assert_eq!(
            (answer.status.is_success(), answer.cors()),
            (true, (Some(PANEL.to_owned()), Some("true".to_owned()))),
            "{} {:?} {}",
            answer.status,
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-02 (the requested method is allowed)
    #[tokio::test]
    async fn allows_requested_method_when_preflight_from_panel() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.preflight_from(PANEL).await;
        assert!(
            answer
                .items("access-control-allow-methods")
                .contains(&"get".to_owned()),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-02 (the requested headers are allowed)
    #[tokio::test]
    async fn allows_requested_headers_when_preflight_from_panel() {
        let daemon = daemon_with_web(false).await;
        let items = daemon
            .preflight_from(PANEL)
            .await
            .items("access-control-allow-headers");
        assert_eq!(
            (
                items.contains(&"authorization".to_owned()),
                items.contains(&"content-type".to_owned())
            ),
            (true, true),
            "{items:?}"
        );
    }

    // TC: WEB-02 (the preflight's answer depends on `Origin` too)
    #[tokio::test]
    async fn varies_on_origin_when_preflight_from_panel() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.preflight_from(PANEL).await;
        assert!(
            answer.items("vary").contains(&"origin".to_owned()),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-02 (another origin's preflight allows nothing)
    #[tokio::test]
    async fn no_cors_headers_when_preflight_from_foreign_origin() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.preflight_from(FOREIGN).await;
        assert_eq!(
            (
                answer.cors(),
                answer.header("access-control-allow-methods"),
                answer.header("access-control-allow-headers")
            ),
            ((None, None), None, None),
            "{:?}",
            answer.headers
        );
    }
}

// ---- WEB-03: the pages the panel takes over ----------------------------------

mod pages_taken {
    use super::*;

    // TC: WEB-03 (`render_page(login)` sends the browser to the panel's
    // login with where to return and the address)
    #[tokio::test]
    async fn redirects_login_to_panel_with_return_when_take_pages() {
        let daemon = daemon_with_web(true).await;
        let answer = daemon
            .send(
                "GET",
                &format!("/login?host={HOST}&rd=%2Fprivate%3Fx%3D1"),
                &[],
            )
            .await;
        let location = answer.header("location").unwrap_or_default();
        assert_eq!(
            (answer.status, split_location(&location)),
            (
                StatusCode::FOUND,
                (
                    format!("{PANEL}/login"),
                    vec![
                        ("host".to_owned(), HOST.to_owned()),
                        ("return".to_owned(), format!("https://{HOST}/private?x=1")),
                    ]
                )
            ),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-03 (the gate's handover without a session goes to the API
    // host's `/login`, which the panel takes, not to a module's own
    // login route)
    #[tokio::test]
    async fn sends_gate_start_to_api_login_when_take_pages() {
        let daemon = daemon_with_web(true).await;
        let answer = daemon
            .send(
                "GET",
                &format!("/gate/start?host={HOST}&rd=%2Fprivate"),
                &[],
            )
            .await;
        let location = answer.header("location").unwrap_or_default();
        assert_eq!(
            (answer.status, split_location(&location)),
            (
                StatusCode::FOUND,
                (
                    format!("{LINK_ORIGIN}/login"),
                    vec![
                        ("host".to_owned(), HOST.to_owned()),
                        ("rd".to_owned(), "/private".to_owned()),
                    ]
                )
            ),
            "{:?}",
            answer.headers
        );
    }

    /// The panel's page `kind` for `host`, returning to `back` there,
    /// as [`split_location`] reads it.
    fn panel_page(kind: &str, host: &str, back: &str) -> (String, Vec<(String, String)>) {
        (
            format!("{PANEL}/{kind}"),
            vec![
                ("host".to_owned(), host.to_owned()),
                ("return".to_owned(), format!("https://{host}{back}")),
            ],
        )
    }

    // TC: WEB-03 (the function listener's `403` goes to the panel's
    // forbidden page with the address the browser asked for)
    #[tokio::test]
    async fn redirects_forbidden_function_to_panel_when_take_pages() {
        let daemon = daemon_with_web(true).await;
        let cookie = daemon.stranger_on(HOST).await;
        let listeners = daemon.listeners().await;
        let answer = Listeners::get(
            listeners.functions,
            HOST,
            "/private?x=1",
            &[("cookie", &cookie)],
        )
        .await;
        let location = answer.header("location").unwrap_or_default();
        assert_eq!(
            (answer.status, split_location(&location)),
            (
                StatusCode::FOUND,
                panel_page("forbidden", HOST, "/private?x=1")
            ),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-03 (a function that does not run: the panel's unavailable
    // page instead of the listener's `503`)
    #[tokio::test]
    async fn redirects_idle_function_to_panel_when_take_pages() {
        let daemon = daemon_with_web(true).await;
        let listeners = daemon.listeners().await;
        let answer = Listeners::get(listeners.functions, IDLE_HOST, "/status", &[]).await;
        let location = answer.header("location").unwrap_or_default();
        assert_eq!(
            (answer.status, split_location(&location)),
            (
                StatusCode::FOUND,
                panel_page("unavailable", IDLE_HOST, "/status")
            ),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-03 (without the panel the idle function's `503` stays the
    // listener's plain text)
    #[tokio::test]
    async fn keeps_listener_unavailable_when_web_not_installed() {
        let daemon = daemon().await;
        let listeners = daemon.listeners().await;
        let answer = Listeners::get(listeners.functions, IDLE_HOST, "/status", &[]).await;
        assert_eq!(
            (answer.status, answer.header("location")),
            (StatusCode::SERVICE_UNAVAILABLE, None),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-03 (a tunnel without its `lof dev`: the panel's offline
    // page)
    #[tokio::test]
    async fn redirects_offline_tunnel_to_panel_when_take_pages() {
        let daemon = daemon_with_web(true).await;
        let listeners = daemon.listeners().await;
        let answer = Listeners::get(listeners.tunnel, TUNNEL_HOST, "/", &[]).await;
        let location = answer.header("location").unwrap_or_default();
        assert_eq!(
            (answer.status, split_location(&location)),
            (StatusCode::FOUND, panel_page("offline", TUNNEL_HOST, "/")),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-03 (`take_pages` wins over `oidc`'s login page, even with
    // `oidc` installed first, as in production)
    #[tokio::test]
    async fn redirects_login_to_panel_when_oidc_installed_first() {
        let daemon = daemon().await;
        daemon.install_oidc().await;
        daemon.install_web(true).await.unwrap();
        let answer = daemon
            .send("GET", &format!("/login?host={HOST}&rd=%2Fprivate"), &[])
            .await;
        let location = answer.header("location").unwrap_or_default();
        assert_eq!(
            (answer.status, split_location(&location)),
            (StatusCode::FOUND, panel_page("login", HOST, "/private")),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-03 (without `take_pages`, `oidc`'s login page as before)
    #[tokio::test]
    async fn keeps_oidc_login_page_when_web_takes_no_pages() {
        let daemon = daemon().await;
        daemon.install_oidc().await;
        daemon.install_web_default().await;
        let answer = daemon
            .send("GET", &format!("/login?host={HOST}&rd=%2F"), &[])
            .await;
        assert_eq!(
            (
                answer.status,
                answer.header("location"),
                answer.is_daemon_page()
            ),
            (StatusCode::OK, None, false),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-03 (no open redirect through the panel: a host the daemon
    // does not serve gets no `host` or `return`)
    #[tokio::test]
    async fn redirects_login_without_return_when_host_is_not_served() {
        let daemon = daemon_with_web(true).await;
        let answer = daemon
            .send("GET", "/login?host=evil.example&rd=%2F", &[])
            .await;
        let location = answer.header("location").unwrap_or_default();
        assert_eq!(
            (answer.status, split_location(&location)),
            (StatusCode::FOUND, (format!("{PANEL}/login"), Vec::new())),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-03 (a protected environment's handover without a session
    // goes to the panel's login too, not to a module's login route)
    #[tokio::test]
    async fn sends_protected_gate_start_to_api_login_when_take_pages() {
        let daemon = daemon_with_web(true).await;
        let answer = daemon
            .send(
                "GET",
                &format!("/gate/start?host={PROTECTED_HOST}&rd=%2F"),
                &[],
            )
            .await;
        let location = answer.header("location").unwrap_or_default();
        assert_eq!(
            (answer.status, split_location(&location)),
            (
                StatusCode::FOUND,
                (
                    format!("{LINK_ORIGIN}/login"),
                    vec![
                        ("host".to_owned(), PROTECTED_HOST.to_owned()),
                        ("rd".to_owned(), "/".to_owned()),
                    ]
                )
            ),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-03 (without `take_pages` the module has no opinion: the
    // daemon's own login page)
    #[tokio::test]
    async fn keeps_daemon_login_page_when_take_pages_default() {
        let daemon = daemon().await;
        daemon.install_web_default().await;
        let answer = daemon
            .send("GET", &format!("/login?host={HOST}&rd=%2F"), &[])
            .await;
        assert_eq!(
            (answer.status, answer.is_daemon_page()),
            (StatusCode::OK, true),
            "{:?}",
            answer.headers
        );
    }
}

// ---- WEB-04: the browser cookie's SameSite -------------------------------------

mod cookie {
    use super::*;

    // TC: WEB-04 (the panel's requests are cross-site: the browser sends
    // the cookie only with `SameSite=None`)
    #[tokio::test]
    async fn same_site_none_when_web_installed() {
        let daemon = daemon_with_web(false).await;
        let answer = daemon.open_link().await;
        assert_eq!(
            (answer.status, answer.session_cookie()),
            (StatusCode::FOUND, Some(cookie_attributes("None"))),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-04 (without the module, as)
    #[tokio::test]
    async fn same_site_lax_when_web_not_installed() {
        let daemon = daemon().await;
        let answer = daemon.open_link().await;
        assert_eq!(
            (answer.status, answer.session_cookie()),
            (StatusCode::FOUND, Some(cookie_attributes("Lax"))),
            "{:?}",
            answer.headers
        );
    }
}

// ---- WEB-05: removing the module ----------------------------------------------

mod removal {
    use super::*;

    // TC: WEB-05 (the daemon's embedded login page is back)
    #[tokio::test]
    async fn embedded_login_page_when_web_removed() {
        let daemon = daemon_with_web(true).await;
        let taken = daemon
            .send("GET", &format!("/login?host={HOST}"), &[])
            .await;
        assert_eq!(
            taken.status,
            StatusCode::FOUND,
            "precondition: the panel took the page"
        );
        daemon.remove_web().await;
        let answer = daemon
            .send("GET", &format!("/login?host={HOST}&rd=%2F"), &[])
            .await;
        assert_eq!(
            (answer.status, answer.is_daemon_page()),
            (StatusCode::OK, true),
            "{:?} {}",
            answer.headers,
            answer.text()
        );
    }

    // TC: WEB-05 (the browser cookie is `Lax` again)
    #[tokio::test]
    async fn same_site_lax_when_web_removed() {
        let daemon = daemon_with_web(false).await;
        let installed = daemon.open_link().await;
        assert_eq!(
            installed.session_cookie(),
            Some(cookie_attributes("None")),
            "precondition: SameSite=None while installed"
        );
        daemon.remove_web().await;
        let answer = daemon.open_link().await;
        assert_eq!(
            answer.session_cookie(),
            Some(cookie_attributes("Lax")),
            "{:?}",
            answer.headers
        );
    }

    // TC: WEB-05 (no CORS for the panel any more, without a restart)
    #[tokio::test]
    async fn no_cors_headers_when_web_removed() {
        let daemon = daemon_with_web(false).await;
        let allowed = daemon.me_from(PANEL).await;
        assert_eq!(
            allowed.cors().0,
            Some(PANEL.to_owned()),
            "precondition: CORS while installed"
        );
        daemon.remove_web().await;
        let answer = daemon.me_from(PANEL).await;
        assert_eq!(answer.cors(), (None, None), "{:?}", answer.headers);
    }
}
