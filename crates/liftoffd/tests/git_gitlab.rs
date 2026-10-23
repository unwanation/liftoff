#![cfg(test)]
//! Through the REST router: `lof git connect
//! gitlab` as the CLI sends it (GIT-14), the same routes as
//! GitHub's (`tests/git_github.rs`).
//!
//! The built `git.gitlab` component (`modules/git-gitlab`) is loaded by a
//! module host whose `wasi:http` to `gitlab.com` is routed to a fake
//! GitLab on loopback; the core's `ModuleGit` over it is the `Git` port of
//! the world's ports (`WithGit`), so the REST router runs the daemon's own
//! `GitService` against the component. The module's own cases are in
//! `crates/wasm/tests/git_gitlab.rs`.
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use liftoff_api::{ConnectionView, GitDevicePoll, GitDeviceStart, GitDeviceView};
use liftoff_core::domain::{Host, ModuleName};
use liftoff_core::fakes::{InMemoryStore, WithGit, World};
use liftoff_core::module::ModuleGit;
use liftoff_core::ports::{Secrets as _, Store as _};
use liftoff_rest::{AppState, RestConfig, Services, app_router};
use liftoff_wasm::testing::{Host as ModuleHostOf, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TokioTimer, TracingLog};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const GITLAB: &str = "gitlab.com";
const CLIENT_ID: &str = "3f1c7c0a9b0e4f52a6d1e8c2b7a94d10";
const DEVICE_CODE: &str = "dc-9f3e5a0b8c1d4e7f";
const USER_CODE: &str = "0A44L90H";
/// The token the device flow grants: `alice`'s.
const DEVICE_TOKEN: &str = "gloas-device";
/// A personal access token of `alice` with the scope `api`.
const PAT: &str = "glpat-alice";

const DEVICE: &str = "/api/v1/git/connections/gitlab.com/device";
const POLL: &str = "/api/v1/git/connections/gitlab.com/device/poll";
const CONNECTIONS: &str = "/api/v1/git/connections";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- The fake GitLab ------------------------------------------------------------

/// The fake GitLab on loopback.
#[derive(Debug, Clone, Copy)]
struct Gitlab {
    addr: core::net::SocketAddr,
}

impl Gitlab {
    /// The fake whose first `pending` polls answer
    /// `authorization_pending` before the grant.
    async fn start(pending: usize) -> Self {
        let pending = Arc::new(Mutex::new(pending));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().fallback(handle).with_state(pending);
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        Self { addr }
    }
}

fn json_reply(status: StatusCode, body: &Value) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn handle(
    State(pending): State<Arc<Mutex<usize>>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let body = String::from_utf8_lossy(&body).into_owned();
    match uri.path() {
        "/oauth/authorize_device" if body.contains(CLIENT_ID) => json_reply(
            StatusCode::OK,
            &json!({
                "device_code": DEVICE_CODE, "user_code": USER_CODE,
                "verification_uri": "https://gitlab.com/oauth/device",
                "verification_uri_complete": format!("https://gitlab.com/oauth/device?user_code={USER_CODE}"),
                "expires_in": 300, "interval": 5,
            }),
        ),
        "/oauth/token" => {
            let waiting = {
                let mut left = lock(&pending);
                let waiting = *left > 0;
                *left = left.saturating_sub(1);
                waiting
            };
            if waiting {
                return json_reply(
                    StatusCode::BAD_REQUEST,
                    &json!({"error": "authorization_pending", "error_description": "pending"}),
                );
            }
            json_reply(
                StatusCode::OK,
                &json!({
                    "access_token": DEVICE_TOKEN, "token_type": "Bearer", "expires_in": 7200,
                    "refresh_token": "gloas-refresh", "scope": "api", "created_at": 1_760_000_000,
                }),
            )
        }
        "/api/v4/user" => {
            let token = headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or_default();
            if token == DEVICE_TOKEN || token == PAT {
                json_reply(
                    StatusCode::OK,
                    &json!({"id": 1, "username": "alice", "state": "active"}),
                )
            } else {
                json_reply(
                    StatusCode::UNAUTHORIZED,
                    &json!({"message": "401 Unauthorized"}),
                )
            }
        }
        _ => json_reply(StatusCode::NOT_FOUND, &json!({"message": "404 Not Found"})),
    }
}

// --- The API over the component --------------------------------------------------

/// The API over the world's fakes with the `Git` port served by the
/// built `git.gitlab` component loaded with `config`, against `gitlab`.
async fn app(world: &World, gitlab: &Gitlab, config: ModuleConfig) -> Router {
    let host: ModuleHostOf<InMemoryStore> =
        ModuleHostOf::new(InMemoryStore::new(), TokioTimer, TracingLog, limits())
            .unwrap()
            .routing_https_to(GITLAB, gitlab.addr);
    let module = host
        .load(
            ModuleName::new("git.gitlab").unwrap(),
            &component("git_gitlab").unwrap(),
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

/// `method path` with a JSON `body` as the world's owner: the status, the
/// body and its text.
async fn owner(
    world: &World,
    app: &Router,
    method: &str,
    path: &str,
    body: &Value,
) -> (StatusCode, Value, String) {
    let secret = world.login(&world.owner).await.unwrap();
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {}", secret.expose()))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::Null),
        text,
    )
}

fn start() -> Value {
    serde_json::to_value(GitDeviceStart {
        provider: "gitlab".to_owned(),
    })
    .unwrap()
}

fn poll() -> Value {
    serde_json::to_value(GitDevicePoll {
        provider: "gitlab".to_owned(),
        device_code: DEVICE_CODE.to_owned(),
    })
    .unwrap()
}

mod device {
    use super::*;

    // TC: GIT-14 (with an OAuth application the CLI runs GitLab's device flow; read as the wire type the CLI reads)
    #[tokio::test(flavor = "multi_thread")]
    async fn started_with_code_of_gitlab_when_client_id_configured() {
        let world = World::new().await;
        let gitlab = Gitlab::start(0).await;
        let app = app(&world, &gitlab, with_oauth()).await;
        let (status, body, text) = owner(&world, &app, "POST", DEVICE, &start()).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let device: GitDeviceView = serde_json::from_value(body).unwrap();
        assert_eq!(
            device,
            GitDeviceView {
                device_code: DEVICE_CODE.to_owned(),
                user_code: USER_CODE.to_owned(),
                verification_uri: "https://gitlab.com/oauth/device".to_owned(),
                interval: 5,
                expires_in: 300,
            }
        );
    }

    // TC: GIT-14 (GitLab's `400 authorization_pending` reaches the CLI as
    // `202 pending`)
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_while_person_has_not_confirmed() {
        let world = World::new().await;
        let gitlab = Gitlab::start(1).await;
        let app = app(&world, &gitlab, with_oauth()).await;
        let (status, body, text) = owner(&world, &app, "POST", POLL, &poll()).await;
        assert_eq!(
            (status, body),
            (StatusCode::ACCEPTED, json!({"status": "pending"})),
            "{text}"
        );
    }

    // TC: GIT-14 (the granted token is stored as `POST /git/connections`
    // stores one, kind `oauth`)
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_oauth_connection_of_gitlab_when_person_confirmed() {
        let world = World::new().await;
        let gitlab = Gitlab::start(0).await;
        let app = app(&world, &gitlab, with_oauth()).await;
        let (status, body, text) = owner(&world, &app, "POST", POLL, &poll()).await;
        assert_eq!(status, StatusCode::CREATED, "{text}");
        let connection: ConnectionView = serde_json::from_value(body).unwrap();
        let stored = world
            .ports
            .store
            .git_connection(world.owner.id, &Host::new(GITLAB).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                connection.provider.as_str(),
                connection.host.as_str(),
                connection.account_login.as_str(),
                connection.kind.as_str(),
                world.ports.secrets.decrypt(&stored.credential).unwrap(),
            ),
            ("gitlab", GITLAB, "alice", "oauth", DEVICE_TOKEN.to_owned())
        );
    }
}

mod create {
    use super::*;

    // TC: GIT-14 (without an OAuth application the CLI sends a token, checked with one call `/user`)
    #[tokio::test(flavor = "multi_thread")]
    async fn created_when_gitlab_token_is_valid() {
        let world = World::new().await;
        let gitlab = Gitlab::start(0).await;
        let app = app(&world, &gitlab, ModuleConfig::new()).await;
        let body = json!({"provider": "gitlab", "host": GITLAB, "token": PAT});
        let (status, body, text) = owner(&world, &app, "POST", CONNECTIONS, &body).await;
        assert_eq!(
            (status, body),
            (
                StatusCode::CREATED,
                json!({
                    "provider": "gitlab", "host": GITLAB, "account_login": "alice",
                    "kind": "token", "expires_at": null,
                })
            ),
            "{text}"
        );
    }
}
