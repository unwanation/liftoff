#![cfg(test)]
//! The session handover of a one-time link to the target's host (6 amendment, 11): `OTL-07` end to end over loopback, and the pages the function
//! listener renders for a browser.
//!
//! The REST API and the function listener serve the world's fakes on
//! ephemeral `127.0.0.1` ports; the browser is hyper, asking the API as
//! itself and the listener as Caddy does (`Host` of the address). The
//! link's `GET /api/v1/auth/links/{token}` sets the API's cookie and
//! sends the browser to `https://<host>/.liftoff/callback?code=…`; the
//! listener of that host redeems the gate's one-time code for the host's
//! session `__Host-liftoff_gate` (the gate's handover, v1 SSO) and
//! returns to the path the code carries. The function is a fake that
//! counts what it is handed. It lives with the composition root because
//! it joins two adapters (`liftoff-rest` and the daemon's listener).

use core::time::Duration;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::TokioIo;
use liftoff_core::domain::{
    EnvState, Environment, EnvironmentId, LinkTarget, UserRole, Visibility,
};
use liftoff_core::fakes::World;
use liftoff_core::ports::Store as _;
use liftoff_rest::{AppState, RestConfig, Services, serve_app_on};
use liftoff_wasm::functions::FunctionError;
use liftoffd::listeners::functions::{FunctionListener, Functions};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// The address of the function `api`.
const HOST: &str = "api.apps.test";

/// The API host of the world.
const API_HOST: &str = "liftoff.test";

/// What a browser sends.
const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml,*/*;q=0.8";

/// The cookie of the session on the API host.
const API_COOKIE: &str = "__Host-liftoff_session";

/// The cookie of the session on the target's host.
const HOST_COOKIE: &str = "__Host-liftoff_gate";

/// How long one exchange may take before the test fails instead of
/// hanging.
const EXCHANGE: Duration = Duration::from_secs(10);

/// The fake function: `200 hello`, counting its calls.
#[derive(Default)]
struct CountingFunctions {
    calls: AtomicUsize,
}

impl Functions for CountingFunctions {
    fn invoke(
        &self,
        _environment: EnvironmentId,
        _request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<Bytes>, FunctionError>> + Send {
        let _before = self.calls.fetch_add(1, Ordering::SeqCst);
        async { Ok(Response::new(Bytes::from_static(b"hello"))) }
    }
}

/// The API and the function listener over a world with the running,
/// private function `api` of `shop` at [`HOST`].
struct Daemon {
    world: World,
    api: Environment,
    functions: Arc<CountingFunctions>,
    api_addr: SocketAddr,
    listener: SocketAddr,
    _stop: Vec<oneshot::Sender<()>>,
}

impl Daemon {
    async fn start() -> Self {
        let world = World::new().await;
        let api = Environment {
            state: EnvState::Running,
            visibility: Visibility::Private,
            ..world.records.function(&world.shop, "api").unwrap()
        };
        world.ports.store.insert_environment(&api).await.unwrap();
        world.add_address(&world.shop, &api, HOST).await.unwrap();

        let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_addr = rest.local_addr().unwrap();
        let (stop_api, api_stopped) = oneshot::channel::<()>();
        tokio::spawn(serve_app_on(
            rest,
            AppState::new(),
            Services::new(Arc::clone(&world.ports), Arc::clone(&world.settings)),
            RestConfig::default(),
            async {
                let _either: Result<(), oneshot::error::RecvError> = api_stopped.await;
            },
        ));

        let functions = Arc::new(CountingFunctions::default());
        let served = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener = served.local_addr().unwrap();
        let (stop_listener, listener_stopped) = oneshot::channel::<()>();
        tokio::spawn(
            FunctionListener::new(
                Arc::clone(&world.ports),
                Arc::clone(&world.settings),
                Arc::clone(&functions),
            )
            .serve(served, async {
                let _either: Result<(), oneshot::error::RecvError> = listener_stopped.await;
            }),
        );
        Self {
            world,
            api,
            functions,
            api_addr,
            listener,
            _stop: vec![stop_api, stop_listener],
        }
    }

    /// A fresh one-time link of `owner` to `api`: its path on the API.
    async fn link_path(&self) -> String {
        let url = self
            .world
            .links()
            .issue(&self.world.owner, &LinkTarget::Environment(self.api.id))
            .await
            .unwrap()
            .url;
        url.strip_prefix(&format!("https://{API_HOST}"))
            .unwrap_or(&url)
            .to_owned()
    }

    /// `GET path` on the API with `headers`.
    async fn api_get(&self, path: &str, headers: &[(&str, &str)]) -> Response<Bytes> {
        send(self.api_addr, get(API_HOST, path, headers)).await
    }

    /// `GET path` on [`HOST`] through the function listener with
    /// `headers`.
    async fn host_get(&self, path: &str, headers: &[(&str, &str)]) -> Response<Bytes> {
        send(self.listener, get(HOST, path, headers)).await
    }

    /// The link opened on the API: the path and query of the callback on
    /// [`HOST`] it sends the browser to, and the API's cookie.
    async fn open_link(&self) -> (String, String) {
        let path = self.link_path().await;
        let response = self.api_get(&path, &[("accept", BROWSER_ACCEPT)]).await;
        let callback = on_host(&location(&response));
        let cookie = set_cookie(&response, API_COOKIE)
            .map(|(value, _attributes)| format!("{API_COOKIE}={value}"))
            .unwrap_or_default();
        (callback, cookie)
    }

    /// How many requests the function was handed.
    fn calls(&self) -> usize {
        self.functions.calls.load(Ordering::SeqCst)
    }
}

/// `request` over a fresh HTTP/1.1 connection to `addr`, the response
/// with its body read.
async fn send(addr: SocketAddr, request: Request<Full<Bytes>>) -> Response<Bytes> {
    tokio::time::timeout(EXCHANGE, async {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(connection);
        let response = sender.send_request(request).await.unwrap();
        let (head, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        Response::from_parts(head, body)
    })
    .await
    .expect("the daemon answers in time")
}

/// `GET path` on `host` with `headers`; an empty `path` (no `Location`
/// to follow) asks for `/`.
fn get(host: &str, path: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(if path.is_empty() { "/" } else { path })
        .header("host", host);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Full::new(Bytes::new())).unwrap()
}

/// The values of the header `name` of `response`.
fn header_values(response: &Response<Bytes>, name: &str) -> Vec<String> {
    response
        .headers()
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok().map(ToOwned::to_owned))
        .collect()
}

/// The `Location` of `response`; empty without one.
fn location(response: &Response<Bytes>) -> String {
    header_values(response, "location")
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The path and query of `url` on [`HOST`]; `url` itself when it is not
/// on it.
fn on_host(url: &str) -> String {
    url.strip_prefix(&format!("https://{HOST}"))
        .unwrap_or(url)
        .to_owned()
}

/// The path and query of `url` on the API host; `url` itself when it is
/// not on it.
fn on_api(url: &str) -> String {
    url.strip_prefix(&format!("https://{API_HOST}"))
        .unwrap_or(url)
        .to_owned()
}

/// The `Set-Cookie` of `response` that sets `name`: its value and its
/// attributes, sorted.
fn set_cookie(response: &Response<Bytes>, name: &str) -> Option<(String, Vec<String>)> {
    header_values(response, "set-cookie")
        .into_iter()
        .find_map(|line| {
            let mut parts = line.split(';').map(str::trim);
            let value = parts.next()?.strip_prefix(name)?.strip_prefix('=')?;
            let mut attributes: Vec<String> = parts.map(ToOwned::to_owned).collect();
            attributes.sort();
            Some((value.to_owned(), attributes))
        })
}

mod handover {
    use super::*;

    // TC: OTL-07 (the listener of the target's host redeems the code for
    // the host's own session: `__Host-liftoff_gate`, `lfg_…`, for as long
    // as the `browser` session lives; the function never runs)
    #[tokio::test]
    async fn sets_host_cookie_without_invoking_when_callback_has_code_of_link() {
        let daemon = Daemon::start().await;
        let (callback, _api_cookie) = daemon.open_link().await;
        let response = daemon
            .host_get(&callback, &[("accept", BROWSER_ACCEPT)])
            .await;
        let cookie = set_cookie(&response, HOST_COOKIE);
        assert_eq!(
            (
                response.status(),
                cookie
                    .as_ref()
                    .is_some_and(|(value, _attributes)| value.starts_with("lfg_")),
                cookie.map(|(_value, attributes)| attributes),
                daemon.calls()
            ),
            (
                StatusCode::FOUND,
                true,
                Some(vec![
                    "HttpOnly".to_owned(),
                    "Max-Age=86400".to_owned(),
                    "Path=/".to_owned(),
                    "SameSite=Lax".to_owned(),
                    "Secure".to_owned(),
                ]),
                0
            ),
            "{callback} -> {:?}",
            response.headers()
        );
    }

    // TC: OTL-07 (a link returns to the root of its target)
    #[tokio::test]
    async fn returns_to_root_when_callback_has_code_of_link() {
        let daemon = Daemon::start().await;
        let (callback, _api_cookie) = daemon.open_link().await;
        let response = daemon
            .host_get(&callback, &[("accept", BROWSER_ACCEPT)])
            .await;
        assert_eq!(
            (response.status(), location(&response)),
            (StatusCode::FOUND, "/".to_owned()),
            "{callback}"
        );
    }

    // TC: OTL-07 (with the host's cookie the browser is let through)
    #[tokio::test]
    async fn function_answers_when_browser_returns_with_host_cookie() {
        let daemon = Daemon::start().await;
        let (callback, _api_cookie) = daemon.open_link().await;
        let entered = daemon
            .host_get(&callback, &[("accept", BROWSER_ACCEPT)])
            .await;
        let cookie = set_cookie(&entered, HOST_COOKIE)
            .map(|(value, _attributes)| format!("{HOST_COOKIE}={value}"))
            .unwrap_or_default();
        let response = daemon
            .host_get("/", &[("accept", BROWSER_ACCEPT), ("cookie", &cookie)])
            .await;
        assert_eq!(
            (response.status(), response.body().as_ref(), daemon.calls()),
            (StatusCode::OK, b"hello".as_slice(), 1),
            "{callback} -> {:?}",
            entered.headers()
        );
    }

    // TC: OTL-07 (the code is one-time: the second callback sets nothing)
    #[tokio::test]
    async fn stale_without_cookie_when_callback_code_used_twice() {
        let daemon = Daemon::start().await;
        let (callback, _api_cookie) = daemon.open_link().await;
        let _first = daemon
            .host_get(&callback, &[("accept", BROWSER_ACCEPT)])
            .await;
        let response = daemon
            .host_get(&callback, &[("accept", BROWSER_ACCEPT)])
            .await;
        assert_eq!(
            (
                response.status(),
                header_values(&response, "set-cookie"),
                daemon.calls()
            ),
            (StatusCode::BAD_REQUEST, Vec::<String>::new(), 0),
            "{callback}"
        );
    }

    // TC: OTL-07 (the browser that has the API's session from a link
    // comes through the listener's login redirect and `/gate/start` to
    // the path it asked for)
    #[tokio::test]
    async fn enters_asked_path_through_gate_start_when_browser_has_api_session() {
        let daemon = Daemon::start().await;
        let (_callback, api_cookie) = daemon.open_link().await;
        let login = daemon
            .host_get("/items?x=1", &[("accept", BROWSER_ACCEPT)])
            .await;
        let start = daemon
            .api_get(
                &on_api(&location(&login)),
                &[("accept", BROWSER_ACCEPT), ("cookie", &api_cookie)],
            )
            .await;
        let callback = on_host(&location(&start));
        let entered = daemon
            .host_get(&callback, &[("accept", BROWSER_ACCEPT)])
            .await;
        assert_eq!(
            (
                entered.status(),
                location(&entered),
                set_cookie(&entered, HOST_COOKIE).is_some()
            ),
            (StatusCode::FOUND, "/items?x=1".to_owned(), true),
            "{} -> {} -> {callback}",
            location(&login),
            location(&start)
        );
    }

    // TC: OTL-07 (a link pasted on the login page of the address returns
    // to the path the browser came from)
    #[tokio::test]
    async fn returns_to_path_of_login_page_when_link_pasted() {
        let daemon = Daemon::start().await;
        let path = daemon.link_path().await;
        let link = format!("https://{API_HOST}{path}");
        let encoded: String = link
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                    char::from(byte).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect();
        let pasted = daemon
            .api_get(
                &format!("/login?link={encoded}&host={HOST}&rd=%2Fitems"),
                &[("accept", BROWSER_ACCEPT)],
            )
            .await;
        let callback = on_host(&location(&pasted));
        let entered = daemon
            .host_get(&callback, &[("accept", BROWSER_ACCEPT)])
            .await;
        assert_eq!(
            (entered.status(), location(&entered)),
            (StatusCode::FOUND, "/items".to_owned()),
            "{callback}"
        );
    }

    // TC: OTL-07 (`/.liftoff/logout` of the address ends its session)
    #[tokio::test]
    async fn clears_host_cookie_and_session_when_logout() {
        let daemon = Daemon::start().await;
        let (callback, _api_cookie) = daemon.open_link().await;
        let entered = daemon
            .host_get(&callback, &[("accept", BROWSER_ACCEPT)])
            .await;
        let cookie = set_cookie(&entered, HOST_COOKIE)
            .map(|(value, _attributes)| format!("{HOST_COOKIE}={value}"))
            .unwrap_or_default();
        let logout = daemon
            .host_get("/.liftoff/logout", &[("cookie", &cookie)])
            .await;
        let after = daemon
            .host_get("/", &[("accept", BROWSER_ACCEPT), ("cookie", &cookie)])
            .await;
        assert_eq!(
            (
                logout.status(),
                set_cookie(&logout, HOST_COOKIE).map(|(value, _attributes)| value),
                after.status(),
                daemon.calls()
            ),
            (StatusCode::OK, Some(String::new()), StatusCode::FOUND, 0),
            "{:?}",
            logout.headers()
        );
    }
}

mod pages {
    use super::*;

    /// Whether `response` is an HTML page of the listener's with the v1
    /// headers (the pages' CSP, which allows their stylesheet) and no
    /// script.
    fn is_page(response: &Response<Bytes>) -> bool {
        let body = String::from_utf8_lossy(response.body()).to_ascii_lowercase();
        header_values(response, "content-type")
            .first()
            .is_some_and(|value| value.starts_with("text/html"))
            && header_values(response, "x-content-type-options") == ["nosniff"]
            && header_values(response, "cache-control") == ["no-store"]
            && header_values(response, "content-security-policy") == [liftoff_rest::pages::CSP]
            && !body.contains("<script")
    }

    // TC: OTL-08 (a browser refused on an address gets the forbidden page, not a plain text)
    #[tokio::test]
    async fn forbidden_page_when_browser_session_is_strangers() {
        let daemon = Daemon::start().await;
        let bob = daemon.world.add_user("bob", UserRole::User).await.unwrap();
        let secret = daemon.world.host_login(&bob, HOST).await.unwrap();
        let cookie = format!("{HOST_COOKIE}={}", secret.expose());
        let response = daemon
            .host_get("/", &[("accept", BROWSER_ACCEPT), ("cookie", &cookie)])
            .await;
        assert_eq!(
            (response.status(), is_page(&response), daemon.calls()),
            (StatusCode::FORBIDDEN, true, 0),
            "{}",
            String::from_utf8_lossy(response.body())
        );
    }
}
