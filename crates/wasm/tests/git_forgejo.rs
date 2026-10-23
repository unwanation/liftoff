#![cfg(test)]
//! The `git.forgejo` module (GITF-01…06, and the core cases DEP-07 and ENV-04 through the module).
//!
//! The built `modules/git-forgejo` component runs in a module host; the
//! core's `ModuleGit` (the `Git` port served by the forge modules) calls
//! its tools through the host's `ModuleSet`. Its `wasi:http` reaches
//! `git.example.com` (the host it is configured for, its `http_allow`),
//! which the host routes to a fake Forgejo API served by this test on
//! loopback (`ModuleHost::routing_https_to`). The core services run
//! over the shared `World` with `ModuleGit` in place of `FakeGit`
//! (`WithGit`).
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.
//!
//! The Forgejo API these tests pin (below `https://<host>/api/v1`, always `Authorization: token <credential>` of the user's own connection and never `Sudo`):
//!
//! | Port method | Request | Answer read |
//! |---|---|---|
//! | `account` | `GET /user` | `login` |
//! | `repo_role` | `GET /repos/{owner}/{name}` | `permissions.{admin,push,pull}`; `404` → none |
//! | `image_exists` | `GET /packages/{owner}/container/{name}/{tag}` | `2xx` → true, `404` → false |
//! | `put_ci_secret` | `PUT /repos/{owner}/{name}/actions/secrets/{KEY}` `{"data"}` | `2xx`; `403`/`404` → forbidden |
//! | `list_templates` | `GET /repos/{owner}/{name}/contents/liftoff.toml` per repository | `content` (base64); `404` → no manifest |
//! | `generate_from_template` | `POST /repos/{owner}/{name}/generate` `{"owner","name","private","git_content":true}` | `full_name` |
//!
//! A `5xx` is retried once, then `Upstream` (GITF-04); a `401` (a revoked
//! or expired token) is `Forbidden` with a `lof git connect` hint
//! (GITF-05); a host no module serves is `Validation` before any request
//! (GITF-06).

use core::net::SocketAddr;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use liftoff_core::domain::{
    Branch, DeployStatus, DeployTokenSecret, EnvKind, EnvName, GitProvider, Host, ImageTag, Login,
    ModuleName, ProjectName, RepoRef, SecretKey, SecretValue,
};
use liftoff_core::fakes::{InMemoryStore, WithGit, World};
use liftoff_core::module::{GitProviderSpec, ModuleGit};
use liftoff_core::ports::{
    Git as _, GitAccess, NewRepo, PackageRef, RepoRole, Store as _, Template,
};
use liftoff_core::service::{CiDeployRequest, DeployService, EnvironmentService, GitService};
use liftoff_core::{Error, System};
use liftoff_wasm::testing::{Host as ModuleHostOf, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TracingLog};
use serde_json::{Value, json};

/// The Forgejo host the module is configured for (the world's).
const GIT_HOST: &str = World::GIT_HOST;
/// The plaintext token of `owner`'s connection (the world's).
const OWNER_TOKEN: &str = World::OWNER_TOKEN;
/// A token Forgejo knew and has since expired.
const EXPIRED_TOKEN: &str = "forgejo-token-expired";
/// The repository of the world's project `shop`.
const SHOP: &str = "owner/shop";

fn module() -> ModuleName {
    ModuleName::new("git.forgejo").unwrap()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- The fake Forgejo API -----------------------------------------------------

/// A request the fake received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    /// The path, without the query.
    path: String,
    authorization: Option<String>,
    sudo: Option<String>,
    body: Value,
}

/// What an account may do on a repository, as Forgejo's `permissions`.
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

#[derive(Debug, Default)]
struct ForgeState {
    /// Token → account login.
    tokens: BTreeMap<String, String>,
    /// Tokens Forgejo refuses with `401` (expired).
    expired: BTreeSet<String>,
    /// (login, `owner/name`) → permissions; absent → `404`.
    perms: BTreeMap<(String, String), Perms>,
    /// (login, `owner/name:tag`) of the container packages it can read.
    packages: BTreeSet<(String, String)>,
    /// `owner/name` → its `liftoff.toml` on the default branch.
    manifests: BTreeMap<String, String>,
    /// How many of the next requests answer `503`.
    failing: usize,
    seen: Vec<Seen>,
}

/// Forgejo's API on `127.0.0.1`, as the module reaches it for
/// [`GIT_HOST`].
#[derive(Debug, Clone)]
struct Forgejo {
    addr: SocketAddr,
    state: Arc<Mutex<ForgeState>>,
}

impl Forgejo {
    /// The fake, knowing `owner`'s token (admin of `owner/shop`) and the
    /// expired [`EXPIRED_TOKEN`] of `owner`.
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = Self {
            addr,
            state: Arc::new(Mutex::new(ForgeState::default())),
        };
        fake.accept(OWNER_TOKEN, "owner");
        lock(&fake.state).expired.insert(EXPIRED_TOKEN.to_owned());
        fake.grant("owner", SHOP, ADMIN);
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(fake.clone());
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        fake
    }

    /// `token` logs in as `login`.
    fn accept(&self, token: &str, login: &str) {
        let _previous = lock(&self.state)
            .tokens
            .insert(token.to_owned(), login.to_owned());
    }

    /// `login` has `perms` on `repo`.
    fn grant(&self, login: &str, repo: &str, perms: Perms) {
        let _previous = lock(&self.state)
            .perms
            .insert((login.to_owned(), repo.to_owned()), perms);
    }

    /// `login` can read the container package `image` (`owner/name:tag`).
    fn publish(&self, login: &str, image: &str) {
        let _new = lock(&self.state)
            .packages
            .insert((login.to_owned(), image.to_owned()));
    }

    /// `repo` (`owner/name`) has `toml` as its `liftoff.toml`.
    fn manifest(&self, repo: &str, toml: &str) {
        let _previous = lock(&self.state)
            .manifests
            .insert(repo.to_owned(), toml.to_owned());
    }

    /// The next `times` requests answer `503`.
    fn fail_next(&self, times: usize) {
        lock(&self.state).failing = times;
    }

    fn seen(&self) -> Vec<Seen> {
        lock(&self.state).seen.clone()
    }

    /// Method and path of every request seen, in order.
    fn requests(&self) -> Vec<(String, String)> {
        self.seen()
            .into_iter()
            .map(|seen| (seen.method, seen.path))
            .collect()
    }

    /// The one request seen.
    fn only(&self) -> Seen {
        let seen = self.seen();
        assert_eq!(seen.len(), 1, "{:?}", self.requests());
        seen.into_iter().next().unwrap()
    }
}

fn reply(status: StatusCode, body: &Value) -> (StatusCode, axum::Json<Value>) {
    (status, axum::Json(body.clone()))
}

fn message(status: StatusCode, text: &str) -> (StatusCode, axum::Json<Value>) {
    reply(status, &json!({ "message": text }))
}

async fn handle(
    State(fake): State<Forgejo>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, axum::Json<Value>) {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let seen = Seen {
        method: method.to_string(),
        path: uri.path().to_owned(),
        authorization: header("authorization"),
        sudo: header("sudo"),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    answer(&mut lock(&fake.state), &seen)
}

/// The answer of a fake Forgejo route.
type Answer = (StatusCode, axum::Json<Value>);

/// What Forgejo answers the request `seen`, which it records.
fn answer(state: &mut ForgeState, seen: &Seen) -> Answer {
    state.seen.push(seen.clone());
    if state.failing > 0 {
        state.failing = state.failing.saturating_sub(1);
        return message(StatusCode::SERVICE_UNAVAILABLE, "down for maintenance");
    }
    let Some(path) = seen.path.strip_prefix("/api/v1/") else {
        return message(StatusCode::NOT_FOUND, "not found");
    };
    let token = seen
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("token "))
        .unwrap_or_default();
    if state.expired.contains(token) {
        return message(StatusCode::UNAUTHORIZED, "token is expired");
    }
    let Some(login) = state.tokens.get(token).cloned() else {
        return message(StatusCode::UNAUTHORIZED, "user does not exist");
    };
    route(state, seen, path, login)
}

/// The API route `path` of `seen`, as `login`.
fn route(state: &ForgeState, seen: &Seen, path: &str, login: String) -> Answer {
    let segments: Vec<&str> = path.split('/').collect();
    match (seen.method.as_str(), segments.as_slice()) {
        ("GET", ["user"]) => reply(StatusCode::OK, &json!({ "id": 7, "login": login })),
        ("GET", ["repos", owner, name]) => {
            let repo = format!("{owner}/{name}");
            state.perms.get(&(login, repo.clone())).map_or_else(
                || message(StatusCode::NOT_FOUND, "repository not found"),
                |perms| {
                    reply(
                        StatusCode::OK,
                        &json!({
                            "id": 1, "full_name": repo,
                            "permissions": {"admin": perms.admin, "push": perms.push, "pull": perms.pull},
                        }),
                    )
                },
            )
        }
        ("GET", ["packages", owner, "container", name, tag]) => {
            if state
                .packages
                .contains(&(login, format!("{owner}/{name}:{tag}")))
            {
                reply(
                    StatusCode::OK,
                    &json!({"id": 3, "type": "container", "name": name, "version": tag}),
                )
            } else {
                message(StatusCode::NOT_FOUND, "package not found")
            }
        }
        ("PUT", ["repos", owner, name, "actions", "secrets", _key]) => {
            let repo = format!("{owner}/{name}");
            match state.perms.get(&(login, repo)) {
                Some(perms) if perms.admin => (StatusCode::CREATED, axum::Json(Value::Null)),
                Some(_) => message(StatusCode::FORBIDDEN, "admin rights required"),
                None => message(StatusCode::NOT_FOUND, "repository not found"),
            }
        }
        ("GET", ["repos", owner, name, "contents", "liftoff.toml"]) => {
            state.manifests.get(&format!("{owner}/{name}")).map_or_else(
                || message(StatusCode::NOT_FOUND, "file not found"),
                |toml| {
                    reply(
                        StatusCode::OK,
                        &json!({
                            "name": "liftoff.toml", "path": "liftoff.toml", "type": "file",
                            "encoding": "base64", "content": STANDARD.encode(toml),
                        }),
                    )
                },
            )
        }
        ("POST", ["repos", template_owner, template_name, "generate"]) => {
            let owner = seen.body["owner"].as_str().unwrap_or_default();
            let name = seen.body["name"].as_str().unwrap_or_default();
            let target = format!("{owner}/{name}");
            if !state
                .perms
                .contains_key(&(login, format!("{template_owner}/{template_name}")))
            {
                message(StatusCode::NOT_FOUND, "repository not found")
            } else if state.perms.keys().any(|(_, repo)| *repo == target) {
                message(StatusCode::CONFLICT, "repository already exists")
            } else {
                reply(
                    StatusCode::CREATED,
                    &json!({"id": 9, "full_name": target, "private": seen.body["private"]}),
                )
            }
        }
        _ => message(StatusCode::NOT_FOUND, "not found"),
    }
}

// --- The module under test -------------------------------------------------------

type Forge = ModuleGit<ModuleSet<InMemoryStore, TracingLog>>;

/// The module's configuration: the Forgejo hosts it serves, a JSON array
/// as `config-get` answers a non-string value.
fn config() -> ModuleConfig {
    core::iter::once(("hosts".to_owned(), format!(r#"["{GIT_HOST}"]"#))).collect()
}

/// The built `git.forgejo` component loaded and migrated from 0, as an
/// install does, its [`GIT_HOST`] routed to a fresh fake; the core's
/// `Git` port over it.
async fn forge() -> (Forge, Forgejo) {
    let forgejo = Forgejo::start().await;
    let host: ModuleHostOf<InMemoryStore> = ModuleHostOf::new(
        InMemoryStore::new(),
        liftoff_wasm::TokioTimer,
        TracingLog,
        limits(),
    )
    .unwrap()
    .routing_https_to(GIT_HOST, forgejo.addr);
    let module = host
        .load(module(), &component("git_forgejo").unwrap(), config())
        .await
        .unwrap();
    module.migrate(0).await.unwrap();
    let set = ModuleSet::new();
    set.insert(module);
    (ModuleGit::new(Arc::clone(host.registry()), set), forgejo)
}

fn host(raw: &str) -> Host {
    Host::new(raw).unwrap()
}

fn repo(raw: &str) -> RepoRef {
    RepoRef::new(raw).unwrap()
}

/// `login`'s connection on `on` with `token`.
fn access_on(on: &str, login: &str, token: &str) -> GitAccess {
    GitAccess {
        provider: GitProvider::Forgejo,
        host: host(on),
        login: Login::new(login).unwrap(),
        token: SecretValue::new(token.to_owned()),
    }
}

/// `owner`'s connection on [`GIT_HOST`].
fn owner() -> GitAccess {
    access_on(GIT_HOST, "owner", OWNER_TOKEN)
}

fn image(owner: &str, name: &str, tag: &str) -> PackageRef {
    PackageRef {
        owner: owner.to_owned(),
        name: name.to_owned(),
        tag: ImageTag::new(tag).unwrap(),
    }
}

mod manifest {
    use super::*;

    // TC: GITF-06 (the module declares the hosts it serves as
    // `git_provider`, which the dispatcher matches)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_configured_hosts_as_git_provider_when_loaded() {
        let (git, _forgejo) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        assert_eq!(
            manifest.git_provider,
            Some(GitProviderSpec {
                hosts: vec![GIT_HOST.to_owned()],
                oauth: false,
            })
        );
    }

    // TC: GITF-06 (MOD-07: `wasi:http` reaches the served hosts only)
    #[tokio::test(flavor = "multi_thread")]
    async fn allows_http_to_configured_hosts_only_when_loaded() {
        let (git, _forgejo) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        let allowed: Vec<String> = manifest
            .http_allow
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(allowed, vec![GIT_HOST.to_owned()]);
    }

    // TC: GITF-03 (the forge tools take a plaintext credential,
    // so they are not MCP tools)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_no_mcp_tools_when_loaded() {
        let (git, _forgejo) = forge().await;
        let manifest = git.registry().manifest(&module()).unwrap();
        assert!(manifest.tools.is_empty(), "{:?}", manifest.tools);
    }
}

mod account {
    use super::*;

    // TC: REST-02 (`lof git connect forgejo --token` stays in
    // the core, the token is checked through the module with one call)
    #[tokio::test(flavor = "multi_thread")]
    async fn login_of_token_without_sudo_when_forgejo_accepts_it() {
        let (git, forgejo) = forge().await;
        forgejo.accept("tok", "Alice");
        let login = git
            .account(
                GitProvider::Forgejo,
                &host(GIT_HOST),
                &SecretValue::new("tok".to_owned()),
            )
            .await
            .unwrap();
        let seen = forgejo.only();
        assert_eq!(
            (
                login.as_str(),
                seen.method.as_str(),
                seen.path.as_str(),
                seen.authorization.as_deref(),
                seen.sudo,
            ),
            ("alice", "GET", "/api/v1/user", Some("token tok"), None)
        );
    }

    // TC: REST-02 (a rejected token stores nothing)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_forgejo_rejects_token() {
        let (git, _forgejo) = forge().await;
        let err = git
            .account(
                GitProvider::Forgejo,
                &host(GIT_HOST),
                &SecretValue::new("nope".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
    }
}

mod repo_role {
    use super::*;

    async fn role_with(perms: Option<Perms>) -> Result<RepoRole, Error> {
        let (git, forgejo) = forge().await;
        if let Some(perms) = perms {
            forgejo.grant("alice", "acme/web", perms);
        }
        forgejo.accept("alice-token", "alice");
        git.repo_role(
            &access_on(GIT_HOST, "alice", "alice-token"),
            &repo("acme/web"),
        )
        .await
    }

    // TC: GITF-01
    #[tokio::test(flavor = "multi_thread")]
    async fn admin_when_connection_administers_repo() {
        assert_eq!(role_with(Some(ADMIN)).await.unwrap(), RepoRole::Admin);
    }

    // TC: GITF-01
    #[tokio::test(flavor = "multi_thread")]
    async fn write_when_connection_may_push() {
        assert_eq!(role_with(Some(WRITE)).await.unwrap(), RepoRole::Write);
    }

    // TC: GITF-01
    #[tokio::test(flavor = "multi_thread")]
    async fn read_when_connection_may_only_pull() {
        assert_eq!(role_with(Some(READ)).await.unwrap(), RepoRole::Read);
    }

    // TC: GITF-01 (no access: Forgejo hides the repository)
    #[tokio::test(flavor = "multi_thread")]
    async fn none_when_forgejo_hides_repo() {
        assert_eq!(role_with(None).await.unwrap(), RepoRole::None);
    }

    // TC: GITF-01 (the user's own connection, no `Sudo`)
    #[tokio::test(flavor = "multi_thread")]
    async fn asks_with_connection_token_without_sudo_when_asked() {
        let (git, forgejo) = forge().await;
        let _role = git.repo_role(&owner(), &repo(SHOP)).await.unwrap();
        let seen = forgejo.only();
        assert_eq!(
            (
                seen.method.as_str(),
                seen.path.as_str(),
                seen.authorization,
                seen.sudo,
            ),
            (
                "GET",
                "/api/v1/repos/owner/shop",
                Some(format!("token {OWNER_TOKEN}")),
                None
            )
        );
    }
}

mod image_exists {
    use super::*;

    // TC: GITF-02
    #[tokio::test(flavor = "multi_thread")]
    async fn true_when_connection_reads_container_package() {
        let (git, forgejo) = forge().await;
        forgejo.publish("owner", "owner/shop:sha-7");
        let exists = git
            .image_exists(&owner(), &image("owner", "shop", "sha-7"))
            .await
            .unwrap();
        assert_eq!(
            (exists, forgejo.requests()),
            (
                true,
                vec![(
                    "GET".to_owned(),
                    "/api/v1/packages/owner/container/shop/sha-7".to_owned()
                )]
            )
        );
    }

    // TC: GITF-02
    #[tokio::test(flavor = "multi_thread")]
    async fn false_when_package_is_missing() {
        let (git, _forgejo) = forge().await;
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

    // TC: GITF-03
    #[tokio::test(flavor = "multi_thread")]
    async fn puts_value_as_data_when_connection_administers_repo() {
        let (git, forgejo) = forge().await;
        git.put_ci_secret(
            &owner(),
            &repo(SHOP),
            &key(),
            &SecretValue::new("lf_secret".to_owned()),
        )
        .await
        .unwrap();
        let seen = forgejo.only();
        assert_eq!(
            (
                seen.method.as_str(),
                seen.path.as_str(),
                seen.authorization,
                seen.body
            ),
            (
                "PUT",
                "/api/v1/repos/owner/shop/actions/secrets/LIFTOFF_TOKEN",
                Some(format!("token {OWNER_TOKEN}")),
                json!({"data": "lf_secret"})
            )
        );
    }

    // TC: GITF-03 (a value longer than one `wasi:http` write chunk:
    // the body is streamed after the request is handed to the host)
    #[tokio::test(flavor = "multi_thread")]
    async fn puts_whole_value_when_value_spans_many_write_chunks() {
        let (git, forgejo) = forge().await;
        let value = "x".repeat(64 * 1024);
        git.put_ci_secret(
            &owner(),
            &repo(SHOP),
            &key(),
            &SecretValue::new(value.clone()),
        )
        .await
        .unwrap();
        assert_eq!(forgejo.only().body, json!({ "data": value }));
    }

    // TC: GITF-03 (only an admin manages CI secrets)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_connection_may_only_push() {
        let (git, forgejo) = forge().await;
        forgejo.grant("owner", SHOP, WRITE);
        let err = git
            .put_ci_secret(
                &owner(),
                &repo(SHOP),
                &key(),
                &SecretValue::new("lf_secret".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
    }
}

mod retry {
    use super::*;

    // TC: GITF-04
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_after_one_retry_when_forgejo_fails_once() {
        let (git, forgejo) = forge().await;
        forgejo.fail_next(1);
        let role = git.repo_role(&owner(), &repo(SHOP)).await.unwrap();
        assert_eq!((role, forgejo.seen().len()), (RepoRole::Admin, 2));
    }

    // TC: GITF-04 (a `PUT` is idempotent: it is sent again once)
    #[tokio::test(flavor = "multi_thread")]
    async fn puts_secret_after_one_retry_when_forgejo_fails_once() {
        let (git, forgejo) = forge().await;
        forgejo.fail_next(1);
        git.put_ci_secret(
            &owner(),
            &repo(SHOP),
            &SecretKey::new("LIFTOFF_TOKEN").unwrap(),
            &SecretValue::new("lf_secret".to_owned()),
        )
        .await
        .unwrap();
        let path = "/api/v1/repos/owner/shop/actions/secrets/LIFTOFF_TOKEN".to_owned();
        assert_eq!(
            forgejo.requests(),
            vec![("PUT".to_owned(), path.clone()), ("PUT".to_owned(), path)]
        );
    }

    // TC: GITF-04 (the core names the forge as the failed system)
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_of_forge_when_forgejo_keeps_failing() {
        let (git, forgejo) = forge().await;
        forgejo.fail_next(5);
        let err = git
            .image_exists(&owner(), &image("owner", "shop", "sha-7"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Upstream { system: System::Forge, detail }
                if detail.contains(GIT_HOST) && detail.contains("down for maintenance")),
            "{err:?}"
        );
    }

    // TC: GITF-04
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_after_exactly_one_retry_when_forgejo_keeps_failing() {
        let (git, forgejo) = forge().await;
        forgejo.fail_next(5);
        let err = git.repo_role(&owner(), &repo(SHOP)).await.unwrap_err();
        assert!(
            matches!(&err, Error::Upstream { detail, .. } if detail.contains("503"))
                && forgejo.seen().len() == 2,
            "{err:?} after {:?}",
            forgejo.requests()
        );
    }

    // TC: GITF-04 (a POST that failed may have created the repository:
    // it is not sent again, so the call never reports its own repository
    // as a conflict)
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_without_resending_when_generate_fails_once() {
        let (git, forgejo) = forge().await;
        forgejo.fail_next(1);
        let err = git
            .generate_from_template(
                &owner(),
                &repo("acme/app"),
                &NewRepo {
                    repo: repo("owner/blog"),
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
            ) && forgejo.seen().len() == 1,
            "{err:?} after {:?}",
            forgejo.requests()
        );
    }
}

mod expired_token {
    use super::*;

    // TC: GITF-05
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_with_connect_hint_when_token_expired() {
        let (git, _forgejo) = forge().await;
        let err = git
            .repo_role(&access_on(GIT_HOST, "owner", EXPIRED_TOKEN), &repo(SHOP))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Forbidden(why)
                if why.contains("lof git connect") && why.contains(GIT_HOST)),
            "{err:?}"
        );
    }

    // TC: GITF-05 (the image check of a CI deploy fails the same way)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_with_connect_hint_when_image_checked_with_expired_token() {
        let (git, _forgejo) = forge().await;
        let err = git
            .image_exists(
                &access_on(GIT_HOST, "owner", EXPIRED_TOKEN),
                &image("owner", "shop", "sha-7"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Forbidden(why) if why.contains("lof git connect")),
            "{err:?}"
        );
    }
}

mod dispatch {
    use super::*;

    // TC: GITF-06
    #[tokio::test(flavor = "multi_thread")]
    async fn validation_without_request_when_no_module_serves_host() {
        let (git, forgejo) = forge().await;
        let err = git
            .repo_role(
                &access_on("code.example.org", "owner", OWNER_TOKEN),
                &repo(SHOP),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Validation(_)) && forgejo.seen().is_empty(),
            "{err:?} after {:?}",
            forgejo.requests()
        );
    }

    // TC: GITF-06 (the module itself refuses a host it is not configured
    // for, whoever calls it)
    #[tokio::test(flavor = "multi_thread")]
    async fn module_refuses_host_it_does_not_serve_when_called_directly() {
        use liftoff_core::module::ModuleCalls as _;

        let (git, forgejo) = forge().await;
        let args = json!({
            "host": "code.example.org", "login": "owner", "token": OWNER_TOKEN, "repo": SHOP,
        });
        let err = git
            .calls()
            .handle_tool(&module(), None, "repo_role", &args.to_string())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Validation(_)) && forgejo.seen().is_empty(),
            "{err:?}"
        );
    }

    // TC: GITF-06 (`lof git connect` to a host no module serves)
    #[tokio::test(flavor = "multi_thread")]
    async fn validation_when_token_is_for_host_no_module_serves() {
        let (git, _forgejo) = forge().await;
        let err = git
            .account(
                GitProvider::Forgejo,
                &host("code.example.org"),
                &SecretValue::new("tok".to_owned()),
            )
            .await
            .unwrap_err();
        assert!(matches!(&err, Error::Validation(_)), "{err:?}");
    }
}

mod templates {
    use super::*;

    /// A template's `liftoff.toml`.
    const APP_TEMPLATE: &str = "[template]\ngroup = \"web\"\nname = \"acme-app\"\n";

    // TC: GIT-07 (forgejo, `list_templates`
    // reads `liftoff.toml` by `contents`, in the order asked)
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_manifest_of_each_repo_when_asked() {
        let (git, forgejo) = forge().await;
        forgejo.grant("owner", "acme/app", READ);
        forgejo.grant("owner", "acme/plain", READ);
        forgejo.manifest("acme/app", APP_TEMPLATE);
        let templates = git
            .list_templates(&owner(), &[repo("acme/app"), repo("acme/plain")])
            .await
            .unwrap();
        assert_eq!(
            templates,
            vec![
                Template {
                    repo: repo("acme/app"),
                    manifest: Some(APP_TEMPLATE.to_owned()),
                },
                Template {
                    repo: repo("acme/plain"),
                    manifest: None,
                },
            ]
        );
    }

    // TC: GIT-06 (forgejo, `generate_from_template` with `private` and `owner`)
    #[tokio::test(flavor = "multi_thread")]
    async fn generates_repo_with_content_when_template_given() {
        let (git, forgejo) = forge().await;
        forgejo.grant("owner", "acme/app", READ);
        let created = git
            .generate_from_template(
                &owner(),
                &repo("acme/app"),
                &NewRepo {
                    repo: repo("owner/blog"),
                    private: true,
                },
            )
            .await
            .unwrap();
        let seen = forgejo.only();
        assert_eq!(
            (
                created,
                seen.method.as_str(),
                seen.path.as_str(),
                seen.body["owner"].clone(),
                seen.body["name"].clone(),
                seen.body["private"].clone(),
                seen.body["git_content"].clone(),
            ),
            (
                repo("owner/blog"),
                "POST",
                "/api/v1/repos/acme/app/generate",
                json!("owner"),
                json!("blog"),
                json!(true),
                json!(true),
            )
        );
    }

    // TC: GIT-06 (the target repository already exists: Forgejo answers
    // `409`)
    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_when_target_repo_exists() {
        let (git, forgejo) = forge().await;
        forgejo.grant("owner", "acme/app", READ);
        let err = git
            .generate_from_template(
                &owner(),
                &repo("acme/app"),
                &NewRepo {
                    repo: repo(SHOP),
                    private: true,
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Conflict(why) if why.contains(SHOP)),
            "{err:?}"
        );
    }

    // TC: GIT-06 (the template is unknown or unreadable: Forgejo answers
    // `404`)
    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_when_template_is_missing() {
        let (git, _forgejo) = forge().await;
        let err = git
            .generate_from_template(
                &owner(),
                &repo("acme/missing"),
                &NewRepo {
                    repo: repo("owner/blog"),
                    private: true,
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::NotFound(what) if what.contains("acme/missing")),
            "{err:?}"
        );
    }
}

// --- The core services over the module -------------------------------------------

/// The world with `ModuleGit` over the module in place of `FakeGit`.
struct Services {
    world: World,
    ports: Arc<WithGit<Forge>>,
    forgejo: Forgejo,
}

async fn services() -> Services {
    let world = World::new().await;
    let (git, forgejo) = forge().await;
    let ports = Arc::new(WithGit::new(Arc::clone(&world.ports), git));
    Services {
        world,
        ports,
        forgejo,
    }
}

impl Services {
    fn environments(&self) -> EnvironmentService<WithGit<Forge>> {
        EnvironmentService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
    }

    fn deploys(&self) -> DeployService<WithGit<Forge>> {
        DeployService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
    }

    fn git(&self) -> GitService<WithGit<Forge>> {
        GitService::new(Arc::clone(&self.ports))
    }

    /// `lof env add staging --branch dev` in `shop` as `owner`.
    async fn add_staging(&self) -> Result<liftoff_core::domain::Environment, Error> {
        self.environments()
            .add(
                &self.world.owner,
                &ProjectName::new("shop").unwrap(),
                EnvName::new("staging").unwrap(),
                Branch::new("dev").unwrap(),
                EnvKind::Compose,
            )
            .await
    }

    /// A CI deploy of `main` with image tag `tag` to `prod`, run.
    async fn ci_deploy(&self, tag: &str) -> liftoff_core::domain::Deploy {
        let token: DeployTokenSecret = self.world.deploy_token(&self.world.shop).await.unwrap();
        let request = CiDeployRequest {
            repo: repo(SHOP),
            git_ref: Branch::new("main").unwrap(),
            tag: ImageTag::new(tag).unwrap(),
            compose: "services:\n  web:\n    image: git.example.com/owner/shop:${IMAGE_TAG}\n"
                .to_owned(),
            env: None,
        };
        let outcome = self.deploys().request_ci(&token, request).await.unwrap();
        assert_eq!(outcome.deploys.len(), 1, "prod is pinned to main");
        self.deploys()
            .run(outcome.deploys[0].deploy_id)
            .await
            .unwrap()
    }
}

mod environment_add {
    use super::*;

    // TC: ENV-04 (through the module)
    #[tokio::test(flavor = "multi_thread")]
    async fn pins_branch_by_actor_when_forgejo_says_actor_is_admin() {
        let services = services().await;
        let added = services.add_staging().await.unwrap();
        assert_eq!(
            added.pin.map(|pin| (pin.branch, pin.pinned_by)),
            Some((Branch::new("dev").unwrap(), services.world.owner.id))
        );
    }

    // TC: ENV-04 (through the module: the actor's own connection is asked)
    #[tokio::test(flavor = "multi_thread")]
    async fn asks_forgejo_with_actor_token_when_adding() {
        let services = services().await;
        let _added = services.add_staging().await.unwrap();
        let asked: Vec<(String, Option<String>)> = services
            .forgejo
            .seen()
            .into_iter()
            .map(|seen| (seen.path, seen.authorization))
            .collect();
        assert_eq!(
            asked,
            vec![(
                "/api/v1/repos/owner/shop".to_owned(),
                Some(format!("token {OWNER_TOKEN}"))
            )]
        );
    }

    // TC: ENV-04 (through the module: role `read` is not enough)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_forgejo_says_actor_may_only_read() {
        let services = services().await;
        services.forgejo.grant("owner", SHOP, READ);
        let err = services.add_staging().await.unwrap_err();
        assert!(
            matches!(&err, Error::Forbidden(why) if why.contains("is not an admin of owner/shop")),
            "{err:?}"
        );
    }

    // TC: ENV-04 (through the module: a refused add stores nothing)
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_nothing_when_forgejo_says_actor_may_only_read() {
        let services = services().await;
        services.forgejo.grant("owner", SHOP, READ);
        let _refused = services.add_staging().await;
        let stored = services
            .world
            .ports
            .store
            .environment_by_name(services.world.shop.id, &EnvName::new("staging").unwrap())
            .await
            .unwrap();
        // The module must have been asked (a stubbed forge also stores nothing).
        assert_eq!(
            (stored, services.forgejo.seen().len()),
            (None, 1),
            "{:?}",
            services.forgejo.requests()
        );
    }
}

mod ci_deploy {
    use super::*;

    // TC: DEP-07 (through the module)
    #[tokio::test(flavor = "multi_thread")]
    async fn succeeds_when_pinned_by_connection_reads_image() {
        let services = services().await;
        services.forgejo.publish("owner", "owner/shop:sha-7");
        let done = services.ci_deploy("sha-7").await;
        assert_eq!(done.status, DeployStatus::Succeeded, "{:?}", done.error);
    }

    // TC: DEP-07 (through the module: the image is checked with the token
    // of `pinned_by`)
    #[tokio::test(flavor = "multi_thread")]
    async fn checks_image_with_pinned_by_token_when_ci_deploys() {
        let services = services().await;
        services.forgejo.publish("owner", "owner/shop:sha-7");
        let _done = services.ci_deploy("sha-7").await;
        let checks: Vec<(String, Option<String>)> = services
            .forgejo
            .seen()
            .into_iter()
            .filter(|seen| seen.path.starts_with("/api/v1/packages/"))
            .map(|seen| (seen.path, seen.authorization))
            .collect();
        assert_eq!(
            checks,
            vec![(
                "/api/v1/packages/owner/container/shop/sha-7".to_owned(),
                Some(format!("token {OWNER_TOKEN}"))
            )]
        );
    }

    // TC: DEP-07 (through the module: the image is not readable)
    #[tokio::test(flavor = "multi_thread")]
    async fn fails_naming_image_when_forgejo_has_no_package() {
        let services = services().await;
        let done = services.ci_deploy("sha-8").await;
        let error = done.error.clone().unwrap_or_default();
        assert!(
            done.status == DeployStatus::Failed
                && error.contains("git.example.com/owner/shop:sha-8")
                && error.contains("not readable"),
            "{:?} {error:?}",
            done.status
        );
    }
}

mod git_connect {
    use super::*;

    // TC: REST-02 (`lof git connect forgejo --token` stays in
    // the core and stores the login the module answered)
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_login_module_answered_when_token_accepted() {
        let services = services().await;
        services.forgejo.accept("new-token", "Owner-On-Forge");
        let stored = services
            .git()
            .connect(
                &services.world.owner,
                GitProvider::Forgejo,
                host(GIT_HOST),
                SecretValue::new("new-token".to_owned()),
            )
            .await
            .unwrap();
        let kept = services
            .world
            .ports
            .store
            .git_connection(services.world.owner.id, &host(GIT_HOST))
            .await
            .unwrap()
            .map(|connection| connection.account_login.to_string());
        assert_eq!(
            (stored.account_login.to_string(), kept),
            (
                "owner-on-forge".to_owned(),
                Some("owner-on-forge".to_owned())
            )
        );
    }
}
