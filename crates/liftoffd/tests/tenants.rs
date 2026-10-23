#![cfg(test)]
//! The `tenants` module (TEN-01…11 and the v1 SSO cases that still apply, v1 catalogue section 14): the
//! built `modules/tenants` component loaded by the module host, the core's
//! `ModuleHooks` over it, the core services over the world with those
//! hooks, and the gate (the core's `GateListener` behind the REST
//! adapter's routers, driven in memory).
//!
//! The module keeps its tables (`m_tenants_tenants`, `m_tenants_memberships`) through `store-query`, which needs SQLite: its host stores in
//! a `SqliteStore` (the core's rows stay in the world's `InMemoryStore`,
//! except for the v1 import, whose core rows are SQLite too), which is
//! why the test lives with the composition root (it joins `liftoff-wasm`
//! and `liftoff-store`). Its `core-call` is the daemon's wiring: a
//! `LateCore` bound by `liftoffd::modules::bind_bridge` over the ports
//! with the module's hooks.
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.
//!
//! What these tests pin, besides the catalogue:
//!
//! - the module's routes below `/api/v1/m/tenants`, one per CLI command of
//!   its manifest (`lof tenant members add` is `POST /tenant/members/add`),
//!   every one behind a session, JSON in and out, `2xx` on success,
//!   problem+json otherwise:
//!
//!   | Route | Body | Rule |
//!   |---|---|---|
//!   | `POST /tenant/create` | `{"slug","name"}` | anyone; the creator becomes owner; `max_projects` 3, `env_cpus` 1, `env_memory_mb` 1024; answers the tenant with its `id` |
//!   | `POST /tenant/ls` | `{}` | the actor's tenants (`[{"id","slug",…}]`), every tenant for the admin |
//!   | `POST /tenant/rm` | `{"tenant"}` | owner or admin; `409` while it has projects (TEN-10) |
//!   | `POST /tenant/members/add` | `{"tenant","login","owner"}` | owner or admin (TEN-03) |
//!   | `POST /tenant/members/rm` | `{"tenant","login"}` | owner or admin (TEN-03) |
//!   | `POST /tenant/members/ls` | `{"tenant"}` | members: `[{"login","role"}]` by login |
//!   | `POST /protect`, `POST /unprotect` | `{"project","env"}` | members, owners and the admin (TEN-06, TEN-09) |
//!   | `POST /admin/tenant/set` | `{"tenant","max_projects","env_cpus","env_memory_mb"}` | admin only (TEN-04, TEN-05) |
//!
//! - the gate (`liftoff_core::service::GateListener`): `__Host-liftoff_session`
//!   is the browser session on the API host, `__Host-liftoff_gate` the
//!   session on one protected host, handed over by a one-time code (the
//!   v1 SSO gate; as short-lived as the one-time links).

use std::sync::Arc;

use liftoff_core::Error;
use liftoff_core::domain::{
    Branch, Cpus, DeployStatus, EnvKind, EnvName, EnvState, Environment, Host, InstalledModule,
    Limits, MemoryMb, ModuleConfigRecord, ModuleName, ModuleVersion, NewProject, Project,
    ProjectName, RepoRef, SessionKind, TemplateSource, TenantId, TenantQuotas, TenantSlug, User,
};
use liftoff_core::error::System;
use liftoff_core::fakes::{FakePorts, SeqRng, WithHooks, WithStore, World};
use liftoff_core::module::{CoreBridge, ModuleHooks};
use liftoff_core::ports::{
    Clock as _, HookResult, Hooks as _, ModuleRequest, ModuleStore as _, RepoRole, Store as _,
};
use liftoff_core::service::{
    ArtifactContent, ArtifactsService, AuthService, DomainService, EnvironmentService,
    GateListener, ProjectService, TemplateCatalog,
};
use liftoff_store::{SqliteStore, V1Import, import_v1};
use liftoff_wasm::testing::{Host as ModuleHostOf, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TokioTimer, TracingLog};
use liftoffd::modules::{LateCore, bind_bridge};
use serde_json::{Value, json};

/// The protected host of the fixture: `blog.apps.test` → `blog/prod`.
const BLOG_HOST: &str = "blog.apps.test";
/// Another protected host of `acme`: `news.apps.test` → `blog/qa`.
const NEWS_HOST: &str = "news.apps.test";

fn tenants() -> ModuleName {
    ModuleName::new("tenants").unwrap()
}

/// The module's tables over SQLite (and, for the v1 import, the core's).
type Tables = SqliteStore;

/// The dispatcher over the loaded `tenants`.
type Tenants = ModuleHooks<ModuleSet<Tables, TracingLog, LateCore>>;

/// The world's fakes with the module's hooks.
type Ports = WithHooks<Tenants>;

/// `tenants` installed and loaded over `ports`' twin, its `core-call`
/// bound to the core over `ports` as the daemon binds it, migrated from
/// 0 as an install does.
async fn load_into<P: liftoff_core::ports::Ports + 'static>(
    host: &ModuleHostOf<Tables, LateCore>,
    core: &LateCore,
    ports: &Arc<P>,
    settings: &Arc<liftoff_core::service::Settings>,
    set: &ModuleSet<Tables, TracingLog, LateCore>,
) {
    let settings = Arc::clone(settings);
    bind_bridge(core, ports, move |ports| {
        CoreBridge::new(ports, Arc::clone(&settings))
    });
    let module = host
        .load(
            tenants(),
            &component("tenants").unwrap(),
            ModuleConfig::new(),
        )
        .await
        .unwrap();
    set.insert(module.clone());
    module.migrate(0).await.unwrap();
}

/// The installed row of `tenants` (what `lof module install` leaves).
fn installed(world_clock: liftoff_core::domain::Timestamp) -> InstalledModule {
    InstalledModule {
        name: tenants(),
        version: ModuleVersion::new("0.1.0").unwrap(),
        sha256: "cd".repeat(32),
        enabled: true,
        config: ModuleConfigRecord::new(),
        error: None,
        installed_at: world_clock,
    }
}

/// The world with `tenants` installed and migrated from 0, nothing else
/// arranged: the single user `owner`, project `shop`, domain
/// `shop.apps.test`.
struct Migrated {
    world: World,
    ports: Arc<Ports>,
    tables: Tables,
}

/// `tenants` loaded by a fresh module host over `tables` and migrated
/// from 0, as `lof module install` does (again, after a `remove` without
/// `--purge`, over the tables it kept): the world's fakes with its hooks.
async fn install(world: &World, tables: &Tables) -> Arc<Ports> {
    let core = LateCore::default();
    let host: ModuleHostOf<Tables, LateCore> =
        ModuleHostOf::new(tables.clone(), TokioTimer, TracingLog, limits())
            .unwrap()
            .with_core_calls(core.clone())
            .unwrap();
    let set = ModuleSet::new();
    let hooks = ModuleHooks::new(Arc::clone(host.registry()), set.clone());
    let ports = Arc::new(WithHooks::new(Arc::clone(&world.ports), hooks));
    load_into(&host, &core, &ports, &world.settings, &set).await;
    ports
}

async fn migrated() -> Migrated {
    let world = World::new().await;
    world
        .ports
        .store
        .insert_module(&installed(world.ports.clock.now()))
        .await
        .unwrap();
    let tables = SqliteStore::open_in_memory().unwrap();
    let ports = install(&world, &tables).await;
    Migrated {
        world,
        ports,
        tables,
    }
}

/// The rows `sql` selects from the module's tables.
async fn rows(tables: &Tables, sql: &str) -> Value {
    serde_json::from_str(&tables.module_query(&tenants(), sql, &[]).await.unwrap()).unwrap()
}

/// `POST <path>` of the module as `actor` with `body`: the status and the
/// JSON answer (`null` when the answer is not JSON).
async fn route(ports: &Ports, actor: &User, path: &str, body: &Value) -> (u16, Value) {
    let answer = ports
        .hooks
        .handle_route(
            &tenants(),
            Some(actor),
            ModuleRequest {
                method: "POST".to_owned(),
                path: path.to_owned(),
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: body.to_string().into_bytes(),
            },
        )
        .await
        .unwrap();
    let parsed = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
    (answer.status, parsed)
}

/// Whether `status` is a success.
fn ok(status: u16) -> bool {
    (200..300).contains(&status)
}

/// The full fixture over [`migrated`]:
///
/// - users `bob`, `carol`, `dave` (role `user`, each with a Forgejo
///   connection on `git.example.com`);
/// - tenant `acme` created by `bob` (its owner), `carol` its member;
///   `dave` is in no tenant (only `owner` is in `default`);
/// - project `blog` of `acme` (`bob/blog`, `bob` admin of the repository)
///   with environments `prod` (running, `blog.apps.test` → `web:3000` at
///   20001) and `qa` (running, `news.apps.test` at 20002).
struct Fixture {
    world: World,
    ports: Arc<Ports>,
    bob: User,
    carol: User,
    dave: User,
    acme: TenantId,
    blog: Project,
    blog_prod: Environment,
    blog_qa: Environment,
}

async fn fixture() -> Fixture {
    let Migrated { world, ports, .. } = migrated().await;
    let bob = world.add_connected_user("bob").await.unwrap();
    let carol = world.add_connected_user("carol").await.unwrap();
    let dave = world.add_connected_user("dave").await.unwrap();
    let (created, tenant) = route(
        &ports,
        &bob,
        "/tenant/create",
        &json!({"slug": "acme", "name": "Acme"}),
    )
    .await;
    assert!(ok(created), "tenant create: {created} {tenant}");
    let acme = TenantId::parse(tenant["id"].as_str().unwrap_or_default()).unwrap();
    let (added, answer) = route(
        &ports,
        &bob,
        "/tenant/members/add",
        &json!({"tenant": "acme", "login": "carol", "owner": false}),
    )
    .await;
    assert!(ok(added), "members add: {added} {answer}");

    let store = &world.ports.store;
    let mut blog = world
        .records
        .project("blog", World::GIT_HOST, "bob/blog", &bob)
        .unwrap();
    blog.tenant_id = Some(acme);
    store.insert_project(&blog).await.unwrap();
    world
        .ports
        .git
        .grant(&bob.login, &blog.repo, RepoRole::Admin);
    let mut bound = Vec::new();
    for (env, host, port) in [("prod", BLOG_HOST, 20_001), ("qa", NEWS_HOST, 20_002)] {
        let environment = world.add_environment(&blog, env).await.unwrap();
        let mut domain = world.records.domain(&blog, host).unwrap();
        domain.tenant_id = Some(acme);
        store.insert_domain(&domain).await.unwrap();
        let binding = world
            .records
            .binding(&environment, &domain, "web", 3000, port)
            .unwrap();
        store.insert_binding(&binding).await.unwrap();
        bound.push(
            world
                .deployed(&environment, EnvState::Running)
                .await
                .unwrap(),
        );
    }
    let blog_qa = bound.pop().unwrap();
    let blog_prod = bound.pop().unwrap();
    world.log().clear();
    Fixture {
        world,
        ports,
        bob,
        carol,
        dave,
        acme,
        blog,
        blog_prod,
        blog_qa,
    }
}

impl Fixture {
    fn projects(&self) -> ProjectService<Ports> {
        ProjectService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
    }

    fn artifacts(&self) -> ArtifactsService<Ports> {
        ArtifactsService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
    }

    fn environments(&self) -> EnvironmentService<Ports> {
        EnvironmentService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
    }

    async fn route(&self, actor: &User, path: &str, body: &Value) -> (u16, Value) {
        route(&self.ports, actor, path, body).await
    }

    /// `lof admin tenant set --tenant acme` with `quotas` as the owner
    /// (the admin); panics unless it succeeds.
    async fn set_acme(&self, quotas: Value) {
        let mut body = quotas;
        body["tenant"] = json!("acme");
        let (status, answer) = self
            .route(&self.world.owner, "/admin/tenant/set", &body)
            .await;
        assert!(ok(status), "admin tenant set: {status} {answer}");
    }

    /// The stored environment `id`.
    async fn stored(&self, environment: &Environment) -> Environment {
        self.world
            .ports
            .store
            .environment(environment.id)
            .await
            .unwrap()
            .unwrap()
    }

    /// Marks `environment` protected in the core's store (what `lof
    /// protect` leaves).
    async fn protect_in_store(&self, environment: &Environment) {
        self.world
            .ports
            .store
            .set_environment_protected(environment.id, true)
            .await
            .unwrap();
    }

    /// A `browser` session of `user` (what the `oidc` module's browser
    /// login opens): the cookie `__Host-liftoff_session=<secret>`.
    async fn central(&self, user: &User) -> String {
        self.central_for(user, time::Duration::hours(24)).await
    }

    /// A `browser` session of `user` that ends in `lifetime`, as
    /// [`Fixture::central`].
    async fn central_for(&self, user: &User, lifetime: time::Duration) -> String {
        let (mut session, secret) = self.world.records.session(user);
        session.kind = SessionKind::Browser;
        session.expires_at = self.world.ports.clock.now().saturating_add(lifetime);
        self.world
            .ports
            .store
            .insert_session(&session)
            .await
            .unwrap();
        format!("__Host-liftoff_session={}", secret.expose())
    }

    /// `GET /gate/start?<query>` on the API with `cookie`, through the
    /// REST router.
    async fn start(&self, query: &str, cookie: Option<&str>) -> GateResponse {
        let app = liftoff_rest::app_router(
            liftoff_rest::AppState::new(),
            liftoff_rest::Services::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings)),
            &liftoff_rest::RestConfig::default(),
        );
        send(
            app,
            None,
            &format!("/gate/start?{query}"),
            cookie_header(cookie),
        )
        .await
    }

    /// `GET <path>` on the gate listener's router from the loopback peer
    /// Caddy is, with `headers`.
    async fn gate_listener(&self, path: &str, headers: Vec<(String, String)>) -> GateResponse {
        let gate = GateListener::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings));
        send(
            liftoff_rest::gate_router(gate),
            Some(LOOPBACK),
            path,
            headers,
        )
        .await
    }

    /// `GET /gate/check` from Caddy for `host` and `uri` with `cookie`.
    async fn check(&self, host: &str, uri: &str, cookie: Option<&str>) -> GateResponse {
        self.check_with(host, uri, cookie, Vec::new()).await
    }

    /// [`Fixture::check`] with the headers `extra` too.
    async fn check_with(
        &self,
        host: &str,
        uri: &str,
        cookie: Option<&str>,
        extra: Vec<(String, String)>,
    ) -> GateResponse {
        let mut headers = vec![
            ("x-liftoff-host".to_owned(), host.to_owned()),
            ("x-forwarded-method".to_owned(), "GET".to_owned()),
            ("x-forwarded-uri".to_owned(), uri.to_owned()),
        ];
        headers.extend(cookie_header(cookie));
        headers.extend(extra);
        self.gate_listener("/gate/check", headers).await
    }

    /// `GET /.liftoff/<rest>` on `host` from Caddy with `cookie`.
    async fn liftoff_path(
        &self,
        host: Option<&str>,
        rest: &str,
        cookie: Option<&str>,
    ) -> GateResponse {
        let mut headers: Vec<(String, String)> = host
            .map(|host| ("x-liftoff-host".to_owned(), host.to_owned()))
            .into_iter()
            .collect();
        headers.extend(cookie_header(cookie));
        self.gate_listener(&format!("/.liftoff/{rest}"), headers)
            .await
    }

    /// The one-time code `/gate/start` issues `user` for `host` with `rd`.
    async fn code(&self, user: &User, host: &str, rd: &str) -> String {
        let central = self.central(user).await;
        let started = self
            .start(&format!("host={host}&rd={rd}"), Some(&central))
            .await;
        let location = started.header("location").unwrap_or_default().to_owned();
        location.split_once("code=").map_or_else(
            || panic!("no code in {started:?}"),
            |(_, code)| code.to_owned(),
        )
    }

    /// The host cookie (`name=value`) `user` ends up with on `host`
    /// through `/gate/start` and `/.liftoff/callback`.
    async fn host_cookie(&self, user: &User, host: &str) -> String {
        let central = self.central(user).await;
        self.host_cookie_from(&central, host).await
    }

    /// The host cookie the browser with the browser session `central`
    /// (from [`Fixture::central`]) ends up with on `host`.
    async fn host_cookie_from(&self, central: &str, host: &str) -> String {
        let started = self
            .start(&format!("host={host}&rd=%2F"), Some(central))
            .await;
        let location = started.header("location").unwrap_or_default().to_owned();
        let code = location.split_once("code=").map_or_else(
            || panic!("no code in {started:?}"),
            |(_, code)| code.to_owned(),
        );
        let redeemed = self
            .liftoff_path(Some(host), &format!("callback?code={code}"), None)
            .await;
        cookie_pair(&redeemed)
    }
}

/// The peer address of the local Caddy.
const LOOPBACK: core::net::SocketAddr = core::net::SocketAddr::new(
    core::net::IpAddr::V4(core::net::Ipv4Addr::LOCALHOST),
    51_000,
);

/// What an HTTP answer of the gate says.
#[derive(Debug)]
struct GateResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl GateResponse {
    /// The first value of the header `name`.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// `GET path` with `headers` through `app`, from `peer` when given (what
/// the server attaches to an accepted connection).
async fn send(
    app: axum::Router,
    peer: Option<core::net::SocketAddr>,
    path: &str,
    headers: Vec<(String, String)>,
) -> GateResponse {
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;
    let mut request = http::Request::builder().method("GET").uri(path);
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let mut request = request.body(axum::body::Body::empty()).unwrap();
    if let Some(peer) = peer {
        let _previous = request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
    }
    let response = app.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    GateResponse {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

fn cookie_header(cookie: Option<&str>) -> Vec<(String, String)> {
    cookie
        .map(|cookie| ("cookie".to_owned(), cookie.to_owned()))
        .into_iter()
        .collect()
}

/// `name=value` of the `Set-Cookie` of `response`.
fn cookie_pair(response: &GateResponse) -> String {
    response
        .header("set-cookie")
        .and_then(|cookie| cookie.split(';').next())
        .unwrap_or_else(|| panic!("no Set-Cookie in {response:?}"))
        .to_owned()
}

fn project(name: &str) -> ProjectName {
    ProjectName::new(name).unwrap()
}

/// The slug of the fixture's tenant.
fn acme_slug() -> TenantSlug {
    TenantSlug::new("acme").unwrap()
}

fn names(projects: &[Project]) -> Vec<String> {
    projects
        .iter()
        .map(|project| project.name.to_string())
        .collect()
}

mod manifest {
    use super::*;

    fn manifest(fixture: &Migrated) -> liftoff_core::module::ModuleManifest {
        fixture.ports.hooks.registry().manifest(&tenants()).unwrap()
    }

    // TC: TEN-01 (`tenants` requires `oidc`)
    #[tokio::test(flavor = "multi_thread")]
    async fn requires_oidc_when_loaded() {
        let fixture = migrated().await;
        assert_eq!(
            manifest(&fixture).requires,
            vec![ModuleName::new("oidc").unwrap()]
        );
    }

    // TC: TEN-02, TEN-05, TEN-07 (the hooks it cuts the core with)
    #[tokio::test(flavor = "multi_thread")]
    async fn subscribes_to_authorize_before_deploy_and_gate_when_loaded() {
        let fixture = migrated().await;
        let hooks: Vec<&str> = manifest(&fixture)
            .hooks
            .iter()
            .map(|hook| hook.as_str())
            .collect();
        assert!(
            ["authorize", "before_deploy", "gate"]
                .iter()
                .all(|hook| hooks.contains(hook)),
            "{hooks:?}"
        );
    }

    // TC: TEN-03, TEN-06 (`lof tenant create|ls|rm|members add|rm|ls`, `lof protect|unprotect`, `lof admin tenant set`)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_tenant_protect_and_admin_commands_when_loaded() {
        let fixture = migrated().await;
        let commands: Vec<String> = manifest(&fixture)
            .commands
            .iter()
            .map(|command| command.name.clone())
            .collect();
        let missing: Vec<&str> = [
            "tenant create",
            "tenant ls",
            "tenant rm",
            "tenant members add",
            "tenant members rm",
            "tenant members ls",
            "protect",
            "unprotect",
            "admin tenant set",
        ]
        .into_iter()
        .filter(|name| !commands.iter().any(|command| command == name))
        .collect();
        assert_eq!(missing, Vec::<&str>::new(), "{commands:?}");
    }

    // TC: TEN-06 (`lof protect --env prod` in a project's directory: the project is an argument the CLI fills)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_protect_project_as_project_arg_when_loaded() {
        let fixture = migrated().await;
        let kinds: Vec<(String, Option<liftoff_core::module::ArgKind>)> = manifest(&fixture)
            .commands
            .iter()
            .filter(|command| command.name == "protect" || command.name == "unprotect")
            .map(|command| {
                let project = command.args.iter().find(|arg| arg.name == "project");
                (command.name.clone(), project.map(|arg| arg.kind))
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                (
                    "protect".to_owned(),
                    Some(liftoff_core::module::ArgKind::Project)
                ),
                (
                    "unprotect".to_owned(),
                    Some(liftoff_core::module::ArgKind::Project)
                ),
            ]
        );
    }

    // TC: TEN-04 (the tenant of a new project is the module's)
    #[tokio::test(flavor = "multi_thread")]
    async fn subscribes_to_tenant_data_hooks_when_loaded() {
        let fixture = migrated().await;
        let hooks: Vec<&str> = manifest(&fixture)
            .hooks
            .iter()
            .map(|hook| hook.as_str())
            .collect();
        assert!(
            ["tenant_quotas", "project_tenant"]
                .iter()
                .all(|hook| hooks.contains(hook)),
            "{hooks:?}"
        );
    }
}

mod migrate {
    use super::*;

    // TC: TEN-01 (the tenant `default` on install)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_default_tenant_when_migrated_from_zero() {
        let fixture = migrated().await;
        assert_eq!(
            rows(&fixture.tables, "SELECT slug FROM m_tenants_tenants").await,
            json!([{"slug": "default"}])
        );
    }

    // TC: TEN-01 (its owner is the single existing user)
    #[tokio::test(flavor = "multi_thread")]
    async fn makes_single_user_owner_of_default_when_migrated_from_zero() {
        let fixture = migrated().await;
        assert_eq!(
            rows(
                &fixture.tables,
                "SELECT user_id, role FROM m_tenants_memberships"
            )
            .await,
            json!([{"user_id": fixture.world.owner.id.to_string(), "role": "owner"}])
        );
    }

    /// The id of `default`.
    async fn default_id(fixture: &Migrated) -> Option<TenantId> {
        let found = rows(
            &fixture.tables,
            "SELECT id FROM m_tenants_tenants WHERE slug = 'default'",
        )
        .await;
        found[0]["id"]
            .as_str()
            .and_then(|id| TenantId::parse(id).ok())
    }

    // TC: TEN-01 (every project gets the tenant_id of `default`)
    #[tokio::test(flavor = "multi_thread")]
    async fn assigns_default_tenant_to_existing_project_when_migrated_from_zero() {
        let fixture = migrated().await;
        let shop = fixture
            .world
            .ports
            .store
            .project(fixture.world.shop.id)
            .await
            .unwrap()
            .unwrap();
        let default = default_id(&fixture).await;
        assert_eq!((default.is_some(), shop.tenant_id), (true, default));
    }

    // TC: REG-07 (the tables `remove` kept without `--purge` serve the next install as they are)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_tenants_and_members_when_reinstalled_over_kept_tables() {
        let fixture = migrated().await;
        let (status, _) = route(
            &fixture.ports,
            &fixture.world.owner,
            "/tenant/create",
            &json!({"slug": "acme", "name": "Acme"}),
        )
        .await;
        assert!(ok(status), "{status}");
        let before = (
            rows(
                &fixture.tables,
                "SELECT id, slug FROM m_tenants_tenants ORDER BY slug",
            )
            .await,
            rows(
                &fixture.tables,
                "SELECT tenant_id, user_id, role FROM m_tenants_memberships ORDER BY tenant_id",
            )
            .await,
        );
        install(&fixture.world, &fixture.tables).await;
        let after = (
            rows(
                &fixture.tables,
                "SELECT id, slug FROM m_tenants_tenants ORDER BY slug",
            )
            .await,
            rows(
                &fixture.tables,
                "SELECT tenant_id, user_id, role FROM m_tenants_memberships ORDER BY tenant_id",
            )
            .await,
        );
        assert_eq!(after, before);
    }

    // TC: TEN-01 (every domain gets the tenant_id of `default`)
    #[tokio::test(flavor = "multi_thread")]
    async fn assigns_default_tenant_to_existing_domain_when_migrated_from_zero() {
        let fixture = migrated().await;
        let domain = fixture
            .world
            .ports
            .store
            .domain(fixture.world.domain.id)
            .await
            .unwrap()
            .unwrap();
        let default = default_id(&fixture).await;
        assert_eq!((default.is_some(), domain.tenant_id), (true, default));
    }
}

/// TEN-01 over a database imported from v1 (`liftoffd setup --import`): the fixture of the store's import tests, whose
/// tenants `acme` (owner alice, member bob; 10 targets, 2 CPUs, 2048 MiB)
/// and `beta` (owner bob; 3, 1 CPU, 1024 MiB) wait in
/// `import_v1_tenants`. The core and the module share the SQLite store.
mod import_v1_tenants {
    use super::*;

    const V1_SQL: &str = include_str!("../../store/tests/fixtures/v1.sql");
    const ALICE: &str = "01HV000000000000000000A001";
    const BOB: &str = "01HV000000000000000000A002";
    const ACME: &str = "01HV000000000000000000B001";
    const BETA: &str = "01HV000000000000000000B002";

    /// The imported store with `tenants` installed, loaded and migrated
    /// from 0 over it.
    async fn imported() -> SqliteStore {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("liftoff-v1.db");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(V1_SQL)
            .unwrap();
        let store = SqliteStore::open_in_memory().unwrap();
        let fakes = Arc::new(FakePorts::new());
        import_v1(
            &path,
            &store,
            V1Import {
                issuer: liftoff_core::domain::OidcIssuer::new("https://auth.example.com").unwrap(),
                git_host: Host::new(World::GIT_HOST).unwrap(),
                secrets: fakes.secrets.clone(),
                rng: SeqRng::new(),
                clock: fakes.clock.clone(),
            },
        )
        .await
        .unwrap();
        store
            .insert_module(&installed(fakes.clock.now()))
            .await
            .unwrap();
        let ports = Arc::new(WithStore::new(fakes, store.clone()));
        let settings = Arc::new((*World::new().await.settings).clone());
        let core = LateCore::default();
        let host: ModuleHostOf<Tables, LateCore> =
            ModuleHostOf::new(store.clone(), TokioTimer, TracingLog, limits())
                .unwrap()
                .with_core_calls(core.clone())
                .unwrap();
        load_into(&host, &core, &ports, &settings, &ModuleSet::new()).await;
        store
    }

    // TC: TEN-01 (import_v1_tenants: the v1 tenants keep their ids, slugs,
    // names and quotas)
    #[tokio::test(flavor = "multi_thread")]
    async fn adopts_v1_tenants_when_import_present() {
        let store = imported().await;
        assert_eq!(
            rows(
                &store,
                "SELECT id, slug, name, max_projects, env_memory_mb FROM m_tenants_tenants \
                 WHERE slug IN ('acme', 'beta') ORDER BY slug"
            )
            .await,
            json!([
                {"id": ACME, "slug": "acme", "name": "Acme Inc.", "max_projects": 10, "env_memory_mb": 2048},
                {"id": BETA, "slug": "beta", "name": "Beta Team", "max_projects": 3, "env_memory_mb": 1024},
            ])
        );
    }

    // TC: TEN-01 (and the v1 memberships with their roles)
    #[tokio::test(flavor = "multi_thread")]
    async fn adopts_v1_memberships_when_import_present() {
        let store = imported().await;
        assert_eq!(
            rows(
                &store,
                &format!(
                    "SELECT tenant_id, user_id, role FROM m_tenants_memberships \
                     WHERE tenant_id IN ('{ACME}', '{BETA}') ORDER BY tenant_id, user_id"
                )
            )
            .await,
            json!([
                {"tenant_id": ACME, "user_id": ALICE, "role": "owner"},
                {"tenant_id": ACME, "user_id": BOB, "role": "member"},
                {"tenant_id": BETA, "user_id": BOB, "role": "owner"},
            ])
        );
    }

    // TC: TEN-01 (the imported projects get their v1 tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn assigns_v1_tenant_to_imported_projects_when_import_present() {
        let store = imported().await;
        let git_host = Host::new(World::GIT_HOST).unwrap();
        let mut tenants = Vec::new();
        for repo in ["acme/shop", "beta/shop"] {
            let project = store
                .project_by_repo(&git_host, &RepoRef::new(repo).unwrap())
                .await
                .unwrap()
                .unwrap();
            tenants.push(project.tenant_id.map(|id| id.to_string()));
        }
        assert_eq!(tenants, vec![Some(ACME.to_owned()), Some(BETA.to_owned())]);
    }

    // TC: TEN-01 (and the imported domains)
    #[tokio::test(flavor = "multi_thread")]
    async fn assigns_v1_tenant_to_imported_domains_when_import_present() {
        let store = imported().await;
        let mut tenants = Vec::new();
        for host in ["shop.apps.test", "www.beta.example"] {
            let domain = store
                .domain_by_host(&Host::new(host).unwrap())
                .await
                .unwrap()
                .unwrap();
            tenants.push(domain.tenant_id.map(|id| id.to_string()));
        }
        assert_eq!(tenants, vec![Some(ACME.to_owned()), Some(BETA.to_owned())]);
    }
}

mod authorize {
    use super::*;

    // TC: TEN-02 (a member sees the projects of their tenant only)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_own_tenant_projects_when_member_lists() {
        let fixture = fixture().await;
        let listed = fixture.projects().list(&fixture.carol).await;
        assert_eq!(
            listed.map(|p| names(&p)).map_err(|e| e.to_string()),
            Ok(vec!["blog".to_owned()])
        );
    }

    // TC: TEN-02 (a project of another tenant → 403)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_member_gets_project_of_another_tenant() {
        let fixture = fixture().await;
        let got = fixture
            .projects()
            .get(&fixture.carol, &project("shop"))
            .await;
        assert!(matches!(got, Err(Error::Forbidden(_))), "{got:?}");
    }

    // TC: TEN-02 (a project of their own tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn gets_project_when_member_of_its_tenant() {
        let fixture = fixture().await;
        let got = fixture
            .projects()
            .get(&fixture.carol, &project("blog"))
            .await;
        assert_eq!(
            got.map(|p| p.id).map_err(|e| e.to_string()),
            Ok(fixture.blog.id)
        );
    }

    // TC: TEN-02 (an environment of another tenant → 403)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_member_stops_environment_of_another_tenant() {
        let fixture = fixture().await;
        let stopped = fixture
            .environments()
            .stop(
                &fixture.carol,
                &project("shop"),
                &EnvName::new("prod").unwrap(),
            )
            .await;
        assert!(matches!(stopped, Err(Error::Forbidden(_))), "{stopped:?}");
    }

    // TC: TEN-02 (the admin does everything: every tenant's projects)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_every_project_when_admin_lists() {
        let fixture = fixture().await;
        let listed = fixture.projects().list(&fixture.world.owner).await;
        assert_eq!(
            listed.map(|p| names(&p)).map_err(|e| e.to_string()),
            Ok(vec!["blog".to_owned(), "shop".to_owned()])
        );
    }

    // TC: TEN-02, TEN-04 (a new project in a tenant: only its members)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_outsider_creates_project_in_tenant() {
        let fixture = fixture().await;
        let repo = RepoRef::new("dave/news").unwrap();
        fixture
            .world
            .ports
            .git
            .grant(&fixture.dave.login, &repo, RepoRole::Admin);
        let created = fixture
            .projects()
            .create(
                &fixture.dave,
                NewProject {
                    tenant: Some(acme_slug()),
                    name: project("news"),
                    git_host: Host::new(World::GIT_HOST).unwrap(),
                    repo,
                    manifest: None,
                },
            )
            .await;
        let stored = fixture
            .world
            .ports
            .store
            .projects()
            .await
            .unwrap()
            .into_iter()
            .any(|stored| stored.name.as_str() == "news");
        assert_eq!(
            (matches!(created, Err(Error::Forbidden(_))), stored),
            (true, false),
            "{created:?}"
        );
    }

    // TC: TEN-02 (a member sees the domains of their tenant only; the
    // admin every one)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_own_tenant_domains_when_member_lists() {
        let fixture = fixture().await;
        let domains = DomainService::new(
            Arc::clone(&fixture.ports),
            Arc::clone(&fixture.world.settings),
        );
        let hosts = |listed: Vec<liftoff_core::service::DomainRecord>| -> Vec<String> {
            listed
                .iter()
                .map(|record| record.domain.host.to_string())
                .collect()
        };
        let member = domains.list(&fixture.carol).await.map(hosts);
        let admin = domains.list(&fixture.world.owner).await.map(hosts);
        assert_eq!(
            (
                member.map_err(|e| e.to_string()),
                admin.map_err(|e| e.to_string())
            ),
            (
                Ok(vec![BLOG_HOST.to_owned(), NEWS_HOST.to_owned()]),
                Ok(vec![
                    BLOG_HOST.to_owned(),
                    NEWS_HOST.to_owned(),
                    "shop.apps.test".to_owned()
                ])
            )
        );
    }

    // TC: TPL-05, TEN-02 (`tenants` opens `git.templates` to
    // members, so the catalogue is read as their own connection)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_templates_when_member_lists_catalogue() {
        let fixture = fixture().await;
        let tpl = RepoRef::new("acme/app").unwrap();
        fixture.world.ports.git.template(
            &tpl,
            "[template]\ngroup = \"web\"\nname = \"acme-app\"\ndescription = \"d\"\n\n[service]\ncompose = \"compose.yaml\"\nexpose = \"web:3000\"\n",
        );
        fixture
            .world
            .ports
            .git
            .grant(&fixture.carol.login, &tpl, RepoRole::Read);
        let catalog = TemplateCatalog::new(
            Arc::clone(&fixture.ports),
            Arc::new(liftoff_core::service::Settings {
                templates: vec![TemplateSource::new("git.example.com/acme/app").unwrap()],
                ..(*fixture.world.settings).clone()
            }),
        );
        let listed = catalog.list(&fixture.carol, &[], None).await;
        assert_eq!(
            listed
                .map(|found| (found.groups.len(), found.warnings.len()))
                .map_err(|e| e.to_string()),
            Ok((1, 0))
        );
    }

    // TC: TEN-02 (someone in no tenant of the project → 403)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_outsider_gets_project() {
        let fixture = fixture().await;
        let got = fixture
            .projects()
            .get(&fixture.dave, &project("blog"))
            .await;
        assert!(matches!(got, Err(Error::Forbidden(_))), "{got:?}");
    }
}

mod artifacts {
    use super::*;

    /// A one-page version.
    fn page() -> ArtifactContent {
        ArtifactContent {
            html: b"<h1>notes</h1>".to_vec(),
            ..ArtifactContent::default()
        }
    }

    // TC: ART-11, TEN-02 (`tenants` opens `artifact.publish`: a member
    // publishes their own artifact)
    #[tokio::test(flavor = "multi_thread")]
    async fn publishes_when_member_publishes() {
        let fixture = fixture().await;
        let published = fixture.artifacts().publish(&fixture.carol, page()).await;
        assert_eq!(
            published
                .map(|published| published.artifact.owner_user_id)
                .map_err(|e| e.to_string()),
            Ok(fixture.carol.id)
        );
    }

    // TC: ART-12, TEN-02 (`artifact.list` is open; one's own only)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_own_artifacts_when_member_lists() {
        let fixture = fixture().await;
        let artifacts = fixture.artifacts();
        let carols = artifacts
            .publish(&fixture.carol, page())
            .await
            .unwrap()
            .artifact
            .id;
        artifacts.publish(&fixture.dave, page()).await.unwrap();
        let listed = artifacts.list(&fixture.carol, false).await;
        assert_eq!(
            listed
                .map(|found| found.iter().map(|a| a.id).collect::<Vec<_>>())
                .map_err(|e| e.to_string()),
            Ok(vec![carols])
        );
    }

    // TC: ART-12 (`ls --all` stays the admin's)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_member_lists_all() {
        let fixture = fixture().await;
        let listed = fixture.artifacts().list(&fixture.carol, true).await;
        assert!(matches!(listed, Err(Error::Forbidden(_))), "{listed:?}");
    }

    // TC: ART-11 (the module allows the `artifact` resource; the owner
    // changes their own)
    #[tokio::test(flavor = "multi_thread")]
    async fn shares_own_artifact_when_owner_shares() {
        let fixture = fixture().await;
        let artifacts = fixture.artifacts();
        let id = artifacts
            .publish(&fixture.carol, page())
            .await
            .unwrap()
            .artifact
            .id;
        let shared = artifacts.share(&fixture.carol, id).await;
        assert!(shared.is_ok(), "{shared:?}");
    }

    // TC: ART-11 (another user, even of the same tenant, gets 403)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_tenant_mate_shares_members_artifact() {
        let fixture = fixture().await;
        let artifacts = fixture.artifacts();
        let id = artifacts
            .publish(&fixture.carol, page())
            .await
            .unwrap()
            .artifact
            .id;
        let shared = artifacts.share(&fixture.bob, id).await;
        assert!(matches!(shared, Err(Error::Forbidden(_))), "{shared:?}");
    }
}

mod members {
    use super::*;

    // TC: TEN-03 (the owner adds a member, who then reaches the tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn member_reaches_tenant_when_owner_adds_them() {
        let fixture = fixture().await;
        let (added, _) = fixture
            .route(
                &fixture.bob,
                "/tenant/members/add",
                &json!({"tenant": "acme", "login": "dave", "owner": false}),
            )
            .await;
        let got = fixture
            .projects()
            .get(&fixture.dave, &project("blog"))
            .await;
        assert_eq!(
            (ok(added), got.map(|p| p.id).map_err(|e| e.to_string())),
            (true, Ok(fixture.blog.id))
        );
    }

    // TC: TEN-03 (a member does not manage members)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_member_adds_member() {
        let fixture = fixture().await;
        let (added, _) = fixture
            .route(
                &fixture.carol,
                "/tenant/members/add",
                &json!({"tenant": "acme", "login": "dave", "owner": false}),
            )
            .await;
        assert_eq!(added, 403);
    }

    // TC: TEN-03
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_member_removes_member() {
        let fixture = fixture().await;
        let (removed, _) = fixture
            .route(
                &fixture.carol,
                "/tenant/members/rm",
                &json!({"tenant": "acme", "login": "bob"}),
            )
            .await;
        assert_eq!(removed, 403);
    }

    // TC: TEN-03 (the owner removes a member, who loses the tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn member_loses_tenant_when_owner_removes_them() {
        let fixture = fixture().await;
        let (removed, _) = fixture
            .route(
                &fixture.bob,
                "/tenant/members/rm",
                &json!({"tenant": "acme", "login": "carol"}),
            )
            .await;
        let got = fixture
            .projects()
            .get(&fixture.carol, &project("blog"))
            .await;
        assert!(
            ok(removed) && matches!(got, Err(Error::Forbidden(_))),
            "{removed} {got:?}"
        );
    }

    // TC: TEN-03 (a tenant keeps an owner: the last one is not removed)
    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_when_owner_removes_last_owner() {
        let fixture = fixture().await;
        let (removed, answer) = fixture
            .route(
                &fixture.bob,
                "/tenant/members/rm",
                &json!({"tenant": "acme", "login": "bob"}),
            )
            .await;
        let (_, members) = fixture
            .route(
                &fixture.bob,
                "/tenant/members/ls",
                &json!({"tenant": "acme"}),
            )
            .await;
        assert_eq!(
            (removed, answer["detail"].clone(), members[0].clone()),
            (
                409,
                json!("bob is the last owner of the tenant acme"),
                json!({"login": "bob", "role": "owner"})
            )
        );
    }

    // TC: TEN-03 (`lof tenant members ls`)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_members_with_roles_when_owner_lists() {
        let fixture = fixture().await;
        let (listed, members) = fixture
            .route(
                &fixture.bob,
                "/tenant/members/ls",
                &json!({"tenant": "acme"}),
            )
            .await;
        assert_eq!(
            (listed, members),
            (
                200,
                json!([
                    {"login": "bob", "role": "owner"},
                    {"login": "carol", "role": "member"},
                ])
            )
        );
    }
}

mod quotas {
    use super::*;

    /// `bob` creates project `news` (`bob/news`) in `acme`.
    async fn create_news(fixture: &Fixture) -> Result<Project, Error> {
        let repo = RepoRef::new("bob/news").unwrap();
        fixture
            .world
            .ports
            .git
            .grant(&fixture.bob.login, &repo, RepoRole::Admin);
        fixture
            .projects()
            .create(
                &fixture.bob,
                NewProject {
                    tenant: Some(acme_slug()),
                    name: project("news"),
                    git_host: Host::new(World::GIT_HOST).unwrap(),
                    repo,
                    manifest: None,
                },
            )
            .await
    }

    /// `POST /api/v1/projects` of `name` (`<login>/<name>`, of which
    /// `user` is admin) as `user`, through the REST router, naming no
    /// tenant: the status and the JSON answer.
    async fn post_project(fixture: &Fixture, user: &User, name: &str) -> (u16, Value) {
        use http_body_util::BodyExt as _;
        use tower::ServiceExt as _;
        let repo = format!("{}/{name}", user.login);
        fixture
            .world
            .ports
            .git
            .grant(&user.login, &RepoRef::new(&repo).unwrap(), RepoRole::Admin);
        let token = fixture.world.login(user).await.unwrap();
        let app = liftoff_rest::app_router(
            liftoff_rest::AppState::new(),
            liftoff_rest::Services::new(
                Arc::clone(&fixture.ports),
                Arc::clone(&fixture.world.settings),
            ),
            &liftoff_rest::RestConfig::default(),
        );
        let body = json!({"name": name, "git_host": World::GIT_HOST, "repo": repo});
        let request = http::Request::builder()
            .method("POST")
            .uri("/api/v1/projects")
            .header("authorization", format!("Bearer {}", token.expose()))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// The stored project `name`, if any.
    async fn stored_project(fixture: &Fixture, name: &str) -> Option<Project> {
        fixture
            .world
            .ports
            .store
            .projects()
            .await
            .unwrap()
            .into_iter()
            .find(|stored| stored.name.as_str() == name)
    }

    // TC: TEN-04 (a member's `POST /projects` without a tenant lands in their only tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_rest_project_in_members_tenant_when_no_tenant_named() {
        let fixture = fixture().await;
        let (status, answer) = post_project(&fixture, &fixture.carol, "news").await;
        assert_eq!(
            (
                status,
                stored_project(&fixture, "news")
                    .await
                    .and_then(|project| project.tenant_id)
            ),
            (201, Some(fixture.acme)),
            "{answer}"
        );
    }

    // TC: TEN-04 (`max_projects` through REST: 409, nothing stored)
    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_when_rest_project_exceeds_max_projects() {
        let fixture = fixture().await;
        fixture.set_acme(json!({"max_projects": 1})).await;
        let (status, answer) = post_project(&fixture, &fixture.carol, "news").await;
        assert_eq!(
            (status, stored_project(&fixture, "news").await.is_none()),
            (409, true),
            "{answer}"
        );
    }

    // TC: TEN-02, TEN-04 (someone in no tenant creates no project outside
    // every tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_outsider_posts_project_without_tenant() {
        let fixture = fixture().await;
        let (status, answer) = post_project(&fixture, &fixture.dave, "news").await;
        assert_eq!(
            (status, stored_project(&fixture, "news").await.is_none()),
            (403, true),
            "{answer}"
        );
    }

    // TC: TEN-04 (an unknown tenant slug → 404)
    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_when_project_names_unknown_tenant() {
        let fixture = fixture().await;
        let repo = RepoRef::new("bob/news").unwrap();
        fixture
            .world
            .ports
            .git
            .grant(&fixture.bob.login, &repo, RepoRole::Admin);
        let created = fixture
            .projects()
            .create(
                &fixture.bob,
                NewProject {
                    tenant: Some(TenantSlug::new("nope").unwrap()),
                    name: project("news"),
                    git_host: Host::new(World::GIT_HOST).unwrap(),
                    repo,
                    manifest: None,
                },
            )
            .await;
        assert!(matches!(created, Err(Error::NotFound(_))), "{created:?}");
    }

    // TC: TEN-05 (an environment within its tenant's limits deploys)
    #[tokio::test(flavor = "multi_thread")]
    async fn continues_deploy_when_environment_within_tenant_limits() {
        let fixture = fixture().await;
        fixture
            .set_acme(json!({"env_cpus": "2", "env_memory_mb": 2048}))
            .await;
        let prod = fixture.stored(&fixture.blog_prod).await;
        let decided = before_deploy(&fixture, &prod).await;
        assert_eq!(decided, HookResult::Continue);
    }

    // TC: TEN-04 (`max_projects` reached → Conflict)
    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_when_project_create_exceeds_max_projects() {
        let fixture = fixture().await;
        fixture.set_acme(json!({"max_projects": 1})).await;
        let created = create_news(&fixture).await;
        assert!(matches!(created, Err(Error::Conflict(_))), "{created:?}");
    }

    // TC: TEN-04 (below the quota the project is created in the tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_project_in_tenant_when_below_max_projects() {
        let fixture = fixture().await;
        fixture.set_acme(json!({"max_projects": 2})).await;
        let created = create_news(&fixture).await;
        assert_eq!(
            created
                .map(|project| project.tenant_id)
                .map_err(|e| e.to_string()),
            Ok(Some(fixture.acme))
        );
    }

    // TC: TEN-04 (only the admin sets quotas)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_owner_sets_tenant_quotas() {
        let fixture = fixture().await;
        let (status, _) = fixture
            .route(
                &fixture.bob,
                "/admin/tenant/set",
                &json!({"tenant": "acme", "max_projects": 100}),
            )
            .await;
        assert_eq!(status, 403);
    }

    // TC: TEN-05 (the tenant's limits go into a new environment)
    #[tokio::test(flavor = "multi_thread")]
    async fn new_environment_gets_tenant_limits_when_added() {
        let fixture = fixture().await;
        fixture
            .set_acme(json!({"env_cpus": "0.5", "env_memory_mb": 256}))
            .await;
        let added = fixture
            .environments()
            .add(
                &fixture.bob,
                &project("blog"),
                EnvName::new("staging").unwrap(),
                Branch::new("staging").unwrap(),
                EnvKind::Compose,
            )
            .await;
        assert_eq!(
            added
                .map(|environment| environment.limits)
                .map_err(|e| e.to_string()),
            Ok(Limits {
                cpus: Cpus::parse("0.5").unwrap(),
                memory_mb: MemoryMb::new(256).unwrap(),
            })
        );
    }

    /// `before_deploy` of a new deploy of `environment` as the dispatcher
    /// asks the module.
    async fn before_deploy(fixture: &Fixture, environment: &Environment) -> HookResult {
        let deploy = fixture
            .world
            .records
            .deploy(environment, 2, DeployStatus::Queued, &fixture.bob)
            .unwrap();
        fixture
            .ports
            .hooks
            .before_deploy(environment, &deploy)
            .await
    }

    // TC: TEN-05 (the tenant was shrunk below the environment)
    #[tokio::test(flavor = "multi_thread")]
    async fn denies_deploy_when_tenant_shrunk_below_environment() {
        let fixture = fixture().await;
        fixture
            .set_acme(json!({"env_cpus": "1", "env_memory_mb": 512}))
            .await;
        let prod = fixture.stored(&fixture.blog_prod).await;
        let decided = before_deploy(&fixture, &prod).await;
        assert!(matches!(decided, HookResult::Deny(_)), "{decided:?}");
    }

    // TC: TEN-05 (the admin raised the environment above the tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn denies_deploy_when_admin_raised_environment_above_tenant() {
        let fixture = fixture().await;
        fixture
            .set_acme(json!({"env_cpus": "1", "env_memory_mb": 1024}))
            .await;
        let mut prod = fixture.stored(&fixture.blog_prod).await;
        prod.limits.cpus = Cpus::parse("2").unwrap();
        fixture
            .world
            .ports
            .store
            .update_environment(&prod)
            .await
            .unwrap();
        let decided = before_deploy(&fixture, &prod).await;
        assert!(matches!(decided, HookResult::Deny(_)), "{decided:?}");
    }
}

mod tenant_create {
    use super::*;

    // TC: TEN-03 (a slug names one tenant)
    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_when_slug_exists() {
        let fixture = fixture().await;
        let (created, answer) = fixture
            .route(
                &fixture.dave,
                "/tenant/create",
                &json!({"slug": "acme", "name": "Not Acme"}),
            )
            .await;
        let (_, members) = fixture
            .route(
                &fixture.bob,
                "/tenant/members/ls",
                &json!({"tenant": "acme"}),
            )
            .await;
        assert_eq!(
            (
                created,
                answer["detail"].clone(),
                members.as_array().map(Vec::len)
            ),
            (409, json!("tenant acme already exists"), Some(2))
        );
    }

    // TC: TEN-04 (v1 defaults: a tenant anyone creates is limited to three
    // projects of one CPU and 1024 MiB until the admin changes it, so
    // creating one does not get around the admin's `max_projects`)
    #[tokio::test(flavor = "multi_thread")]
    async fn limits_tenant_when_user_creates_it() {
        let fixture = fixture().await;
        let (created, tenant) = fixture
            .route(&fixture.dave, "/tenant/create", &json!({"slug": "mine"}))
            .await;
        let id = TenantId::parse(tenant["id"].as_str().unwrap_or_default()).unwrap();
        let quotas = fixture.ports.hooks.tenant_quotas(id).await.unwrap();
        assert_eq!(
            (created, quotas),
            (
                201,
                Some(TenantQuotas {
                    max_projects: Some(3),
                    env_cpus: Some(Cpus::ONE),
                    env_memory_mb: Some(MemoryMb::DEFAULT),
                })
            )
        );
    }
}

mod tenant_ls {
    use super::*;

    // TC: TEN-02 (the admin sees every tenant, with their role where they
    // have one)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_every_tenant_when_admin_lists() {
        let fixture = fixture().await;
        let (listed, tenants) = fixture
            .route(&fixture.world.owner, "/tenant/ls", &json!({}))
            .await;
        let seen: Vec<(Value, Value)> = tenants
            .as_array()
            .map(|tenants| {
                tenants
                    .iter()
                    .map(|tenant| (tenant["slug"].clone(), tenant["role"].clone()))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            (listed, seen),
            (
                200,
                vec![
                    (json!("acme"), Value::Null),
                    (json!("default"), json!("owner")),
                ]
            )
        );
    }

    // TC: TEN-02 (anyone else sees their own tenants only)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_own_tenants_when_member_lists() {
        let fixture = fixture().await;
        let (listed, tenants) = fixture
            .route(&fixture.carol, "/tenant/ls", &json!({}))
            .await;
        let seen: Vec<(Value, Value)> = tenants
            .as_array()
            .map(|tenants| {
                tenants
                    .iter()
                    .map(|tenant| (tenant["slug"].clone(), tenant["role"].clone()))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            (listed, seen),
            (200, vec![(json!("acme"), json!("member"))])
        );
    }
}

mod tenant_rm {
    use super::*;

    // TC: TEN-10 (a tenant with projects is not removed)
    #[tokio::test(flavor = "multi_thread")]
    async fn conflict_when_tenant_has_projects() {
        let fixture = fixture().await;
        let (removed, _) = fixture
            .route(&fixture.bob, "/tenant/rm", &json!({"tenant": "acme"}))
            .await;
        assert_eq!(removed, 409);
    }

    // TC: TEN-10 (an empty one is)
    #[tokio::test(flavor = "multi_thread")]
    async fn removes_tenant_when_it_has_no_projects() {
        let fixture = fixture().await;
        let (created, _) = fixture
            .route(
                &fixture.bob,
                "/tenant/create",
                &json!({"slug": "empty", "name": "Empty"}),
            )
            .await;
        let (removed, _) = fixture
            .route(&fixture.bob, "/tenant/rm", &json!({"tenant": "empty"}))
            .await;
        let (_, listed) = fixture.route(&fixture.bob, "/tenant/ls", &json!({})).await;
        let slugs: Vec<Value> = listed
            .as_array()
            .map(|tenants| tenants.iter().map(|t| t["slug"].clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            (ok(created), ok(removed), slugs),
            (true, true, vec![json!("acme")])
        );
    }
}

mod tenant_secrets {
    use liftoff_core::domain::{ScopeRef, SecretKey, SecretValue};
    use liftoff_core::service::SecretsService;

    use super::*;

    fn secrets(fixture: &Fixture) -> SecretsService<Ports> {
        SecretsService::new(Arc::clone(&fixture.ports))
    }

    async fn set(fixture: &Fixture, actor: &User, scope: &ScopeRef, key: &str, value: &str) {
        secrets(fixture)
            .set(
                actor,
                scope,
                SecretKey::new(key).unwrap(),
                SecretValue::new(value.to_owned()),
                false,
            )
            .await
            .unwrap();
    }

    /// `(key, value)` of the env of `blog/<env>` as `bob` resolves it,
    /// without the system variables.
    async fn env_of(fixture: &Fixture, env: &Environment) -> Vec<(String, String)> {
        secrets(fixture)
            .resolve(&fixture.bob, &fixture.blog.name, &env.name, None)
            .await
            .unwrap()
            .vars()
            .iter()
            .map(|(key, value)| (key.to_string(), value.expose().to_owned()))
            .filter(|(key, _)| !key.starts_with("LIFTOFF_"))
            .collect()
    }

    // TC: TEN-11 (tenant → project → environment)
    #[tokio::test(flavor = "multi_thread")]
    async fn reaches_every_environment_and_yields_to_project_when_member_sets_it() {
        let fixture = fixture().await;
        let of_blog = ScopeRef::Tenant(fixture.blog.name.clone());
        set(&fixture, &fixture.bob, &of_blog, "REGION", "eu").await;
        set(
            &fixture,
            &fixture.carol,
            &of_blog,
            "DATABASE_URL",
            "postgres://acme",
        )
        .await;
        set(
            &fixture,
            &fixture.bob,
            &ScopeRef::Project(fixture.blog.name.clone()),
            "DATABASE_URL",
            "postgres://blog",
        )
        .await;
        let expected = vec![
            ("DATABASE_URL".to_owned(), "postgres://blog".to_owned()),
            ("REGION".to_owned(), "eu".to_owned()),
        ];
        assert_eq!(
            (
                env_of(&fixture, &fixture.blog_prod).await,
                env_of(&fixture, &fixture.blog_qa).await,
            ),
            (expected.clone(), expected)
        );
    }

    // TC: TEN-11 (a foreign tenant → 403)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_non_member_names_the_tenant() {
        let fixture = fixture().await;
        let result = secrets(&fixture)
            .set(
                &fixture.dave,
                &ScopeRef::Tenant(fixture.blog.name.clone()),
                SecretKey::new("REGION").unwrap(),
                SecretValue::new("eu".to_owned()),
                false,
            )
            .await;
        assert!(
            matches!(&result, Err(Error::Forbidden(reason)) if reason.contains("acme")),
            "{result:?}"
        );
    }
}

mod protect {
    use super::*;

    const BLOG_PROD: &str = r#"{"project":"blog","env":"prod"}"#;

    fn body() -> Value {
        serde_json::from_str(BLOG_PROD).unwrap()
    }

    /// The gate of `host` in the configuration last applied.
    fn applied_gate(fixture: &Fixture, host: &str) -> Option<core::net::SocketAddr> {
        let host = Host::new(host).unwrap();
        let config = fixture.world.ports.proxy.applied()?;
        config
            .sites
            .into_iter()
            .find(|site| site.host == host)?
            .gate
    }

    // TC: TEN-06 (`protect` sets the flag; v1 SSO-01: a member may)
    #[tokio::test(flavor = "multi_thread")]
    async fn sets_protected_flag_when_member_protects() {
        let fixture = fixture().await;
        let (status, _) = fixture.route(&fixture.carol, "/protect", &body()).await;
        let stored = fixture.stored(&fixture.blog_prod).await;
        assert_eq!((ok(status), stored.protected), (true, true));
    }

    // TC: TEN-06 (and `ProxyConfig` gets `forward_auth` on `[gate] bind`)
    #[tokio::test(flavor = "multi_thread")]
    async fn applies_forward_auth_when_member_protects() {
        let fixture = fixture().await;
        let _protected = fixture.route(&fixture.carol, "/protect", &body()).await;
        assert_eq!(applied_gate(&fixture, BLOG_HOST), Some(World::GATE_BIND));
    }

    // TC: TEN-06 (v1 SSO-01: someone outside the tenant → 403, no flag)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_and_unprotected_when_outsider_protects() {
        let fixture = fixture().await;
        let (status, _) = fixture.route(&fixture.dave, "/protect", &body()).await;
        let stored = fixture.stored(&fixture.blog_prod).await;
        assert_eq!((status, stored.protected), (403, false));
    }

    // TC: TEN-06 (v1 SSO-01: protecting again changes nothing, Caddy is
    // not asked again)
    #[tokio::test(flavor = "multi_thread")]
    async fn applies_once_when_protected_twice() {
        let fixture = fixture().await;
        let (first, _) = fixture.route(&fixture.carol, "/protect", &body()).await;
        let applied = fixture.world.ports.proxy.apply_count();
        let (second, _) = fixture.route(&fixture.carol, "/protect", &body()).await;
        assert_eq!(
            (
                ok(first),
                ok(second),
                fixture.world.ports.proxy.apply_count()
            ),
            (true, true, applied)
        );
    }

    // TC: TEN-06 (v1 SSO-02: Caddy refuses → the flag stays off and the
    // answer carries Caddy's text)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_flag_off_when_proxy_refuses() {
        let fixture = fixture().await;
        fixture.world.ports.proxy.fail_next(
            "apply",
            Error::upstream(System::Caddy, "caddy: loading config: boom"),
        );
        let (status, answer) = fixture.route(&fixture.carol, "/protect", &body()).await;
        let stored = fixture.stored(&fixture.blog_prod).await;
        assert_eq!(
            (
                status,
                answer["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("boom")),
                stored.protected
            ),
            (502, true, false),
            "{answer}"
        );
    }

    // TC: TEN-09 (`unprotect` clears the flag)
    #[tokio::test(flavor = "multi_thread")]
    async fn clears_protected_flag_when_unprotected() {
        let fixture = fixture().await;
        let (protected, _) = fixture.route(&fixture.carol, "/protect", &body()).await;
        let (unprotected, _) = fixture.route(&fixture.carol, "/unprotect", &body()).await;
        let stored = fixture.stored(&fixture.blog_prod).await;
        assert_eq!(
            (ok(protected), ok(unprotected), stored.protected),
            (true, true, false)
        );
    }

    // TC: TEN-09 (`unprotect` removes `forward_auth`)
    #[tokio::test(flavor = "multi_thread")]
    async fn removes_forward_auth_when_unprotected() {
        let fixture = fixture().await;
        let (protected, _) = fixture.route(&fixture.carol, "/protect", &body()).await;
        let gated = applied_gate(&fixture, BLOG_HOST);
        let (unprotected, _) = fixture.route(&fixture.carol, "/unprotect", &body()).await;
        assert_eq!(
            (
                ok(protected),
                gated,
                ok(unprotected),
                applied_gate(&fixture, BLOG_HOST)
            ),
            (true, Some(World::GATE_BIND), true, None)
        );
    }
}

/// The gate (TEN-07, TEN-08; v1 SSO-04, SSO-06…SSO-12 on the core's
/// sessions): `blog/prod` and `blog/qa` of `acme` are protected.
mod gate {
    use super::*;

    async fn gated() -> Fixture {
        let fixture = fixture().await;
        fixture.protect_in_store(&fixture.blog_prod).await;
        fixture.protect_in_store(&fixture.blog_qa).await;
        fixture
    }

    /// `lof protect --env prod` in `blog`'s directory as `user`: `POST
    /// /api/v1/m/tenants/protect` through the REST router with a session
    /// of `user`; the status.
    async fn lof_protect(fixture: &Fixture, user: &User) -> u16 {
        use tower::ServiceExt as _;
        let token = fixture.world.login(user).await.unwrap();
        let app = liftoff_rest::app_router(
            liftoff_rest::AppState::new(),
            liftoff_rest::Services::new(
                Arc::clone(&fixture.ports),
                Arc::clone(&fixture.world.settings),
            ),
            &liftoff_rest::RestConfig::default(),
        );
        let request = http::Request::builder()
            .method("POST")
            .uri("/api/v1/m/tenants/protect")
            .header("authorization", format!("Bearer {}", token.expose()))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"project": "blog", "env": "prod"}).to_string(),
            ))
            .unwrap();
        app.oneshot(request).await.unwrap().status().as_u16()
    }

    // TC: TEN-06, TEN-07 (`lof protect --env prod`
    // gives an anonymous visitor 302 to the login and a member 200; before
    // it the host is not the gate's, `/gate/start` answers 404)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_anonymous_and_passes_member_when_member_protected_through_api() {
        let fixture = fixture().await;
        let central = fixture.central(&fixture.carol).await;
        let before = fixture
            .start("host=blog.apps.test&rd=%2F", Some(&central))
            .await
            .status;
        let protected = lof_protect(&fixture, &fixture.carol).await;
        let anonymous = fixture.check(BLOG_HOST, "/admin", None).await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let member = fixture.check(BLOG_HOST, "/admin", Some(&cookie)).await;
        assert_eq!(
            (
                before,
                protected,
                anonymous.status,
                anonymous.header("location"),
                member.status,
                member.header("x-liftoff-user"),
            ),
            (
                404,
                200,
                302,
                Some("https://liftoff.test/gate/start?host=blog.apps.test&rd=%2Fadmin"),
                200,
                Some("carol"),
            )
        );
    }

    // TC: TEN-07 (anonymous → 302 to the login, v1 SSO-09: `rd` from
    // `X-Forwarded-Uri`)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_anonymous_to_login_when_no_cookie() {
        let fixture = gated().await;
        let checked = fixture.check(BLOG_HOST, "/admin", None).await;
        assert_eq!(
            (checked.status, checked.header("location")),
            (
                302,
                Some("https://liftoff.test/gate/start?host=blog.apps.test&rd=%2Fadmin")
            )
        );
    }

    // TC: TEN-07 (a member with the host cookie → 200 with
    // `X-Liftoff-User` and `X-Liftoff-Email`)
    #[tokio::test(flavor = "multi_thread")]
    async fn allows_member_with_user_headers_when_cookie_valid() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let checked = fixture.check(BLOG_HOST, "/admin", Some(&cookie)).await;
        assert_eq!(
            (
                checked.status,
                checked.header("x-liftoff-user"),
                checked.header("x-liftoff-email")
            ),
            (200, Some("carol"), Some("carol@example.com"))
        );
    }

    // TC: TEN-07 (not a member → 403 saying who is signed in; v1 SSO-06)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_naming_login_when_non_member_starts() {
        let fixture = gated().await;
        let central = fixture.central(&fixture.dave).await;
        let started = fixture
            .start("host=blog.apps.test&rd=%2F", Some(&central))
            .await;
        assert_eq!(
            (started.status, started.body.contains("dave")),
            (403, true),
            "{started:?}"
        );
    }

    // TC: TEN-07 (v1 SSO-06: the admin passes without a membership)
    #[tokio::test(flavor = "multi_thread")]
    async fn allows_admin_when_not_member() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.world.owner, BLOG_HOST).await;
        let checked = fixture.check(BLOG_HOST, "/", Some(&cookie)).await;
        assert_eq!(
            (checked.status, checked.header("x-liftoff-user")),
            (200, Some("owner"))
        );
    }

    // TC: TEN-08 (v1 SSO-10: removed from the tenant → 403 on the next
    // request, no cache)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_on_next_request_when_member_removed() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let before = fixture.check(BLOG_HOST, "/", Some(&cookie)).await.status;
        let (removed, _) = fixture
            .route(
                &fixture.bob,
                "/tenant/members/rm",
                &json!({"tenant": "acme", "login": "carol"}),
            )
            .await;
        let after = fixture.check(BLOG_HOST, "/", Some(&cookie)).await.status;
        assert_eq!((before, ok(removed), after), (200, true, 403));
    }

    // TC: TEN-07 (v1 SSO-04: `/gate/start` is no open redirect: a host
    // that is not protected → 404)
    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_when_start_host_is_not_protected() {
        let fixture = gated().await;
        let central = fixture.central(&fixture.carol).await;
        let started = fixture
            .start("host=shop.apps.test&rd=%2F", Some(&central))
            .await;
        assert_eq!(started.status, 404);
    }

    // TC: TEN-07 (v1 SSO-04: no browser session → the `oidc` module's
    // browser login, coming back to this start)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_oidc_login_when_start_has_no_session() {
        let fixture = gated().await;
        let started = fixture.start("host=blog.apps.test&rd=%2Fadmin", None).await;
        let location = started.header("location").unwrap_or_default();
        assert_eq!(
            (
                started.status,
                location.starts_with("https://liftoff.test/api/v1/m/oidc/login?rd=%2Fgate%2Fstart")
            ),
            (302, true),
            "{location}"
        );
    }

    // TC: TEN-07 (v1 SSO-07: a browser session → a one-time code to the
    // host's callback, without `rd` in the URL)
    #[tokio::test(flavor = "multi_thread")]
    async fn issues_code_to_host_callback_when_member_starts() {
        let fixture = gated().await;
        let central = fixture.central(&fixture.carol).await;
        let started = fixture
            .start("host=blog.apps.test&rd=%2Fadmin", Some(&central))
            .await;
        let location = started.header("location").unwrap_or_default();
        assert_eq!(
            (
                started.status,
                location.starts_with("https://blog.apps.test/.liftoff/callback?code="),
                location.contains("rd=")
            ),
            (302, true, false),
            "{location}"
        );
    }

    // TC: TEN-07 (v1 SSO-08: the code sets the host cookie and goes on to
    // the code's `rd`)
    #[tokio::test(flavor = "multi_thread")]
    async fn sets_host_cookie_and_redirects_to_rd_when_code_redeemed() {
        let fixture = gated().await;
        let code = fixture.code(&fixture.carol, BLOG_HOST, "%2Fadmin").await;
        let redeemed = fixture
            .liftoff_path(Some(BLOG_HOST), &format!("callback?code={code}"), None)
            .await;
        let cookie = redeemed.header("set-cookie").unwrap_or_default();
        let attributes: Vec<&str> = cookie.split(';').map(str::trim).collect();
        assert_eq!(
            (
                redeemed.status,
                redeemed.header("location"),
                cookie.starts_with("__Host-liftoff_gate="),
                ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"]
                    .iter()
                    .all(|attribute| attributes.contains(attribute))
            ),
            (302, Some("/admin"), true, true),
            "{cookie}"
        );
    }

    // TC: TEN-07 (v1 SSO-04: an `rd` that is not a path becomes `/`)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_root_when_rd_is_not_a_path() {
        let fixture = gated().await;
        let code = fixture
            .code(&fixture.carol, BLOG_HOST, "%2F%2Fevil.example")
            .await;
        let redeemed = fixture
            .liftoff_path(Some(BLOG_HOST), &format!("callback?code={code}"), None)
            .await;
        assert_eq!(redeemed.header("location"), Some("/"));
    }

    // TC: TEN-07 (v1 SSO-08: a code is good once)
    #[tokio::test(flavor = "multi_thread")]
    async fn bad_request_when_code_redeemed_twice() {
        let fixture = gated().await;
        let code = fixture.code(&fixture.carol, BLOG_HOST, "%2F").await;
        let path = format!("callback?code={code}");
        let first = fixture.liftoff_path(Some(BLOG_HOST), &path, None).await;
        let second = fixture.liftoff_path(Some(BLOG_HOST), &path, None).await;
        assert_eq!((first.status, second.status), (302, 400));
    }

    // TC: TEN-07 (v1 SSO-08: the code of another host → 400)
    #[tokio::test(flavor = "multi_thread")]
    async fn bad_request_when_code_of_another_host() {
        let fixture = gated().await;
        let code = fixture.code(&fixture.carol, BLOG_HOST, "%2F").await;
        let redeemed = fixture
            .liftoff_path(Some(NEWS_HOST), &format!("callback?code={code}"), None)
            .await;
        assert_eq!(redeemed.status, 400);
    }

    // TC: TEN-07 (v1 SSO-08: an expired code → 400; one-time links live 10 minutes)
    #[tokio::test(flavor = "multi_thread")]
    async fn bad_request_when_code_expired() {
        let fixture = gated().await;
        let code = fixture.code(&fixture.carol, BLOG_HOST, "%2F").await;
        fixture
            .world
            .ports
            .clock
            .advance(time::Duration::minutes(11));
        let redeemed = fixture
            .liftoff_path(Some(BLOG_HOST), &format!("callback?code={code}"), None)
            .await;
        assert_eq!(redeemed.status, 400);
    }

    // TC: TEN-07 (v1 SSO-08: the gate's paths need `X-Liftoff-Host`)
    #[tokio::test(flavor = "multi_thread")]
    async fn bad_request_when_liftoff_host_missing() {
        let fixture = gated().await;
        let code = fixture.code(&fixture.carol, BLOG_HOST, "%2F").await;
        let redeemed = fixture
            .liftoff_path(None, &format!("callback?code={code}"), None)
            .await;
        assert_eq!(redeemed.status, 400);
    }

    // TC: TEN-07 (v1 SSO-09: the cookie of one host does not open another)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_when_cookie_is_of_another_host() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let checked = fixture.check(NEWS_HOST, "/", Some(&cookie)).await;
        assert_eq!(checked.status, 302);
    }

    // TC: TEN-07 (v1 SSO-09: neither does the browser session of the API
    // host)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_when_cookie_is_the_api_session() {
        let fixture = gated().await;
        let central = fixture.central(&fixture.carol).await;
        let checked = fixture.check(BLOG_HOST, "/", Some(&central)).await;
        assert_eq!(checked.status, 302);
    }

    // TC: TEN-07 (v1 SSO-09: an expired host session → 302, the login
    // again)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_when_host_session_expired() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let before = fixture.check(BLOG_HOST, "/", Some(&cookie)).await.status;
        fixture.world.ports.clock.advance(time::Duration::hours(25));
        let after = fixture.check(BLOG_HOST, "/", Some(&cookie)).await.status;
        assert_eq!((before, after), (200, 302));
    }

    // TC: TEN-07 (v1 SSO-09: the host is `X-Liftoff-Host`, which Caddy
    // sets; an `X-Forwarded-Host` naming the host of the cookie is
    // ignored)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_when_x_forwarded_host_differs() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let checked = fixture
            .check_with(
                NEWS_HOST,
                "/",
                Some(&cookie),
                vec![("x-forwarded-host".to_owned(), BLOG_HOST.to_owned())],
            )
            .await;
        assert_eq!(checked.status, 302);
    }

    // TC: TEN-07 (v1 SSO-04: only a `browser` session starts the
    // handover; a CLI session's secret in the cookie is no session)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_oidc_login_when_start_session_is_not_browser() {
        let fixture = gated().await;
        let (session, secret) = fixture.world.records.session(&fixture.carol);
        assert_ne!(session.kind, SessionKind::Browser);
        fixture
            .world
            .ports
            .store
            .insert_session(&session)
            .await
            .unwrap();
        let cookie = format!("__Host-liftoff_session={}", secret.expose());
        let started = fixture
            .start("host=blog.apps.test&rd=%2F", Some(&cookie))
            .await;
        let location = started.header("location").unwrap_or_default();
        assert_eq!(
            (
                started.status,
                location.starts_with("https://liftoff.test/api/v1/m/oidc/login?")
            ),
            (302, true),
            "{location}"
        );
    }

    // TC: TEN-07 (v1 SSO-04: an expired browser session starts nothing)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_oidc_login_when_start_session_expired() {
        let fixture = gated().await;
        let central = fixture.central(&fixture.carol).await;
        fixture.world.ports.clock.advance(time::Duration::hours(25));
        let started = fixture
            .start("host=blog.apps.test&rd=%2F", Some(&central))
            .await;
        let location = started.header("location").unwrap_or_default();
        assert_eq!(
            (
                started.status,
                location.starts_with("https://liftoff.test/api/v1/m/oidc/login?")
            ),
            (302, true),
            "{location}"
        );
    }

    // TC: TEN-07 (v1 SSO-08: a code whose browser session ended before it
    // was redeemed → 400)
    #[tokio::test(flavor = "multi_thread")]
    async fn bad_request_when_browser_session_ended_before_redeem() {
        let fixture = gated().await;
        let central = fixture
            .central_for(&fixture.carol, time::Duration::minutes(5))
            .await;
        let started = fixture
            .start("host=blog.apps.test&rd=%2F", Some(&central))
            .await;
        let location = started.header("location").unwrap_or_default().to_owned();
        let (_, code) = location
            .split_once("code=")
            .unwrap_or_else(|| panic!("no code in {started:?}"));
        fixture
            .world
            .ports
            .clock
            .advance(time::Duration::minutes(6));
        let redeemed = fixture
            .liftoff_path(Some(BLOG_HOST), &format!("callback?code={code}"), None)
            .await;
        assert_eq!(redeemed.status, 400);
    }

    // TC: TEN-07 (the v1 SSO gate: the gate listener answers only the local
    // Caddy, whose headers it trusts)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_gate_peer_is_not_loopback() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let headers = vec![
            ("x-liftoff-host".to_owned(), BLOG_HOST.to_owned()),
            ("cookie".to_owned(), cookie),
        ];
        let gate = || {
            liftoff_rest::gate_router(GateListener::new(
                Arc::clone(&fixture.ports),
                Arc::clone(&fixture.world.settings),
            ))
        };
        let foreign = "203.0.113.7:51000".parse().unwrap();
        let from_outside = send(gate(), Some(foreign), "/gate/check", headers.clone()).await;
        let unknown_peer = send(gate(), None, "/gate/check", headers).await;
        assert_eq!((from_outside.status, unknown_peer.status), (403, 403));
    }

    // TC: TEN-08 (v1 SSO-13: expired codes and host sessions are deleted)
    #[tokio::test(flavor = "multi_thread")]
    async fn deletes_expired_gate_records_when_pruned() {
        let fixture = gated().await;
        let _cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let _code = fixture.code(&fixture.carol, NEWS_HOST, "%2F").await;
        let gate = GateListener::new(
            Arc::clone(&fixture.ports),
            Arc::clone(&fixture.world.settings),
        );
        let early = gate.prune().await.unwrap();
        fixture.world.ports.clock.advance(time::Duration::hours(25));
        let late = gate.prune().await.unwrap();
        assert_eq!((early, late), ((0, 0), (1, 1)));
    }

    // TC: TEN-08 (v1 SSO-11: logout ends the host session)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_after_logout_when_cookie_used_again() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        let _out = fixture
            .liftoff_path(Some(BLOG_HOST), "logout", Some(&cookie))
            .await;
        let checked = fixture.check(BLOG_HOST, "/", Some(&cookie)).await;
        assert_eq!(checked.status, 302);
    }

    // TC: TEN-08 (v1 SSO-11: logout on one host also ends the browser
    // session and the other host sessions of the same login)
    #[tokio::test(flavor = "multi_thread")]
    async fn ends_browser_and_other_host_sessions_when_logged_out() {
        let fixture = gated().await;
        let central = fixture.central(&fixture.carol).await;
        let blog = fixture.host_cookie_from(&central, BLOG_HOST).await;
        let news = fixture.host_cookie_from(&central, NEWS_HOST).await;
        let before = fixture.check(NEWS_HOST, "/", Some(&news)).await.status;
        let out = fixture
            .liftoff_path(Some(BLOG_HOST), "logout", Some(&blog))
            .await;
        let other_host = fixture.check(NEWS_HOST, "/", Some(&news)).await.status;
        let started = fixture
            .start("host=news.apps.test&rd=%2F", Some(&central))
            .await;
        let location = started.header("location").unwrap_or_default();
        assert_eq!(
            (
                before,
                out.status,
                other_host,
                started.status,
                location.starts_with("https://liftoff.test/api/v1/m/oidc/login?")
            ),
            (200, 200, 302, 302, true),
            "{started:?}"
        );
    }

    // TC: TEN-08 (v1 SSO-12: the admin revoking a login's sessions ends
    // its gate sessions too)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_when_admin_revoked_sessions() {
        let fixture = gated().await;
        let cookie = fixture.host_cookie(&fixture.carol, BLOG_HOST).await;
        AuthService::new(Arc::clone(&fixture.ports))
            .revoke_sessions(&fixture.world.owner, &fixture.carol.login)
            .await
            .unwrap();
        let checked = fixture.check(BLOG_HOST, "/", Some(&cookie)).await;
        assert_eq!(checked.status, 302);
    }
}

/// VIS-05 against the built module: a private
/// tunnel of `blog` (tenant `acme`, owned by `bob`) on its dev address
/// `blog-dev.apps.test`, a managed domain of the project bound to the
/// tunnel as `GET /tunnel` creates it, checked by the core's
/// `Access` with the module's `gate` hook; the cookie is the session of
/// that host.
mod access {
    use super::*;
    use liftoff_core::domain::LinkTarget;
    use liftoff_core::ports::HookRequest;
    use liftoff_core::service::{Access, AccessDecision, AccessRequest};

    const DEV_HOST: &str = "blog-dev.apps.test";

    /// The fixture with the tunnel `blog-dev` of `bob` and its dev
    /// address; the tunnel's target.
    async fn tunnelled() -> (Fixture, LinkTarget) {
        let fixture = fixture().await;
        let tunnel = fixture
            .world
            .add_tunnel(&fixture.blog, "blog-dev", &fixture.bob)
            .await
            .unwrap();
        fixture
            .world
            .add_address(&fixture.blog, &tunnel, DEV_HOST)
            .await
            .unwrap();
        (fixture, LinkTarget::Environment(tunnel.id))
    }

    /// What `Access` decides for `user`'s session on the dev address.
    async fn decided(fixture: &Fixture, user: &User, target: &LinkTarget) -> AccessDecision {
        let cookie = fixture.world.host_login(user, DEV_HOST).await.unwrap();
        let request = AccessRequest {
            host: Host::new(DEV_HOST).unwrap(),
            cookie: Some(cookie),
            session: None,
            bearer: None,
            accept: Some("text/html".to_owned()),
            request: HookRequest {
                method: "GET".to_owned(),
                path: "/".to_owned(),
                headers: Vec::new(),
            },
        };
        Access::new(
            Arc::clone(&fixture.ports),
            Arc::clone(&fixture.world.settings),
        )
        .check(&request, target)
        .await
        .unwrap()
    }

    // TC: VIS-05 (a member of the project's tenant opens another member's
    // dev address)
    #[tokio::test(flavor = "multi_thread")]
    async fn allows_member_of_tenant_when_tunnel_is_private() {
        let (fixture, target) = tunnelled().await;
        let decision = decided(&fixture, &fixture.carol, &target).await;
        assert_eq!(
            decision,
            AccessDecision::Allow {
                user: Some(fixture.carol.clone())
            }
        );
    }

    // TC: VIS-05 (someone in no tenant of the project is refused)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_user_is_in_no_tenant_of_project() {
        let (fixture, target) = tunnelled().await;
        let decision = decided(&fixture, &fixture.dave, &target).await;
        assert!(
            matches!(&decision, AccessDecision::Forbidden(_)),
            "{decision:?}"
        );
    }
}
