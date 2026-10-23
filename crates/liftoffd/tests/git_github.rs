#![cfg(test)]
//! Through the REST router: `lof git connect
//! github` as the CLI sends it (GIT-01, GIT-02).
//!
//! The built `git.github` component (`modules/git-github`) is loaded by a
//! module host whose `wasi:http` to `api.github.com` and `github.com` is
//! routed to a fake GitHub on loopback; the core's `ModuleGit` over it is
//! the `Git` port of the world's ports (`WithGit`), so the REST router
//! runs the daemon's own `GitService` against the component. It lives
//! with the composition root because it joins two adapters
//! (`liftoff-wasm` and `liftoff-rest`). The module's own cases are in
//! `crates/wasm/tests/git_github.rs`.
//!
//! The routes: `POST /api/v1/git/connections/{host}/device`
//! `{"provider"}` starts the forge's device flow (`200`
//! `GitDeviceView`); `POST /api/v1/git/connections/{host}/device/poll`
//! `{"provider","device_code"}` answers `202` `{"status":"pending"}`
//! (`{"status":"slow_down","interval"}` when GitHub asks to poll less
//! often) until the person confirmed, then stores the connection as `POST
//! /api/v1/git/connections` does (`201` `ConnectionView`, kind `oauth`).
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use liftoff_api::{ConnectionView, GitDevicePending, GitDevicePoll, GitDeviceStart, GitDeviceView};
use liftoff_core::domain::{Host, ModuleName, User, UserRole};
use liftoff_core::fakes::{InMemoryStore, WithGit, World};
use liftoff_core::module::ModuleGit;
use liftoff_core::ports::{HookResult, Secrets as _, Store as _};
use liftoff_rest::{AppState, RestConfig, Services, app_router};
use liftoff_wasm::testing::{Host as ModuleHostOf, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TokioTimer, TracingLog};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const API: &str = "api.github.com";
const GITHUB: &str = "github.com";
const CLIENT_ID: &str = "Iv1.liftofftest00001";
const DEVICE_CODE: &str = "dc-3584d83530557fdd1f46af8289938c8ef79f9dc5";
const USER_CODE: &str = "WDJB-MJHT";
/// The token the device flow grants: `alice`'s.
const DEVICE_TOKEN: &str = "gho_device";

const DEVICE: &str = "/api/v1/git/connections/github.com/device";
const POLL: &str = "/api/v1/git/connections/github.com/device/poll";
const CONNECTIONS: &str = "/api/v1/git/connections";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- The fake GitHub ------------------------------------------------------------

#[derive(Debug)]
struct GithubState {
    /// Token → (login, `X-OAuth-Scopes`).
    tokens: BTreeMap<String, (String, String)>,
    /// How many polls answer `authorization_pending` before the grant.
    pending: usize,
    /// How many polls answer `slow_down` (interval 10) before those.
    slow_downs: usize,
}

/// One authority of the fake and the state it shares.
#[derive(Debug, Clone)]
struct Plays {
    authority: &'static str,
    state: Arc<Mutex<GithubState>>,
}

/// The fake: `alice`'s device token, `pending` polls before the grant.
#[derive(Debug, Clone)]
struct Github {
    addrs: Vec<(&'static str, core::net::SocketAddr)>,
    state: Arc<Mutex<GithubState>>,
}

impl Github {
    async fn start(pending: usize) -> Self {
        let state = Arc::new(Mutex::new(GithubState {
            tokens: core::iter::once((
                DEVICE_TOKEN.to_owned(),
                ("alice".to_owned(), "repo, read:packages".to_owned()),
            ))
            .collect(),
            pending,
            slow_downs: 0,
        }));
        let mut addrs = Vec::new();
        for authority in [API, GITHUB] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push((authority, listener.local_addr().unwrap()));
            let app = Router::new().fallback(handle).with_state(Plays {
                authority,
                state: Arc::clone(&state),
            });
            drop(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
        }
        Self { addrs, state }
    }

    /// The next `times` polls answer `slow_down` with interval 10.
    fn slow_down(&self, times: usize) {
        lock(&self.state).slow_downs = times;
    }

    /// `token` logs in as `login` with `scopes`.
    fn accept(&self, token: &str, login: &str, scopes: &str) {
        let _previous = lock(&self.state)
            .tokens
            .insert(token.to_owned(), (login.to_owned(), scopes.to_owned()));
    }
}

/// `a=b&c=d`, or a JSON object, as pairs (values not percent-decoded:
/// the fake's values need none).
fn params(body: &str) -> BTreeMap<String, String> {
    if let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(body) {
        return fields
            .into_iter()
            .map(|(key, value)| (key, value.as_str().unwrap_or_default().to_owned()))
            .collect();
    }
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.replace("%3A", ":")))
        .collect()
}

fn json_reply(status: StatusCode, body: &Value) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn handle(
    State(plays): State<Plays>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let body = String::from_utf8_lossy(&body).into_owned();
    let mut state = lock(&plays.state);
    match (plays.authority, uri.path()) {
        (GITHUB, "/login/device/code")
            if params(&body).get("client_id").map(String::as_str) == Some(CLIENT_ID) =>
        {
            json_reply(
                StatusCode::OK,
                &json!({
                    "device_code": DEVICE_CODE, "user_code": USER_CODE,
                    "verification_uri": "https://github.com/login/device",
                    "expires_in": 900, "interval": 5,
                }),
            )
        }
        (GITHUB, "/login/oauth/access_token") => {
            if state.slow_downs > 0 {
                state.slow_downs = state.slow_downs.saturating_sub(1);
                return json_reply(
                    StatusCode::OK,
                    &json!({"error": "slow_down", "interval": 10}),
                );
            }
            if state.pending > 0 {
                state.pending = state.pending.saturating_sub(1);
                return json_reply(StatusCode::OK, &json!({"error": "authorization_pending"}));
            }
            json_reply(
                StatusCode::OK,
                &json!({"access_token": DEVICE_TOKEN, "token_type": "bearer", "scope": "repo,read:packages"}),
            )
        }
        (API, "/user") => {
            let token = headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| {
                    value
                        .strip_prefix("Bearer ")
                        .or_else(|| value.strip_prefix("token "))
                })
                .unwrap_or_default();
            match state.tokens.get(token) {
                Some((login, scopes)) => axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                    .header("x-oauth-scopes", scopes.as_str())
                    .body(Body::from(json!({"login": login, "id": 1}).to_string()))
                    .unwrap(),
                None => json_reply(
                    StatusCode::UNAUTHORIZED,
                    &json!({"message": "Bad credentials"}),
                ),
            }
        }
        _ => json_reply(StatusCode::NOT_FOUND, &json!({"message": "Not Found"})),
    }
}

// --- The API over the component --------------------------------------------------

/// The API over the world's fakes with the `Git` port served by the
/// built `git.github` component loaded with `config`, against `github`.
async fn app(world: &World, github: &Github, config: ModuleConfig) -> Router {
    let mut host: ModuleHostOf<InMemoryStore> =
        ModuleHostOf::new(InMemoryStore::new(), TokioTimer, TracingLog, limits()).unwrap();
    for (authority, addr) in &github.addrs {
        host = host.routing_https_to(authority, *addr);
    }
    let module = host
        .load(
            ModuleName::new("git.github").unwrap(),
            &component("git_github").unwrap(),
            config,
        )
        .await
        .unwrap();
    module.migrate(0).await.unwrap();
    let set = ModuleSet::new();
    set.insert(module);
    let git = ModuleGit::new(Arc::clone(host.registry()), set);
    let ports = Arc::new(WithGit::new(Arc::clone(&world.ports), git));
    let services = Services::new(ports, Arc::clone(&world.settings));
    app_router(AppState::new(), services, &RestConfig::default())
}

fn with_oauth() -> ModuleConfig {
    core::iter::once(("client_id".to_owned(), CLIENT_ID.to_owned())).collect()
}

/// `method path` with a JSON `body` as `owner`'s fresh session: the
/// status and the body.
async fn owner(
    world: &World,
    app: &Router,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value, String) {
    send_as(world, &world.owner, app, method, path, body).await
}

/// `method path` with a JSON `body` as `user`'s fresh session: the
/// status and the body.
async fn send_as(
    world: &World,
    user: &User,
    app: &Router,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (StatusCode, Value, String) {
    let secret = world.login(user).await.unwrap();
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", secret.expose()));
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::Null),
        text,
    )
}

/// The start body the CLI sends.
fn start() -> Value {
    serde_json::to_value(GitDeviceStart {
        provider: "github".to_owned(),
    })
    .unwrap()
}

/// The poll body the CLI sends.
fn poll() -> Value {
    serde_json::to_value(GitDevicePoll {
        provider: "github".to_owned(),
        device_code: DEVICE_CODE.to_owned(),
    })
    .unwrap()
}

fn github_com() -> Host {
    Host::new(GITHUB).unwrap()
}

mod device {
    use super::*;

    // TC: GIT-01 (with an OAuth application the CLI runs the forge's device flow; read as the wire type the CLI reads)
    #[tokio::test(flavor = "multi_thread")]
    async fn started_with_code_of_github_when_client_id_configured() {
        let world = World::new().await;
        let github = Github::start(0).await;
        let app = app(&world, &github, with_oauth()).await;
        let (status, body, text) = owner(&world, &app, "POST", DEVICE, Some(&start())).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let device: GitDeviceView = serde_json::from_value(body).unwrap();
        assert_eq!(
            device,
            GitDeviceView {
                device_code: DEVICE_CODE.to_owned(),
                user_code: USER_CODE.to_owned(),
                verification_uri: "https://github.com/login/device".to_owned(),
                interval: 5,
                expires_in: 900,
            }
        );
    }

    // TC: GIT-01 (the device flow is not the admin's only; a user whom a module lets `git.connect` starts it although only the admin may `GET /modules` without `tenants`)
    #[tokio::test(flavor = "multi_thread")]
    async fn started_for_user_whom_hook_lets_connect_when_modules_are_admin_only() {
        let world = World::new().await;
        let bob = world.add_user("bob", UserRole::User).await.unwrap();
        let github = Github::start(0).await;
        let app = app(&world, &github, with_oauth()).await;
        // No hook has an opinion on `module.list`: admin only.
        let (modules, _, _) = send_as(&world, &bob, &app, "GET", "/api/v1/modules", None).await;
        // A hook (as `tenants` does) lets bob `git.connect`.
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let (status, _, text) = send_as(&world, &bob, &app, "POST", DEVICE, Some(&start())).await;
        assert_eq!(
            (modules, status),
            (StatusCode::FORBIDDEN, StatusCode::OK),
            "{text}"
        );
    }

    // TC: GIT-01 (no OAuth application: the route says so and points to
    // `--token`)
    #[tokio::test(flavor = "multi_thread")]
    async fn bad_request_with_token_hint_when_no_client_id() {
        let world = World::new().await;
        let github = Github::start(0).await;
        let app = app(&world, &github, ModuleConfig::new()).await;
        let (status, body, text) = owner(&world, &app, "POST", DEVICE, Some(&start())).await;
        assert_eq!(
            (
                status,
                body["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("--token")
            ),
            (StatusCode::BAD_REQUEST, true),
            "{text}"
        );
    }
}

mod device_poll {
    use super::*;

    // TC: GIT-01
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_while_person_has_not_confirmed() {
        let world = World::new().await;
        let github = Github::start(1).await;
        let app = app(&world, &github, with_oauth()).await;
        let (status, body, text) = owner(&world, &app, "POST", POLL, Some(&poll())).await;
        assert_eq!(
            (status, body),
            (StatusCode::ACCEPTED, json!({"status": "pending"})),
            "{text}"
        );
    }

    // TC: GIT-01 (RFC 8628 3.5: GitHub's `slow_down` reaches the CLI with
    // its new interval)
    #[tokio::test(flavor = "multi_thread")]
    async fn slow_down_with_github_interval_when_github_asks_to_slow_down() {
        let world = World::new().await;
        let github = Github::start(1).await;
        github.slow_down(1);
        let app = app(&world, &github, with_oauth()).await;
        let (status, body, text) = owner(&world, &app, "POST", POLL, Some(&poll())).await;
        let pending: GitDevicePending = serde_json::from_value(body).unwrap();
        assert_eq!(
            (status, pending),
            (
                StatusCode::ACCEPTED,
                GitDevicePending {
                    status: "slow_down".to_owned(),
                    interval: Some(10),
                }
            ),
            "{text}"
        );
    }

    // TC: GIT-01 (the device code is validated at the boundary: an empty
    // one never reaches the module)
    #[tokio::test(flavor = "multi_thread")]
    async fn bad_request_naming_device_code_when_it_is_empty() {
        let world = World::new().await;
        let github = Github::start(1).await;
        let app = app(&world, &github, with_oauth()).await;
        let empty = json!({"provider": "github", "device_code": ""});
        let (status, body, text) = owner(&world, &app, "POST", POLL, Some(&empty)).await;
        assert_eq!(
            (
                status,
                body["detail"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("device_code")
            ),
            (StatusCode::BAD_REQUEST, true),
            "{text}"
        );
    }

    // TC: GIT-01 (the granted token is stored as `POST /git/connections`
    // stores one, kind `oauth`)
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_oauth_connection_when_person_confirmed() {
        let world = World::new().await;
        let github = Github::start(1).await;
        let app = app(&world, &github, with_oauth()).await;
        let (first, _, _) = owner(&world, &app, "POST", POLL, Some(&poll())).await;
        let (status, body, text) = owner(&world, &app, "POST", POLL, Some(&poll())).await;
        assert_eq!(
            (first, status),
            (StatusCode::ACCEPTED, StatusCode::CREATED),
            "{text}"
        );
        let connection: ConnectionView = serde_json::from_value(body).unwrap();
        let stored = world
            .ports
            .store
            .git_connection(world.owner.id, &github_com())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                connection,
                world.ports.secrets.decrypt(&stored.credential).unwrap(),
            ),
            (
                ConnectionView {
                    provider: "github".to_owned(),
                    host: GITHUB.to_owned(),
                    account_login: "alice".to_owned(),
                    kind: "oauth".to_owned(),
                    expires_at: None,
                },
                DEVICE_TOKEN.to_owned(),
            )
        );
    }

    // TC: GIT-02 (the granted token is checked like a pasted one)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_and_nothing_stored_when_granted_token_lacks_repo() {
        let world = World::new().await;
        let github = Github::start(0).await;
        github.accept(DEVICE_TOKEN, "alice", "read:packages");
        let app = app(&world, &github, with_oauth()).await;
        let (status, _, text) = owner(&world, &app, "POST", POLL, Some(&poll())).await;
        let stored = world
            .ports
            .store
            .git_connection(world.owner.id, &github_com())
            .await
            .unwrap();
        assert!(
            status == StatusCode::FORBIDDEN && stored.is_none(),
            "{status} {text} {stored:?}"
        );
    }
}

mod create {
    use super::*;

    // TC: GIT-02 (without an OAuth application the CLI sends a token, checked with one call `/user`)
    #[tokio::test(flavor = "multi_thread")]
    async fn created_when_github_token_has_repo_scope() {
        let world = World::new().await;
        let github = Github::start(0).await;
        github.accept("ghp_alice", "Alice", "repo, read:packages, gist");
        let app = app(&world, &github, ModuleConfig::new()).await;
        let body = json!({"provider": "github", "host": GITHUB, "token": "ghp_alice"});
        let (status, body, text) = owner(&world, &app, "POST", CONNECTIONS, Some(&body)).await;
        assert_eq!(
            (status, body),
            (
                StatusCode::CREATED,
                json!({
                    "provider": "github", "host": GITHUB, "account_login": "alice",
                    "kind": "token", "expires_at": null,
                })
            ),
            "{text}"
        );
    }

    // TC: GIT-02
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_naming_needed_scopes_when_token_lacks_repo() {
        let world = World::new().await;
        let github = Github::start(0).await;
        github.accept("ghp_narrow", "alice", "gist");
        let app = app(&world, &github, ModuleConfig::new()).await;
        let body = json!({"provider": "github", "host": GITHUB, "token": "ghp_narrow"});
        let (status, body, text) = owner(&world, &app, "POST", CONNECTIONS, Some(&body)).await;
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            status == StatusCode::FORBIDDEN
                && detail.contains("repo")
                && detail.contains("read:packages"),
            "{status} {text}"
        );
    }
}
