#![cfg(test)]
//! The `git.gitlab` module (GIT-09…14).
//!
//! The built `modules/git-gitlab` component runs in a module host; the
//! core's `ModuleGit` (the `Git` port served by the forge modules) calls
//! its tools through the host's `ModuleSet`. Its `wasi:http` reaches the
//! GitLab it is configured for (`host`, `gitlab.com` by default: its REST
//! API below `/api/v4` and its OAuth endpoints below `/oauth`), which the
//! host routes to a fake GitLab served by this test on loopback
//! (`ModuleHost::routing_https_to`).
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.
//!
//! The GitLab API these tests pin (always the user's own connection,
//! `Authorization: Bearer <credential>`; `{id}` is the project's
//! URL-encoded path, `acme%2Fweb`):
//!
//! | Port method | Request | Answer read |
//! |---|---|---|
//! | `account` | `GET /api/v4/user` | `username`; `401` / `403 insufficient_scope` → forbidden naming the scope `api` |
//! | `repo_role` | `GET /api/v4/projects/{id}` | the higher of `permissions.project_access.access_level` and `permissions.group_access.access_level`: ≥ 40 admin, 30 write, 20 (or a public or internal project) read; `404` → none |
//! | `image_exists` | `GET /api/v4/projects/{id}/registry/repositories?tags=true` | the repository whose `path` is `owner/name` has a tag `name` |
//! | `put_ci_secret` | `POST /api/v4/projects/{id}/variables` `{"key","value","masked":true,"protected":true}`; on `400` "has already been taken" `PUT /api/v4/projects/{id}/variables/{KEY}` `{"value","masked":true,"protected":true}` | `2xx` |
//! | `list_templates` | `GET /api/v4/projects/{id}/repository/files/liftoff.toml/raw?ref=HEAD` per repository | the raw file; `404` → no manifest |
//! | `generate_from_template` | `POST /api/v4/projects/{template id}/fork` `{"namespace_path","name","path","visibility"}`, `GET /api/v4/projects/{new}` until `import_status` is `finished`, then `DELETE /api/v4/projects/{new}/fork` | `path_with_namespace` |
//! | `device_start` | `POST /oauth/authorize_device` `client_id`, `scope=api` | RFC 8628 3.2 |
//! | `device_poll` | `POST /oauth/token` `client_id`, `device_code`, `grant_type` | `400 authorization_pending` → pending, `400 slow_down` → slow down (GitLab names no interval), `200 access_token` → granted with `expires_in` |
//!
//! `generate_from_template` forks and renames, as designed
//! (GitLab has no template repositories), and then removes the fork
//! relation once GitLab finished copying it, so the new project is not a
//! fork of the template. A `5xx` of an idempotent request is retried
//! once, then `Upstream`; the fork is sent once (GIT-14). A `403
//! insufficient_scope` of any data call is `Forbidden` naming the scope
//! `api`.

use core::net::SocketAddr;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::Response;
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

/// The GitLab the module serves by default.
const GITLAB: &str = "gitlab.com";
/// A self-hosted GitLab of the module's config `host`.
const SELF_HOSTED: &str = "gitlab.example.com";
/// The OAuth application of the module's config (`client_id`).
const CLIENT_ID: &str = "3f1c7c0a9b0e4f52a6d1e8c2b7a94d10";
/// The token of `owner`'s connection, with the scope `api`.
const OWNER_TOKEN: &str = "glpat-owner";
/// The project of the world's project, maintained by `owner`.
const SHOP: &str = "owner/shop";
/// The device code the fake hands out.
const DEVICE_CODE: &str = "dc-9f3e5a0b8c1d4e7f";
/// The user code the fake hands out.
const USER_CODE: &str = "0A44L90H";
/// The token the fake grants after the person confirmed the device code.
const DEVICE_TOKEN: &str = "gloas-device";

/// GitLab's access levels.
const GUEST: u64 = 10;
const REPORTER: u64 = 20;
const DEVELOPER: u64 = 30;
const MAINTAINER: u64 = 40;
const OWNER: u64 = 50;

fn module() -> ModuleName {
    ModuleName::new("git.gitlab").unwrap()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- The fake GitLab ------------------------------------------------------------

/// A request the fake received.
#[derive(Debug, Clone)]
struct Seen {
    /// The authority it was sent to.
    authority: String,
    method: String,
    /// The raw path, without the query (`%2F` kept as sent).
    path: String,
    /// The query decoded into pairs.
    query: BTreeMap<String, String>,
    authorization: Option<String>,
    /// The raw body.
    body: String,
}

impl Seen {
    /// The body as JSON (`Null` when it is none).
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// The parameters of a JSON or `application/x-www-form-urlencoded`
    /// body: GitLab's OAuth endpoints take either.
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

/// Percent-decoding of a form, query or path component.
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

/// What an account has on a project, as GitLab's `permissions`.
#[derive(Debug, Clone, Copy, Default)]
struct Access {
    project: Option<u64>,
    group: Option<u64>,
}

impl Access {
    const fn project(level: u64) -> Self {
        Self {
            project: Some(level),
            group: None,
        }
    }

    const fn group(level: u64) -> Self {
        Self {
            project: None,
            group: Some(level),
        }
    }

    fn level(self) -> u64 {
        self.project.unwrap_or(0).max(self.group.unwrap_or(0))
    }
}

/// How the fake answers the polls of the device flow, in order; the last
/// one repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Poll {
    Pending,
    SlowDown,
    Granted,
    Expired,
    Denied,
}

/// A CI variable as the fake stored it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Variable {
    value: String,
    masked: bool,
    protected: bool,
}

/// One image repository of a project's container registry.
#[derive(Debug, Clone)]
struct Registry {
    /// `owner/name` (or `owner/name/sub`).
    path: String,
    tags: Vec<String>,
}

#[derive(Debug)]
struct GitlabState {
    /// Token → (username as GitLab spells it, scopes).
    tokens: BTreeMap<String, (String, Vec<String>)>,
    /// `path_with_namespace` → its numeric id.
    projects: BTreeMap<String, u64>,
    /// (username, project) → access; absent → `404`.
    access: BTreeMap<(String, String), Access>,
    /// Project → its registry's image repositories.
    registries: BTreeMap<String, Vec<Registry>>,
    /// Project → its `liftoff.toml` on the default branch.
    manifests: BTreeMap<String, String>,
    /// (project, key) → the variable.
    variables: BTreeMap<(String, String), Variable>,
    /// Project → the project it is a fork of.
    forked_from: BTreeMap<String, String>,
    /// Project → its `visibility` (`private` when absent).
    visibility: BTreeMap<String, String>,
    /// The `import_status` a new fork answers to its next `GET`s, in
    /// order; the last one repeats.
    imports: Vec<&'static str>,
    /// Project → the `import_status` its next `GET`s answer (a fork).
    importing: BTreeMap<String, Vec<&'static str>>,
    /// How many of the next `DELETE …/fork` answer which status.
    failing_unlink: Option<(StatusCode, usize)>,
    /// The answers of the next polls.
    polls: Vec<Poll>,
    /// How many of the next requests answer `503`.
    failing: usize,
    seen: Vec<Seen>,
}

impl Default for GitlabState {
    fn default() -> Self {
        Self {
            tokens: BTreeMap::new(),
            projects: BTreeMap::new(),
            access: BTreeMap::new(),
            registries: BTreeMap::new(),
            manifests: BTreeMap::new(),
            variables: BTreeMap::new(),
            forked_from: BTreeMap::new(),
            visibility: BTreeMap::new(),
            imports: vec!["finished"],
            importing: BTreeMap::new(),
            failing_unlink: None,
            polls: vec![Poll::Pending, Poll::Granted],
            failing: 0,
            seen: Vec::new(),
        }
    }
}

impl GitlabState {
    /// The id of `project`, assigned on first sight.
    fn id_of(&mut self, project: &str) -> u64 {
        let next = u64::try_from(self.projects.len())
            .unwrap()
            .saturating_add(7);
        *self.projects.entry(project.to_owned()).or_insert(next)
    }

    /// The project a path segment names: its numeric id or its
    /// URL-encoded path.
    fn resolve(&self, segment: &str) -> String {
        if segment.bytes().all(|byte| byte.is_ascii_digit()) {
            let id: u64 = segment.parse().unwrap();
            return self
                .projects
                .iter()
                .find(|(_, known)| **known == id)
                .map(|(path, _)| path.clone())
                .unwrap_or_default();
        }
        decode(segment)
    }
}

/// GitLab on `127.0.0.1`: one listener per authority the module reaches.
#[derive(Debug, Clone)]
struct Gitlab {
    /// Authority → the listener the host routes it to.
    addrs: BTreeMap<String, SocketAddr>,
    state: Arc<Mutex<GitlabState>>,
}

/// The state of one listener: which authority it plays.
#[derive(Debug, Clone)]
struct Plays {
    authority: String,
    state: Arc<Mutex<GitlabState>>,
}

impl Gitlab {
    /// The fake playing `authorities`, knowing `owner`'s token (maintainer
    /// of `owner/shop`) and the token the device flow grants (`alice`'s).
    async fn start(authorities: &[&str]) -> Self {
        let state = Arc::new(Mutex::new(GitlabState::default()));
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
        fake.accept(OWNER_TOKEN, "owner", &["api"]);
        fake.accept(DEVICE_TOKEN, "alice", &["api"]);
        fake.grant("owner", SHOP, Access::project(MAINTAINER));
        fake
    }

    /// `token` logs in as `username` with `scopes`.
    fn accept(&self, token: &str, username: &str, scopes: &[&str]) {
        let _previous = lock(&self.state).tokens.insert(
            token.to_owned(),
            (
                username.to_owned(),
                scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            ),
        );
    }

    /// `username` has `access` on `project`.
    fn grant(&self, username: &str, project: &str, access: Access) {
        let mut state = lock(&self.state);
        let _id = state.id_of(project);
        let _previous = state
            .access
            .insert((username.to_owned(), project.to_owned()), access);
    }

    /// `project`'s registry has the image repository `path` with `tags`.
    fn push_image(&self, project: &str, path: &str, tags: &[&str]) {
        lock(&self.state)
            .registries
            .entry(project.to_owned())
            .or_default()
            .push(Registry {
                path: path.to_owned(),
                tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            });
    }

    /// `project` has `toml` as its `liftoff.toml`.
    fn manifest(&self, project: &str, toml: &str) {
        let _previous = lock(&self.state)
            .manifests
            .insert(project.to_owned(), toml.to_owned());
    }

    /// `project` already has the CI variable `key` = `value`.
    fn variable(&self, project: &str, key: &str, value: &str) {
        let _previous = lock(&self.state).variables.insert(
            (project.to_owned(), key.to_owned()),
            Variable {
                value: value.to_owned(),
                masked: false,
                protected: false,
            },
        );
    }

    /// The CI variable `key` of `project` as the fake stored it.
    fn stored(&self, project: &str, key: &str) -> Option<Variable> {
        lock(&self.state)
            .variables
            .get(&(project.to_owned(), key.to_owned()))
            .cloned()
    }

    /// The project `project` is a fork of, if any.
    fn forked_from(&self, project: &str) -> Option<String> {
        lock(&self.state).forked_from.get(project).cloned()
    }

    /// `project` is `visibility` (`public`, `internal`).
    fn visible(&self, project: &str, visibility: &str) {
        let mut state = lock(&self.state);
        let _id = state.id_of(project);
        let _previous = state
            .visibility
            .insert(project.to_owned(), visibility.to_owned());
    }

    /// A new fork answers `statuses` as its `import_status`, in order.
    fn imports(&self, statuses: &[&'static str]) {
        lock(&self.state).imports = statuses.to_vec();
    }

    /// The next `times` requests removing a fork relation answer `status`.
    fn fail_unlink(&self, status: StatusCode, times: usize) {
        lock(&self.state).failing_unlink = Some((status, times));
    }

    /// The polls of the device flow answer `polls` in order.
    fn polls(&self, polls: &[Poll]) {
        lock(&self.state).polls = polls.to_vec();
    }

    /// The next `times` requests answer `503`.
    fn fail_next(&self, times: usize) {
        lock(&self.state).failing = times;
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
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// GitLab's error body.
fn message(status: StatusCode, text: &str) -> Response {
    reply(status, &json!({ "message": text }))
}

/// `status` without a body.
fn empty(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .unwrap()
}

async fn handle(
    State(plays): State<Plays>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let seen = Seen {
        authority: plays.authority.clone(),
        method: method.to_string(),
        path: uri.path().to_owned(),
        query: pairs(uri.query().unwrap_or_default()),
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    answer(&mut lock(&plays.state), &seen)
}

/// What GitLab answers the request `seen`, which it records.
fn answer(state: &mut GitlabState, seen: &Seen) -> Response {
    state.seen.push(seen.clone());
    if state.failing > 0 {
        state.failing = state.failing.saturating_sub(1);
        return message(StatusCode::SERVICE_UNAVAILABLE, "503 Service Unavailable");
    }
    if seen.authority != GITLAB && seen.authority != SELF_HOSTED {
        // A sink of another forge's host (see `dispatch`).
        return message(StatusCode::NOT_FOUND, "404 Not Found");
    }
    if let Some(rest) = seen.path.strip_prefix("/oauth/") {
        return oauth_route(state, seen, rest);
    }
    let Some(rest) = seen.path.strip_prefix("/api/v4/") else {
        return message(StatusCode::NOT_FOUND, "404 Not Found");
    };
    let token = seen
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    let Some((username, scopes)) = state.tokens.get(token).cloned() else {
        return reply(
            StatusCode::UNAUTHORIZED,
            &json!({ "message": "401 Unauthorized" }),
        );
    };
    let segments: Vec<&str> = rest.split('/').collect();
    let reads_user = segments == ["user"];
    let allowed = scopes.iter().any(|scope| {
        scope == "api" || (reads_user && (scope == "read_user" || scope == "read_api"))
    });
    if !allowed {
        return reply(
            StatusCode::FORBIDDEN,
            &json!({
                "error": "insufficient_scope",
                "error_description": "The request requires higher privileges than provided by the access token.",
                "scope": "api read_user",
            }),
        );
    }
    api_route(state, seen, &segments, &username)
}

/// GitLab's OAuth answers: always JSON, errors with `400` (Doorkeeper,
/// RFC 6749 5.2).
fn oauth_error(code: &str) -> Response {
    reply(
        StatusCode::BAD_REQUEST,
        &json!({ "error": code, "error_description": format!("{code} (fake)") }),
    )
}

/// `/oauth/…`: the device flow (RFC 8628) of the OAuth application
/// [`CLIENT_ID`].
fn oauth_route(state: &mut GitlabState, seen: &Seen, rest: &str) -> Response {
    let params = seen.params();
    if seen.method != "POST" || params.get("client_id").map(String::as_str) != Some(CLIENT_ID) {
        return oauth_error("invalid_client");
    }
    match rest {
        "authorize_device" => reply(
            StatusCode::OK,
            &json!({
                "device_code": DEVICE_CODE,
                "user_code": USER_CODE,
                "verification_uri": format!("https://{}/oauth/device", seen.authority),
                "verification_uri_complete": format!("https://{}/oauth/device?user_code={USER_CODE}", seen.authority),
                "expires_in": 300,
                "interval": 5,
            }),
        ),
        "token" => {
            if params.get("device_code").map(String::as_str) != Some(DEVICE_CODE)
                || params.get("grant_type").map(String::as_str)
                    != Some("urn:ietf:params:oauth:grant-type:device_code")
            {
                return oauth_error("invalid_grant");
            }
            let poll = if state.polls.len() > 1 {
                state.polls.remove(0)
            } else {
                state.polls.first().copied().unwrap_or(Poll::Pending)
            };
            match poll {
                Poll::Pending => oauth_error("authorization_pending"),
                Poll::SlowDown => oauth_error("slow_down"),
                Poll::Expired => oauth_error("expired_token"),
                Poll::Denied => oauth_error("access_denied"),
                Poll::Granted => reply(
                    StatusCode::OK,
                    &json!({
                        "access_token": DEVICE_TOKEN,
                        "token_type": "Bearer",
                        "expires_in": 7200,
                        "refresh_token": "gloas-refresh",
                        "scope": "api",
                        "created_at": 1_760_000_000,
                    }),
                ),
            }
        }
        _ => message(StatusCode::NOT_FOUND, "404 Not Found"),
    }
}

/// `/api/v4/…` as `username`.
#[expect(clippy::too_many_lines, reason = "one match over the fake's routes")]
fn api_route(state: &mut GitlabState, seen: &Seen, segments: &[&str], username: &str) -> Response {
    let access = |state: &GitlabState, project: &str| {
        state
            .access
            .get(&(username.to_owned(), project.to_owned()))
            .copied()
    };
    let not_found = || message(StatusCode::NOT_FOUND, "404 Project Not Found");
    let forbidden = || message(StatusCode::FORBIDDEN, "403 Forbidden");
    match (seen.method.as_str(), segments) {
        ("GET", ["user"]) => reply(
            StatusCode::OK,
            &json!({ "id": 1, "username": username, "name": username, "state": "active" }),
        ),
        ("GET", ["projects", id]) => {
            let project = state.resolve(id);
            let visibility = state
                .visibility
                .get(&project)
                .cloned()
                .unwrap_or_else(|| "private".to_owned());
            // A public or internal project is seen by every signed-in
            // account, without `permissions` when it is no member.
            let granted = match access(state, &project) {
                Some(granted) => granted,
                None if visibility != "private" => Access::default(),
                None => return not_found(),
            };
            let import_status = match state.importing.get_mut(&project) {
                Some(statuses) if statuses.len() > 1 => statuses.remove(0),
                Some(statuses) => statuses.first().copied().unwrap_or("finished"),
                None => "none",
            };
            let level = |level: Option<u64>| {
                level.map_or(
                    Value::Null,
                    |level| json!({ "access_level": level, "notification_level": 3 }),
                )
            };
            let numeric = state.id_of(&project);
            reply(
                StatusCode::OK,
                &json!({
                    "id": numeric, "path_with_namespace": project, "default_branch": "main",
                    "visibility": visibility,
                    "import_status": import_status,
                    "permissions": {
                        "project_access": level(granted.project),
                        "group_access": level(granted.group),
                    },
                }),
            )
        }
        ("GET", ["projects", id, "registry", "repositories"]) => {
            let project = state.resolve(id);
            if access(state, &project).is_none() {
                return not_found();
            }
            let with_tags = seen.query.get("tags").map(String::as_str) == Some("true");
            let repositories: Vec<Value> = state
                .registries
                .get(&project)
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(at, registry)| {
                    let location = format!("registry.{GITLAB}/{}", registry.path);
                    let mut repository = json!({
                        "id": at.saturating_add(1),
                        "name": registry.path.strip_prefix(&project).unwrap_or_default().trim_start_matches('/'),
                        "path": registry.path,
                        "project_id": 7,
                        "location": location,
                        "tags_count": registry.tags.len(),
                    });
                    if with_tags {
                        repository["tags"] = registry
                            .tags
                            .iter()
                            .map(|tag| {
                                json!({
                                    "name": tag,
                                    "path": format!("{}:{tag}", registry.path),
                                    "location": format!("{location}:{tag}"),
                                })
                            })
                            .collect();
                    }
                    repository
                })
                .collect();
            reply(StatusCode::OK, &Value::Array(repositories))
        }
        ("POST", ["projects", id, "variables"]) => {
            let project = state.resolve(id);
            match access(state, &project) {
                Some(granted) if granted.level() >= MAINTAINER => {}
                Some(_) => return forbidden(),
                None => return not_found(),
            }
            let body = seen.json();
            let key = body["key"].as_str().unwrap_or_default().to_owned();
            // GitLab masks only a value of 8 characters or more.
            let short = body["value"].as_str().is_some_and(|value| value.len() < 8);
            if body["masked"].as_bool() == Some(true) && short {
                return reply(
                    StatusCode::BAD_REQUEST,
                    &json!({ "message": { "value": ["is invalid"] } }),
                );
            }
            let slot = (project, key.clone());
            if state.variables.contains_key(&slot) {
                return reply(
                    StatusCode::BAD_REQUEST,
                    &json!({ "message": { "key": [format!("({key}) has already been taken")] } }),
                );
            }
            let variable = Variable {
                value: body["value"].as_str().unwrap_or_default().to_owned(),
                masked: body["masked"].as_bool().unwrap_or(false),
                protected: body["protected"].as_bool().unwrap_or(false),
            };
            let _previous = state.variables.insert(slot, variable);
            reply(StatusCode::CREATED, &json!({ "key": key }))
        }
        ("PUT", ["projects", id, "variables", key]) => {
            let project = state.resolve(id);
            match access(state, &project) {
                Some(granted) if granted.level() >= MAINTAINER => {}
                Some(_) => return forbidden(),
                None => return not_found(),
            }
            let slot = (project, decode(key));
            if !state.variables.contains_key(&slot) {
                return message(StatusCode::NOT_FOUND, "404 Variable Not Found");
            }
            let body = seen.json();
            let variable = Variable {
                value: body["value"].as_str().unwrap_or_default().to_owned(),
                masked: body["masked"].as_bool().unwrap_or(false),
                protected: body["protected"].as_bool().unwrap_or(false),
            };
            let _previous = state.variables.insert(slot, variable);
            reply(StatusCode::OK, &json!({ "key": decode(key) }))
        }
        ("GET", ["projects", id, "repository", "files", "liftoff.toml", "raw"]) => {
            let project = state.resolve(id);
            let readable =
                access(state, &project).is_some_and(|granted| granted.level() >= REPORTER);
            match state.manifests.get(&project) {
                Some(toml) if readable => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                    .body(Body::from(toml.clone()))
                    .unwrap(),
                _ => message(StatusCode::NOT_FOUND, "404 File Not Found"),
            }
        }
        ("POST", ["projects", id, "fork"]) => {
            let template = state.resolve(id);
            if access(state, &template).is_none_or(|granted| granted.level() < REPORTER) {
                return not_found();
            }
            let body = seen.json();
            let namespace = body["namespace_path"]
                .as_str()
                .unwrap_or(username)
                .to_owned();
            let path = body["path"].as_str().unwrap_or_default().to_owned();
            let created = format!("{namespace}/{path}");
            if state.projects.contains_key(&created) {
                return reply(
                    StatusCode::CONFLICT,
                    &json!({ "message": { "name": ["has already been taken"], "path": ["has already been taken"] } }),
                );
            }
            let numeric = state.id_of(&created);
            let _previous = state.access.insert(
                (username.to_owned(), created.clone()),
                Access::project(OWNER),
            );
            let _previous = state.forked_from.insert(created.clone(), template);
            let statuses = state.imports.clone();
            let _previous = state.importing.insert(created.clone(), statuses);
            reply(
                StatusCode::CREATED,
                &json!({
                    "id": numeric, "path_with_namespace": created,
                    "name": body["name"], "visibility": body["visibility"],
                    "import_status": "scheduled",
                }),
            )
        }
        ("DELETE", ["projects", id, "fork"]) => {
            if let Some((status, times)) = state.failing_unlink
                && times > 0
            {
                state.failing_unlink = Some((status, times.saturating_sub(1)));
                return message(status, "refused (fake)");
            }
            let project = state.resolve(id);
            match access(state, &project) {
                Some(granted) if granted.level() >= OWNER => {}
                Some(_) => return forbidden(),
                None => return not_found(),
            }
            if state.forked_from.remove(&project).is_some() {
                empty(StatusCode::NO_CONTENT)
            } else {
                empty(StatusCode::NOT_MODIFIED)
            }
        }
        _ => message(StatusCode::NOT_FOUND, "404 Not Found"),
    }
}

// --- The module under test -------------------------------------------------------

type Forge = ModuleGit<ModuleSet<InMemoryStore, TracingLog>>;

/// `pairs` as a module configuration.
fn config(pairs: &[(&str, &str)]) -> ModuleConfig {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The module's configuration with the OAuth application [`CLIENT_ID`].
fn with_oauth() -> ModuleConfig {
    config(&[("client_id", CLIENT_ID)])
}

/// The built `git.gitlab` component loaded with `config` and migrated
/// from 0, as an install does, its hosts routed to a fresh fake; the
/// core's `Git` port over it.
async fn forge_with(config: ModuleConfig) -> (Forge, Gitlab) {
    let gitlab = Gitlab::start(&[GITLAB, SELF_HOSTED]).await;
    let mut host: ModuleHostOf<InMemoryStore> = ModuleHostOf::new(
        InMemoryStore::new(),
        liftoff_wasm::TokioTimer,
        TracingLog,
        limits(),
    )
    .unwrap();
    for (authority, addr) in &gitlab.addrs {
        host = host.routing_https_to(authority, *addr);
    }
    let module = host
        .load(module(), &component("git_gitlab").unwrap(), config)
        .await
        .unwrap();
    module.migrate(0).await.unwrap();
    let set = ModuleSet::new();
    set.insert(module);
    (ModuleGit::new(Arc::clone(host.registry()), set), gitlab)
}

/// [`forge_with`] without configuration: `gitlab.com`, tokens only.
async fn forge() -> (Forge, Gitlab) {
    forge_with(ModuleConfig::new()).await
}

fn host(raw: &str) -> Host {
    Host::new(raw).unwrap()
}

fn gitlab_com() -> Host {
    host(GITLAB)
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
        provider: GitProvider::Gitlab,
        host: host(on),
        login: Login::new(login).unwrap(),
        token: SecretValue::new(token.to_owned()),
    }
}

/// `owner`'s connection on `gitlab.com`.
fn owner() -> GitAccess {
    access_on(GITLAB, "owner", OWNER_TOKEN)
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

    // TC: GIT-14 (`host` defaults to `gitlab.com`; without
    // `client_id` the module connects by token only, and its `http_allow`
    // is the host it serves)
    #[tokio::test(flavor = "multi_thread")]
    async fn serves_and_allows_gitlab_com_without_oauth_when_not_configured() {
        let (git, _gitlab) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        let allowed: Vec<String> = manifest
            .http_allow
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            (manifest.git_provider, allowed),
            (
                Some(GitProviderSpec {
                    hosts: vec![GITLAB.to_owned()],
                    oauth: false,
                }),
                vec![GITLAB.to_owned()],
            )
        );
    }

    // TC: GIT-14 (with an OAuth application the CLI runs the device flow)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_oauth_when_client_id_configured() {
        let (git, _gitlab) = forge_with(with_oauth()).await;
        let manifest = git.registry().manifest(&module()).unwrap();
        assert_eq!(
            manifest.git_provider,
            Some(GitProviderSpec {
                hosts: vec![GITLAB.to_owned()],
                oauth: true,
            })
        );
    }

    // TC: GIT-09 (a self-hosted GitLab is the config's `host`,
    // served and allowed from the load on)
    #[tokio::test(flavor = "multi_thread")]
    async fn serves_and_allows_configured_host_when_host_configured() {
        let (git, _gitlab) = forge_with(config(&[("host", SELF_HOSTED)])).await;
        let manifest = git.registry().manifest(&module()).unwrap();
        let allowed: Vec<String> = manifest
            .http_allow
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            (manifest.git_provider, allowed),
            (
                Some(GitProviderSpec {
                    hosts: vec![SELF_HOSTED.to_owned()],
                    oauth: false,
                }),
                vec![SELF_HOSTED.to_owned()],
            )
        );
    }

    // TC: GIT-14 (`host` and `client_id` are optional, the
    // OAuth application a secret)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_host_and_secret_client_id_as_optional_config_when_loaded() {
        let (git, _gitlab) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        let schema = manifest.config_schema.unwrap_or(Value::Null);
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(
            (
                schema["properties"]["host"]["type"].as_str(),
                schema["properties"]["client_id"]["format"].as_str(),
                required,
            ),
            (Some("string"), Some("secret"), Vec::<&str>::new()),
            "{schema}"
        );
    }
}

mod device_flow {
    use super::*;

    // TC: GIT-14 (the forge's device flow when an OAuth application is configured)
    #[tokio::test(flavor = "multi_thread")]
    async fn device_code_of_gitlab_when_client_id_configured() {
        let (git, _gitlab) = forge_with(with_oauth()).await;
        let started = git
            .device_start(GitProvider::Gitlab, &gitlab_com())
            .await
            .unwrap();
        assert_eq!(
            started,
            DeviceStart {
                device_code: device_code(),
                user_code: USER_CODE.to_owned(),
                verification_uri: "https://gitlab.com/oauth/device".to_owned(),
                interval: 5,
                expires_in: 300,
            }
        );
    }

    // TC: GIT-14 (the scope the module needs: `api`)
    #[tokio::test(flavor = "multi_thread")]
    async fn asks_client_id_and_api_scope_at_authorize_device_when_started() {
        let (git, gitlab) = forge_with(with_oauth()).await;
        let _started = git
            .device_start(GitProvider::Gitlab, &gitlab_com())
            .await
            .unwrap();
        let seen = gitlab.only();
        let params = seen.params();
        assert_eq!(
            (
                seen.authority.as_str(),
                seen.method.as_str(),
                seen.path.as_str(),
                params.get("client_id").map(String::as_str),
                params.get("scope").map(|scope| words(scope)),
            ),
            (
                GITLAB,
                "POST",
                "/oauth/authorize_device",
                Some(CLIENT_ID),
                Some(["api".to_owned()].into()),
            )
        );
    }

    // TC: GIT-14 (a self-hosted GitLab runs its own device flow)
    #[tokio::test(flavor = "multi_thread")]
    async fn starts_at_configured_host_when_host_and_client_id_configured() {
        let (git, gitlab) =
            forge_with(config(&[("host", SELF_HOSTED), ("client_id", CLIENT_ID)])).await;
        let started = git
            .device_start(GitProvider::Gitlab, &host(SELF_HOSTED))
            .await
            .unwrap();
        assert_eq!(
            (started.verification_uri, gitlab.requests()),
            (
                "https://gitlab.example.com/oauth/device".to_owned(),
                vec![request(SELF_HOSTED, "POST", "/oauth/authorize_device")],
            )
        );
    }

    // TC: GIT-14 (GitLab answers a pending grant with `400`)
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_when_person_has_not_confirmed() {
        let (git, gitlab) = forge_with(with_oauth()).await;
        let grant = git
            .device_poll(GitProvider::Gitlab, &gitlab_com(), &device_code())
            .await
            .unwrap();
        let seen = gitlab.only();
        let params = seen.params();
        assert_eq!(
            (
                grant,
                seen.method.as_str(),
                seen.path.as_str(),
                params.get("client_id").map(String::as_str),
                params.get("device_code").map(String::as_str),
                params.get("grant_type").map(String::as_str),
            ),
            (
                DeviceGrant::Pending,
                "POST",
                "/oauth/token",
                Some(CLIENT_ID),
                Some(DEVICE_CODE),
                Some("urn:ietf:params:oauth:grant-type:device_code"),
            )
        );
    }

    // TC: GIT-14 (GitLab's OAuth token lasts `expires_in` seconds: the
    // core records when the connection expires)
    #[tokio::test(flavor = "multi_thread")]
    async fn granted_token_with_its_lifetime_when_person_confirmed() {
        let (git, gitlab) = forge_with(with_oauth()).await;
        gitlab.polls(&[Poll::Granted]);
        let grant = git
            .device_poll(GitProvider::Gitlab, &gitlab_com(), &device_code())
            .await
            .unwrap();
        assert_eq!(
            grant,
            DeviceGrant::Granted {
                token: SecretValue::new(DEVICE_TOKEN.to_owned()),
                expires_in: Some(7200),
            }
        );
    }

    // TC: GIT-14 (RFC 8628 3.5 `slow_down`: GitLab names no new interval,
    // so the CLI adds 5 s)
    #[tokio::test(flavor = "multi_thread")]
    async fn slow_down_without_interval_when_gitlab_asks_to_slow_down() {
        let (git, gitlab) = forge_with(with_oauth()).await;
        gitlab.polls(&[Poll::SlowDown, Poll::Pending]);
        let grant = git
            .device_poll(GitProvider::Gitlab, &gitlab_com(), &device_code())
            .await
            .unwrap();
        assert_eq!(grant, DeviceGrant::SlowDown { interval: None });
    }

    // TC: GIT-14 (RFC 8628 3.5 `expired_token`)
    #[tokio::test(flavor = "multi_thread")]
    async fn gone_when_device_code_expired() {
        let (git, gitlab) = forge_with(with_oauth()).await;
        gitlab.polls(&[Poll::Expired]);
        let err = git
            .device_poll(GitProvider::Gitlab, &gitlab_com(), &device_code())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Gone(_)), "{err:?}");
    }

    // TC: GIT-14 (RFC 8628 3.5 `access_denied`)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_person_denied() {
        let (git, gitlab) = forge_with(with_oauth()).await;
        gitlab.polls(&[Poll::Denied]);
        let err = git
            .device_poll(GitProvider::Gitlab, &gitlab_com(), &device_code())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
    }
}

mod account {
    use super::*;

    // TC: GIT-14 (the token is checked with one call, `/user`)
    #[tokio::test(flavor = "multi_thread")]
    async fn login_of_token_when_token_has_api_scope() {
        let (git, gitlab) = forge().await;
        gitlab.accept("glpat-alice", "Alice", &["api"]);
        let login = git
            .account(
                GitProvider::Gitlab,
                &gitlab_com(),
                &SecretValue::new("glpat-alice".to_owned()),
            )
            .await
            .unwrap();
        let seen = gitlab.only();
        assert_eq!(
            (
                login.as_str(),
                seen.authority.as_str(),
                seen.method.as_str(),
                seen.path.as_str(),
                seen.authorization.as_deref(),
            ),
            (
                "alice",
                GITLAB,
                "GET",
                "/api/v4/user",
                Some("Bearer glpat-alice")
            )
        );
    }

    // TC: GIT-14 (a token GitLab refuses for its scope: `Forbidden` naming
    // the scope `api`, never the token)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_naming_api_scope_when_token_lacks_it() {
        let (git, gitlab) = forge().await;
        gitlab.accept("glpat-narrow", "alice", &["read_repository"]);
        let err = git
            .account(
                GitProvider::Gitlab,
                &gitlab_com(),
                &SecretValue::new("glpat-narrow".to_owned()),
            )
            .await
            .unwrap_err();
        let Error::Forbidden(why) = &err else {
            panic!("expected Forbidden, got {err:?}");
        };
        assert!(
            words(why).contains("api") && !why.contains("glpat-"),
            "{why}"
        );
    }

    // TC: GIT-14 (GitLab refuses the token itself: nothing is stored)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_gitlab_rejects_token() {
        let (git, gitlab) = forge().await;
        let err = git
            .account(
                GitProvider::Gitlab,
                &gitlab_com(),
                &SecretValue::new("glpat-unknown".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Forbidden(_)) && gitlab.seen().len() == 1,
            "{err:?} after {:?}",
            gitlab.requests()
        );
    }
}

mod repo_role {
    use super::*;

    async fn role_with(access: Option<Access>) -> Result<RepoRole, Error> {
        let (git, gitlab) = forge().await;
        if let Some(access) = access {
            gitlab.grant("alice", "acme/web", access);
        }
        gitlab.accept("glpat-alice", "alice", &["api"]);
        git.repo_role(
            &access_on(GITLAB, "alice", "glpat-alice"),
            &repo("acme/web"),
        )
        .await
    }

    // TC: GIT-09 (access level ≥ 40: Maintainer)
    #[tokio::test(flavor = "multi_thread")]
    async fn admin_when_project_access_is_maintainer() {
        assert_eq!(
            role_with(Some(Access::project(MAINTAINER))).await.unwrap(),
            RepoRole::Admin
        );
    }

    // TC: GIT-09 (access level ≥ 40: Owner)
    #[tokio::test(flavor = "multi_thread")]
    async fn admin_when_project_access_is_owner() {
        assert_eq!(
            role_with(Some(Access::project(OWNER))).await.unwrap(),
            RepoRole::Admin
        );
    }

    // TC: GIT-09 (the level may come from the group alone)
    #[tokio::test(flavor = "multi_thread")]
    async fn admin_when_only_group_access_is_maintainer() {
        assert_eq!(
            role_with(Some(Access::group(MAINTAINER))).await.unwrap(),
            RepoRole::Admin
        );
    }

    // TC: GIT-09 (the higher of the two levels counts)
    #[tokio::test(flavor = "multi_thread")]
    async fn admin_when_group_access_exceeds_project_access() {
        let both = Access {
            project: Some(DEVELOPER),
            group: Some(OWNER),
        };
        assert_eq!(role_with(Some(both)).await.unwrap(), RepoRole::Admin);
    }

    // TC: GIT-09
    #[tokio::test(flavor = "multi_thread")]
    async fn write_when_access_level_is_developer() {
        assert_eq!(
            role_with(Some(Access::project(DEVELOPER))).await.unwrap(),
            RepoRole::Write
        );
    }

    // TC: GIT-09
    #[tokio::test(flavor = "multi_thread")]
    async fn read_when_access_level_is_reporter() {
        assert_eq!(
            role_with(Some(Access::project(REPORTER))).await.unwrap(),
            RepoRole::Read
        );
    }

    // TC: GIT-09 (a Guest does not read the code of a private project)
    #[tokio::test(flavor = "multi_thread")]
    async fn none_when_access_level_is_guest() {
        assert_eq!(
            role_with(Some(Access::project(GUEST))).await.unwrap(),
            RepoRole::None
        );
    }

    // TC: GIT-09 (GitLab hides a project the account may not see)
    #[tokio::test(flavor = "multi_thread")]
    async fn none_when_gitlab_hides_project() {
        assert_eq!(role_with(None).await.unwrap(), RepoRole::None);
    }

    /// The role of `alice`, no member, on `acme/web` of `visibility`.
    async fn role_of_non_member_on(visibility: &str) -> RepoRole {
        let (git, gitlab) = forge().await;
        gitlab.visible("acme/web", visibility);
        gitlab.accept("glpat-alice", "alice", &["api"]);
        git.repo_role(
            &access_on(GITLAB, "alice", "glpat-alice"),
            &repo("acme/web"),
        )
        .await
        .unwrap()
    }

    // TC: GIT-09 (every signed-in account reads the code of a public
    // project)
    #[tokio::test(flavor = "multi_thread")]
    async fn read_when_project_is_public_and_account_is_not_member() {
        assert_eq!(role_of_non_member_on("public").await, RepoRole::Read);
    }

    // TC: GIT-09 (every signed-in account reads the code of an internal
    // project)
    #[tokio::test(flavor = "multi_thread")]
    async fn read_when_project_is_internal_and_account_is_not_member() {
        assert_eq!(role_of_non_member_on("internal").await, RepoRole::Read);
    }

    // TC: GIT-09 (a Guest of a public project still reads it)
    #[tokio::test(flavor = "multi_thread")]
    async fn read_when_project_is_public_and_account_is_guest() {
        let (git, gitlab) = forge().await;
        gitlab.visible("acme/web", "public");
        gitlab.grant("alice", "acme/web", Access::project(GUEST));
        gitlab.accept("glpat-alice", "alice", &["api"]);
        let role = git
            .repo_role(
                &access_on(GITLAB, "alice", "glpat-alice"),
                &repo("acme/web"),
            )
            .await
            .unwrap();
        assert_eq!(role, RepoRole::Read);
    }

    // TC: GIT-14 (a token that reads the user but lacks `api`: GitLab's
    // `403 insufficient_scope` is `Forbidden` naming the scope, not a
    // silent "none")
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_naming_api_scope_when_token_lacks_it() {
        let (git, gitlab) = forge().await;
        gitlab.accept("glpat-narrow", "owner", &["read_user"]);
        let err = git
            .repo_role(&access_on(GITLAB, "owner", "glpat-narrow"), &repo(SHOP))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Forbidden(why)
                if words(why).contains("api") && why.contains("lof git connect gitlab")),
            "{err:?}"
        );
    }

    // TC: GIT-09 (the user's own connection; the project by its URL-encoded path)
    #[tokio::test(flavor = "multi_thread")]
    async fn asks_project_by_encoded_path_with_connection_token_when_asked() {
        let (git, gitlab) = forge().await;
        let _role = git.repo_role(&owner(), &repo(SHOP)).await.unwrap();
        let seen = gitlab.only();
        assert_eq!(
            (
                seen.authority.as_str(),
                seen.method.as_str(),
                seen.path.as_str(),
                seen.authorization,
            ),
            (
                GITLAB,
                "GET",
                "/api/v4/projects/owner%2Fshop",
                Some(format!("Bearer {OWNER_TOKEN}")),
            )
        );
    }
}

mod dispatch {
    use super::*;

    /// The REST API of GitHub, which `git.github` reaches.
    const GITHUB_API: &str = "api.github.com";

    // TC: GIT-09 (a self-hosted GitLab is reached at the host of
    // the config)
    #[tokio::test(flavor = "multi_thread")]
    async fn reaches_configured_host_when_connection_is_on_it() {
        let (git, gitlab) = forge_with(config(&[("host", SELF_HOSTED)])).await;
        let role = git
            .repo_role(&access_on(SELF_HOSTED, "owner", OWNER_TOKEN), &repo(SHOP))
            .await
            .unwrap();
        assert_eq!(
            (role, gitlab.requests()),
            (
                RepoRole::Admin,
                vec![request(SELF_HOSTED, "GET", "/api/v4/projects/owner%2Fshop")],
            )
        );
    }

    // TC: GIT-09 (the core's `Git` port picks `git.gitlab` for `gitlab.com` and `git.github` for `github.com`)
    #[tokio::test(flavor = "multi_thread")]
    async fn reaches_gitlab_for_gitlab_com_and_github_for_github_com_when_both_loaded() {
        let gitlab = Gitlab::start(&[GITLAB, GITHUB_API]).await;
        let mut host: ModuleHostOf<InMemoryStore> = ModuleHostOf::new(
            InMemoryStore::new(),
            liftoff_wasm::TokioTimer,
            TracingLog,
            limits(),
        )
        .unwrap();
        for (authority, addr) in &gitlab.addrs {
            host = host.routing_https_to(authority, *addr);
        }
        let set = ModuleSet::new();
        for (name, file) in [("git.gitlab", "git_gitlab"), ("git.github", "git_github")] {
            let loaded = host
                .load(
                    ModuleName::new(name).unwrap(),
                    &component(file).unwrap(),
                    ModuleConfig::new(),
                )
                .await
                .unwrap();
            loaded.migrate(0).await.unwrap();
            set.insert(loaded);
        }
        let git = ModuleGit::new(Arc::clone(host.registry()), set);
        let on_gitlab = git.repo_role(&owner(), &repo(SHOP)).await.unwrap();
        let github_access = GitAccess {
            provider: GitProvider::Github,
            ..access_on("github.com", "owner", "ghp_owner")
        };
        // The sink answers 404: GitHub hides the repository.
        let on_github = git.repo_role(&github_access, &repo(SHOP)).await.unwrap();
        assert_eq!(
            (on_gitlab, on_github, gitlab.requests()),
            (
                RepoRole::Admin,
                RepoRole::None,
                vec![
                    request(GITLAB, "GET", "/api/v4/projects/owner%2Fshop"),
                    request(GITHUB_API, "GET", "/repos/owner/shop"),
                ]
            )
        );
    }
}

mod image_exists {
    use super::*;

    // TC: GIT-10 (the project's container registry, tags included)
    #[tokio::test(flavor = "multi_thread")]
    async fn true_when_registry_repository_of_image_has_tag() {
        let (git, gitlab) = forge().await;
        gitlab.push_image(SHOP, SHOP, &["sha-6", "sha-7"]);
        let exists = git
            .image_exists(&owner(), &image("owner", "shop", "sha-7"))
            .await
            .unwrap();
        let seen = gitlab.only();
        assert_eq!(
            (
                exists,
                seen.method.as_str(),
                seen.path.as_str(),
                seen.query.get("tags").map(String::as_str),
            ),
            (
                true,
                "GET",
                "/api/v4/projects/owner%2Fshop/registry/repositories",
                Some("true"),
            )
        );
    }

    // TC: GIT-10
    #[tokio::test(flavor = "multi_thread")]
    async fn false_when_tag_is_missing() {
        let (git, gitlab) = forge().await;
        gitlab.push_image(SHOP, SHOP, &["sha-7"]);
        let exists = git
            .image_exists(&owner(), &image("owner", "shop", "sha-8"))
            .await
            .unwrap();
        assert!(!exists);
    }

    // TC: GIT-10 (the tag of another image repository of the project is
    // not the image's)
    #[tokio::test(flavor = "multi_thread")]
    async fn false_when_only_other_image_of_project_has_tag() {
        let (git, gitlab) = forge().await;
        gitlab.push_image(SHOP, "owner/shop/worker", &["sha-7"]);
        let exists = git
            .image_exists(&owner(), &image("owner", "shop", "sha-7"))
            .await
            .unwrap();
        assert!(!exists);
    }

    // TC: GIT-14 (a token without `api` does not read the registry: the
    // deploy is told so instead of "no such image")
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_naming_api_scope_when_token_lacks_it() {
        let (git, gitlab) = forge().await;
        gitlab.accept("glpat-narrow", "owner", &["read_user"]);
        gitlab.push_image(SHOP, SHOP, &["sha-7"]);
        let err = git
            .image_exists(
                &access_on(GITLAB, "owner", "glpat-narrow"),
                &image("owner", "shop", "sha-7"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Forbidden(why) if words(why).contains("api")),
            "{err:?}"
        );
    }

    // TC: GIT-10 (a project the account may not see has no image it can
    // pull)
    #[tokio::test(flavor = "multi_thread")]
    async fn false_when_gitlab_hides_project() {
        let (git, _gitlab) = forge().await;
        let exists = git
            .image_exists(&owner(), &image("acme", "hidden", "sha-7"))
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

    fn value() -> SecretValue {
        SecretValue::new("lf_secret_value".to_owned())
    }

    // TC: GIT-11 (a CI variable, masked and protected)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_masked_protected_variable_when_key_is_new() {
        let (git, gitlab) = forge().await;
        git.put_ci_secret(&owner(), &repo(SHOP), &key(), &value())
            .await
            .unwrap();
        let seen = gitlab.only();
        assert_eq!(
            (
                gitlab.stored(SHOP, "LIFTOFF_TOKEN"),
                seen.method.as_str(),
                seen.path.as_str(),
                seen.json()["key"].clone(),
            ),
            (
                Some(Variable {
                    value: "lf_secret_value".to_owned(),
                    masked: true,
                    protected: true,
                }),
                "POST",
                "/api/v4/projects/owner%2Fshop/variables",
                json!("LIFTOFF_TOKEN"),
            )
        );
    }

    // TC: GIT-11 (a key that exists is updated: `400` "has already been
    // taken", then `PUT`)
    #[tokio::test(flavor = "multi_thread")]
    async fn updates_variable_masked_and_protected_when_key_already_exists() {
        let (git, gitlab) = forge().await;
        gitlab.variable(SHOP, "LIFTOFF_TOKEN", "lf_old_value");
        git.put_ci_secret(&owner(), &repo(SHOP), &key(), &value())
            .await
            .unwrap();
        assert_eq!(
            (gitlab.stored(SHOP, "LIFTOFF_TOKEN"), gitlab.requests()),
            (
                Some(Variable {
                    value: "lf_secret_value".to_owned(),
                    masked: true,
                    protected: true,
                }),
                vec![
                    request(GITLAB, "POST", "/api/v4/projects/owner%2Fshop/variables"),
                    request(
                        GITLAB,
                        "PUT",
                        "/api/v4/projects/owner%2Fshop/variables/LIFTOFF_TOKEN"
                    ),
                ]
            )
        );
    }

    // TC: GIT-14 (a token without `api` is told the scope, not "it takes
    // Maintainer")
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_naming_api_scope_when_token_lacks_it() {
        let (git, gitlab) = forge().await;
        gitlab.accept("glpat-narrow", "owner", &["read_user"]);
        let err = git
            .put_ci_secret(
                &access_on(GITLAB, "owner", "glpat-narrow"),
                &repo(SHOP),
                &key(),
                &value(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Forbidden(why)
                if words(why).contains("api") && !why.contains("Maintainer")),
            "{err:?}"
        );
    }

    // TC: GIT-11 (GitLab cannot mask a short value: `Validation` that
    // says why, and nothing is stored unmasked)
    #[tokio::test(flavor = "multi_thread")]
    async fn validation_naming_mask_rule_when_value_is_too_short_to_mask() {
        let (git, gitlab) = forge().await;
        let err = git
            .put_ci_secret(
                &owner(),
                &repo(SHOP),
                &key(),
                &SecretValue::new("short".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Validation(_))
                && format!("{err:?}").contains("8 characters")
                && gitlab.stored(SHOP, "LIFTOFF_TOKEN").is_none()
                && gitlab.seen().len() == 1,
            "{err:?} after {:?}",
            gitlab.requests()
        );
    }

    // TC: GIT-11 (only a Maintainer manages CI variables)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_connection_is_developer() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", SHOP, Access::project(DEVELOPER));
        let err = git
            .put_ci_secret(&owner(), &repo(SHOP), &key(), &value())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Forbidden(_)) && gitlab.stored(SHOP, "LIFTOFF_TOKEN").is_none(),
            "{err:?}"
        );
    }
}

mod generate_from_template {
    use super::*;

    async fn generate(private: bool) -> (Result<RepoRef, Error>, Gitlab) {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
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
        (created, gitlab)
    }

    // TC: GIT-12 (GitLab has no template repositories, so the template is forked under the new name into the owner's namespace)
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_template_into_owner_namespace_under_new_name_when_private() {
        let (created, gitlab) = generate(true).await;
        let seen = gitlab.seen();
        let fork = seen.first().unwrap();
        let body = fork.json();
        assert_eq!(
            (
                created.unwrap(),
                fork.method.as_str(),
                fork.path.as_str(),
                body["namespace_path"].clone(),
                body["name"].clone(),
                body["path"].clone(),
                body["visibility"].clone(),
            ),
            (
                repo("acme/web"),
                "POST",
                "/api/v4/projects/acme%2Fapp/fork",
                json!("acme"),
                json!("web"),
                json!("web"),
                json!("private"),
            )
        );
    }

    // TC: GIT-12
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_with_public_visibility_when_repo_is_public() {
        let (_created, gitlab) = generate(false).await;
        let seen = gitlab.seen();
        assert_eq!(
            seen.first().map(|fork| fork.json()["visibility"].clone()),
            Some(json!("public"))
        );
    }

    // TC: GIT-12 (the new project is not left a fork of the template: the
    // relation of the new project, not of the template, is removed once
    // GitLab finished copying it)
    #[tokio::test(flavor = "multi_thread")]
    async fn removes_fork_relation_of_new_project_when_import_finished() {
        let (created, gitlab) = generate(true).await;
        assert_eq!(
            (
                created.is_ok(),
                gitlab.forked_from("acme/web"),
                gitlab.requests()
            ),
            (
                true,
                None,
                vec![
                    request(GITLAB, "POST", "/api/v4/projects/acme%2Fapp/fork"),
                    request(GITLAB, "GET", "/api/v4/projects/acme%2Fweb"),
                    request(GITLAB, "DELETE", "/api/v4/projects/acme%2Fweb/fork"),
                ]
            ),
            "{created:?}"
        );
    }

    // TC: GIT-12 (the CLI clones the new project right away: the
    // module answers only after GitLab copied the template)
    #[tokio::test(flavor = "multi_thread")]
    async fn waits_until_import_finished_when_fork_is_still_importing() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.imports(&["started", "finished"]);
        let created = git
            .generate_from_template(
                &owner(),
                &repo("acme/app"),
                &NewRepo {
                    repo: repo("acme/web"),
                    private: true,
                },
            )
            .await;
        assert_eq!(
            (created.ok(), gitlab.requests()),
            (
                Some(repo("acme/web")),
                vec![
                    request(GITLAB, "POST", "/api/v4/projects/acme%2Fapp/fork"),
                    request(GITLAB, "GET", "/api/v4/projects/acme%2Fweb"),
                    request(GITLAB, "GET", "/api/v4/projects/acme%2Fweb"),
                    request(GITLAB, "DELETE", "/api/v4/projects/acme%2Fweb/fork"),
                ]
            )
        );
    }

    // TC: GIT-12, GIT-14 (GitLab failed to copy the template: `Upstream`,
    // and the half-made project is not unlinked)
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_without_unlinking_when_import_failed() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.imports(&["failed"]);
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
        let methods: Vec<String> = gitlab
            .requests()
            .into_iter()
            .map(|(_, method, _)| method)
            .collect();
        assert!(
            matches!(
                &err,
                Error::Upstream {
                    system: System::Forge,
                    detail,
                } if detail.contains("acme/web")
            ) && methods == ["POST", "GET"],
            "{err:?} after {:?}",
            gitlab.requests()
        );
    }

    // TC: GIT-12, GIT-14 (GitLab still copies after the bounded wait:
    // `Upstream` naming the project, which is not unlinked mid-import)
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_naming_project_when_import_outlasts_wait() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.imports(&["started"]);
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
        let unlinked = gitlab
            .requests()
            .iter()
            .any(|(_, method, _)| method == "DELETE");
        assert!(
            matches!(
                &err,
                Error::Upstream {
                    system: System::Forge,
                    detail,
                } if detail.contains("acme/web")
            ) && !unlinked,
            "{err:?} after {:?}",
            gitlab.requests()
        );
    }

    // TC: GIT-12 (a project that exists and works is answered even when
    // GitLab refuses to remove its fork relation)
    #[tokio::test(flavor = "multi_thread")]
    async fn created_when_unlinking_fork_is_refused() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.fail_unlink(StatusCode::FORBIDDEN, 1);
        let created = git
            .generate_from_template(
                &owner(),
                &repo("acme/app"),
                &NewRepo {
                    repo: repo("acme/web"),
                    private: true,
                },
            )
            .await;
        assert_eq!(
            (created.ok(), gitlab.forked_from("acme/web")),
            (Some(repo("acme/web")), Some("acme/app".to_owned()))
        );
    }

    // TC: GIT-12, GIT-14 (the unlink is retried once on `5xx`, then the
    // project is still answered)
    #[tokio::test(flavor = "multi_thread")]
    async fn created_after_one_retry_when_unlinking_fork_keeps_failing() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.fail_unlink(StatusCode::SERVICE_UNAVAILABLE, 5);
        let created = git
            .generate_from_template(
                &owner(),
                &repo("acme/app"),
                &NewRepo {
                    repo: repo("acme/web"),
                    private: true,
                },
            )
            .await;
        let unlinks = gitlab
            .requests()
            .into_iter()
            .filter(|(_, method, _)| method == "DELETE")
            .count();
        assert_eq!(
            (created.ok(), unlinks, gitlab.forked_from("acme/web")),
            (Some(repo("acme/web")), 2, Some("acme/app".to_owned()))
        );
    }

    // TC: GIT-12 (the template is unknown or unreadable: GitLab answers
    // `404`)
    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_when_template_is_missing() {
        let (git, _gitlab) = forge().await;
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

    // TC: GIT-12 (the name is taken in the namespace: GitLab answers `409`)
    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_when_project_name_is_taken() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.grant("owner", "acme/web", Access::project(OWNER));
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
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");
    }
}

mod templates {
    use super::*;

    /// A template's `liftoff.toml`.
    const APP_TEMPLATE: &str = "[template]\ngroup = \"web\"\nname = \"acme-app\"\ndescription = \"An acme-ui app on Bun and Vite\"\n\n[service]\ncompose = \"compose.yaml\"\nexpose = \"web:3000\"\n";

    // TC: GIT-13 (`list_templates` reads the raw `liftoff.toml` of the
    // default branch, in the order asked; a project without one has no
    // manifest)
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_raw_manifest_of_each_repo_when_asked() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.grant("owner", "acme/plain", Access::project(REPORTER));
        gitlab.manifest("acme/app", APP_TEMPLATE);
        let templates = git
            .list_templates(&owner(), &[repo("acme/app"), repo("acme/plain")])
            .await
            .unwrap();
        assert_eq!(
            (templates, gitlab.requests()),
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
                    request(
                        GITLAB,
                        "GET",
                        "/api/v4/projects/acme%2Fapp/repository/files/liftoff.toml/raw"
                    ),
                    request(
                        GITLAB,
                        "GET",
                        "/api/v4/projects/acme%2Fplain/repository/files/liftoff.toml/raw"
                    ),
                ]
            )
        );
    }

    // TC: GIT-13 (the default branch: `ref=HEAD`)
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_manifest_at_head_when_asked() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.manifest("acme/app", APP_TEMPLATE);
        let _templates = git
            .list_templates(&owner(), &[repo("acme/app")])
            .await
            .unwrap();
        assert_eq!(
            gitlab.only().query.get("ref").map(String::as_str),
            Some("HEAD")
        );
    }

    // TC: GIT-14 (a token without `api` reads no file: `Forbidden` naming
    // the scope, not a catalogue without manifests)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_naming_api_scope_when_token_lacks_it() {
        let (git, gitlab) = forge().await;
        gitlab.accept("glpat-narrow", "owner", &["read_user"]);
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.manifest("acme/app", APP_TEMPLATE);
        let err = git
            .list_templates(
                &access_on(GITLAB, "owner", "glpat-narrow"),
                &[repo("acme/app")],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Forbidden(why) if words(why).contains("api")),
            "{err:?}"
        );
    }

    // TC: GIT-13 (a project the account may not see has no manifest)
    #[tokio::test(flavor = "multi_thread")]
    async fn no_manifest_when_gitlab_hides_project() {
        let (git, _gitlab) = forge().await;
        let templates = git
            .list_templates(&owner(), &[repo("acme/hidden")])
            .await
            .unwrap();
        assert_eq!(
            templates,
            vec![Template {
                repo: repo("acme/hidden"),
                manifest: None,
            }]
        );
    }
}

mod errors {
    use super::*;

    // TC: GIT-14
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_after_one_retry_when_gitlab_fails_once() {
        let (git, gitlab) = forge().await;
        gitlab.fail_next(1);
        let role = git.repo_role(&owner(), &repo(SHOP)).await.unwrap();
        assert_eq!((role, gitlab.seen().len()), (RepoRole::Admin, 2));
    }

    // TC: GIT-14
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_of_forge_after_exactly_one_retry_when_gitlab_keeps_failing() {
        let (git, gitlab) = forge().await;
        gitlab.fail_next(5);
        let err = git.repo_role(&owner(), &repo(SHOP)).await.unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Upstream {
                    system: System::Forge,
                    detail,
                } if detail.contains("503")
            ) && gitlab.seen().len() == 2,
            "{err:?} after {:?}",
            gitlab.requests()
        );
    }

    // TC: GIT-14 (a poll of the device flow redeems nothing until the
    // person confirmed: it is retried once)
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_after_one_retry_when_device_poll_fails_once() {
        let (git, gitlab) = forge_with(with_oauth()).await;
        gitlab.fail_next(1);
        let grant = git
            .device_poll(GitProvider::Gitlab, &gitlab_com(), &device_code())
            .await
            .unwrap();
        assert_eq!(
            (grant, gitlab.requests()),
            (
                DeviceGrant::Pending,
                vec![
                    request(GITLAB, "POST", "/oauth/token"),
                    request(GITLAB, "POST", "/oauth/token"),
                ]
            )
        );
    }

    // TC: GIT-14 (a fork that failed may have been created: it is not sent
    // again)
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_without_resending_when_fork_fails_once() {
        let (git, gitlab) = forge().await;
        gitlab.grant("owner", "acme/app", Access::project(REPORTER));
        gitlab.fail_next(1);
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
            ) && gitlab.seen().len() == 1,
            "{err:?} after {:?}",
            gitlab.requests()
        );
    }

    // TC: GIT-14 (a revoked stored connection is `Forbidden` with the hint to connect again, not a silent "none")
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_with_reconnect_hint_when_stored_token_is_revoked() {
        let (git, _gitlab) = forge().await;
        let err = git
            .repo_role(&access_on(GITLAB, "owner", "glpat-revoked"), &repo(SHOP))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Forbidden(why) if why.contains("lof git connect gitlab")),
            "{err:?}"
        );
    }
}

/// The plan's live checks of (section 3), run by the owner
/// against the real GitLab with `cargo nextest run -p liftoff-wasm
/// --run-ignored only -E 'test(live)'`.
mod live {
    use super::*;

    fn env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for this live check"))
    }

    /// The component loaded with `config` in a host whose `wasi:http`
    /// reaches the real network.
    async fn real_forge(config: ModuleConfig) -> Forge {
        let host: ModuleHostOf<InMemoryStore> = ModuleHostOf::new(
            InMemoryStore::new(),
            liftoff_wasm::TokioTimer,
            TracingLog,
            limits(),
        )
        .unwrap();
        let module = host
            .load(module(), &component("git_gitlab").unwrap(), config)
            .await
            .unwrap();
        module.migrate(0).await.unwrap();
        let set = ModuleSet::new();
        set.insert(module);
        ModuleGit::new(Arc::clone(host.registry()), set)
    }

    // TC: GIT-14 (the GitLab device flow on gitlab.com and self-hosted >= 17.2; LIFTOFF_LIVE_GITLAB_HOST defaults to gitlab.com)
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live: needs a GitLab OAuth application with the device flow (LIFTOFF_LIVE_GITLAB_CLIENT_ID) and the network"]
    async fn live_device_flow_starts_when_client_id_given() {
        let on = std::env::var("LIFTOFF_LIVE_GITLAB_HOST").unwrap_or_else(|_| GITLAB.to_owned());
        let client_id = env("LIFTOFF_LIVE_GITLAB_CLIENT_ID");
        let git = real_forge(config(&[("host", &on), ("client_id", &client_id)])).await;
        let started = git
            .device_start(GitProvider::Gitlab, &host(&on))
            .await
            .unwrap();
        assert!(
            !started.user_code.is_empty()
                && started
                    .verification_uri
                    .starts_with(&format!("https://{on}/")),
            "{started:?}"
        );
    }

    // TC: GIT-12 (fork and rename on the real GitLab, the rights it needs and how long it takes; creates LIFTOFF_LIVE_GITLAB_TARGET, which the owner deletes afterwards)
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "live: creates a project on a real GitLab (LIFTOFF_LIVE_GITLAB_TOKEN, LIFTOFF_LIVE_GITLAB_TEMPLATE, LIFTOFF_LIVE_GITLAB_TARGET)"]
    async fn live_fork_of_template_is_administered_by_connection_when_created() {
        let token = SecretValue::new(env("LIFTOFF_LIVE_GITLAB_TOKEN"));
        let template = repo(&env("LIFTOFF_LIVE_GITLAB_TEMPLATE"));
        let target = repo(&env("LIFTOFF_LIVE_GITLAB_TARGET"));
        let git = real_forge(ModuleConfig::new()).await;
        let login = git
            .account(GitProvider::Gitlab, &gitlab_com(), &token)
            .await
            .unwrap();
        let connection = GitAccess {
            provider: GitProvider::Gitlab,
            host: gitlab_com(),
            login,
            token,
        };
        let began = std::time::Instant::now();
        let created = git
            .generate_from_template(
                &connection,
                &template,
                &NewRepo {
                    repo: target.clone(),
                    private: true,
                },
            )
            .await
            .unwrap();
        let took = began.elapsed();
        let role = git.repo_role(&connection, &created).await.unwrap();
        let templates = git
            .list_templates(&connection, core::slice::from_ref(&created))
            .await
            .unwrap();
        assert!(
            created == target
                && role == RepoRole::Admin
                && templates
                    .first()
                    .is_some_and(|found| found.manifest.is_some()),
            "{created:?} {role:?} {templates:?} after {took:?}"
        );
    }
}
