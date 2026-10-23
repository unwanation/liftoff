#![cfg(test)]
//! The `oidc` module as a component (OIDC-01…07, the v1 catalogue's AUTH-01…07 carried over to the module).
//!
//! The built `modules/oidc` component runs in a module host over the
//! world's store with the core's `CoreBridge` behind `core-call`, so the
//! users and sessions the module asks the core for land in the world's
//! `Store` and are read back through it. Its `wasi:http` reaches a fake
//! `OpenID Connect` provider served by this test on loopback (the host
//! allows private networks for it): discovery, the device authorization
//! and token endpoints, and a JWKS of RSA keys generated per run that sign
//! the RS256 ID tokens it hands out.
//!
//! The component is built by `just modules` (part of `just test` and
//! `just ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.
//!
//! The module's wire contract these tests pin (the CLI's side is
//! `crates/liftoff/tests/cli/login.rs`, `device_flow`):
//!
//! | Route (below `/api/v1/m/oidc`) | Answer |
//! |---|---|
//! | `POST /device` (public) | `200` `{"device_code","user_code","verification_uri_complete","interval","expires_in"}` |
//! | `POST /device/poll` (public) `{"device_code","name"}` | `202` `{"status":"pending"}`; `200` `{"token","user"}` (a `cli` session of the core); `403` denied or invalid ID token; `410` expired; `502` provider failure |
//! | `GET /login` (public) | `302` to the provider's authorization endpoint (code flow with PKCE) |
//! | `GET /callback` (public) `?code&state` | `200` `{"token","user"}` (a `browser` session); `400` for a `state` it did not issue or already used; `403` an invalid ID token |

use core::sync::atomic::{AtomicUsize, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize, PublicKeyComponents};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use axum::extract::{Form, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use liftoff_core::domain::{
    Bearer, ConfigValue, InstalledModule, ModuleConfigRecord, ModuleName, ModuleVersion, Session,
    SessionKind, SessionSecret, User, UserRole, UserSource,
};
use liftoff_core::fakes::{FakePorts, InMemoryStore, World};
use liftoff_core::module::{CoreBridge, HookName, ModuleCalls as _, ModuleHooks, RouteSpec};
use liftoff_core::ports::{
    AuthResult, Clock as _, HookRequest, Hooks as _, ModulePage, ModuleRequest, ModuleResponse,
    ModuleStore as _, PageKind, PageRequest, Store as _,
};
use liftoff_wasm::testing::{Host, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TokioTimer, TracingLog};
use num_bigint::BigUint;
use serde_json::{Value, json};
use time::Duration;

/// The client the module is configured with.
const CLIENT: &str = "liftoff-cli";
/// Zitadel's claim of project roles.
const ROLES_CLAIM: &str = "urn:zitadel:iam:org:project:roles";
/// The `scopes` configuration value: a JSON array, as `config-get`
/// answers a non-string value.
const SCOPES: &str = r#"["openid","profile","email","urn:zitadel:iam:org:project:roles"]"#;
/// The scopes the device authorization request carries.
const SCOPE: &str = "openid profile email urn:zitadel:iam:org:project:roles";
const DISCOVERY: &str = "/.well-known/openid-configuration";
const AUTHORIZE: &str = "/oauth/v2/authorize";
const DEVICE: &str = "/oauth/v2/device_authorization";
const TOKEN: &str = "/oauth/v2/token";
const KEYS: &str = "/oauth/v2/keys";
const DEVICE_CODE: &str = "dc-1";
const USER_CODE: &str = "ABCD-EFGH";
const VERIFICATION: &str = "https://id.example/device?user_code=ABCD-EFGH";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// How long the module trusts the JWKS it keeps in its store.
const JWKS_TTL_SECS: u64 = 60 * 60;
/// How often a `kid` the kept JWKS does not hold reads it again
/// (`modules/oidc/src/provider.rs`, `JWKS_REFRESH_SECS`).
const JWKS_REFRESH_SECS: u64 = 60;

type Oidc = ModuleHooks<ModuleSet<InMemoryStore, TracingLog, CoreBridge<FakePorts>>>;

fn oidc() -> ModuleName {
    ModuleName::new("oidc").unwrap()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// --- The fake provider ------------------------------------------------------

/// One scripted answer of the provider.
#[derive(Debug, Clone)]
struct Reply {
    status: u16,
    body: Value,
}

fn ok(body: Value) -> Reply {
    Reply { status: 200, body }
}

/// An OAuth error answer (RFC 6749 5.2).
fn oauth_error(error: &str) -> Reply {
    Reply {
        status: 400,
        body: json!({"error": error, "error_description": "described"}),
    }
}

/// The token endpoint's answer carrying `id_token`.
fn token_reply(id_token: &str) -> Reply {
    ok(json!({"access_token": "at", "token_type": "Bearer", "id_token": id_token}))
}

/// The device authorization answer of the catalogue.
fn device_ok() -> Reply {
    ok(json!({
        "device_code": DEVICE_CODE,
        "user_code": USER_CODE,
        "verification_uri": "https://id.example/device",
        "verification_uri_complete": VERIFICATION,
        "expires_in": 300,
        "interval": 7
    }))
}

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        (
            StatusCode::from_u16(self.status).unwrap(),
            [(header::CONTENT_TYPE, "application/json")],
            self.body.to_string(),
        )
            .into_response()
    }
}

/// An RSA signing key of the provider, generated for the run; nothing
/// is committed.
struct Signer {
    kid: String,
    pair: KeyPair,
    n: String,
    e: String,
}

impl Signer {
    fn generate(kid: &str) -> Self {
        let pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let public = PublicKeyComponents::<Vec<u8>>::from(pair.public_key());
        Self {
            kid: kid.to_owned(),
            n: URL_SAFE_NO_PAD.encode(&public.n),
            e: URL_SAFE_NO_PAD.encode(&public.e),
            pair,
        }
    }

    /// Its entry in a JWKS.
    fn jwk(&self) -> Value {
        json!({"kty": "RSA", "kid": self.kid, "use": "sig", "alg": "RS256", "n": self.n, "e": self.e})
    }

    /// An RS256 JWT of `claims` signed by this key, naming its `kid`.
    fn mint(&self, claims: &Value) -> String {
        let header = json!({"alg": "RS256", "typ": "JWT", "kid": self.kid});
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0_u8; self.pair.public_modulus_len()];
        self.pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }
}

/// A fixed 1024-bit RSA key (`kid` `k-weak`, `e = 65537`), too short for
/// the module. `aws-lc-rs` signs with 2048 bits and more only, so it signs
/// RSASSA-PKCS1-v1_5 with SHA-256 by hand (RFC 8017 8.2.1); the key is a
/// test fixture of no value.
struct WeakSigner;

impl WeakSigner {
    const KID: &str = "k-weak";
    const N: &str = "a3a567d119c6c7dad41ca9bcff1cb61229226e4094a1f8adbeb340ccfa31214bfede7356fe40c9241ae21c22d12360326deef41d4379d5c8d303bef80332c601bb8a2c0aa4b7bf6aa665c4738605073d2491700958de35c81440cd3b34a05660270d2b22a3fdd24f14257668596730093bfa446990f16f5b398e96e9a5f2872b";
    const D: &str = "85f81f699b8494909d1d6a080381e9bc2c58b791bf1c6a8d329564f369cf56c6f43c0088b3552e55947c8f3421aaa852209582db8225f7d2948d78856978e387b05df8207eb5d374b598ae67aa550cf829596f63c127715cf74827ec78065c1a9ed5ae22dfa410e4d15febf38f5d7828aa1879fe9c1706b911f2b62f6d76efc1";
    /// The DER prefix of a SHA-256 `DigestInfo` (RFC 8017 9.2).
    const DIGEST_INFO: [u8; 19] = [
        0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
        0x05, 0x00, 0x04, 0x20,
    ];

    fn n() -> BigUint {
        BigUint::parse_bytes(Self::N.as_bytes(), 16).unwrap()
    }

    fn jwk() -> Value {
        json!({
            "kty": "RSA", "kid": Self::KID, "use": "sig", "alg": "RS256",
            "n": URL_SAFE_NO_PAD.encode(Self::n().to_bytes_be()), "e": "AQAB",
        })
    }

    /// An RS256 JWT of `claims` under this key; the signature is checked
    /// against the public key here, so a refusal is the key's size.
    fn mint(claims: &Value) -> String {
        let header = json!({"alg": "RS256", "typ": "JWT", "kid": Self::KID});
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let n = Self::n();
        let k = usize::try_from(n.bits().div_ceil(8)).unwrap();
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, input.as_bytes());
        let mut encoded = vec![0x00, 0x01];
        // `00 01 FF…FF 00 DigestInfo digest`: the `FF`s fill all but the
        // 19 + 32 bytes of `DigestInfo` and digest and the `00` before.
        encoded.resize(k.checked_sub(52).unwrap(), 0xff);
        encoded.push(0x00);
        encoded.extend_from_slice(&Self::DIGEST_INFO);
        encoded.extend_from_slice(digest.as_ref());
        let message = BigUint::from_bytes_be(&encoded);
        let d = BigUint::parse_bytes(Self::D.as_bytes(), 16).unwrap();
        let signed = message.modpow(&d, &n);
        assert_eq!(signed.modpow(&BigUint::from(65_537_u32), &n), message);
        let mut signature = signed.to_bytes_be();
        while signature.len() < k {
            signature.insert(0, 0);
        }
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }
}

/// The key `k1` every provider publishes first.
fn signer() -> &'static Signer {
    static KEY: OnceLock<Signer> = OnceLock::new();
    KEY.get_or_init(|| Signer::generate("k1"))
}

/// A request the provider received: method, path and form fields.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    form: Vec<(String, String)>,
}

impl Seen {
    fn field(&self, name: &str) -> Option<&str> {
        self.form
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug)]
struct ProviderState {
    url: String,
    device: Mutex<Reply>,
    /// Token answers in order; the last one repeats.
    token: Mutex<VecDeque<Reply>>,
    /// An answer of the JWKS endpoint in place of the published keys.
    keys_fail: Mutex<Option<Reply>>,
    jwks: Mutex<Vec<Value>>,
    keys_served: AtomicUsize,
    seen: Mutex<Vec<Seen>>,
    /// The `issuer` the discovery document names in place of the URL.
    discovery_issuer: Mutex<Option<String>>,
}

/// A fake `OpenID Connect` provider on `127.0.0.1`, Zitadel's endpoint
/// layout, issuer = its URL. The device endpoint answers [`device_ok`],
/// the token endpoint `authorization_pending` until scripted, the JWKS
/// holds [`signer`].
#[derive(Debug, Clone)]
struct Provider(Arc<ProviderState>);

impl Provider {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let provider = Self(Arc::new(ProviderState {
            url,
            device: Mutex::new(device_ok()),
            token: Mutex::new(VecDeque::from([oauth_error("authorization_pending")])),
            keys_fail: Mutex::new(None),
            jwks: Mutex::new(vec![signer().jwk()]),
            keys_served: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            discovery_issuer: Mutex::new(None),
        }));
        let app = axum::Router::new()
            .route(DISCOVERY, get(discovery))
            .route(DEVICE, post(device))
            .route(TOKEN, post(token))
            .route(KEYS, get(keys))
            .with_state(provider.clone());
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        provider
    }

    fn url(&self) -> &str {
        &self.0.url
    }

    fn answer_device(&self, reply: Reply) {
        *lock(&self.0.device) = reply;
    }

    /// The token endpoint answers `replies` in order, the last repeating.
    fn answer_token(&self, replies: Vec<Reply>) {
        *lock(&self.0.token) = replies.into();
    }

    /// The JWKS holds exactly `signers` from now on.
    fn publish(&self, signers: &[&Signer]) {
        *lock(&self.0.jwks) = signers.iter().map(|signer| signer.jwk()).collect();
    }

    /// The JWKS holds exactly `jwks` from now on.
    fn publish_jwks(&self, jwks: Vec<Value>) {
        *lock(&self.0.jwks) = jwks;
    }

    fn fail_keys(&self, reply: Reply) {
        *lock(&self.0.keys_fail) = Some(reply);
    }

    /// How many times the JWKS was fetched.
    fn keys_served(&self) -> usize {
        self.0.keys_served.load(Ordering::SeqCst)
    }

    /// The discovery document names `issuer` from now on.
    fn name_issuer(&self, issuer: &str) {
        *lock(&self.0.discovery_issuer) = Some(issuer.to_owned());
    }

    /// How many requests reached `path`.
    fn served(&self, path: &str) -> usize {
        lock(&self.0.seen)
            .iter()
            .filter(|seen| seen.path == path)
            .count()
    }

    /// The first request seen at `path`.
    fn first_at(&self, path: &str) -> Option<Seen> {
        lock(&self.0.seen)
            .iter()
            .find(|seen| seen.path == path)
            .cloned()
    }

    /// Claims of a good ID token for this provider and [`CLIENT`]:
    /// `email_verified`, valid for an hour, `roles` in Zitadel's claim.
    fn claims(&self, sub: &str, login: &str, email: &str, roles: &[&str]) -> Value {
        let roles: serde_json::Map<String, Value> = roles
            .iter()
            .map(|role| ((*role).to_owned(), json!({"org-1": "example.org"})))
            .collect();
        json!({
            "iss": self.url(),
            "aud": [CLIENT, "project-1"],
            "sub": sub,
            "preferred_username": login,
            "email": email,
            "email_verified": true,
            "iat": unix_now(),
            "exp": unix_now().saturating_add(3600),
            (ROLES_CLAIM): roles,
        })
    }
}

fn record(provider: &Provider, path: &str, form: Vec<(String, String)>) {
    lock(&provider.0.seen).push(Seen {
        path: path.to_owned(),
        form,
    });
}

async fn discovery(State(provider): State<Provider>) -> Reply {
    record(&provider, DISCOVERY, Vec::new());
    let url = provider.url();
    let issuer = lock(&provider.0.discovery_issuer)
        .clone()
        .unwrap_or_else(|| url.to_owned());
    ok(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{url}{AUTHORIZE}"),
        "device_authorization_endpoint": format!("{url}{DEVICE}"),
        "token_endpoint": format!("{url}{TOKEN}"),
        "jwks_uri": format!("{url}{KEYS}"),
    }))
}

async fn device(
    State(provider): State<Provider>,
    Form(form): Form<Vec<(String, String)>>,
) -> Reply {
    record(&provider, DEVICE, form);
    lock(&provider.0.device).clone()
}

async fn token(State(provider): State<Provider>, Form(form): Form<Vec<(String, String)>>) -> Reply {
    record(&provider, TOKEN, form);
    let mut replies = lock(&provider.0.token);
    if replies.len() > 1 {
        replies.pop_front().unwrap()
    } else {
        replies.front().cloned().unwrap()
    }
}

async fn keys(State(provider): State<Provider>) -> Reply {
    record(&provider, KEYS, Vec::new());
    provider.0.keys_served.fetch_add(1, Ordering::SeqCst);
    let failure = lock(&provider.0.keys_fail).clone();
    if let Some(failure) = failure {
        return failure;
    }
    ok(json!({"keys": *lock(&provider.0.jwks)}))
}

// --- The module under test --------------------------------------------------

/// The module's configuration against `provider`: `issuer`, `client_id`
/// (a secret, decrypted by the host), `admin_role`, `scopes`.
fn config(provider: &Provider, admin_role: &str) -> ModuleConfig {
    [
        ("issuer", provider.url()),
        ("client_id", CLIENT),
        ("admin_role", admin_role),
        ("scopes", SCOPES),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

/// The installed row of `oidc` with the `issuer` of `config` (inserted,
/// or replaced on a second load), as an install keeps it: `core-call auth.create_session` opens sessions only
/// for the issuer the admin configured.
async fn install_row(world: &World, config: &ModuleConfig) {
    let mut record = ModuleConfigRecord::new();
    if let Some(issuer) = config.get("issuer") {
        let _none = record.insert(
            "issuer".to_owned(),
            ConfigValue::Plain(Value::from(issuer.as_str())),
        );
    }
    let row = InstalledModule {
        name: oidc(),
        version: ModuleVersion::new("0.1.0").unwrap(),
        sha256: "ab".repeat(32),
        enabled: true,
        config: record,
        error: None,
        installed_at: world.ports.clock.now(),
    };
    let store = &world.ports.store;
    if store.module(&oidc()).await.unwrap().is_some() {
        store.update_module(&row).await.unwrap();
    } else {
        store.insert_module(&row).await.unwrap();
    }
}

/// The built `oidc` component installed and loaded with `config` and
/// migrated from 0, as an install does, into a host over the world's
/// store whose `core-call` is the core's bridge over the world.
async fn load_with(world: &World, config: ModuleConfig) -> Oidc {
    install_row(world, &config).await;
    let host: Host<InMemoryStore> =
        Host::new(world.ports.store.clone(), TokioTimer, TracingLog, limits()).unwrap();
    let host = host
        .with_core_calls(
            CoreBridge::new(Arc::clone(&world.ports), Arc::clone(&world.settings))
                .allowing_loopback_issuers(),
        )
        .unwrap()
        .allowing_private_networks();
    let module = host
        .load(oidc(), &component("oidc").unwrap(), config)
        .await
        .unwrap();
    module.migrate(0).await.unwrap();
    let set = ModuleSet::new();
    set.insert(module);
    ModuleHooks::new(Arc::clone(host.registry()), set)
}

/// The module against `provider` with the admin role `admin`.
async fn load(world: &World, provider: &Provider) -> Oidc {
    load_with(world, config(provider, "admin")).await
}

fn request(method: &str, path: &str, body: &Value) -> ModuleRequest {
    ModuleRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: body.to_string().into_bytes(),
    }
}

/// `request` from the browser holding the login cookie of `state`: the
/// header the core shows its value in (`ModuleRequest::LOGIN_STATE_HEADER`).
fn bound(mut request: ModuleRequest, state: &str) -> ModuleRequest {
    request.headers.push((
        ModuleRequest::LOGIN_STATE_HEADER.to_owned(),
        state.to_owned(),
    ));
    request
}

fn get_route(path: &str) -> ModuleRequest {
    ModuleRequest {
        method: "GET".to_owned(),
        path: path.to_owned(),
        ..ModuleRequest::default()
    }
}

/// `POST /device` without a session.
async fn start(hooks: &Oidc) -> ModuleResponse {
    hooks
        .handle_route(&oidc(), None, request("POST", "/device", &json!({})))
        .await
        .unwrap()
}

/// `POST /device/poll` of [`DEVICE_CODE`] from the machine `laptop`.
async fn poll(hooks: &Oidc) -> ModuleResponse {
    hooks
        .handle_route(
            &oidc(),
            None,
            request(
                "POST",
                "/device/poll",
                &json!({"device_code": DEVICE_CODE, "name": "laptop"}),
            ),
        )
        .await
        .unwrap()
}

/// One device login whose token endpoint answers `reply`: start, poll.
async fn log_in_with(hooks: &Oidc, provider: &Provider, reply: Reply) -> ModuleResponse {
    provider.answer_token(vec![reply]);
    let _started = start(hooks).await;
    poll(hooks).await
}

/// One device login whose ID token carries `claims`, signed by [`signer`].
async fn log_in(hooks: &Oidc, provider: &Provider, claims: &Value) -> ModuleResponse {
    log_in_with(hooks, provider, token_reply(&signer().mint(claims))).await
}

fn body(response: &ModuleResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap_or_else(
        |_| json!({"unparsed": String::from_utf8_lossy(&response.body).into_owned()}),
    )
}

/// The session token a finished login answered.
fn granted_token(response: &ModuleResponse) -> String {
    assert_eq!(response.status, 200, "{:?}", body(response));
    body(response)["token"].as_str().unwrap().to_owned()
}

fn header_value<'a>(response: &'a ModuleResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The users that did not come from `liftoffd setup`.
async fn oidc_users(world: &World) -> Vec<User> {
    world
        .ports
        .store
        .users()
        .await
        .unwrap()
        .into_iter()
        .filter(|user| user.source != UserSource::Local)
        .collect()
}

/// The one user that logged in through the provider.
async fn the_oidc_user(world: &World) -> User {
    let users = oidc_users(world).await;
    assert_eq!(users.len(), 1, "{users:?}");
    users.into_iter().next().unwrap()
}

/// Every session of every user.
async fn all_sessions(world: &World) -> Vec<Session> {
    let mut sessions = Vec::new();
    for user in world.ports.store.users().await.unwrap() {
        sessions.extend(world.ports.store.sessions_of_user(user.id).await.unwrap());
    }
    sessions
}

/// The stored session of the token `raw`.
async fn session_of(world: &World, raw: &str) -> Session {
    let secret = SessionSecret::parse(raw).unwrap();
    world
        .ports
        .store
        .session_by_hash(&secret.hash())
        .await
        .unwrap()
        .unwrap()
}

/// Makes the module's cached `key` (`jwks`, `discovery`) `secs` older,
/// as if it had been fetched that long before.
async fn age_cache(world: &World, key: &str, secs: u64) {
    let store = &world.ports.store;
    let bytes = store.module_get(&oidc(), key).await.unwrap().unwrap();
    let mut cached: Value = serde_json::from_slice(&bytes).unwrap();
    let fetched_at = cached["fetched_at"].as_u64().unwrap();
    cached["fetched_at"] = json!(fetched_at.saturating_sub(secs));
    store
        .module_set(&oidc(), key, cached.to_string().as_bytes())
        .await
        .unwrap();
}

/// Neither a user nor a session was added to the world.
async fn created_nothing(world: &World) -> bool {
    oidc_users(world).await.is_empty() && all_sessions(world).await.is_empty()
}

// --- Tests --------------------------------------------------------------------

mod manifest {
    use super::*;

    async fn route(hooks: &Oidc, method: &str, path: &str) -> RouteSpec {
        hooks.module_route(&oidc(), method, path).await.unwrap()
    }

    fn public(method: &str, path: &str) -> RouteSpec {
        RouteSpec {
            method: method.to_owned(),
            path: path.to_owned(),
            public: true,
        }
    }

    // TC: OIDC-01 (`POST /m/oidc/device` is public)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_public_device_route_when_loaded() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        assert_eq!(
            route(&hooks, "POST", "/device").await,
            public("POST", "/device")
        );
    }

    // TC: OIDC-01 (`POST /m/oidc/device/poll` is public)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_public_poll_route_when_loaded() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        assert_eq!(
            route(&hooks, "POST", "/device/poll").await,
            public("POST", "/device/poll")
        );
    }

    // TC: OIDC-01 (the browser login `GET /m/oidc/login` and
    // `GET /m/oidc/callback` are public)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_public_browser_routes_when_loaded() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        assert_eq!(
            (
                route(&hooks, "GET", "/login").await,
                route(&hooks, "GET", "/callback").await
            ),
            (public("GET", "/login"), public("GET", "/callback"))
        );
    }

    // TC: OIDC-01 (the manifest declares `replaces: ["login"]`,
    // so `lof login` runs through the module when it is installed)
    #[tokio::test(flavor = "multi_thread")]
    async fn replaces_login_when_loaded() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let manifest = hooks.registry().manifest(&oidc()).unwrap();
        assert_eq!(manifest.replaces, vec!["login".to_owned()]);
    }

    // TC: OIDC-01 (hooks `authenticate` and `render_page`)
    #[tokio::test(flavor = "multi_thread")]
    async fn subscribes_to_authenticate_and_render_page_when_loaded() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let manifest = hooks.registry().manifest(&oidc()).unwrap();
        assert!(
            manifest.hooks.contains(&HookName::Authenticate)
                && manifest.hooks.contains(&HookName::RenderPage),
            "{:?}",
            manifest.hooks
        );
    }

    // TC: OIDC-01 (config `issuer`, `client_id` (secret),
    // `admin_role`, `scopes`; `redirect_uri` of the browser login; plan
    // 2.8: its web client `web_client_id` and `web_client_secret`)
    #[tokio::test(flavor = "multi_thread")]
    async fn declares_config_fields_when_loaded() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let schema = hooks
            .registry()
            .manifest(&oidc())
            .unwrap()
            .config_schema
            .unwrap_or(Value::Null);
        let mut fields: Vec<&str> = schema["properties"]
            .as_object()
            .map(|properties| properties.keys().map(String::as_str).collect())
            .unwrap_or_default();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "admin_role",
                "client_id",
                "issuer",
                "redirect_uri",
                "scopes",
                "web_client_id",
                "web_client_secret"
            ]
        );
    }

    // TC: OIDC-01 (`client_id` is a secret field, sealed at
    // install, REG-05)
    #[tokio::test(flavor = "multi_thread")]
    async fn marks_client_id_secret_when_loaded() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let schema = hooks
            .registry()
            .manifest(&oidc())
            .unwrap()
            .config_schema
            .unwrap_or(Value::Null);
        assert_eq!(schema["properties"]["client_id"]["format"], json!("secret"));
    }
}

mod device_start {
    use super::*;

    // TC: OIDC-01 (start: the provider's codes reach the CLI)
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_provider_codes_when_provider_answers() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let started = start(&hooks).await;
        assert_eq!(
            (started.status, body(&started)),
            (
                200,
                json!({
                    "device_code": DEVICE_CODE,
                    "user_code": USER_CODE,
                    "verification_uri_complete": VERIFICATION,
                    "interval": 7,
                    "expires_in": 300
                })
            )
        );
    }

    // TC: OIDC-01 (the device request names the client and the scopes)
    #[tokio::test(flavor = "multi_thread")]
    async fn asks_provider_with_client_and_scopes_when_started() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let _started = start(&hooks).await;
        let seen = provider.first_at(DEVICE);
        assert_eq!(
            seen.as_ref()
                .map(|seen| (seen.field("client_id"), seen.field("scope"))),
            Some((Some(CLIENT), Some(SCOPE)))
        );
    }

    // TC: OIDC-06 (OpenID Connect Discovery 1.0, 4.3: a discovery
    // document that names another issuer is not this provider's; nothing
    // is asked of the endpoints it names)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_502_and_asks_nothing_when_discovery_names_another_issuer() {
        let (world, provider) = (World::new().await, Provider::start().await);
        provider.name_issuer("https://evil.example");
        let hooks = load(&world, &provider).await;
        let started = start(&hooks).await;
        assert_eq!(
            (started.status, provider.served(DEVICE)),
            (502, 0),
            "{:?}",
            body(&started)
        );
    }

    // TC: OIDC-06 (a provider that refuses is the provider's failure: 502)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_502_when_provider_refuses_device_request() {
        let (world, provider) = (World::new().await, Provider::start().await);
        provider.answer_device(oauth_error("invalid_client"));
        let hooks = load(&world, &provider).await;
        assert_eq!(start(&hooks).await.status, 502);
    }
}

mod poll_body {
    use super::*;

    // TC: OIDC-04 (the public poll takes `{"device_code","name"}` and
    // nothing else, as `liftoff_api::DevicePoll`)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_400_and_asks_nothing_when_body_has_unknown_field() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let polled = hooks
            .handle_route(
                &oidc(),
                None,
                request(
                    "POST",
                    "/device/poll",
                    &json!({"device_code": DEVICE_CODE, "name": "laptop", "role": "admin"}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(
            (polled.status, provider.first_at(TOKEN).is_none()),
            (400, true)
        );
    }
}

mod first_login {
    use super::*;

    // TC: OIDC-01 (sub `z1`, preferred_username `bob`, no roles → user
    // `bob`, role `user`, source `oidc:<issuer>`, subject `z1`)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_user_from_claims_when_first_login() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _granted = granted_token(&log_in(&hooks, &provider, &claims).await);
        let bob = the_oidc_user(&world).await;
        assert_eq!(
            (
                bob.login.as_str(),
                bob.email.as_str(),
                bob.role,
                bob.source.as_string(),
                bob.subject.as_deref()
            ),
            (
                "bob",
                "bob@example.com",
                UserRole::User,
                format!("oidc:{}", provider.url()),
                Some("z1")
            )
        );
    }

    // TC: OIDC-01 (the answer is the session token and the user)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_session_token_and_user_when_first_login() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let granted = log_in(&hooks, &provider, &claims).await;
        let answer = body(&granted);
        assert_eq!(
            (
                granted.status,
                answer["token"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("lfs_")),
                answer["user"]["login"].as_str(),
                answer["user"]["role"].as_str()
            ),
            (200, true, Some("bob"), Some("user")),
            "{answer}"
        );
    }

    // TC: OIDC-01 (only the SHA-256 is stored; a `cli` session of 30 days
    // named after the machine)
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_thirty_day_cli_session_when_first_login() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = granted_token(&log_in(&hooks, &provider, &claims).await);
        let session = session_of(&world, &token).await;
        let now = world.ports.clock.now();
        assert_eq!(
            (
                session.user_id,
                session.kind,
                session.name.as_str(),
                session.expires_at,
                session.last_used_at
            ),
            (
                the_oidc_user(&world).await.id,
                SessionKind::Cli,
                "laptop",
                now + Duration::days(30),
                now
            )
        );
    }

    // TC: OIDC-01 (the token request is the device grant of the client)
    #[tokio::test(flavor = "multi_thread")]
    async fn polls_token_endpoint_with_device_grant_when_polled() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let _pending = log_in_with(&hooks, &provider, oauth_error("authorization_pending")).await;
        let seen = provider.first_at(TOKEN);
        assert_eq!(
            seen.as_ref().map(|seen| (
                seen.field("grant_type"),
                seen.field("device_code"),
                seen.field("client_id")
            )),
            Some((Some(DEVICE_GRANT), Some(DEVICE_CODE), Some(CLIENT)))
        );
    }

    // TC: OIDC-01 (an address the provider has not verified is not kept)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_no_email_when_it_is_not_verified() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["email_verified"] = json!(false);
        let _granted = granted_token(&log_in(&hooks, &provider, &claims).await);
        assert_eq!(the_oidc_user(&world).await.email, "");
    }
}

mod repeat_login {
    use super::*;

    // TC: OIDC-02 (same `sub`, new email → one user, email refreshed)
    #[tokio::test(flavor = "multi_thread")]
    async fn updates_user_when_same_subject_logs_in_again() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let first = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _first = granted_token(&log_in(&hooks, &provider, &first).await);
        let id = the_oidc_user(&world).await.id;
        let second = provider.claims("z1", "bob", "new@example.com", &[]);
        let _second = granted_token(&log_in(&hooks, &provider, &second).await);
        let bob = the_oidc_user(&world).await;
        assert_eq!((bob.id, bob.email.as_str()), (id, "new@example.com"));
    }

    // TC: OIDC-02 (the new session is added to the old one)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_both_sessions_when_same_subject_logs_in_again() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let first = granted_token(&log_in(&hooks, &provider, &claims).await);
        let second = granted_token(&log_in(&hooks, &provider, &claims).await);
        let bob = the_oidc_user(&world).await;
        assert_eq!(
            (
                session_of(&world, &first).await.user_id,
                session_of(&world, &second).await.user_id,
                world
                    .ports
                    .store
                    .sessions_of_user(bob.id)
                    .await
                    .unwrap()
                    .len()
            ),
            (bob.id, bob.id, 2)
        );
    }

    // TC: OIDC-02 (the JWKS is cached, not fetched per login)
    #[tokio::test(flavor = "multi_thread")]
    async fn fetches_jwks_once_when_two_logins_follow() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _first = granted_token(&log_in(&hooks, &provider, &claims).await);
        let _second = granted_token(&log_in(&hooks, &provider, &claims).await);
        assert_eq!(provider.keys_served(), 1);
    }

    // TC: OIDC-02 (the JWKS cache lives in the module's store,
    // so a reloaded module does not fetch it again)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_jwks_when_module_is_loaded_again() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let first = load(&world, &provider).await;
        let _first = granted_token(&log_in(&first, &provider, &claims).await);
        drop(first);
        let again = load(&world, &provider).await;
        let _second = granted_token(&log_in(&again, &provider, &claims).await);
        assert_eq!(provider.keys_served(), 1);
    }

    // TC: OIDC-02 (a token signed by a key the cache does not hold reads
    // the JWKS again once: the provider rotated its keys)
    #[tokio::test(flavor = "multi_thread")]
    async fn accepts_rotated_key_when_jwks_changed() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _first = granted_token(&log_in(&hooks, &provider, &claims).await);
        let rotated = Signer::generate("k2");
        provider.publish(&[&rotated]);
        let second = log_in_with(&hooks, &provider, token_reply(&rotated.mint(&claims))).await;
        assert_eq!((second.status, provider.keys_served()), (200, 2));
    }

    // TC: OIDC-06 (the kept JWKS expires: a key the provider took out of
    // its JWKS stops verifying once the cache is older than its TTL, also
    // across a reload)
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_removed_key_when_cached_jwks_is_stale() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _first = granted_token(&log_in(&hooks, &provider, &claims).await);
        provider.publish(&[&Signer::generate("k2")]);
        age_cache(&world, "jwks", JWKS_TTL_SECS + 1).await;
        let second = log_in(&hooks, &provider, &claims).await;
        assert_eq!((second.status, provider.keys_served()), (403, 2));
    }

    // TC: OIDC-02 (a JWKS younger than its TTL is used as kept)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_jwks_when_cache_is_younger_than_ttl() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _first = granted_token(&log_in(&hooks, &provider, &claims).await);
        age_cache(&world, "jwks", JWKS_TTL_SECS - 60).await;
        let _second = granted_token(&log_in(&hooks, &provider, &claims).await);
        assert_eq!(provider.keys_served(), 1);
    }

    // TC: MCPH-03 (a `kid` the JWKS does not hold reads it again at most
    // once a minute: forged kids at `/mcp` do not make the daemon fetch it
    // per request)
    #[tokio::test(flavor = "multi_thread")]
    async fn fetches_jwks_once_when_two_tokens_of_unknown_key_follow() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _first = granted_token(&log_in(&hooks, &provider, &claims).await);
        let stranger = Signer::generate("k9");
        let _refused = log_in_with(&hooks, &provider, token_reply(&stranger.mint(&claims))).await;
        let _again = log_in_with(&hooks, &provider, token_reply(&stranger.mint(&claims))).await;
        assert_eq!(provider.keys_served(), 2);
    }

    // TC: MCPH-03 (the minute over, an unknown `kid` reads the JWKS again)
    #[tokio::test(flavor = "multi_thread")]
    async fn fetches_jwks_again_when_last_refresh_is_older_than_a_minute() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _first = granted_token(&log_in(&hooks, &provider, &claims).await);
        let stranger = Signer::generate("k9");
        let _refused = log_in_with(&hooks, &provider, token_reply(&stranger.mint(&claims))).await;
        age_cache(&world, "jwks_refresh", JWKS_REFRESH_SECS + 1).await;
        let _again = log_in_with(&hooks, &provider, token_reply(&stranger.mint(&claims))).await;
        assert_eq!(provider.keys_served(), 3);
    }

    // TC: OIDC-02 (the discovery document is cached too: two logins read
    // it once)
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_discovery_once_when_two_logins_follow() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _first = granted_token(&log_in(&hooks, &provider, &claims).await);
        let _second = granted_token(&log_in(&hooks, &provider, &claims).await);
        assert_eq!(provider.served(DISCOVERY), 1);
    }
}

mod role {
    use super::*;

    // TC: OIDC-03 (the roles claim holds the configured `admin_role`)
    #[tokio::test(flavor = "multi_thread")]
    async fn grants_admin_when_roles_claim_has_admin_role() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z9", "alice", "alice@example.com", &["admin"]);
        let _granted = granted_token(&log_in(&hooks, &provider, &claims).await);
        assert_eq!(the_oidc_user(&world).await.role, UserRole::Admin);
    }

    // TC: OIDC-03 (the next login without the role takes it away)
    #[tokio::test(flavor = "multi_thread")]
    async fn revokes_admin_when_next_login_lacks_admin_role() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let admin = provider.claims("z9", "alice", "alice@example.com", &["admin"]);
        let _first = granted_token(&log_in(&hooks, &provider, &admin).await);
        let plain = provider.claims("z9", "alice", "alice@example.com", &[]);
        let _second = granted_token(&log_in(&hooks, &provider, &plain).await);
        assert_eq!(the_oidc_user(&world).await.role, UserRole::User);
    }

    // TC: OIDC-03 (other roles do not make an admin)
    #[tokio::test(flavor = "multi_thread")]
    async fn stays_user_when_roles_claim_has_other_roles() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z9", "alice", "alice@example.com", &["viewer"]);
        let _granted = granted_token(&log_in(&hooks, &provider, &claims).await);
        assert_eq!(the_oidc_user(&world).await.role, UserRole::User);
    }

    // TC: OIDC-03 (the admin role is the configured one, not `admin`)
    #[tokio::test(flavor = "multi_thread")]
    async fn grants_admin_when_claim_has_configured_role() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load_with(&world, config(&provider, "liftoff-admin")).await;
        let claims = provider.claims("z9", "alice", "alice@example.com", &["liftoff-admin"]);
        let _granted = granted_token(&log_in(&hooks, &provider, &claims).await);
        assert_eq!(the_oidc_user(&world).await.role, UserRole::Admin);
    }

    // TC: OIDC-03
    #[tokio::test(flavor = "multi_thread")]
    async fn stays_user_when_claim_has_admin_but_config_names_another_role() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load_with(&world, config(&provider, "liftoff-admin")).await;
        let claims = provider.claims("z9", "alice", "alice@example.com", &["admin"]);
        let _granted = granted_token(&log_in(&hooks, &provider, &claims).await);
        assert_eq!(the_oidc_user(&world).await.role, UserRole::User);
    }
}

mod pending {
    use super::*;

    // TC: OIDC-04 (`authorization_pending` → 202 without a session)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_202_pending_when_authorization_pending() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let polled = log_in_with(&hooks, &provider, oauth_error("authorization_pending")).await;
        assert_eq!(
            (polled.status, body(&polled)),
            (202, json!({"status": "pending"}))
        );
    }

    // TC: OIDC-04 (`slow_down` is pending too)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_202_pending_when_slow_down() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let polled = log_in_with(&hooks, &provider, oauth_error("slow_down")).await;
        assert_eq!(
            (polled.status, body(&polled)),
            (202, json!({"status": "pending"}))
        );
    }

    // TC: OIDC-04 (no user, no session while pending)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_nothing_when_pending() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let polled = log_in_with(&hooks, &provider, oauth_error("authorization_pending")).await;
        assert_eq!((polled.status, created_nothing(&world).await), (202, true));
    }
}

mod refused {
    use super::*;

    // TC: OIDC-05 (`access_denied` → 403, no user)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_access_denied() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let polled = log_in_with(&hooks, &provider, oauth_error("access_denied")).await;
        assert_eq!((polled.status, created_nothing(&world).await), (403, true));
    }

    // TC: OIDC-05 (`expired_token` → 410, no user)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_410_and_creates_nothing_when_expired_token() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let polled = log_in_with(&hooks, &provider, oauth_error("expired_token")).await;
        assert_eq!((polled.status, created_nothing(&world).await), (410, true));
    }
}

mod id_token {
    use super::*;

    /// The poll answer for an ID token of `claims` signed by `key`.
    async fn polled_with(world: &World, provider: &Provider, id_token: &str) -> (u16, bool) {
        let hooks = load(world, provider).await;
        let polled = log_in_with(&hooks, provider, token_reply(id_token)).await;
        (polled.status, created_nothing(world).await)
    }

    // TC: OIDC-06 (audience is not the client)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_audience_is_wrong() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["aud"] = json!(["some-other-client"]);
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (issuer is not the configured one)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_issuer_is_wrong() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["iss"] = json!("https://evil.example");
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (`exp` in the past)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_expired() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["iat"] = json!(unix_now().saturating_sub(7200));
        claims["exp"] = json!(unix_now().saturating_sub(3600));
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (signature: another key under the published `kid`)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_signed_by_another_key() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = Signer::generate("k1").mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (a `kid` the JWKS does not hold, even after reading it
    // again)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_key_id_is_unknown() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = Signer::generate("k-unknown").mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (`alg: none`, no signature)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_token_is_unsigned() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(json!({"alg": "none", "kid": "k1"}).to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (not a JWT at all)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_token_is_not_a_jwt() {
        let (world, provider) = (World::new().await, Provider::start().await);
        assert_eq!(
            polled_with(&world, &provider, "not-a-jwt").await,
            (403, true)
        );
    }

    // TC: OIDC-06 (the keys cannot be fetched: the provider's failure,
    // 502, not 403)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_502_and_creates_nothing_when_jwks_cannot_be_fetched() {
        let (world, provider) = (World::new().await, Provider::start().await);
        provider.fail_keys(Reply {
            status: 503,
            body: Value::Null,
        });
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (502, true));
    }

    // TC: OIDC-06 (`azp` names another client)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_authorized_party_is_another_client() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["azp"] = json!("some-other-client");
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (`azp` naming this client is accepted: the refusal
    // above is the `azp` check, not something else of the token)
    #[tokio::test(flavor = "multi_thread")]
    async fn logs_in_when_authorized_party_is_this_client() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["azp"] = json!(CLIENT);
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (200, false));
    }

    // TC: OIDC-06 (`nbf` later than now plus the leeway)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_not_valid_yet() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["nbf"] = json!(unix_now().saturating_add(3600));
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (an empty `sub` names nobody)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_subject_is_empty() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let claims = provider.claims("", "bob", "bob@example.com", &[]);
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (no `preferred_username`: no login to give the user)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_preferred_username_is_missing() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let _removed = claims
            .as_object_mut()
            .and_then(|claims| claims.remove("preferred_username"));
        let token = signer().mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (a correct signature of a key under 2048 bits is too
    // weak: refused)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_key_is_under_2048_bits() {
        let (world, provider) = (World::new().await, Provider::start().await);
        provider.publish_jwks(vec![signer().jwk(), WeakSigner::jwk()]);
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = WeakSigner::mint(&claims);
        assert_eq!(polled_with(&world, &provider, &token).await, (403, true));
    }

    // TC: OIDC-06 (a token answer without `id_token`)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_502_and_creates_nothing_when_token_answer_has_no_id_token() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let polled = log_in_with(
            &hooks,
            &provider,
            ok(json!({"access_token": "at", "token_type": "Bearer"})),
        )
        .await;
        assert_eq!((polled.status, created_nothing(&world).await), (502, true));
    }
}

mod session {
    use super::*;

    // TC: OIDC-07 (the core prolongs a module-made `cli` session on use:
    // `expires_at = now + 30d`, `last_used_at = now`)
    #[tokio::test(flavor = "multi_thread")]
    async fn prolongs_session_to_thirty_days_when_used() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = granted_token(&log_in(&hooks, &provider, &claims).await);
        world.ports.clock.advance(Duration::days(29));
        let now = world.ports.clock.now();
        let _principal = world
            .auth()
            .authenticate(
                &HookRequest::default(),
                Some(&Bearer::parse(&token).unwrap()),
            )
            .await
            .unwrap();
        let stored = session_of(&world, &token).await;
        assert_eq!(
            (stored.expires_at, stored.last_used_at),
            (now + Duration::days(30), now)
        );
    }

    // TC: OIDC-07 (the session authenticates its user)
    #[tokio::test(flavor = "multi_thread")]
    async fn authenticates_logged_in_user_when_session_used() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = granted_token(&log_in(&hooks, &provider, &claims).await);
        let principal = world
            .auth()
            .authenticate(
                &HookRequest::default(),
                Some(&Bearer::parse(&token).unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(principal.user().login.as_str(), "bob");
    }
}

mod authenticate {
    use super::*;

    fn bearer_request(token: &str) -> HookRequest {
        HookRequest {
            method: "GET".to_owned(),
            path: "/api/v1/me".to_owned(),
            headers: vec![("authorization".to_owned(), format!("Bearer {token}"))],
        }
    }

    // TC: OIDC-07 (a bearer the core does not know → the module
    // has no opinion; sessions are the core's)
    #[tokio::test(flavor = "multi_thread")]
    async fn continues_when_bearer_is_unknown() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let request = bearer_request("lfs_unknown0123456789abcdefghijABCDEFGHIJ");
        assert_eq!(
            hooks.calls().authenticate(&oidc(), &request).await.unwrap(),
            AuthResult::Continue
        );
    }
}

mod render_page {
    use super::*;

    // TC: OIDC-01 (`render_page(login)` sends the browser to the
    // module's login route, which redirects to the provider)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_login_route_when_login_page_rendered() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        // The page's context travels in the query, which the module's
        // route ignores.
        let host = liftoff_core::domain::Host::new("api.apps.test").unwrap();
        let page = hooks
            .render_page(&PageRequest::at(PageKind::Login, &host, "/x?y=1"))
            .await;
        assert!(
            matches!(&page, Some(ModulePage::Html(html)) if html.contains("/api/v1/m/oidc/login")),
            "{page:?}"
        );
    }

    // TC: OIDC-01 (other pages stay the core's)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_core_page_when_forbidden_page_rendered() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        assert_eq!(
            hooks
                .render_page(&PageRequest::of(PageKind::Forbidden))
                .await,
            None
        );
    }
}

mod browser_login {
    use super::*;

    // TC: OIDC-01 (`GET /m/oidc/login` → 302 to the provider's
    // authorization endpoint, code flow with `state`, `nonce` and PKCE)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_provider_with_pkce_when_login_started() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let started = hooks
            .handle_route(&oidc(), None, get_route("/login"))
            .await
            .unwrap();
        let location = header_value(&started, "location").unwrap_or_default();
        let query = location
            .strip_prefix(&format!("{}{AUTHORIZE}?", provider.url()))
            .unwrap_or_default();
        let has = |pair: &str| query.split('&').any(|part| part == pair);
        let named = |name: &str| {
            query.split('&').any(|part| {
                part.strip_prefix(&format!("{name}="))
                    .is_some_and(|v| !v.is_empty())
            })
        };
        assert_eq!(
            (
                started.status,
                has("response_type=code"),
                has(&format!("client_id={CLIENT}")),
                has("code_challenge_method=S256"),
                named("code_challenge"),
                named("state"),
                named("nonce"),
            ),
            (302, true, true, true, true, true, true),
            "{location}"
        );
    }

    /// The `Location` of `GET /login` with `headers`.
    async fn login_location(hooks: &Oidc, headers: &[(&str, &str)]) -> String {
        let started = hooks
            .handle_route(
                &oidc(),
                None,
                ModuleRequest {
                    headers: headers
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                        .collect(),
                    ..get_route("/login")
                },
            )
            .await
            .unwrap();
        header_value(&started, "location")
            .unwrap_or_default()
            .to_owned()
    }

    // TC: OIDC-06 (the callback the provider sends the code to is never
    // taken from headers the client controls)
    #[tokio::test(flavor = "multi_thread")]
    async fn ignores_forwarded_host_when_login_started() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let location = login_location(
            &hooks,
            &[
                ("host", "evil.example"),
                ("x-forwarded-host", "evil.example"),
                ("x-forwarded-proto", "https"),
            ],
        )
        .await;
        assert!(
            location.starts_with(provider.url())
                && !location.contains("evil.example")
                && location.contains(
                    "redirect_uri=https%3A%2F%2Fliftoff.test%2Fapi%2Fv1%2Fm%2Foidc%2Fcallback"
                ),
            "{location}"
        );
    }

    // TC: OIDC-01 (without a configured `redirect_uri` the callback is that of `[api] public_url`, and the code is traded with it)
    #[tokio::test(flavor = "multi_thread")]
    async fn derives_redirect_uri_from_api_host_when_not_configured() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let (state, _, _) = started(&hooks).await;
        provider.answer_token(vec![oauth_error("invalid_grant")]);
        let _refused = callback(&hooks, &state).await;
        assert_eq!(
            provider
                .first_at(TOKEN)
                .and_then(|seen| seen.field("redirect_uri").map(str::to_owned)),
            Some(format!(
                "https://{}/api/v1/m/oidc/callback",
                world.settings.proxy.api_host
            ))
        );
    }

    // TC: OIDC-01 (the browser login uses the web client `liftoff gate` when one is configured, its secret with the code exchange, and takes ID tokens issued to it)
    #[tokio::test(flavor = "multi_thread")]
    async fn logs_in_with_web_client_when_configured() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let mut config = config(&provider, "admin");
        let _none = config.insert("web_client_id".to_owned(), "liftoff-gate".to_owned());
        let _none = config.insert("web_client_secret".to_owned(), "gate-secret".to_owned());
        let hooks = load_with(&world, config).await;
        let started = hooks
            .handle_route(&oidc(), None, get_route("/login"))
            .await
            .unwrap();
        let location = header_value(&started, "location")
            .unwrap_or_default()
            .to_owned();
        let query = location.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let param = |name: &str| {
            query
                .split('&')
                .find_map(|part| part.strip_prefix(&format!("{name}=")))
                .unwrap_or_default()
                .to_owned()
        };
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["aud"] = json!(["liftoff-gate"]);
        claims["nonce"] = json!(param("nonce"));
        provider.answer_token(vec![token_reply(&signer().mint(&claims))]);
        let done = callback(&hooks, &param("state")).await;
        let seen = provider.first_at(TOKEN).unwrap();
        assert_eq!(
            (
                param("client_id"),
                seen.field("client_id").map(str::to_owned),
                seen.field("client_secret").map(str::to_owned),
                done.status
            ),
            (
                "liftoff-gate".to_owned(),
                Some("liftoff-gate".to_owned()),
                Some("gate-secret".to_owned()),
                200
            )
        );
    }

    // TC: TEN-07 (v1 SSO-05: login CSRF; the login binds its `state` to
    // the browser with the cookie `__Host-liftoff_login_<8 of state>`)
    #[tokio::test(flavor = "multi_thread")]
    async fn sets_login_cookie_of_state_when_login_started() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let started = hooks
            .handle_route(&oidc(), None, get_route("/login"))
            .await
            .unwrap();
        let location = header_value(&started, "location").unwrap_or_default();
        let state = location
            .split('&')
            .find_map(|part| part.split("state=").nth(1))
            .unwrap_or_default()
            .to_owned();
        let cookie = header_value(&started, "set-cookie").unwrap_or_default();
        let attributes: Vec<&str> = cookie.split(';').map(str::trim).collect();
        let short: String = state.chars().take(8).collect();
        assert_eq!(
            (
                attributes.first().copied(),
                [
                    "Secure",
                    "HttpOnly",
                    "SameSite=Lax",
                    "Path=/",
                    "Max-Age=600"
                ]
                .iter()
                .all(|attribute| attributes.contains(attribute))
            ),
            (
                Some(format!("__Host-liftoff_login_{short}={state}").as_str()),
                true
            ),
            "{cookie}"
        );
    }

    // TC: TEN-07 (v1 SSO-05: a callback in a browser that does not hold
    // the login's cookie → 400, nothing created: someone else's
    // finished login cannot be planted in this browser)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_400_and_creates_nothing_when_state_not_bound_to_browser() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let (state, nonce, _) = started(&hooks).await;
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["nonce"] = json!(nonce);
        provider.answer_token(vec![token_reply(&signer().mint(&claims))]);
        let path = format!("/callback?code=c1&state={state}");
        let without = hooks
            .handle_route(&oidc(), None, get_route(&path))
            .await
            .unwrap();
        let other = hooks
            .handle_route(&oidc(), None, bound(get_route(&path), "another-state"))
            .await
            .unwrap();
        assert_eq!(
            (without.status, other.status, created_nothing(&world).await),
            (400, 400, true)
        );
    }

    // TC: TEN-07 (v1 SSO-05: the finished login clears its cookie)
    #[tokio::test(flavor = "multi_thread")]
    async fn clears_login_cookie_when_callback_completes_login() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let done = completed(&hooks, &provider, claims).await;
        let cookie = header_value(&done, "set-cookie").unwrap_or_default();
        assert!(
            cookie.starts_with("__Host-liftoff_login_") && cookie.contains("Max-Age=0"),
            "{cookie}"
        );
    }

    // TC: OIDC-01 (the admin's `redirect_uri` goes to the provider, and
    // the code is traded with it)
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_configured_redirect_uri_when_login_started() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let callback_url = "https://liftoff.example/api/v1/m/oidc/callback";
        let mut config = config(&provider, "admin");
        let _none = config.insert("redirect_uri".to_owned(), callback_url.to_owned());
        let hooks = load_with(&world, config).await;
        let location = login_location(&hooks, &[("x-forwarded-host", "evil.example")]).await;
        let query = location.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let state = query
            .split('&')
            .find_map(|part| part.strip_prefix("state="))
            .unwrap_or_default()
            .to_owned();
        provider.answer_token(vec![oauth_error("invalid_grant")]);
        let _refused = callback(&hooks, &state).await;
        assert_eq!(
            (
                query.split('&').any(|part| part
                    == "redirect_uri=https%3A%2F%2Fliftoff.example%2Fapi%2Fv1%2Fm%2Foidc%2Fcallback"),
                provider
                    .first_at(TOKEN)
                    .and_then(|seen| seen.field("redirect_uri").map(str::to_owned))
            ),
            (true, Some(callback_url.to_owned())),
            "{location}"
        );
    }

    /// `GET /login`: the `state`, `nonce` and `code_challenge` of the
    /// redirect.
    async fn started(hooks: &Oidc) -> (String, String, String) {
        let started = hooks
            .handle_route(&oidc(), None, get_route("/login"))
            .await
            .unwrap();
        let location = header_value(&started, "location").unwrap_or_default();
        let query = location.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let param = |name: &str| {
            query
                .split('&')
                .find_map(|part| part.strip_prefix(&format!("{name}=")))
                .unwrap_or_default()
                .to_owned()
        };
        (param("state"), param("nonce"), param("code_challenge"))
    }

    /// `GET /callback?code=c1&state=<state>` in the browser that started
    /// the login (its login cookie, as the core shows it).
    async fn callback(hooks: &Oidc, state: &str) -> ModuleResponse {
        hooks
            .handle_route(
                &oidc(),
                None,
                bound(
                    get_route(&format!("/callback?code=c1&state={state}")),
                    state,
                ),
            )
            .await
            .unwrap()
    }

    /// A login started, its callback after the provider answered an ID
    /// token of `claims` (whose `nonce` is set to the login's).
    async fn completed(hooks: &Oidc, provider: &Provider, mut claims: Value) -> ModuleResponse {
        let (state, nonce, _) = started(hooks).await;
        claims["nonce"] = json!(nonce);
        provider.answer_token(vec![token_reply(&signer().mint(&claims))]);
        callback(hooks, &state).await
    }

    // TC: OIDC-01 (the callback of a login the module started opens a
    // `browser` session of 24 hours)
    #[tokio::test(flavor = "multi_thread")]
    async fn opens_browser_session_when_callback_completes_login() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        let token = granted_token(&completed(&hooks, &provider, claims).await);
        let session = session_of(&world, &token).await;
        assert_eq!(
            (session.kind, session.expires_at),
            (
                SessionKind::Browser,
                world.ports.clock.now() + Duration::hours(24)
            )
        );
    }

    // TC: OIDC-01 (the code is traded with the PKCE verifier of the
    // challenge the browser carried)
    #[tokio::test(flavor = "multi_thread")]
    async fn exchanges_code_with_pkce_verifier_when_callback_arrives() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let (state, _, challenge) = started(&hooks).await;
        provider.answer_token(vec![oauth_error("invalid_grant")]);
        let _refused = callback(&hooks, &state).await;
        let seen = provider.first_at(TOKEN).unwrap();
        let verifier = seen.field("code_verifier").unwrap_or_default();
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, verifier.as_bytes());
        assert_eq!(
            (
                seen.field("grant_type"),
                seen.field("code"),
                URL_SAFE_NO_PAD.encode(digest.as_ref())
            ),
            (Some("authorization_code"), Some("c1"), challenge)
        );
    }

    // TC: OIDC-06 (an ID token without the login's nonce is refused)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_403_and_creates_nothing_when_nonce_differs() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let (state, _, _) = started(&hooks).await;
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["nonce"] = json!("another-nonce");
        provider.answer_token(vec![token_reply(&signer().mint(&claims))]);
        let called = callback(&hooks, &state).await;
        assert_eq!((called.status, created_nothing(&world).await), (403, true));
    }

    // TC: OIDC-06 (a `state` is good for one callback)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_400_when_callback_state_is_used_twice() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let (state, nonce, _) = started(&hooks).await;
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["nonce"] = json!(nonce);
        provider.answer_token(vec![token_reply(&signer().mint(&claims))]);
        let _first = granted_token(&callback(&hooks, &state).await);
        assert_eq!(callback(&hooks, &state).await.status, 400);
    }

    // TC: OIDC-06 (a callback with a `state` the module did not
    // issue → 400, nothing created)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_400_and_creates_nothing_when_callback_state_is_unknown() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let called = hooks
            .handle_route(&oidc(), None, get_route("/callback?code=c1&state=forged"))
            .await
            .unwrap();
        assert_eq!((called.status, created_nothing(&world).await), (400, true));
    }
}

/// The browser login the gate sends a visitor to (v1 SSO-05 on the module): `GET /login?rd=<path>` remembers where to go
/// back, and its callback sets the browser session as the cookie
/// `__Host-liftoff_session` and redirects there.
mod browser_session {
    use super::*;

    /// `GET /login?rd=<rd>` (encoded), the provider's answer for `bob`,
    /// then the callback.
    async fn completed_with_rd(hooks: &Oidc, provider: &Provider, rd: &str) -> ModuleResponse {
        let started = hooks
            .handle_route(&oidc(), None, get_route(&format!("/login?rd={rd}")))
            .await
            .unwrap();
        let location = header_value(&started, "location").unwrap_or_default();
        let query = location.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let param = |name: &str| {
            query
                .split('&')
                .find_map(|part| part.strip_prefix(&format!("{name}=")))
                .unwrap_or_default()
                .to_owned()
        };
        let mut claims = provider.claims("z1", "bob", "bob@example.com", &[]);
        claims["nonce"] = json!(param("nonce"));
        provider.answer_token(vec![token_reply(&signer().mint(&claims))]);
        hooks
            .handle_route(
                &oidc(),
                None,
                bound(
                    get_route(&format!("/callback?code=c1&state={}", param("state"))),
                    &param("state"),
                ),
            )
            .await
            .unwrap()
    }

    // TC: TEN-07 (v1 SSO-05: the callback of a login started with `rd`
    // goes back to `rd`)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_rd_when_login_started_with_rd() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let done = completed_with_rd(
            &hooks,
            &provider,
            "%2Fgate%2Fstart%3Fhost%3Dblog.apps.test%26rd%3D%252F",
        )
        .await;
        assert_eq!(
            (done.status, header_value(&done, "location")),
            (302, Some("/gate/start?host=blog.apps.test&rd=%2F"))
        );
    }

    // TC: TEN-07 (v1 SSO-05: and sets the 24-hour browser session as the
    // `__Host-` cookie of the API host)
    #[tokio::test(flavor = "multi_thread")]
    async fn sets_browser_session_cookie_when_login_started_with_rd() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let done = completed_with_rd(&hooks, &provider, "%2F").await;
        let cookie = header_value(&done, "set-cookie").unwrap_or_default();
        let attributes: Vec<&str> = cookie.split(';').map(str::trim).collect();
        let token = attributes
            .first()
            .and_then(|pair| pair.strip_prefix("__Host-liftoff_session="))
            .unwrap_or_else(|| panic!("no session cookie: {cookie:?}"));
        let session = session_of(&world, token).await;
        assert_eq!(
            (
                session.kind,
                ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"]
                    .iter()
                    .all(|attribute| attributes.contains(attribute))
            ),
            (SessionKind::Browser, true),
            "{cookie}"
        );
    }

    // TC: TEN-07 (v1 SSO-04: an `rd` that is not a path goes to `/`, so
    // the login is no open redirect)
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_root_when_rd_is_not_a_path() {
        let (world, provider) = (World::new().await, Provider::start().await);
        let hooks = load(&world, &provider).await;
        let done = completed_with_rd(&hooks, &provider, "%2F%2Fevil.example").await;
        assert_eq!(
            (done.status, header_value(&done, "location")),
            (302, Some("/"))
        );
    }
}
