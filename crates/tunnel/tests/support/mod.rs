//! The harness of the tunnel's integration tests: the daemon's side
//! behind a real WebSocket upgrade, the developer's local app, a local
//! WebSocket echo app and a listener in front of the server, every one on
//! an ephemeral `127.0.0.1` port.

use core::convert::Infallible;
use core::time::Duration;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::{Request as AxumRequest, State};
use axum::routing::get;
use bytes::Bytes;
use http::{HeaderMap, Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use liftoff_tunnel::{
    Backoff, CertificateDer, ClientConfig, ClientState, Disconnect, Frame, PROTOCOL_VERSION,
    ServerConfig, TunnelClient, TunnelError, TunnelServer,
};
use tokio::net::TcpListener;
use tokio::sync::{Barrier, Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// How long one exchange may take before the test fails instead of
/// hanging.
pub(crate) const EXCHANGE: Duration = Duration::from_secs(10);

/// The CLI session the client presents.
pub(crate) const TOKEN: &str = "lfs_tunnel_test";

/// The environment the daemon finds for the port (step 2).
pub(crate) const ENVIRONMENT: &str = "3000";

/// The daemon's side: [`TunnelServer`] behind `GET /api/v1/tunnel`.
pub(crate) struct Daemon {
    pub(crate) server: TunnelServer,
    pub(crate) addr: SocketAddr,
    inner: Arc<DaemonInner>,
    ended: mpsc::UnboundedReceiver<Result<Disconnect, TunnelError>>,
}

struct DaemonInner {
    server: TunnelServer,
    /// Upgrade requests that reached the route.
    attempts: watch::Sender<usize>,
    /// Upgrades answered `101`.
    upgrades: watch::Sender<usize>,
    /// When each upgrade request reached the route.
    attempted_at: Mutex<Vec<Instant>>,
    /// Upgrade requests still to refuse.
    refuse: Mutex<usize>,
    /// The status the refused requests get.
    refusal: StatusCode,
    /// Whether the daemon answers the client's `Hello` and then neither
    /// reads nor writes (pongs included) instead of serving.
    silent: bool,
    /// The headers of every upgrade request.
    headers: Mutex<Vec<HeaderMap>>,
    /// The tasks that run the connections.
    connections: Mutex<Vec<JoinHandle<()>>>,
    ended: mpsc::UnboundedSender<Result<Disconnect, TunnelError>>,
}

impl Daemon {
    pub(crate) async fn start(config: ServerConfig) -> Self {
        Self::refusing(config, 0).await
    }

    /// A daemon that answers the first `refuse` upgrade requests `503`.
    pub(crate) async fn refusing(config: ServerConfig, refuse: usize) -> Self {
        Self::build(config, refuse, StatusCode::SERVICE_UNAVAILABLE, false).await
    }

    /// A daemon that answers every upgrade request with `status`, as
    /// the daemon's route does without a valid session (`401`) or with a
    /// deploy token (`403`).
    pub(crate) async fn refusing_with(config: ServerConfig, status: StatusCode) -> Self {
        Self::build(config, usize::MAX, status, false).await
    }

    /// A daemon that finishes the handshake and then goes silent: it
    /// keeps the connection open but neither reads it nor answers pings.
    pub(crate) async fn silent(config: ServerConfig) -> Self {
        Self::build(config, 0, StatusCode::SERVICE_UNAVAILABLE, true).await
    }

    async fn build(config: ServerConfig, refuse: usize, refusal: StatusCode, silent: bool) -> Self {
        let server = TunnelServer::new(config);
        let (ended_tx, ended) = mpsc::unbounded_channel();
        let inner = Arc::new(DaemonInner {
            server: server.clone(),
            attempts: watch::Sender::new(0),
            upgrades: watch::Sender::new(0),
            attempted_at: Mutex::new(Vec::new()),
            refuse: Mutex::new(refuse),
            refusal,
            silent,
            headers: Mutex::new(Vec::new()),
            connections: Mutex::new(Vec::new()),
            ended: ended_tx,
        });
        let app = Router::new()
            .route("/api/v1/tunnel", get(upgrade))
            .with_state(Arc::clone(&inner));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let _served = axum::serve(listener, app).await;
        });
        Self {
            server,
            addr,
            inner,
            ended,
        }
    }

    /// The URL the client connects to.
    pub(crate) fn url(&self) -> String {
        format!("ws://{}/api/v1/tunnel", self.addr)
    }

    /// A private client of the local port, with the keepalive and
    /// short waits between attempts.
    pub(crate) fn client_config(&self, local_port: u16, keepalive: Duration) -> ClientConfig {
        ClientConfig {
            url: self.url(),
            token: TOKEN.to_owned(),
            local_port,
            share: false,
            keepalive,
            backoff: Backoff {
                initial: Duration::from_millis(10),
                max: Duration::from_millis(50),
            },
            extra_roots: Vec::new(),
        }
    }

    /// Breaks every connection the daemon holds, as a dropped network
    /// would.
    pub(crate) fn break_connections(&self) {
        for task in self
            .inner
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
    }

    /// Waits until `upgrades` upgrades were answered `101`.
    pub(crate) async fn wait_upgrades(&self, upgrades: usize) {
        let mut seen = self.inner.upgrades.subscribe();
        tokio::time::timeout(EXCHANGE, seen.wait_for(|n| *n >= upgrades))
            .await
            .expect("the client connects in time")
            .expect("the daemon runs");
    }

    pub(crate) fn attempts(&self) -> usize {
        *self.inner.attempts.borrow()
    }

    /// When each upgrade request reached the route, in order.
    pub(crate) fn attempted_at(&self) -> Vec<Instant> {
        self.inner
            .attempted_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn upgrades(&self) -> usize {
        *self.inner.upgrades.borrow()
    }

    pub(crate) fn upgrade_headers(&self) -> Vec<HeaderMap> {
        self.inner
            .headers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How the next connection [`TunnelServer::serve`] ran ended.
    pub(crate) async fn next_end(&mut self) -> Result<Disconnect, TunnelError> {
        tokio::time::timeout(EXCHANGE, self.ended.recv())
            .await
            .expect("a connection ends in time")
            .expect("the daemon runs")
    }

    /// Whether no connection has ended so far.
    pub(crate) fn none_ended(&mut self) -> bool {
        self.ended.try_recv().is_err()
    }

    /// Waits until the server has a tunnel for [`ENVIRONMENT`].
    pub(crate) async fn wait_tunnel(&self) {
        tokio::time::timeout(EXCHANGE, async {
            while !self.server.is_connected(ENVIRONMENT) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the tunnel is registered in time");
    }

    /// `GET path` through the tunnel, its body read.
    pub(crate) async fn get(&self, path: &str) -> (StatusCode, HeaderMap, Bytes) {
        let request = Request::get(path)
            .header(http::header::HOST, "shop-dev.apps.test")
            .body(Full::new(Bytes::new()))
            .expect("request");
        self.send(request).await
    }

    /// `request` through the tunnel, the answer's body read.
    pub(crate) async fn send(
        &self,
        request: Request<Full<Bytes>>,
    ) -> (StatusCode, HeaderMap, Bytes) {
        send(&self.server, request).await
    }

    /// A listener in front of the server, as the tunnel listener of the
    /// daemon serves Caddy: hyper with upgrades, every request handed to
    /// [`TunnelServer::forward`] for [`ENVIRONMENT`].
    pub(crate) async fn listener(&self) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let server = self.server.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _peer)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                        let server = server.clone();
                        async move { Ok::<_, Infallible>(server.forward(ENVIRONMENT, request).await) }
                    });
                    let _served = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        addr
    }
}

/// `request` through `server`'s tunnel of [`ENVIRONMENT`].
pub(crate) async fn send(
    server: &TunnelServer,
    request: Request<Full<Bytes>>,
) -> (StatusCode, HeaderMap, Bytes) {
    tokio::time::timeout(EXCHANGE, async {
        let response = server.forward(ENVIRONMENT, request).await;
        let (parts, body) = response.into_parts();
        let body = body.collect().await.expect("the answer's body").to_bytes();
        (parts.status, parts.headers, body)
    })
    .await
    .expect("the answer arrives in time")
}

/// `GET /api/v1/tunnel`: the upgrade, then [`TunnelServer::serve`] for
/// [`ENVIRONMENT`].
async fn upgrade(
    State(inner): State<Arc<DaemonInner>>,
    mut request: AxumRequest,
) -> Response<axum::body::Body> {
    inner
        .attempted_at
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Instant::now());
    inner.attempts.send_modify(|n| *n = n.saturating_add(1));
    inner
        .headers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(request.headers().clone());
    let refused = {
        let mut refuse = inner.refuse.lock().unwrap_or_else(PoisonError::into_inner);
        let refused = *refuse > 0;
        *refuse = refuse.saturating_sub(1);
        refused
    };
    if refused {
        return status(inner.refusal);
    }
    let Some(key) = request.headers().get("sec-websocket-key").cloned() else {
        return status(StatusCode::BAD_REQUEST);
    };
    let on_upgrade = hyper::upgrade::on(&mut request);
    let task_inner = Arc::clone(&inner);
    let task = tokio::spawn(async move {
        let Ok(upgraded) = on_upgrade.await else {
            return;
        };
        if task_inner.silent {
            hold_silently(TokioIo::new(upgraded)).await;
            return;
        }
        let ended = task_inner
            .server
            .serve(ENVIRONMENT, TokioIo::new(upgraded))
            .await;
        let _sent = task_inner.ended.send(ended);
    });
    inner
        .connections
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(task);
    inner.upgrades.send_modify(|n| *n = n.saturating_add(1));
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(http::header::CONNECTION, "upgrade")
        .header(http::header::UPGRADE, "websocket")
        .header("sec-websocket-accept", derive_accept_key(key.as_bytes()))
        .body(axum::body::Body::empty())
        .expect("response")
}

/// The server's half of a handshake, then silence: the connection stays
/// open, nobody reads it and no ping gets its pong.
async fn hold_silently(io: TokioIo<hyper::upgrade::Upgraded>) {
    use futures_util::{SinkExt as _, StreamExt as _};

    let mut socket = WebSocketStream::from_raw_socket(io, Role::Server, None).await;
    let hello = Frame::Hello {
        environment: ENVIRONMENT.to_owned(),
        version: PROTOCOL_VERSION,
    };
    if socket.send(hello_message(&hello)).await.is_err() {
        return;
    }
    let _answer = socket.next().await;
    core::future::pending::<()>().await;
    drop(socket);
}

/// `frame` as the text message the protocol sends it in.
pub(crate) fn hello_message(frame: &Frame) -> Message {
    Message::text(serde_json::to_string(frame).expect("a frame serialises"))
}

/// A WebSocket of the test's own straight to the daemon's route, in
/// place of a [`TunnelClient`]: the server's `Hello` already read.
pub(crate) async fn raw_client(
    daemon: &Daemon,
) -> (
    WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    Frame,
) {
    use futures_util::StreamExt as _;

    let (mut socket, _response) =
        tokio::time::timeout(EXCHANGE, tokio_tungstenite::connect_async(daemon.url()))
            .await
            .expect("the upgrade ends in time")
            .expect("the daemon upgrades");
    let hello = tokio::time::timeout(EXCHANGE, socket.next())
        .await
        .expect("the server's Hello arrives in time")
        .expect("the connection is open")
        .expect("the server's Hello");
    let Message::Text(text) = hello else {
        panic!("the server's Hello is a text message: {hello:?}");
    };
    let frame = serde_json::from_str(text.as_str()).expect("the server's Hello parses");
    (socket, frame)
}

/// A TLS front for `backend` on an ephemeral `127.0.0.1` port with the
/// certificate of `tests/fixtures/leaf.der` (`IP:127.0.0.1`, issued by
/// [`test_ca`]): it relays every connection's plain bytes to `backend`.
/// Also answers how many TLS handshakes failed so far.
pub(crate) async fn tls_front(backend: SocketAddr) -> (SocketAddr, watch::Receiver<usize>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certificate = CertificateDer::from(include_bytes!("../fixtures/leaf.der").to_vec());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        include_bytes!("../fixtures/leaf.key.der").to_vec().into(),
    );
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .expect("the test certificate");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let (failed_tx, failed) = watch::channel(0_usize);
    let failed_tx = Arc::new(failed_tx);
    tokio::spawn(async move {
        while let Ok((tcp, _peer)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let failed_tx = Arc::clone(&failed_tx);
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    failed_tx.send_modify(|n| *n = n.saturating_add(1));
                    return;
                };
                let Ok(mut plain) = tokio::net::TcpStream::connect(backend).await else {
                    return;
                };
                let _relayed = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
            });
        }
    });
    (addr, failed)
}

/// The CA of the test certificates (`tests/fixtures/ca.der`).
pub(crate) fn test_ca() -> CertificateDer<'static> {
    CertificateDer::from(include_bytes!("../fixtures/ca.der").to_vec())
}

fn status(status: StatusCode) -> Response<axum::body::Body> {
    Response::builder()
        .status(status)
        .body(axum::body::Body::empty())
        .expect("response")
}

/// A request the local app received, its body read.
#[derive(Debug, Clone)]
pub(crate) struct Received {
    pub(crate) method: http::Method,
    pub(crate) uri: String,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

/// The developer's app on `127.0.0.1:<port>` (what `lof expose` exposes).
///
/// - `/bytes` answers `201`, `x-app: tunnel` and [`bytes_body`];
/// - `/hold` notifies `entered` and answers `200 held` once `release`
///   is notified;
/// - `/together/<n>` waits until [`LocalApp::together`] requests arrived,
///   then answers its own path;
/// - anything else answers `200` with the app's name.
pub(crate) struct LocalApp {
    pub(crate) port: u16,
    pub(crate) entered: Arc<Notify>,
    pub(crate) release: Arc<Notify>,
    received: Arc<Mutex<Vec<Received>>>,
}

impl LocalApp {
    /// How many `/together/*` requests the app waits for.
    pub(crate) const TOGETHER: usize = 16;

    pub(crate) async fn start(name: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        Self::serving(name, listener)
    }

    /// The app on `[::1]` only, as a dev server on `localhost` may
    /// listen; `None` on a host without IPv6.
    pub(crate) async fn start_on_ipv6(name: &'static str) -> Option<Self> {
        let listener = TcpListener::bind("[::1]:0").await.ok()?;
        Some(Self::serving(name, listener))
    }

    fn serving(name: &'static str, listener: TcpListener) -> Self {
        let port = listener.local_addr().expect("address").port();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let received = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new(Barrier::new(Self::TOGETHER));
        let app = Arc::new(AppInner {
            name,
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            received: Arc::clone(&received),
            barrier,
        });
        tokio::spawn(async move {
            while let Ok((tcp, _peer)) = listener.accept().await {
                let app = Arc::clone(&app);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                        let app = Arc::clone(&app);
                        async move { Ok::<_, Infallible>(app.answer(request).await) }
                    });
                    let _served = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tcp), service)
                        .await;
                });
            }
        });
        Self {
            port,
            entered,
            release,
            received,
        }
    }

    pub(crate) fn received(&self) -> Vec<Received> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct AppInner {
    name: &'static str,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    received: Arc<Mutex<Vec<Received>>>,
    barrier: Arc<Barrier>,
}

impl AppInner {
    async fn answer(&self, request: Request<Incoming>) -> Response<Full<Bytes>> {
        let (parts, body) = request.into_parts();
        let body = body
            .collect()
            .await
            .map(http_body_util::Collected::to_bytes)
            .unwrap_or_default();
        let path = parts.uri.path().to_owned();
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Received {
                method: parts.method,
                uri: parts.uri.to_string(),
                headers: parts.headers,
                body,
            });
        if path == "/bytes" {
            return Response::builder()
                .status(StatusCode::CREATED)
                .header("x-app", "tunnel")
                .body(Full::new(bytes_body()))
                .expect("response");
        }
        if path == "/hold" {
            self.entered.notify_one();
            self.release.notified().await;
            return Response::new(Full::new(Bytes::from_static(b"held")));
        }
        if path.starts_with("/together/") {
            self.barrier.wait().await;
            return Response::new(Full::new(Bytes::from(path)));
        }
        Response::new(Full::new(Bytes::from_static(self.name.as_bytes())))
    }
}

/// What `/bytes` answers: every byte value, over and over, 100 000 bytes.
pub(crate) fn bytes_body() -> Bytes {
    (0..=255_u8)
        .cycle()
        .take(100_000)
        .collect::<Vec<u8>>()
        .into()
}

/// A local WebSocket app that echoes every message.
pub(crate) async fn websocket_echo() -> u16 {
    use futures_util::{SinkExt as _, StreamExt as _};

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        while let Ok((tcp, _peer)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                while let Some(Ok(message)) = socket.next().await {
                    if (message.is_text() || message.is_binary())
                        && socket.send(message).await.is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    port
}

/// A port on `127.0.0.1` nothing listens on.
pub(crate) async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    listener.local_addr().expect("address").port()
}

/// Waits until the client's state matches.
pub(crate) async fn wait_state(
    client: &TunnelClient,
    matches: impl Fn(&ClientState) -> bool + Send + Sync,
) {
    let mut state = client.state();
    tokio::time::timeout(EXCHANGE, state.wait_for(|s| matches(s)))
        .await
        .expect("the client reaches the state in time")
        .expect("the client runs");
}

/// Waits until the client's task has ended (its state can change no
/// more) and answers the state it ended in.
pub(crate) async fn wait_ended(client: &TunnelClient) -> ClientState {
    let mut state = client.state();
    tokio::time::timeout(EXCHANGE, async { while state.changed().await.is_ok() {} })
        .await
        .expect("the client ends in time");
    state.borrow().clone()
}

/// Starts a client of a configuration the test knows to be valid.
pub(crate) fn start(config: ClientConfig) -> TunnelClient {
    TunnelClient::start(config).expect("a valid client configuration")
}

/// Waits until the client is connected.
pub(crate) async fn wait_connected(client: &TunnelClient) {
    wait_state(client, |s| matches!(s, ClientState::Connected { .. })).await;
}
