//! The daemon's side of the tunnel.

use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use http::{Request, Response, StatusCode, Uri};
use http_body::Frame as BodyFrame;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Empty, Full};
use hyper::body::Incoming;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::bandwidth::{Bandwidth, Bucket};
use crate::link::{self, Link, Source};
use crate::mux::{Mux, MuxError, Slot};
use crate::{Frame, PROTOCOL_VERSION, TunnelError};

/// The error of a body that crosses the tunnel.
pub type BoxError = Box<dyn core::error::Error + Send + Sync>;

/// The body of what [`TunnelServer::forward`] answers.
pub type ResponseBody = BoxBody<Bytes, BoxError>;

/// The limits of the server side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Streams of one connection at once (`tunnel.max_streams`); one
    /// request more is answered `503` without reaching the client.
    pub max_streams: u32,
    /// How long a connection may stay without any frame of its client
    /// (the keepalive's pings included) before the server drops it as
    /// [`Disconnect::Lost`].
    pub idle_timeout: Duration,
    /// The bandwidth of one environment, both directions together; the
    /// bucket outlives a connection, so reconnecting does not refill it.
    /// `None` leaves the connection unlimited.
    pub bandwidth: Option<Bandwidth>,
}

impl Default for ServerConfig {
    /// `max_streams = 64`, an idle timeout of three keepalive
    /// intervals (45 seconds) and [`Bandwidth::default`].
    fn default() -> Self {
        Self {
            max_streams: 64,
            idle_timeout: Duration::from_secs(45),
            bandwidth: Some(Bandwidth::default()),
        }
    }
}

/// How a connection that [`TunnelServer::serve`] ran ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disconnect {
    /// The client sent [`Frame::Shutdown`]:
    /// the environment is offline at once.
    Shutdown,
    /// A newer connection of the same environment replaced this one.
    Evicted,
    /// The connection broke or went idle: the environment waits for its
    /// client `offline_after` long.
    Lost,
}

/// The tunnels of the daemon: one active connection per environment.
/// Cheap to clone; clones share the connections.
#[derive(Debug, Clone)]
pub struct TunnelServer {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: ServerConfig,
    tunnels: Mutex<HashMap<String, Tunnel>>,
    /// The bandwidth bucket of every environment that had a tunnel.
    buckets: Mutex<HashMap<String, Arc<Bucket>>>,
    next_id: AtomicU64,
}

/// The active connection of one environment.
#[derive(Debug)]
struct Tunnel {
    /// Tells this registration from a newer one of the same environment.
    id: u64,
    mux: Arc<Mux>,
    /// Fired when a newer connection replaces this one.
    evict: oneshot::Sender<()>,
}

impl TunnelServer {
    /// A server with no tunnels yet.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                tunnels: Mutex::new(HashMap::new()),
                buckets: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// Runs one upgraded connection (`io` is the raw connection after
    /// the `101` of `GET /api/v1/tunnel`) as the tunnel of `environment`
    /// until it ends: sends [`Frame::Hello`], waits
    /// for the client's, then makes it the environment's tunnel, evicting
    /// the previous one with [`Frame::Evicted`].
    ///
    /// # Errors
    ///
    /// [`TunnelError`] when the handshake fails; once it succeeded the
    /// end is a [`Disconnect`].
    pub async fn serve<S>(&self, environment: &str, io: S) -> Result<Disconnect, TunnelError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_as(environment, environment, io, |_connection| {})
            .await
    }

    /// [`TunnelServer::serve`] with the tunnel kept under `key` (what
    /// [`TunnelServer::forward`] and [`TunnelServer::is_connected`] name,
    /// and what the bandwidth bucket belongs to) while the
    /// [`Frame::Hello`] names `environment`; `ready` runs once both
    /// `Hello`s were exchanged and the connection is the tunnel of `key`,
    /// with the number of that registration: a connection registered
    /// later under any key has a greater one, so a caller can tell the
    /// end of a replaced connection from that of the current one.
    ///
    /// The daemon keys its tunnels by environment id: names are unique
    /// within a project only.
    ///
    /// # Errors
    ///
    /// As [`TunnelServer::serve`]; `ready` does not run when the
    /// handshake fails.
    pub async fn serve_as<S>(
        &self,
        key: &str,
        environment: &str,
        io: S,
        ready: impl FnOnce(u64) + Send,
    ) -> Result<Disconnect, TunnelError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = &self.inner.config;
        let socket =
            WebSocketStream::from_raw_socket(io, Role::Server, Some(link::websocket_config()))
                .await;
        let (mut sink, mut source) = socket.split();
        let hello = Frame::Hello {
            environment: environment.to_owned(),
            version: PROTOCOL_VERSION,
        };
        sink.send(link::text(&hello))
            .await
            .map_err(|error| TunnelError::Connection(error.to_string()))?;
        let answer = tokio::time::timeout(config.idle_timeout, link::next_frame(&mut source))
            .await
            .map_err(|_elapsed| TunnelError::Connection("no Hello from the client".to_owned()))??;
        check_hello(&answer, environment)?;

        let bucket = self.bucket(key);
        let (mut link, mux_io) = Link::start(sink, None, bucket.clone());
        let mux = Arc::new(Mux::server(mux_io, config.max_streams));
        let (evict, evicted) = oneshot::channel();
        // `mux` is held here as well as in the map: the connection that
        // evicts this one replaces the map's handle, and a mux whose last
        // handle is gone ends, which makes the writer close the socket
        // before the `Evicted` below is out. The registration forgets the
        // tunnel also when the caller drops this future.
        let registration = self.register(key, Arc::clone(&mux), evict);
        ready(registration.id);
        let end = run(
            &mut source,
            &mut link,
            evicted,
            config.idle_timeout,
            bucket.as_deref(),
        )
        .await;
        drop(registration);
        let last = (end == Disconnect::Evicted).then_some(Frame::Evicted);
        link.finish(last).await;
        drop(mux);
        if end == Disconnect::Evicted {
            link::drain(&mut source).await;
        }
        Ok(end)
    }

    /// Whether `environment` has a tunnel now.
    #[must_use]
    pub fn is_connected(&self, environment: &str) -> bool {
        self.tunnels().contains_key(environment)
    }

    /// Hands `request` to the tunnel of `environment` in a stream of its
    /// own and answers what the developer's app answered: status, headers
    /// and body as they came. Method, path, query, headers and body of the
    /// request reach the app as they are here (the listener sets the
    /// `X-Forwarded-*` headers and strips liftoff's own). When the
    /// request carries hyper's upgrade and the app answers `101`, the two
    /// upgraded connections are spliced (an application's WebSocket).
    ///
    /// Answers `503` without a tunnel or when the connection already has
    /// `max_streams` streams open; `502` comes from the client when its
    /// local port refuses.
    pub async fn forward<B>(
        &self,
        environment: &str,
        mut request: Request<B>,
    ) -> Response<ResponseBody>
    where
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        let mux = self
            .tunnels()
            .get(environment)
            .map(|tunnel| Arc::clone(&tunnel.mux));
        let Some(mux) = mux else {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "dev offline\n");
        };
        let (slot, io) = match mux.open().await {
            Ok(mut stream) => (stream.take_slot(), TokioIo::new(stream)),
            Err(MuxError::TooManyStreams) => {
                return plain(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "too many requests in flight\n",
                );
            }
            Err(MuxError::Closed) => {
                return plain(StatusCode::SERVICE_UNAVAILABLE, "dev offline\n");
            }
        };
        drop(mux);
        let (mut sender, connection) = match hyper::client::conn::http1::handshake(io).await {
            Ok(handshake) => handshake,
            Err(error) => {
                tracing::debug!(%error, "the tunnel stream refused HTTP");
                return plain(StatusCode::BAD_GATEWAY, "dev unreachable\n");
            }
        };
        tokio::spawn(async move {
            if let Err(error) = connection.with_upgrades().await {
                tracing::debug!(%error, "a tunnel stream ended with an error");
            }
        });
        let upgrade = request.extensions_mut().remove::<OnUpgrade>();
        origin_form(request.uri_mut());
        let response = match sender.send_request(request).await {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(%error, "the tunnel stream gave no answer");
                return plain(StatusCode::BAD_GATEWAY, "dev unreachable\n");
            }
        };
        drop(sender);
        answer(response, upgrade, slot)
    }

    fn tunnels(&self) -> std::sync::MutexGuard<'_, HashMap<String, Tunnel>> {
        self.inner
            .tunnels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The bandwidth bucket of `environment`, when the server limits it.
    fn bucket(&self, environment: &str) -> Option<Arc<Bucket>> {
        let limit = self.inner.config.bandwidth?;
        let bucket = Arc::clone(
            self.inner
                .buckets
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(environment.to_owned())
                .or_insert_with(|| Arc::new(Bucket::new(limit))),
        );
        Some(bucket)
    }

    /// Makes `mux` the tunnel of `environment` and evicts the previous one.
    fn register(
        &self,
        environment: &str,
        mux: Arc<Mux>,
        evict: oneshot::Sender<()>,
    ) -> Registration<'_> {
        let (id, previous) = {
            let mut tunnels = self.tunnels();
            // Numbered under the lock: the later registration of a key
            // has the greater number.
            let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
            let previous = tunnels.insert(environment.to_owned(), Tunnel { id, mux, evict });
            drop(tunnels);
            (id, previous)
        };
        if let Some(previous) = previous {
            tracing::info!(
                environment,
                "a new tunnel connection replaces the previous one"
            );
            if previous.evict.send(()).is_err() {
                tracing::debug!(
                    environment,
                    "the previous tunnel connection had already ended"
                );
            }
        }
        Registration {
            server: self,
            environment: environment.to_owned(),
            id,
        }
    }

    /// Forgets the tunnel of `environment` if it is still the one `id`
    /// registered.
    fn unregister(&self, environment: &str, id: u64) {
        let mut tunnels = self.tunnels();
        if tunnels
            .get(environment)
            .is_some_and(|tunnel| tunnel.id == id)
        {
            tunnels.remove(environment);
        }
    }
}

/// A connection's place as its environment's tunnel; forgets it when
/// dropped.
struct Registration<'server> {
    server: &'server TunnelServer,
    environment: String,
    id: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.server.unregister(&self.environment, self.id);
    }
}

/// Checks the client's answer to the server's `Hello`.
fn check_hello(answer: &Frame, environment: &str) -> Result<(), TunnelError> {
    match answer {
        Frame::Hello { version, .. } if *version != PROTOCOL_VERSION => Err(TunnelError::Protocol(
            format!("the client speaks version {version}, the server {PROTOCOL_VERSION}"),
        )),
        Frame::Hello {
            environment: named, ..
        } if named != environment => Err(TunnelError::Protocol(format!(
            "the client answered for {named}, not {environment}"
        ))),
        Frame::Hello { .. } => Ok(()),
        Frame::Shutdown | Frame::Evicted => Err(TunnelError::Protocol(
            "the client did not answer the Hello".to_owned(),
        )),
    }
}

/// Reads the client's messages until the connection ends.
async fn run<S>(
    source: &mut Source<S>,
    link: &mut Link,
    mut evicted: oneshot::Receiver<()>,
    idle_timeout: Duration,
    bucket: Option<&Bucket>,
) -> Disconnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            message = source.next() => {
                idle.set(tokio::time::sleep(idle_timeout));
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        if let Some(bucket) = bucket {
                            bucket.take(bytes.len()).await;
                            // The wait for the bucket is no silence.
                            idle.set(tokio::time::sleep(idle_timeout));
                        }
                        if link.to_mux.write_all(&bytes).await.is_err() {
                            return Disconnect::Lost;
                        }
                    }
                    Some(Ok(Message::Text(text))) => match link::parse(text.as_str()) {
                        Ok(Frame::Shutdown) => return Disconnect::Shutdown,
                        Ok(frame) => tracing::debug!(?frame, "an unexpected frame from the client"),
                        Err(error) => tracing::debug!(%error, "a bad frame from the client"),
                    },
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return Disconnect::Lost,
                }
            }
            () = &mut idle => {
                tracing::info!("a tunnel connection went silent");
                return Disconnect::Lost;
            }
            _ = &mut evicted => return Disconnect::Evicted,
        }
    }
}

/// The app's `response` as the answer of [`TunnelServer::forward`]: its
/// body holds the stream's slot until it ends; a `101` to an upgrade
/// splices the two upgraded connections, which then hold it.
fn answer(
    mut response: Response<Incoming>,
    upgrade: Option<OnUpgrade>,
    slot: Option<Slot>,
) -> Response<ResponseBody> {
    if response.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(browser) = upgrade
    {
        let app = hyper::upgrade::on(&mut response);
        tokio::spawn(splice(browser, app, slot));
        return response.map(|_body| Empty::new().map_err(|never| match never {}).boxed());
    }
    response.map(|body| Slotted { body, slot }.boxed())
}

/// Relays the bytes between the browser's and the app's upgraded
/// connections until either ends.
async fn splice(browser: OnUpgrade, app: OnUpgrade, slot: Option<Slot>) {
    let (browser, app) = tokio::join!(browser, app);
    match (browser, app) {
        (Ok(browser), Ok(app)) => {
            let mut browser = TokioIo::new(browser);
            let mut app = TokioIo::new(app);
            if let Err(error) = tokio::io::copy_bidirectional(&mut browser, &mut app).await {
                tracing::debug!(%error, "an upgraded connection through the tunnel ended");
            }
        }
        (Err(error), _) | (_, Err(error)) => {
            tracing::debug!(%error, "an upgrade through the tunnel failed");
        }
    }
    drop(slot);
}

/// Requests cross the stream in origin form (`/path?query`).
fn origin_form(uri: &mut Uri) {
    if uri.scheme().is_some() || uri.authority().is_some() {
        let path = uri
            .path_and_query()
            .map_or_else(|| "/".to_owned(), |path| path.as_str().to_owned());
        if let Ok(origin) = path.parse() {
            *uri = origin;
        }
    }
}

/// A short plain-text answer of the server itself.
fn plain(status: StatusCode, text: &'static str) -> Response<ResponseBody> {
    let mut response = Response::new(
        Full::new(Bytes::from_static(text.as_bytes()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// The app's body; frees the stream's slot once it ended.
struct Slotted {
    body: Incoming,
    slot: Option<Slot>,
}

impl http_body::Body for Slotted {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<BodyFrame<Bytes>, BoxError>>> {
        let polled = Pin::new(&mut self.body).poll_frame(cx);
        if matches!(polled, Poll::Ready(None | Some(Err(_)))) {
            drop(self.slot.take());
        }
        polled.map(|frame| frame.map(|frame| frame.map_err(Into::into)))
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}
