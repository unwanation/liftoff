#![cfg(test)]
//! MCP over HTTP end to end (MCPH-01…06): the built `oidc` and `mcp` components
//! (`modules/oidc`, `modules/mcp`) installed by the core's `ModuleService`
//! from the fake registry over the SQLite store in a file and the
//! daemon's module runtime (`DaemonModules` over the wasmtime host, as
//! `tests/registry.rs`), the core's `ModuleHooks` over the modules it
//! loads, the REST router over those ports with `liftoffd::mcp_http`'s
//! transport mounted at `/mcp`, served on a loopback port: the transport's
//! tools call the API through that listener with the `mcp` session of the
//! request's access token. A fake OAuth provider on loopback serves the
//! discovery document and a JWKS with an RSA key of the run, and the test
//! signs its access tokens. It lives with the composition root because
//! it joins three adapters (`liftoff-wasm`, `liftoff-store`,
//! `liftoff-rest`) and `liftoff-mcp`.
//!
//! The requests to `/mcp` are hand-written JSON-RPC (MCP Streamable
//! HTTP, protocol `2025-06-18`) driven in memory against the same router;
//! an answer is read as JSON or as the `data:` of its server-sent
//! events, whichever the transport chose.
//!
//! The components are built by `just modules` (part of `just test` and
//! `just ci`); the test reads them from `WASM_MODULES_DIR`, which the just
//! recipes set, else from `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize, PublicKeyComponents};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use liftoff_core::Error;
use liftoff_core::domain::{Login, ModuleName, SessionKind};
use liftoff_core::fakes::{FakeRelease, WithModules, World};
use liftoff_core::module::ModuleHooks;
use liftoff_core::ports::Store as _;
use liftoff_core::service::{InstallRequest, McpAuthService, ModuleService};
use liftoff_rest::{AppState, RestConfig, Services, app_router};
use liftoff_store::SqliteStore;
use liftoff_wasm::testing::component;
use liftoff_wasm::{Limits, ModuleHost, ModuleSet, TokioTimer, TracingLog};
use liftoffd::mcp_http::{McpMount, mount};
use liftoffd::modules::DaemonModules;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

type Runtime = DaemonModules<SqliteStore, TokioTimer, TracingLog>;
type DaemonHooks = ModuleHooks<ModuleSet<SqliteStore, TracingLog>>;
type Ports = WithModules<SqliteStore, Runtime, DaemonHooks>;

/// `[api] public_url` of the world (`World::API_HOST`).
const PUBLIC_URL: &str = "https://liftoff.test";
/// The `resource` the module `mcp` is installed with.
const RESOURCE: &str = "https://liftoff.test/mcp";
/// The one audience the module `mcp` allows.
const AUDIENCE: &str = "liftoff-mcp";
/// The CLI client of `oidc`.
const CLIENT: &str = "liftoff-cli";
/// The MCP protocol revision the test speaks.
const PROTOCOL: &str = "2025-06-18";

// ---- the fake OAuth provider ------------------------------------------------

/// The fake provider: its URL (the issuer) and its signing key.
struct Provider {
    url: String,
    key: KeyPair,
}

/// What an access token of the provider says.
struct Claims<'a> {
    audience: &'a str,
    login: &'a str,
    expires_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// An hour from now, in Unix seconds.
fn in_an_hour() -> u64 {
    now().saturating_add(3600)
}

/// `header` and `body` as the signing input of a JWS.
fn signing_input(header: &Value, body: &Value) -> String {
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(body.to_string())
    )
}

/// The RS256 JWS of `header` and `body` signed by `key`.
fn signed(key: &KeyPair, header: &Value, body: &Value) -> String {
    let input = signing_input(header, body);
    let mut signature = vec![0_u8; key.public_modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        input.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// The header of the provider's access tokens (RFC 9068).
fn rs256_header() -> Value {
    json!({"alg": "RS256", "typ": "at+jwt", "kid": "k1"})
}

impl Provider {
    /// The claims of an access token of this provider for `claims`,
    /// subject `z1`.
    fn body(&self, claims: &Claims<'_>) -> Value {
        json!({
            "iss": self.url, "aud": claims.audience, "sub": "z1",
            "preferred_username": claims.login, "email": "bob@example.com",
            "email_verified": true, "iat": now(), "exp": claims.expires_at,
        })
    }

    /// An RS256 access token of this provider (RFC 9068 shape) for
    /// `claims`, subject `z1`.
    fn token(&self, claims: &Claims<'_>) -> String {
        self.sign(&self.body(claims))
    }

    /// `body` signed as this provider signs its access tokens.
    fn sign(&self, body: &Value) -> String {
        signed(&self.key, &rs256_header(), body)
    }

    /// A valid token of `bob` for [`AUDIENCE`], expiring at `expires_at`.
    fn bob(&self, expires_at: u64) -> String {
        self.token(&bob_claims(expires_at))
    }

    /// The claims of a valid token of `bob` for an hour.
    fn bob_body(&self) -> Value {
        self.body(&bob_claims(in_an_hour()))
    }
}

/// The claims of a token of `bob` for [`AUDIENCE`], expiring at
/// `expires_at`.
const fn bob_claims(expires_at: u64) -> Claims<'static> {
    Claims {
        audience: AUDIENCE,
        login: "bob",
        expires_at,
    }
}

type Shared = Arc<Provider>;

async fn discovery(State(provider): State<Shared>) -> axum::Json<Value> {
    let url = &provider.url;
    axum::Json(json!({
        "issuer": url,
        "authorization_endpoint": format!("{url}/oauth/v2/authorize"),
        "device_authorization_endpoint": format!("{url}/oauth/v2/device_authorization"),
        "token_endpoint": format!("{url}/oauth/v2/token"),
        "jwks_uri": format!("{url}/oauth/v2/keys"),
    }))
}

async fn keys(State(provider): State<Shared>) -> axum::Json<Value> {
    let public = PublicKeyComponents::<Vec<u8>>::from(provider.key.public_key());
    axum::Json(json!({"keys": [{
        "kty": "RSA", "kid": "k1", "use": "sig", "alg": "RS256",
        "n": URL_SAFE_NO_PAD.encode(&public.n), "e": URL_SAFE_NO_PAD.encode(&public.e),
    }]}))
}

/// The provider on `127.0.0.1`.
async fn provider() -> Shared {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = Arc::new(Provider {
        url: format!("http://{}", listener.local_addr().unwrap()),
        key: KeyPair::generate(KeySize::Rsa2048).unwrap(),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/oauth/v2/keys", get(keys))
        .with_state(Arc::clone(&provider));
    drop(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    provider
}

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

/// The registry's release of the built `module`: its index entry names
/// what the component's own manifest says (`requires`, `config_schema`),
/// as `modules/registry/release.sh` writes it.
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
    provider: Shared,
    /// The router behind the loopback listener.
    app: Router,
    /// The API as the listener serves it (`http://127.0.0.1:<port>`).
    api_url: String,
    /// `owner`'s `cli` session in the SQLite store.
    owner_token: String,
    /// The daemon's stop: the transport's event streams end with it.
    stop: CancellationToken,
}

impl Daemon {
    fn modules(&self) -> ModuleService<Ports> {
        ModuleService::new(Arc::clone(&self.ports), Arc::clone(&self.world.settings))
    }

    /// Installs `module` as `owner` with its test configuration: `oidc`
    /// against the fake provider, `mcp` for [`RESOURCE`] and
    /// [`AUDIENCE`].
    async fn install(&self, module: &str) -> Result<(), Error> {
        let config = match module {
            "oidc" => config(json!({
                "issuer": self.provider.url, "client_id": CLIENT, "admin_role": "admin",
            })),
            "mcp" => config(json!({"resource": RESOURCE, "allowed_audiences": [AUDIENCE]})),
            other => panic!("no test configuration of {other}"),
        };
        self.modules()
            .install(
                &self.world.owner,
                InstallRequest {
                    name: name(module),
                    version: None,
                    config,
                },
            )
            .await
            .map(drop)
    }

    async fn remove(&self, module: &str) {
        self.modules()
            .remove(&self.world.owner, &name(module), false)
            .await
            .unwrap();
    }

    /// The `mcp` sessions of the user `login` in the store.
    async fn mcp_sessions(&self, login: &str) -> Vec<liftoff_core::domain::Session> {
        let user = self
            .ports
            .store
            .user_by_login(&Login::new(login).unwrap())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no user {login}"));
        self.ports
            .store
            .sessions_of_user(user.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|session| session.kind == SessionKind::Mcp)
            .collect()
    }
}

/// A daemon over a fresh data directory with the fake registry serving
/// the built `oidc` and `mcp`, the modules of `installed` installed in
/// that order, served on a loopback port.
async fn daemon_with(installed: &[&str]) -> Daemon {
    let world = World::new().await;
    let data = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(data.path().join("liftoff.db")).unwrap();
    store.insert_user(&world.owner).await.unwrap();
    let (session, secret) = world.records.session(&world.owner);
    store.insert_session(&session).await.unwrap();
    let limits = Limits {
        max_memory_bytes: 64 * 1024 * 1024,
        call_timeout: Duration::from_secs(10),
    };
    let host = ModuleHost::new(store.clone(), TokioTimer, TracingLog, limits)
        .unwrap()
        .allowing_private_networks();
    let runtime = DaemonModules::new(host, data.path().join("modules"));
    for module in ["oidc", "mcp"] {
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let services = Services::new(Arc::clone(&ports), Arc::clone(&world.settings));
    let stop = CancellationToken::new();
    let app = mount(
        app_router(AppState::new(), services, &RestConfig::default()),
        McpAuthService::new(Arc::clone(&ports)).allowing_loopback_issuers(),
        &McpMount {
            api_url: api_url.clone(),
            public_url: PUBLIC_URL.to_owned(),
        },
        &stop,
    );
    let served = app.clone();
    drop(tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    }));
    let daemon = Daemon {
        _data: data,
        world,
        ports,
        provider: provider().await,
        app,
        api_url,
        owner_token: secret.expose().to_owned(),
        stop,
    };
    for module in installed {
        daemon
            .install(module)
            .await
            .unwrap_or_else(|error| panic!("install {module}: {error:?}"));
    }
    daemon
}

/// Both modules installed.
async fn daemon() -> Daemon {
    daemon_with(&["oidc", "mcp"]).await
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

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    /// The body as JSON (`null` when it is none).
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The JSON-RPC message of the answer: the body as JSON, or, for a
    /// stream of server-sent events, the last `data:` that is a response
    /// (it has an `id`).
    fn message(&self) -> Value {
        let event_stream = self
            .header("content-type")
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
        if !event_stream {
            return self.json();
        }
        self.text()
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .rfind(|message| message.get("id").is_some())
            .unwrap_or(Value::Null)
    }
}

/// `method path` on the API host with a bearer, the `Mcp-Session-Id` of
/// `session` and a JSON `body`, as an MCP client sends it.
async fn send(
    app: &Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    session: Option<&str>,
    body: Option<&Value>,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, World::API_HOST)
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(id) = session {
        request = request
            .header("mcp-session-id", id)
            .header("mcp-protocol-version", PROTOCOL);
    }
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
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: body.to_vec(),
    }
}

/// The JSON-RPC `initialize` request.
fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "mcph-test", "version": "0.1.0"},
        },
    })
}

/// `POST /mcp` with `initialize` and `bearer`.
async fn post_initialize(daemon: &Daemon, bearer: Option<&str>) -> Answer {
    send(
        &daemon.app,
        "POST",
        "/mcp",
        bearer,
        None,
        Some(&initialize()),
    )
    .await
}

/// An MCP client of `/mcp` with one access token, past `initialize`.
struct McpClient<'a> {
    app: &'a Router,
    bearer: String,
    session: Option<String>,
}

impl<'a> McpClient<'a> {
    /// Initializes with `bearer` (and sends `notifications/initialized`).
    async fn connect(daemon: &'a Daemon, bearer: String) -> Self {
        let answer = post_initialize(daemon, Some(&bearer)).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        let client = Self {
            app: &daemon.app,
            session: answer.header("mcp-session-id"),
            bearer,
        };
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let sent = client.post(&initialized).await;
        assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.text());
        client
    }

    async fn post(&self, message: &Value) -> Answer {
        send(
            self.app,
            "POST",
            "/mcp",
            Some(&self.bearer),
            self.session.as_deref(),
            Some(message),
        )
        .await
    }

    /// The `result` of the request `method` with `params`.
    async fn request(&self, id: u64, method: &str, params: Value) -> Value {
        let answer = self
            .post(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        let message = answer.message();
        message
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("{method}: no result in {message}"))
    }
}

/// The problem `detail` of an answer.
fn detail(answer: &Answer) -> String {
    answer.json()["detail"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Whether `answer` is the `401` of a refused bearer (RFC 6750 3.1:
/// `error="invalid_token"`, with the resource metadata).
fn invalid_token(answer: &Answer) -> bool {
    answer.status == StatusCode::UNAUTHORIZED
        && answer.header("www-authenticate").is_some_and(|value| {
            value.starts_with("Bearer ")
                && value.contains("resource_metadata=")
                && value.contains("error=\"invalid_token\"")
        })
}

/// `initialize` with `token`: whether it was refused as an invalid token
/// and no user `bob` was created.
async fn refused_creating_nothing(daemon: &Daemon, token: &str) -> (bool, bool) {
    let answer = post_initialize(daemon, Some(token)).await;
    let bob = daemon
        .ports
        .store
        .user_by_login(&Login::new("bob").unwrap())
        .await
        .unwrap();
    (invalid_token(&answer), bob.is_none())
}

/// `GET /api/v1/me` with the bearer `token`: the status.
async fn rest_me(daemon: &Daemon, token: &str) -> StatusCode {
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/me")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    daemon.app.clone().oneshot(request).await.unwrap().status()
}

// ---- MCPH-01, MCPH-03: refused requests ------------------------------------

mod unauthenticated {
    use super::*;

    // TC: MCPH-01 (no token: 401 with the resource metadata, RFC 9728 5.1)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_401_with_resource_metadata_when_no_bearer() {
        let daemon = daemon().await;
        let answer = post_initialize(&daemon, None).await;
        assert_eq!(
            (answer.status, answer.header("www-authenticate")),
            (
                StatusCode::UNAUTHORIZED,
                Some(
                    "Bearer resource_metadata=\"https://liftoff.test/.well-known/oauth-protected-resource\""
                        .to_owned()
                )
            ),
            "{}",
            answer.text()
        );
    }

    // TC: MCPH-01 (the event stream of `GET /mcp` is refused the same way)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_401_on_event_stream_when_no_bearer() {
        let daemon = daemon().await;
        let answer = send(&daemon.app, "GET", "/mcp", None, None, None).await;
        assert_eq!(
            (
                answer.status,
                answer
                    .header("www-authenticate")
                    .is_some_and(|value| value.contains("resource_metadata=")),
            ),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            answer.text()
        );
    }

    // TC: MCPH-03 (a token for another audience is refused)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_401_when_token_has_foreign_audience() {
        let daemon = daemon().await;
        let token = daemon.provider.token(&Claims {
            audience: "other-app",
            login: "bob",
            expires_at: in_an_hour(),
        });
        let answer = post_initialize(&daemon, Some(&token)).await;
        assert_eq!(
            (
                answer.status,
                answer
                    .header("www-authenticate")
                    .is_some_and(|value| value.starts_with("Bearer ")
                        && value.contains("resource_metadata=")),
            ),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            answer.text()
        );
    }

    // TC: MCPH-03 (a refused token opens no session and creates no user;
    // the challenge shows the verifier saw it)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_no_user_when_token_has_foreign_audience() {
        let daemon = daemon().await;
        let token = daemon.provider.token(&Claims {
            audience: "other-app",
            login: "bob",
            expires_at: in_an_hour(),
        });
        assert_eq!(
            refused_creating_nothing(&daemon, &token).await,
            (true, true)
        );
    }

    // TC: MCPH-03 (an expired token: `oidc` checks `exp` by the wall
    // clock, past its minute of leeway)
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_creating_nothing_when_token_expired() {
        let daemon = daemon().await;
        let token = daemon.provider.bob(now().saturating_sub(600));
        assert_eq!(
            refused_creating_nothing(&daemon, &token).await,
            (true, true)
        );
    }

    // TC: MCPH-03 (signed by a key the JWKS does not hold, under its `kid`)
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_creating_nothing_when_signature_is_foreign_key() {
        let daemon = daemon().await;
        let stranger = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let token = signed(&stranger, &rs256_header(), &daemon.provider.bob_body());
        assert_eq!(
            refused_creating_nothing(&daemon, &token).await,
            (true, true)
        );
    }

    // TC: MCPH-03 (`iss` is not the issuer configured for `oidc`)
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_creating_nothing_when_issuer_is_not_oidcs() {
        let daemon = daemon().await;
        let mut body = daemon.provider.bob_body();
        body["iss"] = json!("https://evil.example");
        let token = daemon.provider.sign(&body);
        assert_eq!(
            refused_creating_nothing(&daemon, &token).await,
            (true, true)
        );
    }

    // TC: MCPH-03 (an unsigned token, `alg: none`)
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_creating_nothing_when_alg_is_none() {
        let daemon = daemon().await;
        let token = format!(
            "{}.",
            signing_input(
                &json!({"alg": "none", "typ": "at+jwt"}),
                &daemon.provider.bob_body()
            )
        );
        assert_eq!(
            refused_creating_nothing(&daemon, &token).await,
            (true, true)
        );
    }

    // TC: MCPH-03 (`alg: HS256` keyed with the public key: algorithm
    // confusion)
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_creating_nothing_when_alg_is_hs256() {
        let daemon = daemon().await;
        let input = signing_input(
            &json!({"alg": "HS256", "typ": "at+jwt", "kid": "k1"}),
            &daemon.provider.bob_body(),
        );
        let token = format!("{input}.{}", URL_SAFE_NO_PAD.encode([7_u8; 32]));
        assert_eq!(
            refused_creating_nothing(&daemon, &token).await,
            (true, true)
        );
    }
}

// ---- MCPH-02: the protected-resource metadata ------------------------------

mod protected_resource {
    use super::*;

    // TC: MCPH-02 (the metadata names the module's resource and the issuer
    // of `oidc` as the authorization server)
    #[tokio::test(flavor = "multi_thread")]
    async fn names_oidc_issuer_as_authorization_server_when_mcp_installed() {
        let daemon = daemon().await;
        let answer = send(
            &daemon.app,
            "GET",
            "/.well-known/oauth-protected-resource",
            None,
            None,
            None,
        )
        .await;
        let body = answer.json();
        assert_eq!(
            (
                answer.status,
                body["resource"].clone(),
                body["authorization_servers"].clone(),
                body["scopes_supported"].clone()
            ),
            (
                StatusCode::OK,
                json!(RESOURCE),
                json!([daemon.provider.url]),
                json!([
                    "openid",
                    "profile",
                    "email",
                    "urn:zitadel:iam:org:project:roles",
                    "offline_access"
                ])
            ),
            "{}",
            answer.text()
        );
    }

    // TC: MCPH-02 (the URL of the 401 serves the metadata: it is open and
    // JSON)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_json_without_session_when_mcp_installed() {
        let daemon = daemon().await;
        let answer = send(
            &daemon.app,
            "GET",
            "/.well-known/oauth-protected-resource",
            None,
            None,
            None,
        )
        .await;
        assert_eq!(
            (
                answer.status,
                answer
                    .header("content-type")
                    .is_some_and(|kind| kind.starts_with("application/json")),
            ),
            (StatusCode::OK, true),
            "{}",
            answer.text()
        );
    }
}

// ---- MCPH-04: a valid token --------------------------------------------------

mod authenticated {
    use super::*;

    // TC: MCPH-04 (a valid token opens one `mcp` session of its user, which
    // ends with the token, reused for the requests that carry it)
    #[tokio::test(flavor = "multi_thread")]
    async fn opens_one_mcp_session_until_token_expiry_when_token_valid() {
        let daemon = daemon().await;
        let expires_at = in_an_hour();
        let client = McpClient::connect(&daemon, daemon.provider.bob(expires_at)).await;
        let _tools = client.request(2, "tools/list", json!({})).await;
        let sessions = daemon.mcp_sessions("bob").await;
        assert_eq!(
            sessions
                .iter()
                .map(|session| session.expires_at.unix_timestamp())
                .collect::<Vec<_>>(),
            vec![i64::try_from(expires_at).unwrap()],
            "{sessions:?}"
        );
    }

    // TC: MCPH-04 (the tools of stdio, the aliases among them)
    #[tokio::test(flavor = "multi_thread")]
    async fn lists_liftoff_tools_when_token_valid() {
        let daemon = daemon().await;
        let client = McpClient::connect(&daemon, daemon.provider.bob(in_an_hour())).await;
        let result = client.request(2, "tools/list", json!({})).await;
        let names: Vec<&str> = result["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["name"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            (
                names.contains(&"liftoff_me_get"),
                names.contains(&"liftoff_projects_list"),
                names.contains(&"publish_artifact"),
            ),
            (true, true, true),
            "{names:?}"
        );
    }

    // TC: MCPH-04 (the role from the roles claim, as the device flow
    // reads it)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_admin_when_token_has_admin_role() {
        let daemon = daemon().await;
        let mut body = daemon.provider.bob_body();
        body["urn:zitadel:iam:org:project:roles"] = json!({"admin": {"o1": "acme"}});
        let client = McpClient::connect(&daemon, daemon.provider.sign(&body)).await;
        let _tools = client.request(2, "tools/list", json!({})).await;
        let bob = daemon
            .ports
            .store
            .user_by_login(&Login::new("bob").unwrap())
            .await
            .unwrap()
            .map(|user| user.role);
        assert_eq!(bob, Some(liftoff_core::domain::UserRole::Admin));
    }

    // TC: MCPH-04 (the `mcp` session is the bearer the tools call the API
    // with)
    #[tokio::test(flavor = "multi_thread")]
    async fn rest_answers_mcp_session_when_mcp_installed() {
        let daemon = daemon().await;
        let grant = McpAuthService::new(Arc::clone(&daemon.ports))
            .allowing_loopback_issuers()
            .verify_oauth(Some(&daemon.provider.bob(in_an_hour())))
            .await
            .unwrap();
        assert_eq!(rest_me(&daemon, grant.token.expose()).await, StatusCode::OK);
    }

    // TC: MCPH-04 (the access token itself is no bearer of the REST API)
    #[tokio::test(flavor = "multi_thread")]
    async fn rest_refuses_access_token_as_bearer_when_mcp_installed() {
        let daemon = daemon().await;
        let token = daemon.provider.bob(in_an_hour());
        let _client = McpClient::connect(&daemon, token.clone()).await;
        assert_eq!(rest_me(&daemon, &token).await, StatusCode::UNAUTHORIZED);
    }

    // The daemon stopping ends the open event stream of `GET /mcp`, so
    // its graceful shutdown does not wait on it.
    #[tokio::test(flavor = "multi_thread")]
    async fn ends_event_stream_when_daemon_stops() {
        let daemon = daemon().await;
        let token = daemon.provider.bob(in_an_hour());
        let client = McpClient::connect(&daemon, token.clone()).await;
        let request = Request::builder()
            .method("GET")
            .uri("/mcp")
            .header(header::HOST, World::API_HOST)
            .header(header::ACCEPT, "text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("mcp-session-id", client.session.clone().unwrap())
            .header("mcp-protocol-version", PROTOCOL)
            .body(Body::empty())
            .unwrap();
        let response = daemon.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        daemon.stop.cancel();
        let ended = tokio::time::timeout(Duration::from_secs(10), response.into_body().collect())
            .await
            .is_ok();
        assert_eq!((status, ended), (StatusCode::OK, true));
    }

    // TC: MCPH-04 (the tools answer as the user of the token)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_me_as_token_user_when_tool_called() {
        let daemon = daemon().await;
        let client = McpClient::connect(&daemon, daemon.provider.bob(in_an_hour())).await;
        let result = client
            .request(
                2,
                "tools/call",
                json!({"name": "liftoff_me_get", "arguments": {}}),
            )
            .await;
        let me: Value = result["content"][0]["text"]
            .as_str()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        assert_eq!(
            (
                result["isError"].as_bool().unwrap_or(false),
                me["login"].clone(),
                me["source"].clone(),
            ),
            (
                false,
                json!("bob"),
                json!(format!("oidc:{}", daemon.provider.url))
            ),
            "{result}"
        );
    }
}

// ---- MCPH-05: `mcp` requires `oidc` ------------------------------------------

mod install {
    use super::*;

    // TC: MCPH-05 (installing `mcp` without `oidc` is refused:
    // 424 "install the module `oidc`")
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_mcp_naming_oidc_when_oidc_not_installed() {
        let daemon = daemon_with(&[]).await;
        let refused = daemon.install("mcp").await;
        assert!(
            matches!(&refused, Err(Error::ModuleRequired(module)) if module == "oidc"),
            "{refused:?}"
        );
    }

    // TC: MCPH-05 (through `POST /api/v1/modules`, as `lof module install
    // mcp` sends it)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_424_naming_oidc_when_mcp_installed_over_rest_without_oidc() {
        let daemon = daemon_with(&[]).await;
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/modules")
            .header(header::AUTHORIZATION, format!("Bearer {}", daemon.owner_token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"name": "mcp", "config": {"resource": RESOURCE, "allowed_audiences": [AUDIENCE]}})
                    .to_string(),
            ))
            .unwrap();
        let response = daemon.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap_or(Value::Null);
        assert_eq!(
            (status, body["detail"].clone()),
            (
                StatusCode::FAILED_DEPENDENCY,
                json!("install the module `oidc`")
            ),
            "{body}"
        );
    }

    // TC: MCPH-05 (after the refused install `/mcp` still needs the module)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_mcp_route_424_when_install_refused() {
        let daemon = daemon_with(&[]).await;
        let _refused = daemon.install("mcp").await;
        let answer = post_initialize(&daemon, None).await;
        assert_eq!(
            (answer.status, detail(&answer)),
            (
                StatusCode::FAILED_DEPENDENCY,
                "install the module `mcp`".to_owned()
            ),
            "{}",
            answer.text()
        );
    }
}

// ---- MCPH-06: removing `mcp` -------------------------------------------------

mod removal {
    use super::*;

    // TC: MCPH-06 (`/mcp` answers 424 once the module is removed, even with
    // a valid token)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_424_on_mcp_when_module_removed() {
        let daemon = daemon().await;
        daemon.remove("mcp").await;
        let token = daemon.provider.bob(in_an_hour());
        let answer = post_initialize(&daemon, Some(&token)).await;
        assert_eq!(
            (answer.status, detail(&answer)),
            (
                StatusCode::FAILED_DEPENDENCY,
                "install the module `mcp`".to_owned()
            ),
            "{}",
            answer.text()
        );
    }

    // TC: MCPH-06 (the protected-resource metadata goes with the module)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_424_on_protected_resource_when_module_removed() {
        let daemon = daemon().await;
        daemon.remove("mcp").await;
        let answer = send(
            &daemon.app,
            "GET",
            "/.well-known/oauth-protected-resource",
            None,
            None,
            None,
        )
        .await;
        assert_eq!(
            (answer.status, detail(&answer)),
            (
                StatusCode::FAILED_DEPENDENCY,
                "install the module `mcp`".to_owned()
            ),
            "{}",
            answer.text()
        );
    }

    // TC: MCPH-06 (a transport session opened before the removal ends with
    // it: its next request is 424 too)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_424_in_open_session_when_module_removed() {
        let daemon = daemon().await;
        let client = McpClient::connect(&daemon, daemon.provider.bob(in_an_hour())).await;
        daemon.remove("mcp").await;
        let answer = client
            .post(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}))
            .await;
        assert_eq!(
            answer.status,
            StatusCode::FAILED_DEPENDENCY,
            "{}",
            answer.text()
        );
    }

    // TC: MCPH-06 (the `mcp` sessions go with the module at the REST API
    // too: the transport's bearer is refused there)
    #[tokio::test(flavor = "multi_thread")]
    async fn rest_refuses_mcp_session_when_module_removed() {
        let daemon = daemon().await;
        let grant = McpAuthService::new(Arc::clone(&daemon.ports))
            .allowing_loopback_issuers()
            .verify_oauth(Some(&daemon.provider.bob(in_an_hour())))
            .await
            .unwrap();
        daemon.remove("mcp").await;
        assert_eq!(
            rest_me(&daemon, grant.token.expose()).await,
            StatusCode::UNAUTHORIZED
        );
    }

    // TC: MCPH-06 (`lof mcp` over stdio keeps working: its tools call the
    // API with the `cli` session of the CLI config)
    #[tokio::test(flavor = "multi_thread")]
    async fn stdio_server_answers_as_owner_when_mcp_removed() {
        let daemon = daemon().await;
        daemon.remove("mcp").await;
        let (client_end, server_end) = tokio::io::duplex(1 << 16);
        let (server_in, server_out) = tokio::io::split(server_end);
        let session = liftoff_mcp::Session {
            url: daemon.api_url.clone(),
            token: daemon.owner_token.clone(),
        };
        drop(tokio::spawn(liftoff_mcp::serve(
            session, server_in, server_out,
        )));
        let (client_in, client_out) = tokio::io::split(client_end);
        let client = ().serve((client_in, client_out)).await.unwrap();
        let result = client
            .call_tool(CallToolRequestParams::new("liftoff_me_get".to_owned()))
            .await
            .unwrap();
        let result = serde_json::to_value(result).unwrap();
        let me: Value = result["content"][0]["text"]
            .as_str()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        assert_eq!(
            (
                result["isError"].as_bool().unwrap_or(false),
                me["login"].clone()
            ),
            (false, json!("owner")),
            "{result}"
        );
    }
}
