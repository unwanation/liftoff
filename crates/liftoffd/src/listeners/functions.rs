//! The function listener on `[functions] bind`.
//!
//! Caddy sends it every request for a domain bound to a running function
//! environment, `Host` unchanged. The listener:
//!
//! - takes connections from loopback only (the peer is checked);
//! - finds the environment by `Host` → domain → binding → function
//!   environment ([`ListenerRoute`]); any other host is `404` and reaches
//!   no function (FN-11); a function environment that does not run
//!   (stopped, never deployed) is `503` (a module's
//!   `render_page(unavailable)` when one renders it: the panel
//!   `liftoff-web`); a path under `/.liftoff/` is
//!   liftoff's on every address: the session handover's
//!   `/.liftoff/callback` and `/.liftoff/logout`, any other
//!   `404`, so its one-time code never reaches user code;
//! - asks [`Access::check`] before the function is called: a `private`
//!   environment without a session sends a browser to the login (`302`
//!   to `/gate/start?host&rd` on the API, the v1 SSO handover) and a
//!   JSON client gets `401`, the function never runs (FN-12); a session
//!   of someone who may not open it is `403`, the forbidden page;
//! - removes the client's `X-Liftoff-*` headers, the cookies
//!   `__Host-liftoff*` and an `Authorization` that carries a liftoff
//!   session (`lfs_…`) and the hop-by-hop
//!   headers of this connection, sets `X-Liftoff-User` and
//!   `X-Liftoff-Email` of the person [`Access::check`] let through (as
//!   the gate does), then hands the request to the environment's pool
//!   with its method, path, query, every other header and its body as
//!   they came, and answers the function's status, headers and body as
//!   they are (FN-13);
//! - bounds what it holds in memory ([`ListenerLimits`]): a body above
//!   the cap is `413`, one that does not arrive within the deadline
//!   `408`, and only so many requests read and run at once (one more
//!   waits for its turn as long as the deadline, then `503`);
//! - renders the runtime's refusals: no active component `503`, a timeout
//!   `504` (FN-05), a trap `500` (FN-06). It keeps no queue of its own:
//!   waiting for an instance is part of `functions.timeout`.
//!
//! The 404 and 503 answers are short plain texts (the 503 of a function
//! that does not run a module's page when one renders it); the refusal
//! is the daemon's forbidden page or a module's `render_page`.
//!
//! [`Access::check`]: liftoff_core::service::Access::check
//! [`ListenerRoute`]: liftoff_core::service::ListenerRoute

use core::net::SocketAddr;
use core::time::Duration;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use liftoff_core::domain::{EnvKind, EnvState, Environment, EnvironmentId, Host, LinkTarget, User};
use liftoff_core::ports::{PageKind, Ports, Store};
use liftoff_core::service::{Access, AccessDecision, GateListener, ListenerRoute, Settings};
use liftoff_rest::pages;
use liftoff_wasm::functions::{FunctionError, FunctionRuntime};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use super::common::{
    LIFTOFF_PATHS, access_request, admits, failure, host_of, liftoff_path, login_url, module_page,
    page, redirect, strip_liftoff, text,
};

/// The largest request body handed to a function (as large as the
/// largest response a function may answer); a larger one is `413`.
pub const MAX_REQUEST_BYTES: usize = liftoff_wasm::functions::MAX_RESPONSE_BYTES;

/// How many requests the listener reads and runs at once by default:
/// with [`MAX_REQUEST_BYTES`] each, what it may hold of bodies.
pub const MAX_IN_FLIGHT: usize = 64;

/// What the listener holds in memory for requests (see the [module
/// documentation](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenerLimits {
    /// The largest request body; a larger one is `413`.
    pub max_request_bytes: usize,
    /// How long the body of a request may take to arrive (`408`), and
    /// how long a request waits for its turn among
    /// [`ListenerLimits::max_in_flight`] (`503`): the daemon's
    /// `functions.timeout`.
    pub body_timeout: Duration,
    /// How many requests are read and run at once.
    pub max_in_flight: usize,
}

impl Default for ListenerLimits {
    /// [`MAX_REQUEST_BYTES`], 30 s (the default `functions.timeout`) and
    /// [`MAX_IN_FLIGHT`].
    fn default() -> Self {
        Self {
            max_request_bytes: MAX_REQUEST_BYTES,
            body_timeout: Duration::from_secs(30),
            max_in_flight: MAX_IN_FLIGHT,
        }
    }
}

/// The headers of this connection rather than of the request (RFC 9110,
/// 7.6.1): the listener reads the whole body, so they mean nothing to the
/// function. `Proxy-Authorization` is a credential for a proxy.
const HOP_BY_HOP: [HeaderName; 7] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// The functions as the listener calls them: the daemon's
/// [`FunctionRuntime`], a fake in tests.
pub trait Functions: Send + Sync + 'static {
    /// Hands `request` to the active function of `environment` and
    /// returns its response.
    ///
    /// # Errors
    ///
    /// As [`FunctionRuntime::invoke`].
    fn invoke(
        &self,
        environment: EnvironmentId,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<Bytes>, FunctionError>> + Send;
}

impl<S: Store + 'static> Functions for FunctionRuntime<S> {
    fn invoke(
        &self,
        environment: EnvironmentId,
        request: Request<Bytes>,
    ) -> impl Future<Output = Result<Response<Bytes>, FunctionError>> + Send {
        Self::invoke(self, environment, request)
    }
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct FunctionListener<P, F> {
    ports: Arc<P>,
    settings: Arc<Settings>,
    functions: Arc<F>,
    limits: ListenerLimits,
    in_flight: Arc<Semaphore>,
}

impl<P, F> Clone for FunctionListener<P, F> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
            functions: Arc::clone(&self.functions),
            limits: self.limits,
            in_flight: Arc::clone(&self.in_flight),
        }
    }
}

impl<P: Ports + 'static, F: Functions> FunctionListener<P, F> {
    /// The listener over `ports` (the store it routes by, the sessions
    /// and hooks [`Access::check`](liftoff_core::service::Access::check)
    /// reads) configured by `settings`, calling `functions`, with the
    /// default [`ListenerLimits`].
    #[must_use]
    pub fn new(ports: Arc<P>, settings: Arc<Settings>, functions: Arc<F>) -> Self {
        let limits = ListenerLimits::default();
        Self {
            ports,
            settings,
            functions,
            limits,
            in_flight: Arc::new(Semaphore::new(limits.max_in_flight.max(1))),
        }
    }

    /// The listener with `limits` instead.
    #[must_use]
    pub fn with_limits(self, limits: ListenerLimits) -> Self {
        Self {
            limits,
            in_flight: Arc::new(Semaphore::new(limits.max_in_flight.max(1))),
            ..self
        }
    }

    /// Serves `listener` until `stop` completes; connections in flight
    /// finish their current request. A connection from outside the
    /// machine is closed unanswered: only the local Caddy may ask.
    ///
    /// # Errors
    ///
    /// When the address of `listener` cannot be read.
    pub async fn serve(
        self,
        listener: TcpListener,
        stop: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let addr = listener.local_addr()?;
        tracing::info!(%addr, "function listener listening");
        let graceful = GracefulShutdown::new();
        let mut stop = core::pin::pin!(stop);
        loop {
            let accepted = tokio::select! {
                () = &mut stop => break,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, peer)) => self.connection(stream, peer, &graceful),
                // A failed accept (the peer gave up, out of descriptors)
                // ends that connection, never the listener.
                Err(error) => tracing::warn!(%error, "the function listener could not accept"),
            }
        }
        graceful.shutdown().await;
        tracing::info!(%addr, "function listener stopped");
        Ok(())
    }

    /// Serves one connection of `peer` in its own task.
    fn connection(&self, stream: TcpStream, peer: SocketAddr, graceful: &GracefulShutdown) {
        if !admits(peer) {
            tracing::warn!(%peer, "refused a function request from outside the machine");
            return;
        }
        let listener = self.clone();
        let service = service_fn(move |request: Request<Incoming>| {
            let listener = listener.clone();
            async move { Ok::<_, core::convert::Infallible>(listener.handle(request).await) }
        });
        let connection = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .serve_connection(TokioIo::new(stream), service);
        let connection = graceful.watch(connection);
        drop(tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, %peer, "a function connection ended with an error");
            }
        }));
    }

    /// One request (see the [module documentation](self)).
    async fn handle(&self, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let Some(host) = host_of(request.headers()) else {
            return text(StatusCode::NOT_FOUND, "not found");
        };
        let gate = GateListener::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        if request.uri().path().starts_with(LIFTOFF_PATHS) {
            return liftoff_path(&gate, &host, &request, LISTENER).await;
        }
        let route = ListenerRoute::new(Arc::clone(&self.ports));
        let environment = match route.environment_of(&host, EnvKind::Function).await {
            Ok(Some(environment)) => environment,
            Ok(None) => return text(StatusCode::NOT_FOUND, "not found"),
            Err(error) => return failure(LISTENER, &error),
        };
        if environment.state != EnvState::Running {
            let status = StatusCode::SERVICE_UNAVAILABLE;
            return module_page(&gate, status, PageKind::Unavailable, (&host, &request))
                .await
                .unwrap_or_else(|| text(status, UNAVAILABLE));
        }
        let user = match self.admitted(&gate, &host, &request, &environment).await {
            Ok(user) => user,
            Err(refused) => return *refused,
        };
        // Held until the function answered: the body stays in memory
        // that long.
        let Ok(Ok(_turn)) = tokio::time::timeout(
            self.limits.body_timeout,
            Arc::clone(&self.in_flight).acquire_owned(),
        )
        .await
        else {
            return text(
                StatusCode::SERVICE_UNAVAILABLE,
                "the function listener is busy; try again",
            );
        };
        let (mut parts, body) = request.into_parts();
        let read = Limited::new(body, self.limits.max_request_bytes).collect();
        let Ok(read) = tokio::time::timeout(self.limits.body_timeout, read).await else {
            return text(
                StatusCode::REQUEST_TIMEOUT,
                "the request body did not arrive in time",
            );
        };
        let body = match read {
            Ok(body) => body.to_bytes(),
            Err(error) => {
                if error.is::<http_body_util::LengthLimitError>() {
                    return text(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "the request body is too large",
                    );
                }
                tracing::debug!(%error, "a request body to a function failed");
                return text(
                    StatusCode::BAD_REQUEST,
                    "the request body could not be read",
                );
            }
        };
        forwarded_headers(&mut parts.headers, user.as_ref());
        match self
            .functions
            .invoke(environment.id, Request::from_parts(parts, body))
            .await
        {
            Ok(response) => response.map(Full::new),
            Err(error) => refusal(environment.id, &error),
        }
    }

    /// Who [`Access::check`] lets through to `environment` on `host`
    /// (`None` for a shared one without a session), or the answer when it
    /// does not: the login redirect, `401`, the forbidden page.
    async fn admitted(
        &self,
        gate: &GateListener<P>,
        host: &Host,
        request: &Request<Incoming>,
        environment: &Environment,
    ) -> Result<Option<User>, Box<Response<Full<Bytes>>>> {
        let access = Access::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        let decision = access
            .check(
                &access_request(host, request),
                &LinkTarget::Environment(environment.id),
            )
            .await;
        let refused = match decision {
            Ok(AccessDecision::Allow { user }) => return Ok(user),
            Ok(AccessDecision::Login) => {
                redirect(&login_url(&self.settings.proxy.api_host, host, request))
            }
            Ok(AccessDecision::Unauthenticated) => {
                text(StatusCode::UNAUTHORIZED, "sign in to open this address")
            }
            Ok(AccessDecision::Forbidden(reason)) => {
                page(
                    gate,
                    StatusCode::FORBIDDEN,
                    PageKind::Forbidden,
                    (host, request),
                    || pages::forbidden(&reason),
                )
                .await
            }
            Err(error) => failure(LISTENER, &error),
        };
        Err(Box::new(refused))
    }
}

/// The listener's name in the log.
const LISTENER: &str = "functions";

/// The headers of the request as the function gets them: the hop-by-hop
/// headers removed (the listener reads the whole body) and what is
/// liftoff's ([`strip_liftoff`]).
fn forwarded_headers(headers: &mut HeaderMap, user: Option<&User>) {
    for name in &HOP_BY_HOP {
        drop(headers.remove(name));
    }
    strip_liftoff(headers, user);
}

/// The text of the `503` of a function that does not run (as Caddy's
/// page for it says).
const UNAVAILABLE: &str = "liftoff: nothing deployed here";

/// What the client gets for a refusal of the runtime: its status
/// ([`FunctionError::status`]) with a fixed text; the reason of a trap or
/// a host failure goes to the daemon's log only.
fn refusal(environment: EnvironmentId, error: &FunctionError) -> Response<Full<Bytes>> {
    let (status, body) = match error {
        FunctionError::NotActive => (StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE),
        FunctionError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "the function timed out"),
        FunctionError::Trap { .. }
        | FunctionError::HostFailed { .. }
        | FunctionError::Load { .. }
        | FunctionError::Warmup { .. } => {
            tracing::warn!(%environment, %error, "a function request failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "the function failed")
        }
    };
    text(status, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod admits {
        use super::*;

        // TC: FN-11 (only the local Caddy may ask)
        #[test]
        fn admitted_when_peer_is_loopback() {
            assert!(admits(SocketAddr::from(([127, 0, 0, 1], 50000))));
        }

        // TC: FN-11
        #[test]
        fn admitted_when_peer_is_ipv6_loopback() {
            assert!(admits("[::1]:50000".parse().unwrap()));
        }

        // TC: FN-11
        #[test]
        fn refused_when_peer_is_outside_the_machine() {
            assert!(!admits(SocketAddr::from(([203, 0, 113, 7], 50000))));
        }
    }
}
