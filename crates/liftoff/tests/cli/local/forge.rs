//! A fake GitHub REST API on loopback, the upstream of the `git.github`
//! module the CLI runs (its `wasi:http` to `https://api.github.com` and,
//! for the device flow, `https://github.com` is routed here).
//!
//! | Request | Answer |
//! |---|---|
//! | `POST /login/device/code` | the codes of a flow, interval 5 s (no token needed) |
//! | `POST /login/oauth/access_token` | `authorization_pending`, then `slow_down` with interval 12, then [`TOKEN`](super::TOKEN) |
//! | `GET /user` | `{"login":"owner"}` with `X-OAuth-Scopes: repo, read:packages` for [`TOKEN`](super::TOKEN), `401` for anything else or after [`FakeGithub::reject_tokens`] |
//! | `GET /repos/{o}/{n}` | `default_branch` `main`, admin `permissions` |
//! | `GET /repos/{o}/{n}/contents/liftoff.toml` | the template's manifest in base64, `404` for a repository that is no template |
//! | `POST /repos/{t}/generate` | `201` `{"full_name":"<owner>/<name>"}` of the body |
//! | anything else | `404` |

use core::net::SocketAddr;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

/// The authority of GitHub's REST API.
pub(crate) const API: &str = "api.github.com";

/// The authority of GitHub's OAuth device flow.
pub(crate) const WEB: &str = "github.com";

/// The user code the fake's device flow shows.
pub(crate) const USER_CODE: &str = "WDJB-MJHT";

/// The template repository of the tests.
pub(crate) const TEMPLATE: &str = "acme/app";

/// Its `liftoff.toml`: a compose service, one generated secret,
/// and `{{project}}` substituted in `README.md`.
pub(crate) const TEMPLATE_MANIFEST: &str = r#"[template]
group = "web"
name = "acme-app"
description = "SolidJS + acme-ui, Vite, Bun"

[service]
compose = "deploy/compose.yaml"
expose = "web:3000"

[secrets]
SESSION_SECRET = { hint = "32+ random bytes", generate = "random:32" }

[substitute]
files = ["README.md"]
"#;

/// A request the fake received.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) authorization: Option<String>,
    pub(crate) body: String,
}

impl Seen {
    /// The body as JSON (`Null` when it is none).
    pub(crate) fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Debug, Default)]
struct Shared {
    seen: Mutex<Vec<Seen>>,
    rejects: Mutex<bool>,
    templates: Mutex<BTreeMap<String, String>>,
    polls: Mutex<usize>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The fake.
#[derive(Debug, Clone)]
pub(crate) struct FakeGithub {
    /// Where it listens.
    pub(crate) addr: SocketAddr,
    shared: Arc<Shared>,
}

impl FakeGithub {
    /// Starts it with the template [`TEMPLATE`].
    pub(crate) async fn start() -> Self {
        let shared = Arc::new(Shared::default());
        let _none =
            lock(&shared.templates).insert(TEMPLATE.to_owned(), TEMPLATE_MANIFEST.to_owned());
        let app = Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&shared));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        Self { addr, shared }
    }

    /// The fake also serves `repo` as a template whose `liftoff.toml` is
    /// `manifest`.
    pub(crate) fn add_template(&self, repo: &str, manifest: &str) {
        let _previous = lock(&self.shared.templates).insert(repo.to_owned(), manifest.to_owned());
    }

    /// From now on every token is rejected (`401`).
    pub(crate) fn reject_tokens(&self) {
        *lock(&self.shared.rejects) = true;
    }

    /// The requests seen for `method` and `path`.
    pub(crate) fn seen_at(&self, method: &str, path: &str) -> Vec<Seen> {
        lock(&self.shared.seen)
            .iter()
            .filter(|seen| seen.method == method && seen.path == path)
            .cloned()
            .collect()
    }
}

fn answer(status: StatusCode, body: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// GitHub's device flow (RFC 8628): the codes, then one pending poll, one
/// `slow_down` and the grant.
fn device_flow(shared: &Shared, seen: &Seen) -> Option<Response> {
    match (seen.method.as_str(), seen.path.as_str()) {
        ("POST", "/login/device/code") => Some(answer(
            StatusCode::OK,
            &json!({
                "device_code": "dc-1",
                "user_code": USER_CODE,
                "verification_uri": "https://github.com/login/device",
                "interval": 5,
                "expires_in": 900,
            }),
        )),
        ("POST", "/login/oauth/access_token") => {
            let poll = {
                let mut polls = lock(&shared.polls);
                *polls = polls.saturating_add(1);
                *polls
            };
            let body = match poll {
                1 => json!({"error": "authorization_pending"}),
                2 => json!({"error": "slow_down", "interval": 12}),
                _ => json!({
                    "access_token": super::TOKEN,
                    "token_type": "bearer",
                    "scope": "repo,read:packages",
                }),
            };
            Some(answer(StatusCode::OK, &body))
        }
        _ => None,
    }
}

fn not_found() -> Response {
    answer(StatusCode::NOT_FOUND, &json!({"message": "Not Found"}))
}

async fn handle(
    State(shared): State<Arc<Shared>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let seen = Seen {
        method: method.to_string(),
        path: uri.path().to_owned(),
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    lock(&shared.seen).push(seen.clone());
    if let Some(answer) = device_flow(&shared, &seen) {
        return answer;
    }
    let accepted = !*lock(&shared.rejects)
        && seen.authorization.as_deref() == Some(&format!("Bearer {}", super::TOKEN));
    if !accepted {
        return answer(
            StatusCode::UNAUTHORIZED,
            &json!({"message": "Bad credentials"}),
        );
    }
    let segments: Vec<&str> = seen.path.trim_matches('/').split('/').collect();
    match (seen.method.as_str(), segments.as_slice()) {
        ("GET", ["user"]) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-oauth-scopes", "repo, read:packages")
            .body(Body::from(json!({"login": "owner", "id": 1}).to_string()))
            .unwrap(),
        ("GET", ["repos", owner, name]) => answer(
            StatusCode::OK,
            &json!({
                "full_name": format!("{owner}/{name}"),
                "default_branch": "main",
                "permissions": {"admin": true, "push": true, "pull": true},
            }),
        ),
        ("GET", ["repos", owner, name, "contents", "liftoff.toml"]) => lock(&shared.templates)
            .get(&format!("{owner}/{name}"))
            .map_or_else(not_found, |manifest| {
                answer(
                    StatusCode::OK,
                    &json!({
                        "name": "liftoff.toml",
                        "encoding": "base64",
                        "content": STANDARD.encode(manifest),
                    }),
                )
            }),
        ("POST", ["repos", _, _, "generate"]) => {
            let body = seen.json();
            let owner = body.get("owner").and_then(Value::as_str).unwrap_or("owner");
            let name = body.get("name").and_then(Value::as_str).unwrap_or_default();
            answer(
                StatusCode::CREATED,
                &json!({"full_name": format!("{owner}/{name}"), "default_branch": "main"}),
            )
        }
        _ => not_found(),
    }
}
