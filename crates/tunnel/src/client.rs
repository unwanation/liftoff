//! The CLI's side of the tunnel.

use core::net::{Ipv4Addr, Ipv6Addr};
use core::time::Duration;
use std::sync::Arc;

use futures_util::{SinkExt as _, StreamExt as _};
use http::{HeaderValue, StatusCode, Uri};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::link::{self, AbortOnDrop, Link, Sink, Source};
use crate::mux::{Inbound, MuxStream};
use crate::{Frame, PROTOCOL_VERSION, TunnelError};

/// How long one attempt may take to connect, upgrade and finish the
/// handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How many keepalive intervals the server may stay silent (its pongs
/// included) before the client takes the connection as lost.
const SILENT_INTERVALS: u32 = 3;

/// What the client writes into a stream when the local port refuses.
const BAD_GATEWAY: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\n\
content-type: text/plain; charset=utf-8\r\n\
content-length: 39\r\n\
connection: close\r\n\
\r\n\
the local app does not accept requests\n";

/// How long the client waits before its next attempt to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first failed attempt.
    pub initial: Duration,
    /// The longest wait: the doubling stops here.
    pub max: Duration,
}

impl Default for Backoff {
    /// One second, doubling up to 60 seconds.
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// The wait before attempt `attempt + 1` after `attempt` failures in a
    /// row: `initial`, doubled per failure, never above `max`.
    #[must_use]
    pub fn delay(&self, attempt: u32) -> Duration {
        1_u32
            .checked_shl(attempt)
            .and_then(|factor| self.initial.checked_mul(factor))
            .map_or(self.max, |wait| wait.min(self.max))
    }
}

/// Where and how the client connects. [`TunnelClient::start`] checks
/// it. Its `Debug` hides the session.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// `wss://<api>/api/v1/tunnel` (`ws://` in tests).
    pub url: String,
    /// The CLI session, sent as `Authorization: Bearer`.
    pub token: String,
    /// The port of the developer's app on `127.0.0.1` (or `[::1]`), sent
    /// as `X-Liftoff-Expose`: the daemon finds or creates its tunnel
    /// (step 2).
    pub local_port: u16,
    /// Sent as `X-Liftoff-Share: 1`: the tunnel is open to everyone.
    pub share: bool,
    /// How often the client pings an idle connection.
    pub keepalive: Duration,
    /// The waits between attempts to connect.
    pub backoff: Backoff,
    /// Certificates `wss://` trusts besides the Web PKI roots (a daemon
    /// behind a private CA; the tests' own CA).
    pub extra_roots: Vec<CertificateDer<'static>>,
}

impl core::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("local_port", &self.local_port)
            .field("share", &self.share)
            .field("keepalive", &self.keepalive)
            .field("backoff", &self.backoff)
            .field("extra_roots", &self.extra_roots.len())
            .finish()
    }
}

impl ClientConfig {
    /// A configuration of a private tunnel with the default keepalive
    /// (15 seconds) and [`Backoff::default`].
    #[must_use]
    pub fn new(url: String, token: String, local_port: u16) -> Self {
        Self {
            url,
            token,
            local_port,
            share: false,
            keepalive: Duration::from_secs(15),
            backoff: Backoff::default(),
            extra_roots: Vec::new(),
        }
    }
}

/// A [`ClientConfig`] checked once, before the first attempt.
#[derive(Debug)]
struct Target {
    uri: Uri,
    host: String,
    port: u16,
    /// For `wss://`: the TLS configuration and the name the daemon's
    /// certificate must carry.
    tls: Option<(Arc<rustls::ClientConfig>, ServerName<'static>)>,
    bearer: HeaderValue,
    local_port: u16,
    share: bool,
    keepalive: Duration,
    backoff: Backoff,
}

impl Target {
    fn new(config: ClientConfig) -> Result<Self, TunnelError> {
        let invalid = |what: &str| TunnelError::Invalid(what.to_owned());
        let uri: Uri = config
            .url
            .parse()
            .map_err(|_bad| invalid("the tunnel URL is no URL"))?;
        let secure = match uri.scheme_str() {
            Some("wss") => true,
            Some("ws") => false,
            _ => return Err(invalid("the tunnel URL is no ws:// or wss:// URL")),
        };
        let host = uri
            .host()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| invalid("the tunnel URL names no host"))?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        let tls = if secure {
            let name = ServerName::try_from(host.clone())
                .map_err(|_bad| invalid("the tunnel URL's host is no TLS server name"))?;
            Some((tls_config(config.extra_roots)?, name))
        } else {
            None
        };
        let mut bearer = HeaderValue::try_from(format!("Bearer {}", config.token))
            .map_err(|_bad| invalid("the session is no header value"))?;
        bearer.set_sensitive(true);
        Ok(Self {
            uri,
            host,
            port,
            tls,
            bearer,
            local_port: config.local_port,
            share: config.share,
            keepalive: config.keepalive,
            backoff: config.backoff,
        })
    }
}

/// The TLS configuration of `wss://`: the Web PKI roots and `extra`.
fn tls_config(
    extra: Vec<CertificateDer<'static>>,
) -> Result<Arc<rustls::ClientConfig>, TunnelError> {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    for certificate in extra {
        roots
            .add(certificate)
            .map_err(|error| TunnelError::Invalid(format!("an extra root: {error}")))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| TunnelError::Invalid(error.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Where the client is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientState {
    /// Connecting, or waiting [`Backoff::delay`] before the next attempt.
    Connecting,
    /// The handshake succeeded: the server's streams reach the local port.
    Connected {
        /// The environment the server named in its `Hello`.
        environment: String,
        /// `X-Liftoff-Address` of the upgrade: the tunnel's `https://`
        /// address.
        address: Option<String>,
        /// `X-Liftoff-Login-Url` of the upgrade: the one-time link that
        /// logs a browser in to a private environment (DEVT-07); `None`
        /// for a shared one.
        login_url: Option<String>,
    },
    /// A newer connection of the same environment replaced this one; the
    /// client stopped and does not reconnect.
    Evicted,
    /// The daemon refused the upgrade for good (`401`: the session is
    /// missing or expired, `403`: not allowed, any other `4xx` but `408`
    /// and `429`: the request itself, a port another user exposes); the
    /// client stopped and does not reconnect.
    Refused {
        /// The status of the daemon's answer.
        status: StatusCode,
        /// The body of the answer, as text, if it had one.
        detail: Option<String>,
    },
    /// [`TunnelClient::shutdown`] ended it.
    Stopped,
}

/// The running client: connects, serves the server's streams from the
/// local port and reconnects until it is shut down or evicted. Dropping
/// it stops it without a [`Frame::Shutdown`].
#[derive(Debug)]
pub struct TunnelClient {
    state: watch::Receiver<ClientState>,
    stop: watch::Sender<bool>,
    task: Option<AbortOnDrop>,
}

impl TunnelClient {
    /// Checks `config` and starts the client on the current tokio
    /// runtime.
    ///
    /// # Errors
    ///
    /// [`TunnelError::Invalid`] when `config` cannot work: its URL is no
    /// `ws://` or `wss://` URL with a host, or its token or project is no
    /// header value.
    pub fn start(config: ClientConfig) -> Result<Self, TunnelError> {
        let target = Target::new(config)?;
        let (state_tx, state) = watch::channel(ClientState::Connecting);
        let (stop, stopped) = watch::channel(false);
        let task = AbortOnDrop(tokio::spawn(run(target, state_tx, stopped)));
        Ok(Self {
            state,
            stop,
            task: Some(task),
        })
    }

    /// The client's state, updated as it changes. Once the client ended
    /// (evicted, refused or stopped) the sender is gone and
    /// [`watch::Receiver::changed`] fails.
    #[must_use]
    pub fn state(&self) -> watch::Receiver<ClientState> {
        self.state.clone()
    }

    /// Sends [`Frame::Shutdown`], closes the
    /// connection and stops reconnecting (Ctrl-C of `lof expose`).
    pub async fn shutdown(mut self) {
        self.stop.send_replace(true);
        if let Some(mut task) = self.task.take()
            && let Err(error) = (&mut task.0).await
        {
            tracing::debug!(%error, "the tunnel client ended abnormally");
        }
    }
}

/// How one connection ended.
enum Outcome {
    /// It never got through the handshake.
    Failed(TunnelError),
    /// It was connected and broke.
    Lost,
    /// The server sent [`Frame::Evicted`].
    Evicted,
    /// [`TunnelClient::shutdown`].
    Stopped,
}

/// Connects again and again until stopped, evicted or refused.
async fn run(target: Target, state: watch::Sender<ClientState>, mut stop: watch::Receiver<bool>) {
    let mut failures: u32 = 0;
    loop {
        state.send_replace(ClientState::Connecting);
        match session(&target, &state, &mut stop).await {
            Outcome::Stopped => break,
            // The caller tells the person from the state; the log is for
            // the record only.
            Outcome::Evicted => {
                tracing::info!("another `lof expose` of this port took over the tunnel");
                state.send_replace(ClientState::Evicted);
                return;
            }
            Outcome::Failed(TunnelError::Refused { status, detail }) => {
                tracing::info!(%status, "the daemon refused the tunnel");
                state.send_replace(ClientState::Refused { status, detail });
                return;
            }
            Outcome::Lost => {
                tracing::info!("the tunnel connection broke; reconnecting");
                failures = 0;
            }
            // The first failure in a row is worth the person's attention,
            // the retries after it are not.
            Outcome::Failed(error) if failures == 0 => {
                tracing::warn!(%error, "the tunnel did not connect; retrying");
            }
            Outcome::Failed(error) => tracing::debug!(%error, "the tunnel did not connect"),
        }
        let wait = target.backoff.delay(failures);
        failures = failures.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = stopped(&mut stop) => break,
        }
    }
    state.send_replace(ClientState::Stopped);
}

/// Resolves once the client is told to stop (or its handle is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    drop(stop.wait_for(|stop| *stop).await);
}

/// One connection, from the upgrade to its end.
async fn session(
    config: &Target,
    state: &watch::Sender<ClientState>,
    stop: &mut watch::Receiver<bool>,
) -> Outcome {
    let connecting = tokio::time::timeout(CONNECT_TIMEOUT, handshake(config));
    let (sink, mut source, environment, address, login_url) = tokio::select! {
        // A handshake that ended in the same wake-up as the stop is the
        // tunnel: it ends below with `Shutdown`, which the server, having
        // taken the client's `Hello`, waits for.
        biased;
        connected = connecting => match connected {
            Ok(Ok(connected)) => connected,
            Ok(Err(error)) => return Outcome::Failed(error),
            Err(_elapsed) => return Outcome::Failed(TunnelError::Connection("timed out".to_owned())),
        },
        () = stopped(stop) => return Outcome::Stopped,
    };
    state.send_replace(ClientState::Connected {
        environment,
        address,
        login_url,
    });

    let (mut link, mux_io) = Link::start(sink, Some(config.keepalive), None);
    let port = config.local_port;
    let _accept = AbortOnDrop(tokio::spawn(async move {
        let mut inbound = Inbound::client(mux_io);
        while let Some(stream) = inbound.next().await {
            tokio::spawn(relay(stream, port));
        }
    }));
    let silence = config.keepalive.saturating_mul(SILENT_INTERVALS);
    let end = read(&mut source, &mut link, silence, stop).await;
    let last = matches!(end, Outcome::Stopped).then_some(Frame::Shutdown);
    link.finish(last).await;
    if matches!(end, Outcome::Stopped) {
        link::drain(&mut source).await;
    }
    end
}

/// Reads the server's messages until the connection ends.
async fn read<S>(
    source: &mut Source<S>,
    link: &mut Link,
    silence: Duration,
    stop: &mut watch::Receiver<bool>,
) -> Outcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let deadline = tokio::time::sleep(silence);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            message = source.next() => {
                deadline.set(tokio::time::sleep(silence));
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        if link.to_mux.write_all(&bytes).await.is_err() {
                            return Outcome::Lost;
                        }
                    }
                    Some(Ok(Message::Text(text))) => match link::parse(text.as_str()) {
                        Ok(Frame::Evicted) => return Outcome::Evicted,
                        Ok(frame) => tracing::debug!(?frame, "an unexpected frame from the server"),
                        Err(error) => tracing::debug!(%error, "a bad frame from the server"),
                    },
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return Outcome::Lost,
                }
            }
            () = &mut deadline => return Outcome::Lost,
            () = stopped(stop) => return Outcome::Stopped,
        }
    }
}

/// The IO a WebSocket runs over: TCP, or TLS over it for `wss://`.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// The halves of a connected WebSocket, the environment of the server's
/// `Hello` and the upgrade's login link.
type Connected = (
    Sink<Box<dyn Io>>,
    Source<Box<dyn Io>>,
    String,
    Option<String>,
    Option<String>,
);

/// The header of the upgrade's answer that carries the login link.
const LOGIN_URL_HEADER: &str = "x-liftoff-login-url";

/// The header of the upgrade's answer that carries the tunnel's address.
const ADDRESS_HEADER: &str = "x-liftoff-address";

/// The longest body of a refusal [`ClientState::Refused`] keeps.
const DETAIL_MAX: usize = 1024;

/// Connects, upgrades with the session, the port and `--share`, and
/// answers the server's `Hello`.
async fn handshake(config: &Target) -> Result<Connected, TunnelError> {
    let connection = |error: &dyn core::fmt::Display| TunnelError::Connection(error.to_string());
    let mut request = config
        .uri
        .clone()
        .into_client_request()
        .map_err(|e| connection(&e))?;
    let headers = request.headers_mut();
    headers.insert(http::header::AUTHORIZATION, config.bearer.clone());
    headers.insert("x-liftoff-expose", HeaderValue::from(config.local_port));
    if config.share {
        headers.insert("x-liftoff-share", HeaderValue::from_static("1"));
    }

    let tcp = TcpStream::connect((config.host.as_str(), config.port))
        .await
        .map_err(|e| connection(&e))?;
    let io: Box<dyn Io> = match &config.tls {
        Some((tls, name)) => Box::new(secure(tcp, name.clone(), Arc::clone(tls)).await?),
        None => Box::new(tcp),
    };
    let upgraded =
        tokio_tungstenite::client_async_with_config(request, io, Some(link::websocket_config()))
            .await;
    let (socket, response): (WebSocketStream<Box<dyn Io>>, _) = match upgraded {
        Ok(upgraded) => upgraded,
        Err(WsError::Http(response)) if is_final(response.status()) => {
            let detail = response
                .body()
                .as_deref()
                .map(|body| String::from_utf8_lossy(body).trim().to_owned())
                .filter(|body| !body.is_empty())
                .map(|body| body.chars().take(DETAIL_MAX).collect());
            return Err(TunnelError::Refused {
                status: response.status(),
                detail,
            });
        }
        Err(error) => return Err(connection(&error)),
    };

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let (address, login_url) = (header(ADDRESS_HEADER), header(LOGIN_URL_HEADER));
    let (mut sink, mut source) = socket.split();
    let hello = link::next_frame(&mut source).await?;
    let environment = match hello {
        Frame::Hello {
            environment,
            version,
        } if version == PROTOCOL_VERSION => environment,
        Frame::Hello { version, .. } => {
            return Err(TunnelError::Protocol(format!(
                "the server speaks version {version}, the client {PROTOCOL_VERSION}"
            )));
        }
        Frame::Shutdown | Frame::Evicted => {
            return Err(TunnelError::Protocol("the server sent no Hello".to_owned()));
        }
    };
    let answer = Frame::Hello {
        environment: environment.clone(),
        version: PROTOCOL_VERSION,
    };
    sink.send(link::text(&answer))
        .await
        .map_err(|e| connection(&e))?;
    Ok((sink, source, environment, address, login_url))
}

/// Whether the daemon's `status` refuses the upgrade for good: a `4xx`
/// other than a timeout (`408`) or too many requests (`429`), which a
/// later attempt may pass.
fn is_final(status: StatusCode) -> bool {
    status.is_client_error()
        && status != StatusCode::REQUEST_TIMEOUT
        && status != StatusCode::TOO_MANY_REQUESTS
}

/// TLS over `tcp` to `host` with the client's TLS configuration.
async fn secure(
    tcp: TcpStream,
    name: ServerName<'static>,
    config: Arc<rustls::ClientConfig>,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, TunnelError> {
    let connection = |error: &dyn core::fmt::Display| TunnelError::Connection(error.to_string());
    tokio_rustls::TlsConnector::from(config)
        .connect(name, tcp)
        .await
        .map_err(|e| connection(&e))
}

/// Connects one stream of the server to the local port, on `127.0.0.1`
/// and else on `[::1]` (a dev server on `localhost` may listen on IPv6
/// only), and relays the bytes both ways; answers `502` itself when the
/// port refuses on both.
async fn relay(mut stream: MuxStream, port: u16) {
    let local = match TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await {
        Ok(local) => Ok(local),
        Err(v4) => TcpStream::connect((Ipv6Addr::LOCALHOST, port))
            .await
            .map_err(|_v6| v4),
    };
    match local {
        Ok(mut local) => {
            if let Err(error) = tokio::io::copy_bidirectional(&mut stream, &mut local).await {
                tracing::debug!(%error, "a tunnel stream ended with an error");
            }
        }
        Err(error) => {
            tracing::warn!(%error, port, "the local app does not accept connections");
            if let Err(error) = stream.write_all(BAD_GATEWAY).await {
                tracing::debug!(%error, "the 502 did not reach the tunnel");
            }
            drop(stream.shutdown().await);
        }
    }
}
