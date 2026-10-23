#![cfg(test)]
//! The `git.github` module (GIT-01…08).
//!
//! The built `modules/git-github` component runs in a module host; the
//! core's `ModuleGit` (the `Git` port served by the forge modules) calls
//! its tools through the host's `ModuleSet`. Its `wasi:http` reaches the
//! three hosts of its `http_allow` (`api.github.com`, `github.com`,
//! `ghcr.io`), which the host routes to a fake GitHub served by this test
//! on loopback, one listener per authority
//! (`ModuleHost::routing_https_to`).
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.
//!
//! The GitHub API these tests pin (always the user's own connection):
//!
//! | Port method | Request | Answer read |
//! |---|---|---|
//! | `account` | `GET api.github.com/user` | `login`; header `X-OAuth-Scopes`, when sent, must name `repo` and `read:packages` (or `write:packages`) |
//! | `repo_role` | `GET api.github.com/repos/{owner}/{name}` | `permissions.{admin,push,pull}`; `404` → none |
//! | `image_exists` | `GET ghcr.io/token?scope=repository:{owner}/{name}:pull&service=ghcr.io` (Basic `login:token`), then `HEAD ghcr.io/v2/{owner}/{name}/manifests/{tag}` (Bearer the registry token) | `200` → true, `404` → false |
//! | `put_ci_secret` | `GET …/repos/{r}/actions/secrets/public-key`, then `PUT …/repos/{r}/actions/secrets/{KEY}` `{"encrypted_value","key_id"}` | the value in a libsodium sealed box of the repository's key |
//! | `list_templates` | `GET …/repos/{r}/contents/liftoff.toml` per repository | `content` (base64 in lines); `404` → no manifest |
//! | `generate_from_template` | `POST …/repos/{t}/generate` `{"owner","name","private","include_all":true}` | `full_name` |
//! | `device_start` | `POST github.com/login/device/code` `client_id`, `scope=repo read:packages` | RFC 8628 3.2 |
//! | `device_poll` | `POST github.com/login/oauth/access_token` `client_id`, `device_code`, `grant_type` | `authorization_pending` → pending, `slow_down` → slow down to its `interval`, `access_token` → granted |
//!
//! A `5xx` of an idempotent request is retried once, then `Upstream`
//! (GIT-08); without `client_id` the device flow is refused with a hint
//! to connect with a token (GIT-01).

use core::net::SocketAddr;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use liftoff_core::domain::{
    DeviceCode, GitProvider, Host, ImageTag, Login, ModuleName, RepoRef, SecretKey, SecretValue,
};
use liftoff_core::fakes::InMemoryStore;
use liftoff_core::module::{GitProviderSpec, ModuleGit};
use liftoff_core::ports::{
    DeviceGrant, DeviceStart, Git as _, GitAccess, NewRepo, PackageRef, RepoRole, Template,
};
use liftoff_core::{Error, System};
use liftoff_wasm::testing::{Host as ModuleHostOf, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TracingLog};
use serde_json::{Value, json};

/// The forge host the module serves.
const GITHUB: &str = "github.com";
/// The REST API of [`GITHUB`].
const API: &str = "api.github.com";
/// The container registry of [`GITHUB`].
const GHCR: &str = "ghcr.io";
/// The OAuth application of the module's config (`client_id`).
const CLIENT_ID: &str = "Iv1.liftofftest00001";
/// The token of `owner`'s connection, with the scopes it needs.
const OWNER_TOKEN: &str = "ghp_owner";
/// What GitHub answers in `X-OAuth-Scopes` for a token that may do
/// everything the module needs.
const ALL_SCOPES: &str = "repo, read:packages";
/// The repository of the world's project, administered by `owner`.
const SHOP: &str = "owner/shop";
/// The device code the fake hands out.
const DEVICE_CODE: &str = "dc-3584d83530557fdd1f46af8289938c8ef79f9dc5";
/// The user code the fake hands out.
const USER_CODE: &str = "WDJB-MJHT";
/// The token the fake grants after the person confirmed the device code.
const DEVICE_TOKEN: &str = "gho_device";
/// The `key_id` of every repository's Actions public key.
const KEY_ID: &str = "568250167242549743";
/// The repository's Actions secret key, which only the fake holds.
const REPO_SECRET_KEY: [u8; 32] = [7; 32];

fn module() -> ModuleName {
    ModuleName::new("git.github").unwrap()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- The fake GitHub ------------------------------------------------------------

/// A request the fake received.
#[derive(Debug, Clone)]
struct Seen {
    /// The authority it was sent to (`api.github.com`, `github.com`,
    /// `ghcr.io`, or a sink's).
    authority: String,
    method: String,
    /// The path, without the query.
    path: String,
    /// The query decoded into pairs.
    query: BTreeMap<String, String>,
    authorization: Option<String>,
    user_agent: Option<String>,
    accept: Option<String>,
    /// The raw body.
    body: String,
}

impl Seen {
    /// The body as JSON (`Null` when it is none).
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// The parameters of a JSON or `application/x-www-form-urlencoded`
    /// body: GitHub's OAuth endpoints take either.
    fn params(&self) -> BTreeMap<String, String> {
        if let Value::Object(fields) = self.json() {
            return fields
                .into_iter()
                .map(|(key, value)| {
                    let text = value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned);
                    (key, text)
                })
                .collect();
        }
        pairs(&self.body)
    }
}

/// `a=b&c=d` decoded (`+` is a space).
fn pairs(raw: &str) -> BTreeMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// Percent-decoding of a form or query component.
fn decode(text: &str) -> String {
    let spaced = text.replace('+', " ");
    let mut parts = spaced.split('%');
    let mut out = parts.next().unwrap_or_default().as_bytes().to_vec();
    for part in parts {
        let (hex, rest) = part.split_at_checked(2).unwrap();
        out.push(u8::from_str_radix(hex, 16).unwrap());
        out.extend_from_slice(rest.as_bytes());
    }
    String::from_utf8(out).unwrap()
}

/// What an account may do on a repository, as GitHub's `permissions`.
#[derive(Debug, Clone, Copy)]
struct Perms {
    pull: bool,
    push: bool,
    admin: bool,
}

const ADMIN: Perms = Perms {
    pull: true,
    push: true,
    admin: true,
};
const WRITE: Perms = Perms {
    pull: true,
    push: true,
    admin: false,
};
const READ: Perms = Perms {
    pull: true,
    push: false,
    admin: false,
};

/// How the fake answers the polls of the device flow, in order; the last
/// one repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Poll {
    Pending,
    /// `slow_down` with GitHub's new `interval`, [`SLOWED_INTERVAL`].
    SlowDown,
    Granted,
    Expired,
    Denied,
}

/// The `interval` GitHub sends with `slow_down`: the 5 s of the flow plus
/// the 5 s RFC 8628 3.5 adds.
const SLOWED_INTERVAL: u64 = 10;

#[derive(Debug)]
struct GithubState {
    /// Token → (account login as GitHub spells it, `X-OAuth-Scopes`;
    /// `None` for a fine-grained token, whose answers carry no such
    /// header).
    tokens: BTreeMap<String, (String, Option<String>)>,
    /// (login, `owner/name`) → permissions; absent → `404`.
    perms: BTreeMap<(String, String), Perms>,
    /// (login, `owner/name:tag`) of the images whose manifest the
    /// account can read in the registry.
    images: BTreeSet<(String, String)>,
    /// `owner/name` → its `liftoff.toml` on the default branch.
    manifests: BTreeMap<String, String>,
    /// (`owner/name`, key) → the value the fake opened from the sealed box.
    secrets: BTreeMap<(String, String), String>,
    /// The answers of the next polls.
    polls: Vec<Poll>,
    /// How many of the next requests answer `503`.
    failing: usize,
    /// (method, path) of requests that answer `503`, each once.
    failing_on: Vec<(String, String)>,
    seen: Vec<Seen>,
}

impl Default for GithubState {
    fn default() -> Self {
        Self {
            tokens: BTreeMap::new(),
            perms: BTreeMap::new(),
            images: BTreeSet::new(),
            manifests: BTreeMap::new(),
            secrets: BTreeMap::new(),
            polls: vec![Poll::Pending, Poll::Granted],
            failing: 0,
            failing_on: Vec::new(),
            seen: Vec::new(),
        }
    }
}

/// GitHub on `127.0.0.1`: one listener per authority the module reaches.
#[derive(Debug, Clone)]
struct Github {
    /// Authority → the listener the host routes it to.
    addrs: BTreeMap<String, SocketAddr>,
    state: Arc<Mutex<GithubState>>,
}

/// The state of one listener: which authority it plays.
#[derive(Debug, Clone)]
struct Plays {
    authority: String,
    state: Arc<Mutex<GithubState>>,
}

impl Github {
    /// The fake playing `authorities`, knowing `owner`'s token (admin of
    /// `owner/shop`) and the token the device flow grants (`alice`'s).
    async fn start(authorities: &[&str]) -> Self {
        let state = Arc::new(Mutex::new(GithubState::default()));
        let mut addrs = BTreeMap::new();
        for authority in authorities {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let _previous = addrs.insert((*authority).to_owned(), listener.local_addr().unwrap());
            let app = axum::Router::new().fallback(handle).with_state(Plays {
                authority: (*authority).to_owned(),
                state: Arc::clone(&state),
            });
            drop(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
        }
        let fake = Self { addrs, state };
        fake.accept(OWNER_TOKEN, "owner", ALL_SCOPES);
        fake.accept(DEVICE_TOKEN, "alice", ALL_SCOPES);
        fake.grant("owner", SHOP, ADMIN);
        fake
    }

    /// `token` logs in as `login` with the scopes `scopes`.
    fn accept(&self, token: &str, login: &str, scopes: &str) {
        let _previous = lock(&self.state).tokens.insert(
            token.to_owned(),
            (login.to_owned(), Some(scopes.to_owned())),
        );
    }

    /// `token` is a fine-grained token of `login`: GitHub sends no
    /// `X-OAuth-Scopes` with its answers.
    fn accept_fine_grained(&self, token: &str, login: &str) {
        let _previous = lock(&self.state)
            .tokens
            .insert(token.to_owned(), (login.to_owned(), None));
    }

    /// `login` has `perms` on `repo`.
    fn grant(&self, login: &str, repo: &str, perms: Perms) {
        let _previous = lock(&self.state)
            .perms
            .insert((login.to_owned(), repo.to_owned()), perms);
    }

    /// `login` can read the image `image` (`owner/name:tag`) in the
    /// registry.
    fn publish(&self, login: &str, image: &str) {
        let _new = lock(&self.state)
            .images
            .insert((login.to_owned(), image.to_owned()));
    }

    /// `repo` has `toml` as its `liftoff.toml`.
    fn manifest(&self, repo: &str, toml: &str) {
        let _previous = lock(&self.state)
            .manifests
            .insert(repo.to_owned(), toml.to_owned());
    }

    /// The polls of the device flow answer `polls` in order.
    fn polls(&self, polls: &[Poll]) {
        lock(&self.state).polls = polls.to_vec();
    }

    /// The next `times` requests answer `503`.
    fn fail_next(&self, times: usize) {
        lock(&self.state).failing = times;
    }

    /// The next `times` requests `method path` answer `503`; the others
    /// are answered as usual.
    fn fail_next_on(&self, method: &str, path: &str, times: usize) {
        lock(&self.state).failing_on.extend(core::iter::repeat_n(
            (method.to_owned(), path.to_owned()),
            times,
        ));
    }

    /// The CI secret `key` of `repo` as the fake opened it.
    fn secret(&self, repo: &str, key: &str) -> Option<String> {
        lock(&self.state)
            .secrets
            .get(&(repo.to_owned(), key.to_owned()))
            .cloned()
    }

    fn seen(&self) -> Vec<Seen> {
        lock(&self.state).seen.clone()
    }

    /// Authority, method and path of every request seen, in order.
    fn requests(&self) -> Vec<(String, String, String)> {
        self.seen()
            .into_iter()
            .map(|seen| (seen.authority, seen.method, seen.path))
            .collect()
    }

    /// The one request seen.
    fn only(&self) -> Seen {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "{:?}", self.requests());
        seen.into_iter().next().unwrap()
    }
}

/// `status` with a JSON `body`.
fn reply(status: StatusCode, body: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// GitHub's error body.
fn message(status: StatusCode, text: &str) -> Response {
    reply(
        status,
        &json!({ "message": text, "documentation_url": "https://docs.github.com/rest" }),
    )
}

/// `status` without a body.
fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

/// What GitHub's OAuth endpoints answer: JSON when asked for it, else
/// `application/x-www-form-urlencoded` (as GitHub does).
fn oauth(seen: &Seen, fields: &[(&str, &str)]) -> Response {
    let wants_json = seen
        .accept
        .as_deref()
        .is_some_and(|accept| accept.contains("application/json"));
    if wants_json {
        let object: serde_json::Map<String, Value> = fields
            .iter()
            .map(|(key, value)| {
                let value = value
                    .parse::<u64>()
                    .map_or_else(|_| Value::from(*value), Value::from);
                ((*key).to_owned(), value)
            })
            .collect();
        return reply(StatusCode::OK, &Value::Object(object));
    }
    let form = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap()
}

async fn handle(
    State(plays): State<Plays>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |name: header::HeaderName| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let seen = Seen {
        authority: plays.authority.clone(),
        method: method.to_string(),
        path: uri.path().to_owned(),
        query: pairs(uri.query().unwrap_or_default()),
        authorization: header(header::AUTHORIZATION),
        user_agent: header(header::USER_AGENT),
        accept: header(header::ACCEPT),
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    answer(&mut lock(&plays.state), &seen)
}

/// What GitHub answers the request `seen`, which it records.
fn answer(state: &mut GithubState, seen: &Seen) -> Response {
    state.seen.push(seen.clone());
    if state.failing > 0 {
        state.failing = state.failing.saturating_sub(1);
        return message(StatusCode::SERVICE_UNAVAILABLE, "unicorn: try again later");
    }
    if let Some(at) = state
        .failing_on
        .iter()
        .position(|(method, path)| *method == seen.method && *path == seen.path)
    {
        let _failed = state.failing_on.remove(at);
        return message(StatusCode::SERVICE_UNAVAILABLE, "unicorn: try again later");
    }
    match seen.authority.as_str() {
        GITHUB => oauth_route(state, seen),
        GHCR => registry_route(state, seen),
        API => {
            let token = seen
                .authorization
                .as_deref()
                .and_then(|value| {
                    value
                        .strip_prefix("Bearer ")
                        .or_else(|| value.strip_prefix("token "))
                })
                .unwrap_or_default();
            let Some((login, scopes)) = state.tokens.get(token).cloned() else {
                return message(StatusCode::UNAUTHORIZED, "Bad credentials");
            };
            api_route(state, seen, &login, scopes.as_deref())
        }
        // A sink of another forge's host (see `dispatch`).
        _ => message(StatusCode::NOT_FOUND, "Not Found"),
    }
}

/// `github.com`: the device flow (RFC 8628) of the OAuth application
/// [`CLIENT_ID`].
fn oauth_route(state: &mut GithubState, seen: &Seen) -> Response {
    let params = seen.params();
    if params.get("client_id").map(String::as_str) != Some(CLIENT_ID) {
        return message(StatusCode::NOT_FOUND, "Not Found");
    }
    match (seen.method.as_str(), seen.path.as_str()) {
        ("POST", "/login/device/code") => oauth(
            seen,
            &[
                ("device_code", DEVICE_CODE),
                ("user_code", USER_CODE),
                ("verification_uri", "https://github.com/login/device"),
                ("expires_in", "900"),
                ("interval", "5"),
            ],
        ),
        ("POST", "/login/oauth/access_token") => {
            if params.get("device_code").map(String::as_str) != Some(DEVICE_CODE)
                || params.get("grant_type").map(String::as_str)
                    != Some("urn:ietf:params:oauth:grant-type:device_code")
            {
                return oauth(seen, &[("error", "incorrect_device_code")]);
            }
            let poll = if state.polls.len() > 1 {
                state.polls.remove(0)
            } else {
                state.polls.first().copied().unwrap_or(Poll::Pending)
            };
            match poll {
                Poll::Pending => oauth(seen, &[("error", "authorization_pending")]),
                Poll::SlowDown => oauth(
                    seen,
                    &[
                        ("error", "slow_down"),
                        (
                            "error_description",
                            "Too many requests have been made in the same timeframe.",
                        ),
                        ("interval", &SLOWED_INTERVAL.to_string()),
                    ],
                ),
                Poll::Expired => oauth(seen, &[("error", "expired_token")]),
                Poll::Denied => oauth(seen, &[("error", "access_denied")]),
                Poll::Granted => oauth(
                    seen,
                    &[
                        ("access_token", DEVICE_TOKEN),
                        ("token_type", "bearer"),
                        ("scope", "repo,read:packages"),
                    ],
                ),
            }
        }
        _ => message(StatusCode::NOT_FOUND, "Not Found"),
    }
}

/// `ghcr.io`: the registry's token exchange and its manifests.
fn registry_route(state: &GithubState, seen: &Seen) -> Response {
    match (seen.method.as_str(), seen.path.as_str()) {
        ("GET", "/token") => {
            // `docker login ghcr.io -u <login> -p <token>`: Basic auth of
            // the connection's login and token.
            let basic = seen
                .authorization
                .as_deref()
                .and_then(|value| value.strip_prefix("Basic "))
                .and_then(|encoded| STANDARD.decode(encoded).ok())
                .and_then(|decoded| String::from_utf8(decoded).ok());
            let token = basic
                .as_deref()
                .and_then(|pair| pair.split_once(':'))
                .map(|(_login, token)| token.to_owned())
                .unwrap_or_default();
            if !state.tokens.contains_key(&token) {
                return reply(
                    StatusCode::UNAUTHORIZED,
                    &json!({"errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]}),
                );
            }
            reply(
                StatusCode::OK,
                &json!({ "token": format!("registry-{token}") }),
            )
        }
        ("HEAD" | "GET", path) => {
            let Some(rest) = path.strip_prefix("/v2/") else {
                return empty(StatusCode::NOT_FOUND);
            };
            let Some((image, tag)) = rest.rsplit_once("/manifests/") else {
                return empty(StatusCode::NOT_FOUND);
            };
            let token = seen
                .authorization
                .as_deref()
                .and_then(|value| value.strip_prefix("Bearer registry-"))
                .unwrap_or_default();
            let Some((login, _scopes)) = state.tokens.get(token) else {
                return empty(StatusCode::UNAUTHORIZED);
            };
            if state
                .images
                .contains(&(login.clone(), format!("{image}:{tag}")))
            {
                Response::builder()
                    .status(StatusCode::OK)
                    .header(
                        header::CONTENT_TYPE,
                        "application/vnd.oci.image.index.v1+json",
                    )
                    .header("docker-content-digest", "sha256:0123")
                    .body(Body::empty())
                    .unwrap()
            } else {
                empty(StatusCode::NOT_FOUND)
            }
        }
        _ => empty(StatusCode::NOT_FOUND),
    }
}

/// The repository's Actions public key, base64.
fn repo_public_key() -> String {
    STANDARD.encode(
        crypto_box::SecretKey::from(REPO_SECRET_KEY)
            .public_key()
            .as_bytes(),
    )
}

/// `api.github.com` as `login` with the token's `scopes` (`None`: a
/// fine-grained token).
#[expect(clippy::too_many_lines, reason = "one match over the fake's routes")]
fn api_route(state: &mut GithubState, seen: &Seen, login: &str, scopes: Option<&str>) -> Response {
    let segments: Vec<&str> = seen.path.trim_start_matches('/').split('/').collect();
    let perms = |state: &GithubState, owner: &str, name: &str| {
        state
            .perms
            .get(&(login.to_owned(), format!("{owner}/{name}")))
            .copied()
    };
    match (seen.method.as_str(), segments.as_slice()) {
        ("GET", ["user"]) => {
            let mut user = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8");
            if let Some(scopes) = scopes {
                user = user
                    .header("x-oauth-scopes", scopes)
                    .header("x-accepted-oauth-scopes", "");
            }
            user.body(Body::from(
                json!({ "login": login, "id": 1, "type": "User" }).to_string(),
            ))
            .unwrap()
        }
        ("GET", ["repos", owner, name]) => perms(state, owner, name).map_or_else(
            || message(StatusCode::NOT_FOUND, "Not Found"),
            |perms| {
                reply(
                    StatusCode::OK,
                    &json!({
                        "id": 1, "full_name": format!("{owner}/{name}"), "private": true,
                        "permissions": {
                            "admin": perms.admin, "maintain": perms.admin, "push": perms.push,
                            "triage": perms.push, "pull": perms.pull,
                        },
                    }),
                )
            },
        ),
        ("GET", ["repos", owner, name, "actions", "secrets", "public-key"]) => {
            match perms(state, owner, name) {
                Some(perms) if perms.admin => reply(
                    StatusCode::OK,
                    &json!({ "key_id": KEY_ID, "key": repo_public_key() }),
                ),
                Some(_) => message(
                    StatusCode::FORBIDDEN,
                    "Resource not accessible by integration",
                ),
                None => message(StatusCode::NOT_FOUND, "Not Found"),
            }
        }
        ("PUT", ["repos", owner, name, "actions", "secrets", key]) => {
            match perms(state, owner, name) {
                Some(perms) if perms.admin => {}
                Some(_) => {
                    return message(
                        StatusCode::FORBIDDEN,
                        "Resource not accessible by integration",
                    );
                }
                None => return message(StatusCode::NOT_FOUND, "Not Found"),
            }
            let body = seen.json();
            if body["key_id"].as_str() != Some(KEY_ID) {
                return message(StatusCode::UNPROCESSABLE_ENTITY, "Bad key_id");
            }
            let opened = body["encrypted_value"]
                .as_str()
                .and_then(|sealed| STANDARD.decode(sealed).ok())
                .and_then(|sealed| {
                    crypto_box::SecretKey::from(REPO_SECRET_KEY)
                        .unseal(&sealed)
                        .ok()
                })
                .and_then(|plain| String::from_utf8(plain).ok());
            let Some(value) = opened else {
                return message(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "encrypted_value is not a sealed box of the repository's key",
                );
            };
            let _previous = state
                .secrets
                .insert((format!("{owner}/{name}"), (*key).to_owned()), value);
            empty(StatusCode::CREATED)
        }
        ("GET", ["repos", owner, name, "contents", "liftoff.toml"]) => {
            let repo = format!("{owner}/{name}");
            let readable = perms(state, owner, name).is_some_and(|perms| perms.pull);
            match state.manifests.get(&repo) {
                Some(toml) if readable => {
                    // GitHub wraps the base64 content in lines of 60.
                    let encoded = STANDARD.encode(toml);
                    let mut wrapped = encoded
                        .as_bytes()
                        .chunks(60)
                        .map(String::from_utf8_lossy)
                        .collect::<Vec<_>>()
                        .join("\n");
                    wrapped.push('\n');
                    reply(
                        StatusCode::OK,
                        &json!({
                            "type": "file", "name": "liftoff.toml", "path": "liftoff.toml",
                            "encoding": "base64", "content": wrapped,
                        }),
                    )
                }
                _ => message(StatusCode::NOT_FOUND, "Not Found"),
            }
        }
        ("POST", ["repos", template_owner, template_name, "generate"]) => {
            if perms(state, template_owner, template_name).is_none() {
                return message(StatusCode::NOT_FOUND, "Not Found");
            }
            let body = seen.json();
            let owner = body["owner"].as_str().unwrap_or(login).to_owned();
            let name = body["name"].as_str().unwrap_or_default().to_owned();
            let _previous = state
                .perms
                .insert((login.to_owned(), format!("{owner}/{name}")), ADMIN);
            reply(
                StatusCode::CREATED,
                &json!({
                    "id": 9, "full_name": format!("{owner}/{name}"),
                    "private": body["private"],
                }),
            )
        }
        _ => message(StatusCode::NOT_FOUND, "Not Found"),
    }
}

// --- The module under test -------------------------------------------------------

type Forge = ModuleGit<ModuleSet<InMemoryStore, TracingLog>>;

/// The module's configuration without an OAuth application.
fn no_oauth() -> ModuleConfig {
    ModuleConfig::new()
}

/// The module's configuration with the OAuth application [`CLIENT_ID`].
fn with_oauth() -> ModuleConfig {
    core::iter::once(("client_id".to_owned(), CLIENT_ID.to_owned())).collect()
}

/// The built `git.github` component loaded with `config` and migrated
/// from 0, as an install does, its three hosts routed to a fresh fake;
/// the core's `Git` port over it.
async fn forge_with(config: ModuleConfig) -> (Forge, Github) {
    let github = Github::start(&[API, GITHUB, GHCR]).await;
    let mut host: ModuleHostOf<InMemoryStore> = ModuleHostOf::new(
        InMemoryStore::new(),
        liftoff_wasm::TokioTimer,
        TracingLog,
        limits(),
    )
    .unwrap();
    for (authority, addr) in &github.addrs {
        host = host.routing_https_to(authority, *addr);
    }
    let module = host
        .load(module(), &component("git_github").unwrap(), config)
        .await
        .unwrap();
    module.migrate(0).await.unwrap();
    let set = ModuleSet::new();
    set.insert(module);
    (ModuleGit::new(Arc::clone(host.registry()), set), github)
}

/// [`forge_with`] without an OAuth application: tokens only.
async fn forge() -> (Forge, Github) {
    forge_with(no_oauth()).await
}

fn host(raw: &str) -> Host {
    Host::new(raw).unwrap()
}

fn github_com() -> Host {
    host(GITHUB)
}

/// The device code the fake hands out, as the core holds it.
fn device_code() -> DeviceCode {
    DeviceCode::new(DEVICE_CODE).unwrap()
}

fn repo(raw: &str) -> RepoRef {
    RepoRef::new(raw).unwrap()
}

/// `login`'s connection on `on` with `token`.
fn access_on(on: &str, login: &str, token: &str) -> GitAccess {
    GitAccess {
        provider: GitProvider::Github,
        host: host(on),
        login: Login::new(login).unwrap(),
        token: SecretValue::new(token.to_owned()),
    }
}

/// `owner`'s connection on `github.com`.
fn owner() -> GitAccess {
    access_on(GITHUB, "owner", OWNER_TOKEN)
}

fn image(owner: &str, name: &str, tag: &str) -> PackageRef {
    PackageRef {
        owner: owner.to_owned(),
        name: name.to_owned(),
        tag: ImageTag::new(tag).unwrap(),
    }
}

fn request(authority: &str, method: &str, path: &str) -> (String, String, String) {
    (authority.to_owned(), method.to_owned(), path.to_owned())
}

/// The words of `text`: what a message lists, without its punctuation.
fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

mod manifest {
    use super::*;

    // TC: GIT-01 (without `client_id` the module connects by
    // token only, so the CLI asks for one)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_github_com_without_oauth_when_no_client_id() {
        let (git, _github) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        assert_eq!(
            manifest.git_provider,
            Some(GitProviderSpec {
                hosts: vec![GITHUB.to_owned()],
                oauth: false,
            })
        );
    }

    // TC: GIT-01 (with an OAuth application the CLI runs the device flow)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_oauth_when_client_id_configured() {
        let (git, _github) = forge_with(with_oauth()).await;
        let manifest = git.registry().manifest(&module()).unwrap();
        assert_eq!(
            manifest.git_provider,
            Some(GitProviderSpec {
                hosts: vec![GITHUB.to_owned()],
                oauth: true,
            })
        );
    }

    // TC: GIT-01 (`client_id` is a secret and optional)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_client_id_as_optional_secret_when_loaded() {
        let (git, _github) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        let schema = manifest.config_schema.unwrap_or(Value::Null);
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(
            (
                schema["properties"]["client_id"]["format"].as_str(),
                required.contains(&"client_id"),
            ),
            (Some("secret"), false),
            "{schema}"
        );
    }

    // TC: GIT-01 (`http_allow` names the API, the OAuth host
    // and the registry, MOD-07)
    #[tokio::test(flavor = "multi_thread")]
    async fn allows_http_to_api_oauth_and_registry_hosts_when_loaded() {
        let (git, _github) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        let allowed: Vec<String> = manifest
            .http_allow
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            allowed,
            vec![API.to_owned(), GITHUB.to_owned(), GHCR.to_owned()]
        );
    }

    // TC: GIT-02 (the forge tools take a plaintext credential, so they are
    // not MCP tools, as with `git.forgejo`)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_no_mcp_tools_when_loaded() {
        let (git, _github) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        assert!(manifest.tools.is_empty(), "{:?}", manifest.tools);
    }
}

mod device_flow {
    use super::*;

    // TC: GIT-01
    #[tokio::test(flavor = "multi_thread")]
    async fn validation_with_token_hint_without_request_when_no_client_id() {
        let (git, github) = forge().await;
        let err = git
            .device_start(GitProvider::Github, &github_com())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Validation(why) if why.to_string().contains("--token"))
                && github.seen().is_empty(),
            "{err:?} after {:?}",
            github.requests()
        );
    }

    // TC: GIT-01 (the forge's device flow when an OAuth application is configured)
    #[tokio::test(flavor = "multi_thread")]
    async fn device_code_of_github_when_client_id_configured() {
        let (git, _github) = forge_with(with_oauth()).await;
        let started = git
            .device_start(GitProvider::Github, &github_com())
            .await
            .unwrap();
        assert_eq!(
            started,
            DeviceStart {
                device_code: device_code(),
                user_code: USER_CODE.to_owned(),
                verification_uri: "https://github.com/login/device".to_owned(),
                interval: 5,
                expires_in: 900,
            }
        );
    }

    // TC: GIT-01 (the scopes the module needs: `repo` and `read:packages`)
    #[tokio::test(flavor = "multi_thread")]
    async fn asks_client_id_and_scopes_at_login_device_code_when_started() {
        let (git, github) = forge_with(with_oauth()).await;
        let _started = git
            .device_start(GitProvider::Github, &github_com())
            .await
            .unwrap();
        let seen = github.only();
        let params = seen.params();
        let scopes: BTreeSet<String> = params
            .get("scope")
            .map(|scope| words(scope))
            .unwrap_or_default();
        assert_eq!(
            (
                seen.authority.as_str(),
                seen.method.as_str(),
                seen.path.as_str(),
                params.get("client_id").map(String::as_str),
                scopes,
            ),
            (
                GITHUB,
                "POST",
                "/login/device/code",
                Some(CLIENT_ID),
                ["read:packages".to_owned(), "repo".to_owned()].into(),
            )
        );
    }

    // TC: GIT-01
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_when_person_has_not_confirmed() {
        let (git, github) = forge_with(with_oauth()).await;
        let grant = git
            .device_poll(GitProvider::Github, &github_com(), &device_code())
            .await
            .unwrap();
        let seen = github.only();
        let params = seen.params();
        assert_eq!(
            (
                grant,
                seen.path.as_str(),
                params.get("client_id").map(String::as_str),
                params.get("device_code").map(String::as_str),
                params.get("grant_type").map(String::as_str),
            ),
            (
                DeviceGrant::Pending,
                "/login/oauth/access_token",
                Some(CLIENT_ID),
                Some(DEVICE_CODE),
                Some("urn:ietf:params:oauth:grant-type:device_code"),
            )
        );
    }

    // TC: GIT-01
    #[tokio::test(flavor = "multi_thread")]
    async fn granted_token_when_person_confirmed() {
        let (git, github) = forge_with(with_oauth()).await;
        github.polls(&[Poll::Granted]);
        let grant = git
            .device_poll(GitProvider::Github, &github_com(), &device_code())
            .await
            .unwrap();
        assert_eq!(
            grant,
            DeviceGrant::Granted {
                token: SecretValue::new(DEVICE_TOKEN.to_owned()),
                expires_in: None,
            }
        );
    }

    // TC: GIT-01 (RFC 8628 3.5 `slow_down`: GitHub raised the interval
    // the CLI must keep from now on)
    #[tokio::test(flavor = "multi_thread")]
    async fn slow_down_to_github_interval_when_github_asks_to_slow_down() {
        let (git, github) = forge_with(with_oauth()).await;
        github.polls(&[Poll::SlowDown, Poll::Pending]);
        let grant = git
            .device_poll(GitProvider::Github, &github_com(), &device_code())
            .await
            .unwrap();
        assert_eq!(
            grant,
            DeviceGrant::SlowDown {
                interval: Some(SLOWED_INTERVAL)
            }
        );
    }

    // TC: GIT-01 (RFC 8628 3.5 `expired_token`)
    #[tokio::test(flavor = "multi_thread")]
    async fn gone_when_device_code_expired() {
        let (git, github) = forge_with(with_oauth()).await;
        github.polls(&[Poll::Expired]);
        let err = git
            .device_poll(GitProvider::Github, &github_com(), &device_code())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Gone(_)), "{err:?}");
    }

    // TC: GIT-01 (RFC 8628 3.5 `access_denied`)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_person_denied() {
        let (git, github) = forge_with(with_oauth()).await;
        github.polls(&[Poll::Denied]);
        let err = git
            .device_poll(GitProvider::Github, &github_com(), &device_code())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
    }
}

mod account {
    use super::*;

    // TC: GIT-02 (the token is checked with one call, `/user`)
    #[tokio::test(flavor = "multi_thread")]
    async fn login_of_token_when_token_has_repo_scope() {
        let (git, github) = forge().await;
        github.accept("ghp_alice", "Alice", ALL_SCOPES);
        let login = git
            .account(
                GitProvider::Github,
                &github_com(),
                &SecretValue::new("ghp_alice".to_owned()),
            )
            .await
            .unwrap();
        let seen = github.only();
        assert_eq!(
            (
                login.as_str(),
                seen.authority.as_str(),
                seen.method.as_str(),
                seen.path.as_str(),
                seen.authorization.as_deref(),
            ),
            ("alice", API, "GET", "/user", Some("Bearer ghp_alice"))
        );
    }

    // TC: GIT-02
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_listing_needed_scopes_when_token_lacks_repo() {
        let (git, github) = forge().await;
        github.accept("ghp_narrow", "alice", "read:packages, gist");
        let err = git
            .account(
                GitProvider::Github,
                &github_com(),
                &SecretValue::new("ghp_narrow".to_owned()),
            )
            .await
            .unwrap_err();
        let Error::Forbidden(why) = &err else {
            panic!("expected Forbidden, got {err:?}");
        };
        let listed = words(why);
        assert!(
            listed.contains("repo") && listed.contains("read:packages") && !why.contains("ghp_"),
            "{why}"
        );
    }

    // TC: GIT-02 (`read:packages` is needed too: the images of `ghcr.io`)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_naming_read_packages_when_token_has_only_repo() {
        let (git, github) = forge().await;
        github.accept("ghp_repo", "alice", "repo");
        let err = git
            .account(
                GitProvider::Github,
                &github_com(),
                &SecretValue::new("ghp_repo".to_owned()),
            )
            .await
            .unwrap_err();
        let Error::Forbidden(why) = &err else {
            panic!("expected Forbidden, got {err:?}");
        };
        assert!(
            why.contains("lacks the scope read:packages") && !why.contains("ghp_"),
            "{why}"
        );
    }

    // TC: GIT-02 (`write:packages` includes `read:packages`)
    #[tokio::test(flavor = "multi_thread")]
    async fn login_of_token_when_token_has_repo_and_write_packages() {
        let (git, github) = forge().await;
        github.accept("ghp_writer", "alice", "repo, write:packages");
        let login = git
            .account(
                GitProvider::Github,
                &github_com(),
                &SecretValue::new("ghp_writer".to_owned()),
            )
            .await
            .unwrap();
        assert_eq!(login.as_str(), "alice");
    }

    // TC: GIT-02 (a fine-grained token has permissions per repository, not
    // scopes, and GitHub sends no `X-OAuth-Scopes` for it: it is accepted,
    // and each later call answers for its permissions)
    #[tokio::test(flavor = "multi_thread")]
    async fn login_of_token_when_github_sends_no_scopes_header() {
        let (git, github) = forge().await;
        github.accept_fine_grained("github_pat_alice", "alice");
        let login = git
            .account(
                GitProvider::Github,
                &github_com(),
                &SecretValue::new("github_pat_alice".to_owned()),
            )
            .await
            .unwrap();
        assert_eq!(login.as_str(), "alice");
    }

    // TC: GIT-02 (GitHub refuses the token itself: nothing is stored)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_github_rejects_token() {
        let (git, _github) = forge().await;
        let err = git
            .account(
                GitProvider::Github,
                &github_com(),
                &SecretValue::new("ghp_unknown".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
    }
}

mod repo_role {
    use super::*;

    async fn role_with(perms: Option<Perms>) -> Result<RepoRole, Error> {
        let (git, github) = forge().await;
        if let Some(perms) = perms {
            github.grant("alice", "acme/web", perms);
        }
        github.accept("ghp_alice", "alice", ALL_SCOPES);
        git.repo_role(&access_on(GITHUB, "alice", "ghp_alice"), &repo("acme/web"))
            .await
    }

    // TC: GIT-03
    #[tokio::test(flavor = "multi_thread")]
    async fn admin_when_permissions_admin() {
        assert_eq!(role_with(Some(ADMIN)).await.unwrap(), RepoRole::Admin);
    }

    // TC: GIT-03
    #[tokio::test(flavor = "multi_thread")]
    async fn write_when_permissions_push_without_admin() {
        assert_eq!(role_with(Some(WRITE)).await.unwrap(), RepoRole::Write);
    }

    // TC: GIT-03
    #[tokio::test(flavor = "multi_thread")]
    async fn read_when_permissions_pull_only() {
        assert_eq!(role_with(Some(READ)).await.unwrap(), RepoRole::Read);
    }

    // TC: GIT-03 (GitHub hides a repository the account may not see)
    #[tokio::test(flavor = "multi_thread")]
    async fn none_when_github_hides_repo() {
        assert_eq!(role_with(None).await.unwrap(), RepoRole::None);
    }

    // TC: GIT-03 (the user's own connection; GitHub refuses a request without `User-Agent`)
    #[tokio::test(flavor = "multi_thread")]
    async fn asks_repos_with_connection_token_and_user_agent_when_asked() {
        let (git, github) = forge().await;
        let _role = git.repo_role(&owner(), &repo(SHOP)).await.unwrap();
        let seen = github.only();
        assert_eq!(
            (
                seen.authority.as_str(),
                seen.method.as_str(),
                seen.path.as_str(),
                seen.authorization,
                seen.user_agent.is_some_and(|agent| !agent.is_empty()),
            ),
            (
                API,
                "GET",
                "/repos/owner/shop",
                Some(format!("Bearer {OWNER_TOKEN}")),
                true
            )
        );
    }
}

mod dispatch {
    use super::*;

    /// The Forgejo host `git.forgejo` serves next to `git.github`.
    const FORGEJO: &str = "git.example.com";

    // TC: GIT-03 (the core's `Git` port picks the module by the connection's host)
    #[tokio::test(flavor = "multi_thread")]
    async fn reaches_github_for_github_com_and_forgejo_for_its_host_when_both_loaded() {
        let github = Github::start(&[API, GITHUB, GHCR, FORGEJO]).await;
        let mut host: ModuleHostOf<InMemoryStore> = ModuleHostOf::new(
            InMemoryStore::new(),
            liftoff_wasm::TokioTimer,
            TracingLog,
            limits(),
        )
        .unwrap();
        for (authority, addr) in &github.addrs {
            host = host.routing_https_to(authority, *addr);
        }
        let set = ModuleSet::new();
        let forgejo_config: ModuleConfig =
            core::iter::once(("hosts".to_owned(), format!(r#"["{FORGEJO}"]"#))).collect();
        for (name, file, config) in [
            ("git.github", "git_github", no_oauth()),
            ("git.forgejo", "git_forgejo", forgejo_config),
        ] {
            let loaded = host
                .load(
                    ModuleName::new(name).unwrap(),
                    &component(file).unwrap(),
                    config,
                )
                .await
                .unwrap();
            loaded.migrate(0).await.unwrap();
            set.insert(loaded);
        }
        let git = ModuleGit::new(Arc::clone(host.registry()), set);
        let on_github = git.repo_role(&owner(), &repo(SHOP)).await.unwrap();
        let forgejo_access = GitAccess {
            provider: GitProvider::Forgejo,
            ..access_on(FORGEJO, "owner", "forgejo-token")
        };
        // The sink answers 404: Forgejo hides the repository.
        let on_forgejo = git.repo_role(&forgejo_access, &repo(SHOP)).await.unwrap();
        assert_eq!(
            (on_github, on_forgejo, github.requests()),
            (
                RepoRole::Admin,
                RepoRole::None,
                vec![
                    request(API, "GET", "/repos/owner/shop"),
                    request(FORGEJO, "GET", "/api/v1/repos/owner/shop"),
                ]
            )
        );
    }

    // TC: GIT-03 (GITF-06 for GitHub: `git.github` serves `github.com`
    // only; GitHub Enterprise is not this module's)
    #[tokio::test(flavor = "multi_thread")]
    async fn validation_without_request_when_host_is_not_github_com() {
        let (git, github) = forge().await;
        let err = git
            .repo_role(
                &access_on("github.example.com", "owner", OWNER_TOKEN),
                &repo(SHOP),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Validation(_)) && github.seen().is_empty(),
            "{err:?} after {:?}",
            github.requests()
        );
    }
}

mod image_exists {
    use super::*;

    // TC: GIT-04 (the registry's manifest, with a pull token exchanged
    // for the connection at `ghcr.io/token`)
    #[tokio::test(flavor = "multi_thread")]
    async fn true_when_registry_has_manifest_of_tag() {
        let (git, github) = forge().await;
        github.publish("owner", "owner/shop:sha-7");
        let exists = git
            .image_exists(&owner(), &image("owner", "shop", "sha-7"))
            .await
            .unwrap();
        assert_eq!(
            (exists, github.requests()),
            (
                true,
                vec![
                    request(GHCR, "GET", "/token"),
                    request(GHCR, "HEAD", "/v2/owner/shop/manifests/sha-7"),
                ]
            )
        );
    }

    // TC: GIT-04 (the token exchange: Basic of the connection, a pull
    // scope on the image's repository)
    #[tokio::test(flavor = "multi_thread")]
    async fn exchanges_connection_for_pull_token_of_image_when_checked() {
        let (git, github) = forge().await;
        github.publish("owner", "owner/shop:sha-7");
        let _exists = git
            .image_exists(&owner(), &image("owner", "shop", "sha-7"))
            .await
            .unwrap();
        let seen = github.seen();
        let [exchange, head] = seen.as_slice() else {
            panic!("two requests expected: {:?}", github.requests());
        };
        assert_eq!(
            (
                exchange.query.get("scope").map(String::as_str),
                exchange.query.get("service").map(String::as_str),
                exchange.authorization.clone(),
                head.authorization.clone(),
            ),
            (
                Some("repository:owner/shop:pull"),
                Some(GHCR),
                Some(format!(
                    "Basic {}",
                    STANDARD.encode(format!("owner:{OWNER_TOKEN}"))
                )),
                Some(format!("Bearer registry-{OWNER_TOKEN}")),
            )
        );
    }

    // TC: GIT-04
    #[tokio::test(flavor = "multi_thread")]
    async fn false_when_tag_is_missing() {
        let (git, github) = forge().await;
        github.publish("owner", "owner/shop:sha-7");
        let exists = git
            .image_exists(&owner(), &image("owner", "shop", "sha-8"))
            .await
            .unwrap();
        assert!(!exists);
    }
}

mod put_ci_secret {
    use super::*;

    fn key() -> SecretKey {
        SecretKey::new("LIFTOFF_TOKEN").unwrap()
    }

    // TC: GIT-05 (the fake opens the sealed box with the repository's private key)
    #[tokio::test(flavor = "multi_thread")]
    async fn seals_value_with_repo_public_key_when_connection_administers_repo() {
        let (git, github) = forge().await;
        git.put_ci_secret(
            &owner(),
            &repo(SHOP),
            &key(),
            &SecretValue::new("lf_secret".to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(
            (github.secret(SHOP, "LIFTOFF_TOKEN"), github.requests(),),
            (
                Some("lf_secret".to_owned()),
                vec![
                    request(API, "GET", "/repos/owner/shop/actions/secrets/public-key"),
                    request(
                        API,
                        "PUT",
                        "/repos/owner/shop/actions/secrets/LIFTOFF_TOKEN"
                    ),
                ]
            )
        );
    }

    // TC: GIT-05 (the value never travels in plaintext)
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_only_sealed_value_and_key_id_when_put() {
        let (git, github) = forge().await;
        git.put_ci_secret(
            &owner(),
            &repo(SHOP),
            &key(),
            &SecretValue::new("lf_secret".to_owned()),
        )
        .await
        .unwrap();
        let put = github.seen().into_iter().last().unwrap();
        let body = put.json();
        let mut fields: Vec<&str> = body
            .as_object()
            .map(|fields| fields.keys().map(String::as_str).collect())
            .unwrap_or_default();
        fields.sort_unstable();
        assert_eq!(
            (
                fields,
                body["key_id"].as_str(),
                put.body.contains("lf_secret"),
            ),
            (vec!["encrypted_value", "key_id"], Some(KEY_ID), false),
            "{}",
            put.body
        );
    }

    // TC: GIT-05 (a value longer than one `wasi:http` write chunk)
    #[tokio::test(flavor = "multi_thread")]
    async fn seals_whole_value_when_value_spans_many_write_chunks() {
        let (git, github) = forge().await;
        let value = "x".repeat(40 * 1024);
        git.put_ci_secret(
            &owner(),
            &repo(SHOP),
            &key(),
            &SecretValue::new(value.clone()),
        )
        .await
        .unwrap();
        assert_eq!(github.secret(SHOP, "LIFTOFF_TOKEN"), Some(value));
    }

    // TC: GIT-05 (only an admin manages Actions secrets)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_connection_may_only_push() {
        let (git, github) = forge().await;
        github.grant("owner", SHOP, WRITE);
        let err = git
            .put_ci_secret(
                &owner(),
                &repo(SHOP),
                &key(),
                &SecretValue::new("lf_secret".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Forbidden(_)) && github.secret(SHOP, "LIFTOFF_TOKEN").is_none(),
            "{err:?}"
        );
    }
}

mod generate_from_template {
    use super::*;

    async fn generate(private: bool) -> (Result<RepoRef, Error>, Github) {
        let (git, github) = forge().await;
        github.grant("owner", "acme/app", READ);
        let created = git
            .generate_from_template(
                &owner(),
                &repo("acme/app"),
                &NewRepo {
                    repo: repo("acme/web"),
                    private,
                },
            )
            .await;
        (created, github)
    }

    // TC: GIT-06
    #[tokio::test(flavor = "multi_thread")]
    async fn generates_private_repo_for_owner_when_template_given() {
        let (created, github) = generate(true).await;
        let seen = github.only();
        assert_eq!(
            (
                created.unwrap(),
                seen.authority.as_str(),
                seen.method.as_str(),
                seen.path.as_str(),
                seen.json()["owner"].clone(),
                seen.json()["name"].clone(),
                seen.json()["private"].clone(),
                seen.json()["include_all"].clone(),
            ),
            (
                repo("acme/web"),
                API,
                "POST",
                "/repos/acme/app/generate",
                json!("acme"),
                json!("web"),
                json!(true),
                json!(true),
            )
        );
    }

    // TC: GIT-06
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_private_false_when_repo_is_public() {
        let (_created, github) = generate(false).await;
        assert_eq!(github.only().json()["private"], json!(false));
    }

    // TC: GIT-06 (the template is unknown or unreadable: GitHub answers
    // `404`)
    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_when_template_is_missing() {
        let (git, _github) = forge().await;
        let err = git
            .generate_from_template(
                &owner(),
                &repo("acme/missing"),
                &NewRepo {
                    repo: repo("acme/web"),
                    private: true,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    }
}

mod templates {
    use super::*;

    /// A template's `liftoff.toml`, longer than one line of GitHub's
    /// base64.
    const APP_TEMPLATE: &str = "[template]\ngroup = \"web\"\nname = \"acme-app\"\ndescription = \"An acme-ui app on Bun and Vite\"\n\n[service]\ncompose = \"compose.yaml\"\nexpose = \"web:3000\"\n";

    // TC: GIT-07 (`list_templates` reads `liftoff.toml` by `contents`, in
    // the order asked; a repository without one has no manifest)
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_manifest_of_each_repo_when_asked() {
        let (git, github) = forge().await;
        github.grant("owner", "acme/app", READ);
        github.grant("owner", "acme/plain", READ);
        github.manifest("acme/app", APP_TEMPLATE);
        let templates = git
            .list_templates(&owner(), &[repo("acme/app"), repo("acme/plain")])
            .await
            .unwrap();
        assert_eq!(
            (templates, github.requests()),
            (
                vec![
                    Template {
                        repo: repo("acme/app"),
                        manifest: Some(APP_TEMPLATE.to_owned()),
                    },
                    Template {
                        repo: repo("acme/plain"),
                        manifest: None,
                    },
                ],
                vec![
                    request(API, "GET", "/repos/acme/app/contents/liftoff.toml"),
                    request(API, "GET", "/repos/acme/plain/contents/liftoff.toml"),
                ]
            )
        );
    }
}

mod retry {
    use super::*;

    // TC: GIT-08
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_after_one_retry_when_github_fails_once() {
        let (git, github) = forge().await;
        github.fail_next(1);
        let role = git.repo_role(&owner(), &repo(SHOP)).await.unwrap();
        assert_eq!((role, github.seen().len()), (RepoRole::Admin, 2));
    }

    // TC: GIT-08
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_of_forge_after_exactly_one_retry_when_github_keeps_failing() {
        let (git, github) = forge().await;
        github.fail_next(5);
        let err = git.repo_role(&owner(), &repo(SHOP)).await.unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Upstream {
                    system: System::Forge,
                    detail,
                } if detail.contains("503")
            ) && github.seen().len() == 2,
            "{err:?} after {:?}",
            github.requests()
        );
    }

    // TC: GIT-08 (a `PUT` of a secret sets the same value again: it is
    // retried once)
    #[tokio::test(flavor = "multi_thread")]
    async fn seals_secret_after_one_retry_when_put_fails_once() {
        let (git, github) = forge().await;
        let put = "/repos/owner/shop/actions/secrets/LIFTOFF_TOKEN";
        github.fail_next_on("PUT", put, 1);
        git.put_ci_secret(
            &owner(),
            &repo(SHOP),
            &SecretKey::new("LIFTOFF_TOKEN").unwrap(),
            &SecretValue::new("lf_secret".to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(
            (github.secret(SHOP, "LIFTOFF_TOKEN"), github.requests()),
            (
                Some("lf_secret".to_owned()),
                vec![
                    request(API, "GET", "/repos/owner/shop/actions/secrets/public-key"),
                    request(API, "PUT", put),
                    request(API, "PUT", put),
                ]
            )
        );
    }

    // TC: GIT-08 (a poll of the device flow redeems nothing until the
    // person confirmed: it is retried once)
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_after_one_retry_when_device_poll_fails_once() {
        let (git, github) = forge_with(with_oauth()).await;
        github.fail_next(1);
        let grant = git
            .device_poll(GitProvider::Github, &github_com(), &device_code())
            .await
            .unwrap();
        assert_eq!(
            (grant, github.requests()),
            (
                DeviceGrant::Pending,
                vec![
                    request(GITHUB, "POST", "/login/oauth/access_token"),
                    request(GITHUB, "POST", "/login/oauth/access_token"),
                ]
            )
        );
    }

    // TC: GIT-08 (a POST that failed may have created the repository: it
    // is not sent again)
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_without_resending_when_generate_fails_once() {
        let (git, github) = forge().await;
        github.grant("owner", "acme/app", READ);
        github.fail_next(1);
        let err = git
            .generate_from_template(
                &owner(),
                &repo("acme/app"),
                &NewRepo {
                    repo: repo("acme/web"),
                    private: true,
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Upstream {
                    system: System::Forge,
                    ..
                }
            ) && github.seen().len() == 1,
            "{err:?} after {:?}",
            github.requests()
        );
    }
}
