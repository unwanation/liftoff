//! `oidc`: the module that logs people in through an `OpenID Connect`
//! provider, carried over from
//! v1's Zitadel device flow and ID-token check.
//!
//! Its routes below `/api/v1/m/oidc`, all public (nobody is logged in
//! yet):
//!
//! | Route | Request | Answers |
//! |---|---|---|
//! | `POST /device` | — | `200` `{"device_code","user_code","verification_uri_complete","interval","expires_in"}`; `502` when the provider fails |
//! | `POST /device/poll` | `{"device_code","name"}` | `202` `{"status":"pending"}`; `200` `{"token","user"}`; `403` denied or an invalid ID token; `410` expired; `502` provider failure |
//! | `GET /login` | `?rd` (optional) | `302` to the provider (code flow with the web client when one is configured, `state`, `nonce`, PKCE `S256`; `redirect_uri` is the configured one or that of the API host, never derived from the request's headers) and the cookie `__Host-liftoff_login_<8 characters of state>=<state>` (10 minutes) that binds the login to this browser; `rd` is kept with the login |
//! | `GET /callback` | `?code&state` | `200` `{"token","user"}` (a `browser` session); for a login started with `rd`, `302` to `rd` with the session as the cookie `__Host-liftoff_session` instead (the gate's login; v1 SSO-05); `400` for a `state` it did not issue, that expired, or whose login cookie this browser does not hold (login CSRF; the core shows the cookie values in `x-liftoff-login-state`); `403` denied or an invalid ID token |
//!
//! `rd` is a path on the API host: one that does not start with `/`, or
//! starts with `//` or `/\`, becomes `/` (no open redirect, v1 SSO-04).
//!
//! Errors are `application/problem+json` with a `detail`, as the core's.
//! A finished login is handed to the core with `core-call
//! auth.create_session`: the core creates or refreshes the user of
//! `(oidc:<issuer>, sub)` and keeps the session (the manifest declares
//! `replaces: ["login"]`, which that call requires, and the core takes
//! only the `issuer` its admin configured). The role is the
//! module's decision: `admin` when Zitadel's project roles claim holds
//! `admin_role`. Its state lives in its key space (no tables): the JWKS
//! and the discovery document, cached (a token signed by a key the cache
//! does not hold reads the JWKS again once), and the started browser
//! logins (one key per slot). Its hooks: `authenticate` has no opinion
//! (sessions are the core's), `render_page(login)` sends the browser to
//! `GET /login`, and `verify_token(token, audiences)` (`POST /.hook/verify_token`) checks an OAuth access
//! token of `/mcp` with the ID token's code against the JWKS, for one of
//! the `audiences` the `mcp` module allows, and names its person.
//!
//! `wasi:http` reaches the issuer's host only (`http_allow` is derived
//! from `issuer`): a provider whose endpoints live on another host is
//! refused.

#[expect(
    clippy::error_impl_error,
    clippy::mem_forget,
    clippy::same_length_and_capacity,
    reason = "code wit-bindgen generates"
)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "liftoff:module/liftoff-module",
        generate_all,
    });
}

mod config;
mod http;
mod jwt;
mod provider;

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error, Level};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::config::Config;
use crate::jwt::Claims;
use crate::provider::{Poll, Provider, Upstream};

/// The component's export.
struct Oidc;

/// The prefix of the module's routes as a browser sees them.
pub(crate) const PREFIX: &str = "/api/v1/m/oidc";

/// How long a started browser login waits for the provider (v1 gate).
const LOGIN_TTL_SECS: u64 = 10 * 60;

/// The session name of a browser login.
const BROWSER_SESSION: &str = "browser";

/// `POST /device/poll`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PollBody {
    device_code: String,
    #[serde(default)]
    name: Option<String>,
}

/// The prefix of the keys of started browser logins ([`Logins`]).
const LOGIN_KEY_PREFIX: &str = "login:";

/// How many hex digits of a `state`'s hash name its slot: `16^3 = 4096`
/// slots, so at most 4096 started logins are kept.
const LOGIN_SLOT_DIGITS: usize = 3;

/// A started browser login.
#[derive(Deserialize, Serialize)]
struct StartedLogin {
    /// The SHA-256 of its `state` (hex): the slot holds one login, and
    /// a callback takes it only with that `state`.
    state_hash: String,
    nonce: String,
    verifier: String,
    redirect_uri: Option<String>,
    /// Where the browser goes back to once logged in (the gate's login).
    #[serde(default)]
    rd: Option<String>,
    expires_at: u64,
}

/// The cookie of the browser session on the API host, which the gate
/// reads (`liftoff_core::service::gate_listener`).
const SESSION_COOKIE: &str = "__Host-liftoff_session";

/// The prefix of the cookie that binds a started login to its browser
/// (`ModuleRequest::LOGIN_STATE_COOKIE_PREFIX` of the core, which shows
/// the module their values in [`LOGIN_STATE_HEADER`]; v1 SSO-05).
const LOGIN_COOKIE_PREFIX: &str = "__Host-liftoff_login_";

/// The header the core shows the values of the login cookies in.
const LOGIN_STATE_HEADER: &str = "x-liftoff-login-state";

/// How many characters of a `state` name its login cookie, so that two
/// logins in one browser keep two cookies.
const LOGIN_COOKIE_NAME_CHARS: usize = 8;

/// The name of the login cookie of `state`.
fn login_cookie(state: &str) -> String {
    let short = state.get(..LOGIN_COOKIE_NAME_CHARS).unwrap_or(state);
    format!("{LOGIN_COOKIE_PREFIX}{short}")
}

/// Whether the browser of `req` started the login of `state`: its login
/// cookies, as the core shows them, hold `state` (login CSRF, v1 SSO-05).
fn started_here(req: &Request, state: &str) -> bool {
    req.headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(LOGIN_STATE_HEADER))
        .flat_map(|(_, value)| value.split(','))
        .any(|value| value.trim() == state)
}

/// How long the browser session cookie lives: the core's `browser`
/// session, 24 hours.
const SESSION_COOKIE_SECS: u64 = 24 * 60 * 60;

/// The longest `rd` kept.
const MAX_RD_BYTES: usize = 2048;

/// `rd` when it is a path on this host, else `/`.
fn safe_rd(rd: &str) -> String {
    let path = rd.starts_with('/')
        && !rd.starts_with("//")
        && !rd.starts_with("/\\")
        && rd.len() <= MAX_RD_BYTES
        && !rd.chars().any(char::is_control);
    if path { rd.to_owned() } else { "/".to_owned() }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn log(level: Level, message: &str) {
    host::log(level, message);
}

fn response(status: u16, content_type: &str, body: Vec<u8>) -> Response {
    Response {
        status,
        headers: vec![
            ("content-type".to_owned(), content_type.to_owned()),
            ("cache-control".to_owned(), "no-store".to_owned()),
        ],
        body,
    }
}

fn json_response(status: u16, body: &Value) -> Response {
    response(status, "application/json", body.to_string().into_bytes())
}

/// A problem document (RFC 9457) with `detail`.
fn problem(status: u16, title: &str, detail: &str) -> Response {
    response(
        status,
        "application/problem+json",
        json!({"type": "about:blank", "title": title, "status": status, "detail": detail})
            .to_string()
            .into_bytes(),
    )
}

/// The provider's failure: logged, `502` for the caller.
fn bad_gateway(Upstream(why): &Upstream) -> Response {
    log(Level::Warn, &format!("the identity provider failed: {why}"));
    problem(502, "Bad Gateway", "the identity provider failed")
}

/// A refused ID token: logged with the reason, `403` without it.
fn invalid_token(why: &str) -> Response {
    log(Level::Warn, &format!("rejected an ID token: {why}"));
    problem(403, "Forbidden", "invalid ID token")
}

/// 32 random bytes in base64url (`state`, `nonce`, PKCE verifier).
fn random_token() -> Result<String, Response> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        log(Level::Error, &format!("no randomness: {error}"));
        problem(500, "Internal Server Error", "internal error")
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn sha256_hex(raw: &str) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(raw.as_bytes()) {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

/// Hands a finished login to the core: the user of the claims and a new
/// session of `kind` named `name`; the core's answer `{"token","user"}`.
fn open_session(config: &Config, claims: &Claims, name: Option<&str>, kind: &str) -> Response {
    let role = if config.is_admin(&claims.roles) {
        "admin"
    } else {
        "user"
    };
    let args = json!({
        "issuer": config.issuer,
        "subject": claims.subject,
        "login": claims.login,
        "email": claims.email,
        "role": role,
        "name": name,
        "kind": kind,
    });
    match host::core_call("auth", "create_session", &args.to_string()) {
        Ok(granted) => response(200, "application/json", granted.into_bytes()),
        Err(error) => {
            log(
                Level::Warn,
                &format!(
                    "the core refused the session ({}): {}",
                    error.code, error.message
                ),
            );
            match error.code.as_str() {
                "validation" => problem(
                    403,
                    "Forbidden",
                    &format!("the account cannot log in: {}", error.message),
                ),
                "conflict" => problem(409, "Conflict", &error.message),
                "forbidden" => problem(403, "Forbidden", &error.message),
                _ => problem(500, "Internal Server Error", "internal error"),
            }
        }
    }
}

/// `POST /device`.
fn device_start(config: &Config) -> Response {
    match Provider::new(config, now()).start_device() {
        Ok(device) => json_response(200, &json!(device)),
        Err(failure) => bad_gateway(&failure),
    }
}

/// `POST /device/poll`.
fn device_poll(config: &Config, body: &[u8]) -> Response {
    let Ok(PollBody { device_code, name }) = serde_json::from_slice(body) else {
        return problem(
            400,
            "Bad Request",
            "the body must be {\"device_code\", \"name\"}",
        );
    };
    let provider = Provider::new(config, now());
    match provider.poll_device(&device_code) {
        Ok(Poll::Pending) => json_response(202, &json!({"status": "pending"})),
        Ok(Poll::Denied) => problem(403, "Forbidden", "login was denied"),
        Ok(Poll::Expired) => problem(410, "Gone", "device code expired"),
        Ok(Poll::Done(id_token)) => match provider.verify(&id_token, None) {
            Ok(Ok(claims)) => open_session(config, &claims, name.as_deref(), "cli"),
            Ok(Err(why)) => invalid_token(&why),
            Err(failure) => bad_gateway(&failure),
        },
        Err(failure) => bad_gateway(&failure),
    }
}

/// `GET /login?rd`: a new login kept by the hash of its `state`, and the
/// redirect to the provider.
fn login_start(config: &Config, query: &str) -> Response {
    let config = &config.for_browser();
    let provider = Provider::new(config, now());
    let discovery = match provider.discovery() {
        Ok(discovery) => discovery,
        Err(failure) => return bad_gateway(&failure),
    };
    let (state, nonce, verifier) = match (random_token(), random_token(), random_token()) {
        (Ok(state), Ok(nonce), Ok(verifier)) => (state, nonce, verifier),
        (Err(failed), _, _) | (_, Err(failed), _) | (_, _, Err(failed)) => return failed,
    };
    let redirect = config.redirect_uri.clone();
    Logins::insert(&StartedLogin {
        state_hash: sha256_hex(&state),
        nonce: nonce.clone(),
        verifier: verifier.clone(),
        redirect_uri: redirect.clone(),
        rd: http::query_value(query, "rd").map(|rd| safe_rd(&rd)),
        expires_at: now().saturating_add(LOGIN_TTL_SECS),
    });
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let scope = config.scope();
    let mut fields = vec![
        ("response_type", "code"),
        ("client_id", config.client_id.as_str()),
        ("scope", scope.as_str()),
        ("state", state.as_str()),
        ("nonce", nonce.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];
    if let Some(redirect) = &redirect {
        fields.push(("redirect_uri", redirect.as_str()));
    }
    let separator = if discovery.authorization_endpoint.contains('?') {
        '&'
    } else {
        '?'
    };
    let location = format!(
        "{}{separator}{}",
        discovery.authorization_endpoint,
        http::form(&fields)
    );
    Response {
        status: 302,
        headers: vec![
            ("location".to_owned(), location),
            (
                "set-cookie".to_owned(),
                format!(
                    "{}={state}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={LOGIN_TTL_SECS}",
                    login_cookie(&state)
                ),
            ),
            ("cache-control".to_owned(), "no-store".to_owned()),
        ],
        body: Vec::new(),
    }
}

/// The started browser logins, one per key `login:<slot>`, the slot
/// being the first [`LOGIN_SLOT_DIGITS`] hex digits of the SHA-256 of
/// its `state`: starting or finishing a login reads and writes one small
/// value, and the public `GET /login` cannot grow the store past 4096
/// keys. A login whose slot a later one takes is lost (its callback gets
/// `400`, start again); an empty value is an empty slot.
///
/// The module's calls run one at a time (the host holds one instance
/// behind a lock), so taking a login cannot race another callback of the
/// same `state`.
struct Logins;

impl Logins {
    /// The key of the slot of `state_hash`.
    fn key(state_hash: &str) -> String {
        let slot = state_hash.get(..LOGIN_SLOT_DIGITS).unwrap_or(state_hash);
        format!("{LOGIN_KEY_PREFIX}{slot}")
    }

    /// Keeps `login` in its slot.
    fn insert(login: &StartedLogin) {
        if let Ok(bytes) = serde_json::to_vec(login) {
            host::store_set(&Self::key(&login.state_hash), &bytes);
        }
    }

    /// Takes the login of `state` out of its slot: `None` for one the
    /// module did not issue, already used, expired or pushed out.
    fn take(state: &str) -> Option<StartedLogin> {
        let state_hash = sha256_hex(state);
        let key = Self::key(&state_hash);
        let started = host::store_get(&key)
            .and_then(|bytes| serde_json::from_slice::<StartedLogin>(&bytes).ok())
            .filter(|login| login.state_hash == state_hash)?;
        host::store_set(&key, &[]);
        (started.expires_at > now()).then_some(started)
    }
}

/// `GET /callback?code&state`: only in the browser that started the
/// login (its login cookie holds `state`), whose cookie it clears.
fn login_finish(config: &Config, req: &Request, query: &str) -> Response {
    let finished = finish(&config.for_browser(), req, query);
    match http::query_value(query, "state").filter(|state| !state.is_empty()) {
        Some(state) if started_here(req, &state) => cleared(finished, &state),
        _ => finished,
    }
}

/// `finished` with the login cookie of `state` cleared.
fn cleared(mut finished: Response, state: &str) -> Response {
    finished.headers.push((
        "set-cookie".to_owned(),
        format!(
            "{}=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0",
            login_cookie(state)
        ),
    ));
    finished
}

/// The callback of [`login_finish`].
fn finish(config: &Config, req: &Request, query: &str) -> Response {
    let unknown = || problem(400, "Bad Request", "unknown or expired login; start again");
    let Some(state) = http::query_value(query, "state").filter(|state| !state.is_empty()) else {
        return unknown();
    };
    if !started_here(req, &state) {
        log(
            Level::Warn,
            "refused a login callback in a browser that did not start the login",
        );
        return problem(
            400,
            "Bad Request",
            "this login was started in another browser; start again",
        );
    }
    let Some(started) = Logins::take(&state) else {
        return unknown();
    };
    if http::query_value(query, "error").is_some() {
        return problem(403, "Forbidden", "login was denied");
    }
    let Some(code) = http::query_value(query, "code").filter(|code| !code.is_empty()) else {
        return problem(400, "Bad Request", "the callback has no code");
    };
    let provider = Provider::new(config, now());
    let id_token =
        match provider.exchange_code(&code, &started.verifier, started.redirect_uri.as_deref()) {
            Ok(id_token) => id_token,
            Err(failure) => return bad_gateway(&failure),
        };
    match provider.verify(&id_token, Some(&started.nonce)) {
        Ok(Ok(claims)) => {
            let opened = open_session(config, &claims, Some(BROWSER_SESSION), BROWSER_SESSION);
            match started.rd {
                Some(rd) if opened.status == 200 => back_to(&rd, &opened),
                _ => opened,
            }
        }
        Ok(Err(why)) => invalid_token(&why),
        Err(failure) => bad_gateway(&failure),
    }
}

/// The browser login of the gate, done: `302` to `rd` with the session
/// of `opened` (the core's `{"token","user"}`) as the `__Host-` cookie
/// of the API host.
fn back_to(rd: &str, opened: &Response) -> Response {
    #[derive(Deserialize)]
    struct Granted {
        token: String,
    }
    let Ok(Granted { token }) = serde_json::from_slice::<Granted>(&opened.body) else {
        log(Level::Error, "the core's session answer has no token");
        return problem(500, "Internal Server Error", "internal error");
    };
    Response {
        status: 302,
        headers: vec![
            ("location".to_owned(), safe_rd(rd)),
            (
                "set-cookie".to_owned(),
                format!(
                    "{SESSION_COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={SESSION_COOKIE_SECS}"
                ),
            ),
            ("cache-control".to_owned(), "no-store".to_owned()),
        ],
        body: Vec::new(),
    }
}

/// `POST /.hook/verify_token`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyBody {
    token: String,
    audiences: Vec<String>,
}

/// `verify_token(token, audiences)`: the access
/// token of `/mcp` checked as an ID token is (signature against the
/// JWKS, `iss`, expiry) but for one of the resource's `audiences`;
/// `200` `{"issuer","subject","login","email","role","expires_at"}`,
/// the role read from the roles claim as a login does; `401` with the
/// reason (the core logs it, the client never sees it); `502` when the
/// provider fails.
fn verify_token(config: &Config, body: &[u8]) -> Response {
    let Ok(VerifyBody { token, audiences }) = serde_json::from_slice(body) else {
        return problem(
            400,
            "Bad Request",
            "the body must be {\"token\", \"audiences\"}",
        );
    };
    if audiences.is_empty() {
        return problem(401, "Unauthorized", "no audience is allowed");
    }
    match Provider::new(config, now()).verify_access(&token, &audiences) {
        Ok(Ok(claims)) => {
            let role = if config.is_admin(&claims.roles) {
                "admin"
            } else {
                "user"
            };
            json_response(
                200,
                &json!({
                    "issuer": config.issuer,
                    "subject": claims.subject,
                    "login": claims.login,
                    "email": claims.email,
                    "role": role,
                    "expires_at": claims.expires_at,
                }),
            )
        }
        Ok(Err(why)) => problem(401, "Unauthorized", &format!("invalid access token: {why}")),
        Err(failure) => bad_gateway(&failure),
    }
}

/// `render_page(login)`: a page that sends the browser to `GET /login`.
fn login_page() -> Response {
    let target = format!("{PREFIX}/login");
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"refresh\" content=\"0; url={target}\">\
         <title>Sign in to liftoff</title></head>\
         <body><p><a href=\"{target}\">Sign in</a></p></body></html>"
    );
    response(200, "text/html; charset=utf-8", html.into_bytes())
}

impl Guest for Oidc {
    fn manifest() -> String {
        let http_allow: Vec<String> = host::config_get("issuer")
            .and_then(|issuer| config::issuer_authority(&issuer))
            .into_iter()
            .collect();
        let route =
            |method: &str, path: &str| json!({"method": method, "path": path, "public": true});
        json!({
            "name": "oidc",
            "version": env!("CARGO_PKG_VERSION"),
            "config_schema": config::schema(),
            "http_allow": http_allow,
            "routes": [
                route("POST", "/device"),
                route("POST", "/device/poll"),
                route("GET", "/login"),
                route("GET", "/callback"),
            ],
            "hooks": ["authenticate", "render_page", "verify_token"],
            "replaces": ["login"],
        })
        .to_string()
    }

    fn migrate(_from: u32) -> Result<(), Error> {
        // No tables: everything lives in the key space.
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        let (path, query) = req.path.split_once('?').unwrap_or((req.path.as_str(), ""));
        match (req.method.as_str(), path) {
            // Sessions are the core's: no opinion on any bearer.
            ("POST", "/.hook/authenticate") => response(204, "text/plain", Vec::new()),
            ("GET", "/.hook/render_page/login") => login_page(),
            _ => {
                let Some(config) = Config::read() else {
                    return problem(
                        503,
                        "Service Unavailable",
                        "the oidc module has no issuer or client_id configured",
                    );
                };
                match (req.method.as_str(), path) {
                    ("POST", "/device") => device_start(&config),
                    ("POST", "/device/poll") => device_poll(&config, &req.body),
                    ("GET", "/login") => login_start(&config, query),
                    ("GET", "/callback") => login_finish(&config, &req, query),
                    ("POST", "/.hook/verify_token") => verify_token(&config, &req.body),
                    _ => problem(404, "Not Found", "no such route"),
                }
            }
        }
    }

    fn handle_tool(name: String, _args: String) -> Result<String, Error> {
        Err(Error {
            code: "not_found".to_owned(),
            message: format!("tool {name}"),
        })
    }

    fn on_hook(_hook: String, _payload: String) -> Result<HookResult, Error> {
        Ok(HookResult::Continue)
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`Oidc`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Oidc;

    crate::bindings::export!(Oidc with_types_in crate::bindings);
}
