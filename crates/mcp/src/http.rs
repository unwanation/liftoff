//! The Streamable HTTP transport of MCP: `POST|GET|DELETE /mcp` as a tower service the
//! daemon mounts, with the same tools and resources as stdio.
//!
//! Every request asks the [`Backend`] for the API session of its bearer
//! first (the daemon's backend verifies it as an OAuth access token in
//! the core and opens the `mcp` session): a [`Refusal::Unauthenticated`]
//! is `401` with `WWW-Authenticate: Bearer
//! resource_metadata="<HttpConfig::resource_metadata>"` (RFC 9728 5.1),
//! so the client finds the authorization server; a
//! [`Refusal::Forbidden`] is `403`, a [`Refusal::ModuleRequired`] `424`
//! and a [`Refusal::Unavailable`] `503`, all problem documents. A granted
//! request goes to `rmcp`'s Streamable HTTP server, whose tools call the
//! API with that session (`GET /mcp` is its event stream).
//!
//! The transport's own sessions are held in memory. Each answers the
//! [`Grant::principal`] that opened it alone (another person's request
//! naming it is `404`, as an unknown session); at most
//! [`HttpConfig::max_sessions_per_principal`] are held per person and
//! [`HttpConfig::max_sessions`] in all (past a bound the oldest is
//! closed); one ends when idle for [`HttpConfig::session_idle`], when the
//! API session of its latest request ends ([`Grant::valid_for`]), with
//! its event stream, and when [`HttpConfig::shutdown`] is cancelled.

use core::convert::Infallible;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures_core::Stream;
use http::{HeaderValue, StatusCode, header};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full};
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{
    LocalSessionManager, LocalSessionManagerError,
};
use rmcp::transport::streamable_http_server::session::{
    RestoreOutcome, ServerSseMessage, SessionId, SessionManager,
};
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use crate::Session;
use crate::api::Api;
use crate::server::Server;

/// The body of the transport's answers (that of `rmcp`'s server).
pub type HttpBody = BoxBody<Bytes, Infallible>;

/// Why a request to `/mcp` gets no API session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// No bearer, or one the authorization server's tokens do not match
    /// (another audience, a bad signature, expired): `401` with the
    /// resource metadata. The reason is for the log only.
    Unauthenticated(String),
    /// The person of a valid token may not use liftoff (`403`, the
    /// reason as `detail`): another user has their login, say.
    Forbidden(String),
    /// The module the transport needs is not installed (`424`, "install
    /// the module `<name>`").
    ModuleRequired(String),
    /// The check could not be made (`503`).
    Unavailable(String),
}

/// What the [`Backend`] grants a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The installation and the session the tools of the request call
    /// the API with.
    pub session: Session,
    /// Who the session is of, stable across their tokens (the user's
    /// ID): a transport session answers the principal that opened it
    /// alone.
    pub principal: String,
    /// How long the API session lasts from now: the transport session
    /// opened or used with it ends then, unless a later request's grant
    /// lasts longer (a refreshed token).
    pub valid_for: Duration,
}

/// What the transport asks before it serves a request.
pub trait Backend: Send + Sync + 'static {
    /// The grant of the caller whose `Authorization: Bearer` is `bearer`
    /// (`None` without one).
    fn session(&self, bearer: Option<&str>) -> impl Future<Output = Result<Grant, Refusal>> + Send;
}

/// How the transport is set up.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// The URL of the protected-resource metadata
    /// (`https://<api>/.well-known/oauth-protected-resource`), named by the
    /// `WWW-Authenticate` of a `401`.
    pub resource_metadata: String,
    /// The `Host` values the transport answers (DNS rebinding: the API
    /// host of `[api] public_url`).
    pub allowed_hosts: Vec<String>,
    /// The most transport sessions held at once.
    pub max_sessions: usize,
    /// The most transport sessions held at once for one principal: one
    /// person opening sessions in a loop closes their own, not others'.
    pub max_sessions_per_principal: usize,
    /// How long a transport session nobody uses is kept.
    pub session_idle: Duration,
    /// Cancelled when the daemon stops: the open event streams end, so a
    /// graceful shutdown does not wait on them.
    pub shutdown: CancellationToken,
}

/// The transport: a [`tower::Service`] of `/mcp` (see the [module
/// documentation](self)).
pub struct StreamableHttp<B> {
    backend: Arc<B>,
    config: Arc<HttpConfig>,
    sessions: Arc<BoundedSessions>,
    inner: StreamableHttpService<Server, BoundedSessions>,
}

impl<B> core::fmt::Debug for StreamableHttp<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamableHttp")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl<B> Clone for StreamableHttp<B> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            config: Arc::clone(&self.config),
            sessions: Arc::clone(&self.sessions),
            inner: self.inner.clone(),
        }
    }
}

impl<B: Backend> StreamableHttp<B> {
    /// The transport asking `backend`, set up by `config`.
    #[must_use]
    pub fn new(backend: B, config: HttpConfig) -> Self {
        let server = StreamableHttpServerConfig::default()
            .with_allowed_hosts(config.allowed_hosts.clone())
            .with_cancellation_token(config.shutdown.child_token());
        let sessions = Arc::new(BoundedSessions::new(&config));
        let inner = StreamableHttpService::new(
            || {
                // The session of each request replaces this empty one.
                Api::new(Session {
                    url: String::new(),
                    token: String::new(),
                })
                .map(Server::per_request)
                .map_err(std::io::Error::other)
            },
            Arc::clone(&sessions),
            server,
        );
        Self {
            backend: Arc::new(backend),
            config: Arc::new(config),
            sessions,
            inner,
        }
    }
}

impl<B, RB> tower::Service<http::Request<RB>> for StreamableHttp<B>
where
    B: Backend,
    RB: http_body::Body + Send + 'static,
    RB::Data: Send + 'static,
    RB::Error: core::fmt::Display,
{
    type Response = http::Response<HttpBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: http::Request<RB>) -> Self::Future {
        let backend = Arc::clone(&self.backend);
        let config = Arc::clone(&self.config);
        let sessions = Arc::clone(&self.sessions);
        let inner = self.inner.clone();
        Box::pin(async move {
            let bearer = bearer(req.headers());
            let grant = match backend.session(bearer.as_deref()).await {
                Ok(grant) => grant,
                Err(refusal) => return Ok(refused(&config, bearer.is_some(), refusal)),
            };
            let until = Instant::now()
                .checked_add(grant.valid_for)
                .unwrap_or_else(far_future);
            let named = session_id(req.headers());
            let closed = match &named {
                Some(id) => match sessions.admit(id, &grant.principal, until) {
                    Some(closed) => Some(closed),
                    None => return Ok(problem(StatusCode::NOT_FOUND, "no such MCP session")),
                },
                None => None,
            };
            // The handlers read it from the `http::request::Parts` that
            // `rmcp` hands them.
            let _previous = req.extensions_mut().insert(grant.session);
            let response = inner.handle(req).await;
            let Some(closed) = closed else {
                if let Some(opened) = session_id(response.headers()) {
                    sessions.adopt(opened, grant.principal, until).await;
                }
                return Ok(response);
            };
            // An event stream of the session ends when the session does.
            Ok(response.map(|body| UntilClosed::new(body, closed).boxed()))
        })
    }
}

/// A body that ends when its session is closed.
struct UntilClosed {
    inner: HttpBody,
    closed: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl UntilClosed {
    fn new(inner: HttpBody, closed: CancellationToken) -> Self {
        Self {
            inner,
            closed: Box::pin(closed.cancelled_owned()),
        }
    }
}

impl http_body::Body for UntilClosed {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        if self.closed.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// An instant no grant reaches (about thirty years from now).
fn far_future() -> Instant {
    Instant::now()
        .checked_add(Duration::from_hours(30 * 365 * 24))
        .unwrap_or_else(Instant::now)
}

/// The `Mcp-Session-Id` of `headers`, if any.
fn session_id(headers: &http::HeaderMap) -> Option<SessionId> {
    let value = headers.get(SESSION_HEADER)?.to_str().ok()?;
    (!value.is_empty()).then(|| SessionId::from(value))
}

/// The header naming a transport session (MCP Streamable HTTP).
const SESSION_HEADER: &str = "mcp-session-id";

/// The token of `Authorization: Bearer <token>`, if the request has one.
fn bearer(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_owned())
}

/// The answer to a request the backend refused; the reason of a `401`
/// goes to the log, never to the client.
fn refused(config: &HttpConfig, had_bearer: bool, refusal: Refusal) -> http::Response<HttpBody> {
    match refusal {
        Refusal::Unauthenticated(why) => {
            if had_bearer {
                tracing::info!(reason = %why, "refused an access token at /mcp");
            }
            let challenge = if had_bearer {
                format!(
                    "Bearer resource_metadata=\"{}\", error=\"invalid_token\"",
                    config.resource_metadata
                )
            } else {
                format!("Bearer resource_metadata=\"{}\"", config.resource_metadata)
            };
            let mut response = problem(
                StatusCode::UNAUTHORIZED,
                "a valid access token of the authorization server is required",
            );
            if let Ok(value) = HeaderValue::from_str(&challenge) {
                let _previous = response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, value);
            }
            response
        }
        Refusal::Forbidden(why) => problem(StatusCode::FORBIDDEN, &why),
        Refusal::ModuleRequired(module) => problem(
            StatusCode::FAILED_DEPENDENCY,
            &format!("install the module `{module}`"),
        ),
        Refusal::Unavailable(why) => {
            tracing::warn!(reason = %why, "could not check an access token at /mcp");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "the access token cannot be checked now",
            )
        }
    }
}

/// A problem document (RFC 9457) of `status` with `detail`, as the REST
/// API's.
fn problem(status: StatusCode, detail: &str) -> http::Response<HttpBody> {
    let body = json!({
        "status": status.as_u16(),
        "title": status.canonical_reason().unwrap_or("Error"),
        "detail": detail,
    })
    .to_string();
    let mut response = http::Response::new(Full::new(Bytes::from(body)).boxed());
    *response.status_mut() = status;
    let _previous = response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
}

/// Who opened a transport session and until when it may live.
#[derive(Debug)]
struct Owner {
    principal: String,
    until: Instant,
    /// Cancelled when the session is closed: its event streams end.
    closed: CancellationToken,
}

/// The transport sessions `rmcp` holds, oldest first, and their owners
/// (known once `initialize` answered).
#[derive(Debug, Default)]
struct Table {
    order: VecDeque<SessionId>,
    owners: HashMap<SessionId, Owner>,
}

impl Table {
    /// Forgets `id`; its event streams end.
    fn forget(&mut self, id: &SessionId) {
        self.order.retain(|known| known != id);
        if let Some(owner) = self.owners.remove(id) {
            owner.closed.cancel();
        }
    }
}

/// `rmcp`'s in-memory sessions with the bounds of the [module
/// documentation](self): a session past a bound closes the oldest (its
/// client gets `404` and starts again, as MCP clients do).
#[derive(Debug)]
struct BoundedSessions {
    inner: LocalSessionManager,
    max: usize,
    per_principal: usize,
    shutdown: CancellationToken,
    table: Mutex<Table>,
}

impl BoundedSessions {
    fn new(config: &HttpConfig) -> Self {
        let mut inner = LocalSessionManager::default();
        inner.session_config.keep_alive = Some(config.session_idle);
        Self {
            inner,
            max: config.max_sessions.max(1),
            per_principal: config.max_sessions_per_principal.max(1),
            shutdown: config.shutdown.clone(),
            table: Mutex::new(Table::default()),
        }
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a request of `principal` may use the session `id`: when it
    /// may, the session lives at least `until`, and the token cancelled
    /// when it is closed.
    fn admit(&self, id: &SessionId, principal: &str, until: Instant) -> Option<CancellationToken> {
        let mut table = self.table();
        let owner = table.owners.get_mut(id)?;
        if owner.principal != principal {
            return None;
        }
        owner.until = owner.until.max(until);
        let closed = owner.closed.clone();
        drop(table);
        Some(closed)
    }

    /// Records `principal` as the opener of the new session `id`, which
    /// lives until `until`: their oldest sessions past
    /// [`HttpConfig::max_sessions_per_principal`] are closed, and `id` is
    /// closed at `until` (or later, if a request extends it).
    async fn adopt(self: &Arc<Self>, id: SessionId, principal: String, until: Instant) {
        let evicted: Vec<SessionId> = {
            let mut table = self.table();
            if !table.order.contains(&id) {
                // Closed meanwhile.
                return;
            }
            let theirs: Vec<SessionId> = table
                .order
                .iter()
                .filter(|known| {
                    table
                        .owners
                        .get(*known)
                        .is_some_and(|owner| owner.principal == principal)
                })
                .cloned()
                .collect();
            let over = theirs
                .len()
                .saturating_add(1)
                .saturating_sub(self.per_principal);
            let evicted: Vec<SessionId> = theirs.into_iter().take(over).collect();
            for gone in &evicted {
                table.forget(gone);
            }
            let _previous = table.owners.insert(
                Arc::clone(&id),
                Owner {
                    principal,
                    until,
                    closed: self.shutdown.child_token(),
                },
            );
            evicted
        };
        self.close_all(&evicted).await;
        drop(tokio::spawn(Arc::clone(self).expire(id)));
    }

    /// Closes the session `id` once its owner's `until` passes, or ends
    /// at shutdown.
    async fn expire(self: Arc<Self>, id: SessionId) {
        loop {
            let until = match self.table().owners.get(&id) {
                Some(owner) => owner.until,
                None => return,
            };
            if Instant::now() >= until {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep_until(until) => {}
                () = self.shutdown.cancelled() => return,
            }
        }
        tracing::debug!("an MCP session outlived its API session and is closed");
        if let Err(error) = self.close_session(&id).await {
            tracing::debug!(%error, "closing an expired MCP session failed");
        }
    }

    /// Closes the sessions `ids` of `rmcp`.
    async fn close_all(&self, ids: &[SessionId]) {
        for id in ids {
            if let Err(error) = self.inner.close_session(id).await {
                tracing::debug!(%error, "closing an evicted MCP session failed");
            }
        }
    }
}

impl SessionManager for BoundedSessions {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let evicted: Vec<SessionId> = {
            let mut table = self.table();
            let over = table.order.len().saturating_add(1).saturating_sub(self.max);
            let evicted: Vec<SessionId> = table.order.iter().take(over).cloned().collect();
            for gone in &evicted {
                table.forget(gone);
            }
            evicted
        };
        self.close_all(&evicted).await;
        let (id, transport) = self.inner.create_session().await?;
        self.table().order.push_back(Arc::clone(&id));
        Ok((id, transport))
    }

    fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> impl Future<Output = Result<ServerJsonRpcMessage, Self::Error>> + Send {
        self.inner.initialize_session(id, message)
    }

    fn has_session(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = Result<bool, Self::Error>> + Send {
        self.inner.has_session(id)
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        self.table().forget(id);
        self.inner.close_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.create_stream(id, message).await
    }

    fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.accept_message(id, message)
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.resume(id, last_event_id).await
    }

    fn restore_session(
        &self,
        _id: SessionId,
    ) -> impl Future<Output = Result<RestoreOutcome<Self::Transport>, Self::Error>> + Send {
        // Sessions live in this process alone.
        core::future::ready(Ok(RestoreOutcome::NotSupported))
    }
}
