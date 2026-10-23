#![cfg(test)]
//! End to end: the device flow of `lof
//! login <url>` through the REST router to a session the core accepts.
//! The built `oidc` component (`modules/oidc`) is installed (its row
//! names the issuer) and loaded by a module host whose `core-call` is the
//! daemon's own wiring: a `LateCore` bound by `liftoffd::modules::bind_bridge`
//! over the world's ports, as `daemon::serve` binds it through
//! `bind_core`, with the one difference that its bridge takes the fake
//! provider's plain-HTTP loopback issuer (`allowing_loopback_issuers`). The core's
//! `ModuleHooks` over it answer the REST router's module routes and
//! hooks, and a fake `OpenID` Connect provider on loopback signs the ID
//! token. The requests are the ones the CLI sends
//! (`crates/liftoff/src/commands/auth.rs`): `POST /api/v1/m/oidc/device`,
//! `POST /api/v1/m/oidc/device/poll` until it is no longer `202`, then
//! the session works on `GET /api/v1/me` (OIDC-01, OIDC-04, OIDC-07). It
//! lives with the composition root because it joins two adapters
//! (`liftoff-wasm` and `liftoff-rest`). The module's own cases are in
//! `crates/wasm/tests/oidc.rs`.
//!
//! The fake provider's handlers do not read the request bodies, as a
//! provider may answer before reading them: the module writes its whole
//! body before handing the request over, so that answer never races it.
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair, KeySize, PublicKeyComponents};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use liftoff_api::{DeviceLogin, DevicePoll, SessionGranted, UserView};
use liftoff_core::domain::{
    ConfigValue, InstalledModule, ModuleConfigRecord, ModuleName, ModuleVersion,
};
use liftoff_core::fakes::{InMemoryStore, WithHooks, World};
use liftoff_core::module::{CoreBridge, ModuleHooks};
use liftoff_core::ports::{Clock as _, Store as _};
use liftoff_rest::{AppState, RestConfig, Services, app_router};
use liftoff_wasm::testing::{Host, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TokioTimer, TracingLog};
use liftoffd::modules::{LateCore, bind_bridge};
use serde_json::{Value, json};
use tower::ServiceExt as _;

const CLIENT: &str = "liftoff-cli";

/// The fake provider: its URL, its signing key, how many polls it saw.
struct Provider {
    url: String,
    key: KeyPair,
    polls: AtomicUsize,
}

impl Provider {
    /// An RS256 ID token for `bob` of this provider and [`CLIENT`].
    fn id_token(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let header = json!({"alg": "RS256", "typ": "JWT", "kid": "k1"});
        let claims = json!({
            "iss": self.url, "aud": CLIENT, "sub": "z1", "preferred_username": "bob",
            "email": "bob@example.com", "email_verified": true, "iat": now, "exp": now.saturating_add(3600),
        });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0_u8; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
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

async fn device() -> axum::Json<Value> {
    axum::Json(json!({
        "device_code": "dc-1", "user_code": "ABCD-EFGH",
        "verification_uri_complete": "https://id.example/device?user_code=ABCD-EFGH",
        "expires_in": 300, "interval": 5,
    }))
}

/// `authorization_pending` once, then the ID token.
async fn token(State(provider): State<Shared>) -> (StatusCode, axum::Json<Value>) {
    if provider.polls.fetch_add(1, Ordering::SeqCst) == 0 {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "authorization_pending"})),
        );
    }
    (
        StatusCode::OK,
        axum::Json(
            json!({"access_token": "at", "token_type": "Bearer", "id_token": provider.id_token()}),
        ),
    )
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
        polls: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/oauth/v2/device_authorization", post(device))
        .route("/oauth/v2/token", post(token))
        .route("/oauth/v2/keys", get(keys))
        .with_state(Arc::clone(&provider));
    drop(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    provider
}

/// The installed row of `oidc` configured with `config`'s `issuer`.
async fn install(world: &World, config: &ModuleConfig) {
    let mut record = ModuleConfigRecord::new();
    if let Some(issuer) = config.get("issuer") {
        let _none = record.insert(
            "issuer".to_owned(),
            ConfigValue::Plain(Value::from(issuer.as_str())),
        );
    }
    world
        .ports
        .store
        .insert_module(&InstalledModule {
            name: oidc(),
            version: ModuleVersion::new("0.1.0").unwrap(),
            sha256: "ab".repeat(32),
            enabled: true,
            config: record,
            error: None,
            installed_at: world.ports.clock.now(),
        })
        .await
        .unwrap();
}

fn oidc() -> ModuleName {
    ModuleName::new("oidc").unwrap()
}

/// The API over the world's fakes with `oidc` against `provider` behind
/// the hooks and the module routes, its `core-call` bound as the daemon
/// binds it.
async fn app(world: &World, provider: &Provider) -> Router {
    let core = LateCore::default();
    let host: Host<InMemoryStore, LateCore> =
        Host::new(world.ports.store.clone(), TokioTimer, TracingLog, limits())
            .unwrap()
            .with_core_calls(core.clone())
            .unwrap()
            .allowing_private_networks();
    let settings = Arc::clone(&world.settings);
    bind_bridge(&core, &world.ports, move |ports| {
        CoreBridge::new(ports, Arc::clone(&settings)).allowing_loopback_issuers()
    });
    let config: ModuleConfig = [
        ("issuer", provider.url.as_str()),
        ("client_id", CLIENT),
        ("admin_role", "admin"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    install(world, &config).await;
    let module = host
        .load(oidc(), &component("oidc").unwrap(), config)
        .await
        .unwrap();
    module.migrate(0).await.unwrap();
    let set = ModuleSet::new();
    set.insert(module);
    let hooks = ModuleHooks::new(Arc::clone(host.registry()), set);
    let ports = Arc::new(WithHooks::new(Arc::clone(&world.ports), hooks));
    let services = Services::new(ports, Arc::clone(&world.settings));
    app_router(AppState::new(), services, &RestConfig::default())
}

/// `method path` with a JSON `body` or a bearer: the status and the
/// body's bytes.
async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<&Value>,
    bearer: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

/// `bytes` as the wire type `T` the CLI reads them as.
fn wire<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(bytes).into_owned()))
}

// TC: OIDC-01, OIDC-04, OIDC-07 (the device flow of
// `lof login` ends in a session the core accepts, through the daemon's
// `core-call` wiring). The answers are read as the `liftoff-api` types
// the CLI reads (`crates/liftoff/src/commands/auth.rs`) and the poll is
// the CLI's `DevicePoll`, so the module's wire format cannot drift from
// the CLI's unnoticed.
#[tokio::test(flavor = "multi_thread")]
async fn logs_in_through_device_flow_when_provider_confirms() {
    let world = World::new().await;
    let provider = provider().await;
    let app = app(&world, &provider).await;
    let (started, device) = call(
        &app,
        "POST",
        "/api/v1/m/oidc/device",
        Some(&json!({})),
        None,
    )
    .await;
    assert_eq!(
        started,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&device)
    );
    let device: DeviceLogin = wire(&device);
    let poll = serde_json::to_value(DevicePoll {
        device_code: device.device_code.clone(),
        name: Some("laptop".to_owned()),
    })
    .unwrap();
    let mut answers = Vec::new();
    let granted = loop {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/m/oidc/device/poll",
            Some(&poll),
            None,
        )
        .await;
        answers.push(status);
        if status != StatusCode::ACCEPTED || answers.len() > 3 {
            break body;
        }
    };
    assert_eq!(
        answers,
        vec![StatusCode::ACCEPTED, StatusCode::OK],
        "{}",
        String::from_utf8_lossy(&granted)
    );
    let granted: SessionGranted = wire(&granted);
    let (me, user) = call(&app, "GET", "/api/v1/me", None, Some(&granted.token)).await;
    assert_eq!(me, StatusCode::OK, "{}", String::from_utf8_lossy(&user));
    let user: UserView = wire(&user);
    assert_eq!(
        (
            device.user_code,
            device.verification_uri_complete,
            device.interval,
            device.expires_in,
            granted.user.login.as_str(),
            granted.user.source.clone(),
            user,
        ),
        (
            "ABCD-EFGH".to_owned(),
            "https://id.example/device?user_code=ABCD-EFGH".to_owned(),
            5,
            300,
            "bob",
            format!("oidc:{}", provider.url),
            granted.user.clone(),
        )
    );
}

// TC: OTL-09 (: with the built `oidc` module the
// daemon's login page is the module's `render_page(login)`, a page that
// sends the browser to the module's own login)
#[tokio::test(flavor = "multi_thread")]
async fn login_page_is_modules_page_when_oidc_installed() {
    let world = World::new().await;
    let provider = provider().await;
    let app = app(&world, &provider).await;
    let (status, body) = call(&app, "GET", "/login?host=api.apps.test&rd=%2F", None, None).await;
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(
        (
            status,
            body.contains("url=/api/v1/m/oidc/login"),
            body.contains("name=\"link\"")
        ),
        (StatusCode::OK, true, false),
        "{body}"
    );
}
