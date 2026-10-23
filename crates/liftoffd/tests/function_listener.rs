#![cfg(test)]
//! The function listener:
//! `FN-11…13` over loopback, the listener of
//! [`liftoffd::listeners::functions`] on an ephemeral `127.0.0.1` port,
//! asked with hyper as Caddy asks it, over the world's store (the
//! function `api` of `shop` at `api.apps.test`) and a hand-written fake
//! of the pool that records what it is handed.

use core::time::Duration;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper_util::rt::TokioIo;
use liftoff_core::domain::{EnvState, Environment, EnvironmentId, UserRole, Visibility};
use liftoff_core::fakes::World;
use liftoff_core::ports::Store as _;
use liftoff_wasm::functions::FunctionError;
use liftoffd::listeners::functions::{FunctionListener, Functions, ListenerLimits};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};

/// The address of the function `api`.
const HOST: &str = "api.apps.test";

/// What a browser sends.
const BROWSER_ACCEPT: &str = "text/html,application/xhtml+xml,*/*;q=0.8";

/// How long one exchange with the listener may take before the test
/// fails instead of hanging.
const EXCHANGE: Duration = Duration::from_secs(10);

/// A request the fake pool was handed, with its body read.
type Handed = (EnvironmentId, Request<Bytes>);

/// The fake pool: answers the scripted responses in order (`200 hello`
/// once they run out) and records every request it is handed. Once
/// [`FakeFunctions::hold`] is called, every call waits for the returned
/// notification before it answers; `entered` is notified as a call
/// starts.
#[derive(Default)]
struct FakeFunctions {
    handed: Mutex<Vec<Handed>>,
    answers: Mutex<VecDeque<Result<Response<Bytes>, FunctionError>>>,
    held: Mutex<Option<Arc<Notify>>>,
    entered: Arc<Notify>,
}

impl FakeFunctions {
    /// Makes every call wait until the returned notification fires.
    fn hold(&self) -> Arc<Notify> {
        let release = Arc::new(Notify::new());
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&release));
        release
    }

    fn answer_next(&self, answer: Result<Response<Bytes>, FunctionError>) {
        self.answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(answer);
    }

    fn handed(&self) -> Vec<Handed> {
        self.handed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn calls(&self) -> usize {
        self.handed().len()
    }
}

impl Functions for FakeFunctions {
    fn invoke(
        &self,
        environment: EnvironmentId,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<Bytes>, FunctionError>> + Send {
        self.handed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((environment, request));
        let next = self
            .answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let held = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let entered = Arc::clone(&self.entered);
        async move {
            entered.notify_one();
            if let Some(release) = held {
                release.notified().await;
            }
            next.unwrap_or_else(|| Ok(Response::new(Bytes::from_static(b"hello"))))
        }
    }
}

/// The listener serving the world, and the function environment `api`
/// of `shop` (running, its address `api.apps.test`).
struct Served {
    world: World,
    api: Environment,
    functions: Arc<FakeFunctions>,
    addr: SocketAddr,
    _stop: oneshot::Sender<()>,
}

impl Served {
    /// The world with `api` of `visibility`, its listener serving.
    async fn start(visibility: Visibility) -> Self {
        Self::start_in(visibility, EnvState::Running).await
    }

    /// The world with `api` of `visibility` in `state`, its listener
    /// serving.
    async fn start_in(visibility: Visibility, state: EnvState) -> Self {
        Self::start_with(visibility, state, ListenerLimits::default()).await
    }

    /// The world with a running `api` of `visibility`, its listener
    /// serving with `limits`.
    async fn limited(visibility: Visibility, limits: ListenerLimits) -> Self {
        Self::start_with(visibility, EnvState::Running, limits).await
    }

    /// The world with `api` of `visibility` in `state`, its listener
    /// serving with `limits`.
    async fn start_with(visibility: Visibility, state: EnvState, limits: ListenerLimits) -> Self {
        let world = World::new().await;
        let api = Environment {
            state,
            visibility,
            ..world.records.function(&world.shop, "api").unwrap()
        };
        world.ports.store.insert_environment(&api).await.unwrap();
        world.add_address(&world.shop, &api, HOST).await.unwrap();
        let functions = Arc::new(FakeFunctions::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let served = FunctionListener::new(
            Arc::clone(&world.ports),
            Arc::clone(&world.settings),
            Arc::clone(&functions),
        )
        .with_limits(limits);
        tokio::spawn(served.serve(listener, async {
            let _either: Result<(), oneshot::error::RecvError> = stopped.await;
        }));
        Self {
            world,
            api,
            functions,
            addr,
            _stop: stop,
        }
    }

    /// A shared `api`.
    async fn shared() -> Self {
        Self::start(Visibility::Shared).await
    }

    /// A private `api`.
    async fn private() -> Self {
        Self::start(Visibility::Private).await
    }

    /// `request` over a fresh HTTP/1.1 connection, the response with its
    /// body read.
    async fn send(&self, request: Request<Full<Bytes>>) -> Response<Bytes> {
        tokio::time::timeout(EXCHANGE, exchange(self.addr, request))
            .await
            .expect("the listener answers in time")
    }
}

/// One exchange with the listener at `addr`.
async fn exchange(addr: SocketAddr, request: Request<Full<Bytes>>) -> Response<Bytes> {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let response = sender.send_request(request).await.unwrap();
    let (head, body) = response.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    Response::from_parts(head, body)
}

/// `GET /` on `host` with the headers `headers`.
fn get(host: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
    get_path(host, "/", headers)
}

/// `GET path` on `host` with the headers `headers`.
fn get_path(host: &str, path: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", host);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Full::new(Bytes::new())).unwrap()
}

/// The values of the header `name` of the request the pool was handed
/// first.
fn handed_header(functions: &FakeFunctions, name: &str) -> Vec<String> {
    functions
        .handed()
        .first()
        .map(|(_environment, request)| {
            request
                .headers()
                .get_all(name)
                .iter()
                .map(|value| value.to_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The values of the header `name` of `response`.
fn header_values(response: &Response<Bytes>, name: &str) -> Vec<String> {
    response
        .headers()
        .get_all(name)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

mod routing {
    use super::*;

    // TC: FN-11
    #[tokio::test]
    async fn invokes_environment_of_host_when_host_is_function_domain() {
        let served = Served::shared().await;
        let response = served.send(get(HOST, &[])).await;
        let environments: Vec<EnvironmentId> = served
            .functions
            .handed()
            .into_iter()
            .map(|(environment, _request)| environment)
            .collect();
        assert_eq!(
            (response.status(), environments, response.body().clone()),
            (
                StatusCode::OK,
                vec![served.api.id],
                Bytes::from_static(b"hello")
            )
        );
    }

    // TC: FN-11 (`Host` with a port names the same host)
    #[tokio::test]
    async fn invokes_environment_of_host_when_host_has_port() {
        let served = Served::shared().await;
        let response = served.send(get("api.apps.test:443", &[])).await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::OK, 1)
        );
    }

    // TC: FN-11 (unknown host)
    #[tokio::test]
    async fn not_found_without_invoking_when_host_is_unknown() {
        let served = Served::shared().await;
        let response = served.send(get("nobody.apps.test", &[])).await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::NOT_FOUND, 0)
        );
    }

    // TC: FN-11 (a domain of a compose environment is no function's)
    #[tokio::test]
    async fn not_found_without_invoking_when_host_is_compose_domain() {
        let served = Served::shared().await;
        let response = served.send(get("shop.apps.test", &[])).await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::NOT_FOUND, 0)
        );
    }

    // TC: FN-11 (a domain of the project bound to nothing)
    #[tokio::test]
    async fn not_found_without_invoking_when_host_is_unbound_domain() {
        let served = Served::shared().await;
        served
            .world
            .add_domain(&served.world.shop, "docs.apps.test")
            .await
            .unwrap();
        let response = served.send(get("docs.apps.test", &[])).await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::NOT_FOUND, 0)
        );
    }
}

mod access {
    use super::*;

    // TC: FN-12
    #[tokio::test]
    async fn redirects_to_login_without_invoking_when_private_and_no_cookie() {
        let served = Served::private().await;
        let response = served
            .send(get_path(HOST, "/items?x=1", &[("accept", BROWSER_ACCEPT)]))
            .await;
        assert_eq!(
            (
                response.status(),
                header_values(&response, "location"),
                served.functions.calls()
            ),
            (
                StatusCode::FOUND,
                vec![
                    "https://liftoff.test/gate/start?host=api.apps.test&rd=%2Fitems%3Fx%3D1"
                        .to_owned()
                ],
                0
            )
        );
    }

    // TC: FN-12 (a session of someone who may not open it)
    #[tokio::test]
    async fn forbidden_without_invoking_when_private_and_session_is_strangers() {
        let served = Served::private().await;
        let bob = served.world.add_user("bob", UserRole::User).await.unwrap();
        let secret = served.world.login(&bob).await.unwrap();
        let bearer = format!("Bearer {}", secret.expose());
        let response = served
            .send(get(
                HOST,
                &[("accept", "application/json"), ("authorization", &bearer)],
            ))
            .await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::FORBIDDEN, 0)
        );
    }

    // TC: FN-12 (`/.liftoff/*` is liftoff's on every function address:
    // a path of it the listener does not serve is `404` and reaches no
    // user code)
    #[tokio::test]
    async fn not_found_without_invoking_when_path_is_liftoffs() {
        let served = Served::shared().await;
        let response = served
            .send(get_path(HOST, "/.liftoff/other?code=lfc_secret", &[]))
            .await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::NOT_FOUND, 0)
        );
    }

    // TC: FN-12, OTL-07 (the listener serves the session handover's `/.liftoff/callback` itself; a code it does not know is stale, `400`, and the one-time code never reaches user code)
    #[tokio::test]
    async fn stale_without_invoking_when_callback_code_is_unknown() {
        let served = Served::shared().await;
        let response = served
            .send(get_path(HOST, "/.liftoff/callback?code=lfc_secret", &[]))
            .await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::BAD_REQUEST, 0)
        );
    }

    // TC: FN-12 (a JSON client gets 401 instead of the login)
    #[tokio::test]
    async fn unauthorized_without_invoking_when_private_and_json_client() {
        let served = Served::private().await;
        let response = served
            .send(get(HOST, &[("accept", "application/json")]))
            .await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::UNAUTHORIZED, 0)
        );
    }

    // TC: FN-12 (a session of another host is none here)
    #[tokio::test]
    async fn redirects_without_invoking_when_cookie_is_session_of_other_host() {
        let served = Served::private().await;
        let other = served
            .world
            .host_login(&served.world.owner, "shop.apps.test")
            .await
            .unwrap();
        let cookie = format!("__Host-liftoff_gate={}", other.expose());
        let response = served
            .send(get(
                HOST,
                &[("accept", BROWSER_ACCEPT), ("cookie", &cookie)],
            ))
            .await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::FOUND, 0)
        );
    }

    // TC: FN-12 (the session of this host lets the owner through)
    #[tokio::test]
    async fn invokes_function_when_private_and_cookie_is_owners_host_session() {
        let served = Served::private().await;
        let secret = served
            .world
            .host_login(&served.world.owner, HOST)
            .await
            .unwrap();
        let cookie = format!("__Host-liftoff_gate={}", secret.expose());
        let response = served
            .send(get(
                HOST,
                &[("accept", BROWSER_ACCEPT), ("cookie", &cookie)],
            ))
            .await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::OK, 1)
        );
    }

    // TC: FN-12 (a `cli` session as the bearer, as `curl` sends it)
    #[tokio::test]
    async fn invokes_function_when_private_and_bearer_is_owners_cli_session() {
        let served = Served::private().await;
        let secret = served.world.login(&served.world.owner).await.unwrap();
        let bearer = format!("Bearer {}", secret.expose());
        let response = served
            .send(get(
                HOST,
                &[("accept", "application/json"), ("authorization", &bearer)],
            ))
            .await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::OK, 1)
        );
    }

    // TC: FN-12 (a shared function needs no session)
    #[tokio::test]
    async fn invokes_function_without_session_when_shared() {
        let served = Served::shared().await;
        let response = served.send(get(HOST, &[("accept", BROWSER_ACCEPT)])).await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::OK, 1)
        );
    }
}

mod passthrough {
    use super::*;

    /// `PUT /items/7?x=1` with a JSON body and a header of its own.
    fn put_item() -> Request<Full<Bytes>> {
        Request::builder()
            .method("PUT")
            .uri("/items/7?x=1")
            .header("host", HOST)
            .header("content-type", "application/json")
            .header("x-custom", "one")
            .header("x-custom", "two")
            .body(Full::new(Bytes::from_static(br#"{"n":1}"#)))
            .unwrap()
    }

    // TC: FN-13 (method, path and query)
    #[tokio::test]
    async fn function_sees_method_path_and_query_when_proxied() {
        let served = Served::shared().await;
        served.send(put_item()).await;
        let seen: Vec<(String, Option<String>)> = served
            .functions
            .handed()
            .iter()
            .map(|(_environment, request)| {
                (
                    request.method().to_string(),
                    request.uri().path_and_query().map(ToString::to_string),
                )
            })
            .collect();
        assert_eq!(
            seen,
            vec![("PUT".to_owned(), Some("/items/7?x=1".to_owned()))]
        );
    }

    // TC: FN-13 (headers, repeated ones in order, `Host` unchanged)
    #[tokio::test]
    async fn function_sees_headers_when_proxied() {
        let served = Served::shared().await;
        served.send(put_item()).await;
        assert_eq!(
            (
                handed_header(&served.functions, "host"),
                handed_header(&served.functions, "content-type"),
                handed_header(&served.functions, "x-custom"),
            ),
            (
                vec![HOST.to_owned()],
                vec!["application/json".to_owned()],
                vec!["one".to_owned(), "two".to_owned()],
            )
        );
    }

    // TC: FN-13 (the headers of this connection and a proxy's
    // credential are not the function's)
    #[tokio::test]
    async fn function_sees_no_hop_by_hop_headers_when_client_sends_them() {
        let served = Served::shared().await;
        served
            .send(get(
                HOST,
                &[
                    ("connection", "keep-alive"),
                    ("keep-alive", "timeout=5"),
                    ("proxy-authorization", "Basic cHJveHk6c2VjcmV0"),
                    ("x-custom", "kept"),
                ],
            ))
            .await;
        assert_eq!(
            (
                handed_header(&served.functions, "connection"),
                handed_header(&served.functions, "keep-alive"),
                handed_header(&served.functions, "proxy-authorization"),
                handed_header(&served.functions, "x-custom"),
            ),
            (
                Vec::<String>::new(),
                Vec::new(),
                Vec::new(),
                vec!["kept".to_owned()]
            )
        );
    }

    // TC: FN-13 (body)
    #[tokio::test]
    async fn function_sees_body_when_proxied() {
        let served = Served::shared().await;
        served.send(put_item()).await;
        let bodies: Vec<Bytes> = served
            .functions
            .handed()
            .into_iter()
            .map(|(_environment, request)| request.into_body())
            .collect();
        assert_eq!(bodies, vec![Bytes::from_static(br#"{"n":1}"#)]);
    }

    // TC: FN-13 (status, headers and body of the function's answer)
    #[tokio::test]
    async fn client_gets_status_headers_and_body_of_function_when_it_answers() {
        let served = Served::shared().await;
        served.functions.answer_next(Ok(Response::builder()
            .status(418)
            .header("content-type", "text/plain")
            .header("set-cookie", "a=1")
            .header("set-cookie", "b=2")
            .body(Bytes::from_static(b"short and stout"))
            .unwrap()));
        let response = served.send(get(HOST, &[])).await;
        assert_eq!(
            (
                response.status().as_u16(),
                header_values(&response, "content-type"),
                header_values(&response, "set-cookie"),
                response.body().clone(),
            ),
            (
                418,
                vec!["text/plain".to_owned()],
                vec!["a=1".to_owned(), "b=2".to_owned()],
                Bytes::from_static(b"short and stout"),
            )
        );
    }
}

/// the request reaches the
/// function without liftoff's credentials and without the client's
/// `X-Liftoff-*` headers; everything else is FN-13's.
mod credentials {
    use super::*;

    // TC: FN-13 (the host session's cookie is removed, the
    // application's cookies stay)
    #[tokio::test]
    async fn function_sees_only_application_cookies_when_cookie_has_liftoffs() {
        let served = Served::shared().await;
        served
            .send(get(
                HOST,
                &[(
                    "cookie",
                    "a=1; __Host-liftoff_gate=lfg_secret; b=2; __Host-liftoff_session=lfs_secret",
                )],
            ))
            .await;
        assert_eq!(
            handed_header(&served.functions, "cookie"),
            vec!["a=1; b=2".to_owned()]
        );
    }

    // TC: FN-13 (a cookie of liftoff's only: no `Cookie` at all)
    #[tokio::test]
    async fn function_sees_no_cookie_when_cookie_is_only_liftoffs() {
        let served = Served::shared().await;
        served
            .send(get(HOST, &[("cookie", "__Host-liftoff_gate=lfg_secret")]))
            .await;
        assert_eq!(
            handed_header(&served.functions, "cookie"),
            Vec::<String>::new()
        );
    }

    // TC: FN-13 (the `cli` session that opened a private function stays
    // with liftoff)
    #[tokio::test]
    async fn function_sees_no_authorization_when_bearer_is_liftoff_session() {
        let served = Served::private().await;
        let secret = served.world.login(&served.world.owner).await.unwrap();
        let bearer = format!("Bearer {}", secret.expose());
        let response = served
            .send(get(
                HOST,
                &[("accept", "application/json"), ("authorization", &bearer)],
            ))
            .await;
        assert_eq!(
            (
                response.status(),
                served.functions.calls(),
                handed_header(&served.functions, "authorization")
            ),
            (StatusCode::OK, 1, Vec::<String>::new())
        );
    }

    // TC: FN-13 (the application's own `Authorization` passes)
    #[tokio::test]
    async fn function_sees_authorization_when_bearer_is_applications() {
        let served = Served::shared().await;
        served
            .send(get(HOST, &[("authorization", "Bearer app-token-123")]))
            .await;
        assert_eq!(
            handed_header(&served.functions, "authorization"),
            vec!["Bearer app-token-123".to_owned()]
        );
    }

    // TC: FN-12 (the person let through is named to the function, as
    // the gate names them to a compose application)
    #[tokio::test]
    async fn function_sees_user_of_session_when_private_and_allowed() {
        let served = Served::private().await;
        let secret = served.world.login(&served.world.owner).await.unwrap();
        let bearer = format!("Bearer {}", secret.expose());
        served
            .send(get(
                HOST,
                &[
                    ("accept", "application/json"),
                    ("authorization", &bearer),
                    ("x-liftoff-user", "mallory"),
                ],
            ))
            .await;
        assert_eq!(
            handed_header(&served.functions, "x-liftoff-user"),
            vec![served.world.owner.login.as_str().to_owned()]
        );
    }

    // TC: FN-13 (a client's `X-Liftoff-*` is never the function's)
    #[tokio::test]
    async fn function_sees_no_client_liftoff_headers_when_client_sends_them() {
        let served = Served::shared().await;
        served
            .send(get(
                HOST,
                &[
                    ("x-liftoff-user", "mallory"),
                    ("x-liftoff-email", "mallory@example.com"),
                    ("x-liftoff-warmup", "1"),
                ],
            ))
            .await;
        assert_eq!(
            (
                handed_header(&served.functions, "x-liftoff-user"),
                handed_header(&served.functions, "x-liftoff-email"),
                handed_header(&served.functions, "x-liftoff-warmup"),
            ),
            (Vec::<String>::new(), Vec::new(), Vec::new())
        );
    }
}

/// What the listener answers for the runtime's refusals.
mod refusals {
    use super::*;

    // TC: FN-05 (the runtime's timeout is the client's 504)
    #[tokio::test]
    async fn gateway_timeout_when_function_times_out() {
        let served = Served::shared().await;
        served.functions.answer_next(Err(FunctionError::Timeout));
        let response = served.send(get(HOST, &[])).await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    // TC: FN-06 (a trap is a 500 without its reason)
    #[tokio::test]
    async fn internal_error_without_reason_when_function_traps() {
        let served = Served::shared().await;
        served.functions.answer_next(Err(FunctionError::Trap {
            reason: "wasm trap: unreachable at 0x1234".to_owned(),
        }));
        let response = served.send(get(HOST, &[])).await;
        let body = String::from_utf8_lossy(response.body()).into_owned();
        assert_eq!(
            (response.status(), body.contains("0x1234")),
            (StatusCode::INTERNAL_SERVER_ERROR, false),
            "body: {body}"
        );
    }

    // TC: FN-11 (a stopped function answers nothing, whatever its pool
    // still holds)
    #[tokio::test]
    async fn service_unavailable_without_invoking_when_function_is_stopped() {
        let served = Served::start_in(Visibility::Shared, EnvState::Stopped).await;
        let response = served.send(get(HOST, &[])).await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::SERVICE_UNAVAILABLE, 0)
        );
    }

    // TC: FN-11 (a function environment without an active component)
    #[tokio::test]
    async fn service_unavailable_when_no_component_is_active() {
        let served = Served::shared().await;
        served.functions.answer_next(Err(FunctionError::NotActive));
        let response = served.send(get(HOST, &[])).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

/// What the listener holds in memory for requests: a capped body, read
/// within a deadline, by a bounded number of requests at once.
mod limits {
    use super::*;

    /// A short deadline: what the tests wait for is the answer, never
    /// the clock.
    const SHORT: Duration = Duration::from_millis(200);

    /// The status code of the first response the listener at `addr`
    /// writes for the raw bytes `request`.
    async fn raw_status(addr: SocketAddr, request: &[u8]) -> u16 {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut read = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !read.windows(4).any(|window| window == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0, "the listener closed without an answer");
            read.extend_from_slice(&chunk[..n]);
        }
        let head = String::from_utf8_lossy(&read).into_owned();
        head.split(' ').nth(1).unwrap().parse().unwrap()
    }

    // TC: FN-13 (a body above the cap is 413, the function never runs)
    #[tokio::test]
    async fn payload_too_large_without_invoking_when_body_exceeds_cap() {
        let served = Served::limited(
            Visibility::Shared,
            ListenerLimits {
                max_request_bytes: 8,
                ..ListenerLimits::default()
            },
        )
        .await;
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("host", HOST)
            .body(Full::new(Bytes::from_static(b"sixteen bytes!!!")))
            .unwrap();
        let response = served.send(request).await;
        assert_eq!(
            (response.status(), served.functions.calls()),
            (StatusCode::PAYLOAD_TOO_LARGE, 0)
        );
    }

    // TC: FN-13 (a body that does not arrive in time is 408)
    #[tokio::test]
    async fn request_timeout_without_invoking_when_body_trickles() {
        let served = Served::limited(
            Visibility::Shared,
            ListenerLimits {
                body_timeout: SHORT,
                ..ListenerLimits::default()
            },
        )
        .await;
        let status = tokio::time::timeout(
            EXCHANGE,
            raw_status(
                served.addr,
                b"POST / HTTP/1.1\r\nhost: api.apps.test\r\ncontent-length: 10\r\n\r\nabc",
            ),
        )
        .await
        .expect("the listener answers in time");
        assert_eq!(
            (status, served.functions.calls()),
            (StatusCode::REQUEST_TIMEOUT.as_u16(), 0)
        );
    }

    // TC: FN-13 (requests in flight are bounded: one more waits at most
    // the body deadline, then 503)
    #[tokio::test]
    async fn service_unavailable_when_requests_in_flight_are_at_limit() {
        let served = Served::limited(
            Visibility::Shared,
            ListenerLimits {
                max_in_flight: 1,
                body_timeout: SHORT,
                ..ListenerLimits::default()
            },
        )
        .await;
        let release = served.functions.hold();
        let first = tokio::spawn(exchange(served.addr, get(HOST, &[])));
        served.functions.entered.notified().await;
        let second = served.send(get(HOST, &[])).await;
        release.notify_one();
        let first = tokio::time::timeout(EXCHANGE, first)
            .await
            .expect("the first request ends in time")
            .unwrap();
        assert_eq!(
            (second.status(), first.status(), served.functions.calls()),
            (StatusCode::SERVICE_UNAVAILABLE, StatusCode::OK, 1)
        );
    }
}
