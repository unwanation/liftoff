//! Sending an allowed `wasi:http` request to the addresses `http.rs`
//! checked (MOD-07).
//!
//! `wasmtime-wasi-http`'s own client (`default_send_request`) connects to
//! the request's authority, which resolves the host a second time: a DNS
//! server answering a public address to the check and a loopback one to
//! the connection (rebinding, TTL 0) would reach the daemon's REST or the
//! Caddy admin API. This client is that one with the connection made to
//! the checked addresses: TCP to one of them, TLS (`rustls`, the
//! `webpki-roots` roots) verifying the request's host, HTTP/1.1 over
//! `hyper`, the same timeouts.

use core::future::poll_fn;
use core::net::{IpAddr, SocketAddr};
use core::pin::{Pin, pin};
use core::task::{Context, Poll, ready};
use core::time::Duration;
use std::sync::Arc;

use bytes::Bytes;
use http::uri::Scheme;
use http_body::{Body, Frame, SizeHint};
use http_body_util::BodyExt;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody};

/// The connection of a sent request: it ends when the response body is
/// read or the connection fails.
pub(crate) type Connection = Box<dyn Future<Output = Result<(), Error>> + Send>;

/// What `wasmtime-wasi-http` waits by default for a connection, the first
/// byte and every next one.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// A plain or TLS stream to the server.
trait Stream: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static> Stream for T {}

/// The name TLS verifies the server's certificate against: the request's
/// host (`host` without the brackets of an IPv6 literal).
fn server_name(host: &str) -> Result<ServerName<'static>, Error> {
    host.parse::<IpAddr>().map_or_else(
        |_name| {
            ServerName::try_from(host.to_owned()).map_err(|_invalid| Error::HttpRequestUriInvalid)
        },
        |ip| Ok(ServerName::from(ip)),
    )
}

/// A connected stream to one of `addresses`, TLS verifying `host` when
/// `tls`, within `timeout`.
async fn connect(
    addresses: &[SocketAddr],
    host: &str,
    tls: bool,
    timeout: Duration,
) -> Result<Box<dyn Stream>, Error> {
    let tcp = tokio::time::timeout(timeout, TcpStream::connect(addresses))
        .await
        .map_err(|_elapsed| Error::ConnectionTimeout)?
        .map_err(Error::Connect)?;
    if !tls {
        return Ok(Box::new(tcp));
    }
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    // The provider is named: the binary also links aws-lc-rs through
    // reqwest, and rustls panics when it has to pick the process default
    // between two providers (the tunnel client names it the same way).
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::Tls(std::io::Error::other(error)))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = tokio::time::timeout(timeout, connector.connect(server_name(host)?, tcp))
        .await
        .map_err(|_elapsed| Error::ConnectionTimeout)?
        .map_err(Error::Tls)?;
    Ok(Box::new(stream))
}

/// A response body that fails when the server pauses longer than the
/// request's `between_bytes_timeout`.
struct Incoming {
    incoming: hyper::body::Incoming,
    timeout: tokio::time::Interval,
}

impl Body for Incoming {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        match Pin::new(&mut self.incoming).poll_frame(cx) {
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(if error.is_timeout() {
                Error::HttpResponseTimeout
            } else {
                Error::from(error)
            }))),
            Poll::Ready(Some(Ok(frame))) => {
                self.timeout.reset();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Pending => {
                ready!(self.timeout.poll_tick(cx));
                Poll::Ready(Some(Err(Error::ConnectionReadTimeout)))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.incoming.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.incoming.size_hint()
    }
}

/// Sends `request`, whose host is `host`, to one of `addresses` (already
/// checked): its response and its connection.
///
/// # Errors
///
/// The `wasi:http` error code of a connection, TLS or protocol failure,
/// or of a timeout of `options`.
pub(crate) async fn send(
    mut request: http::Request<WasiBody>,
    options: Option<RequestOptions>,
    addresses: &[SocketAddr],
    host: &str,
) -> Result<(http::Response<WasiBody>, Connection), Error> {
    let tls = request.uri().scheme() == Some(&Scheme::HTTPS);
    let connect_timeout = options
        .and_then(|options| options.connect_timeout)
        .unwrap_or(DEFAULT_TIMEOUT);
    let first_byte_timeout = options
        .and_then(|options| options.first_byte_timeout)
        .unwrap_or(DEFAULT_TIMEOUT);
    // `tokio::time::interval` panics on zero.
    let between_bytes_timeout = options
        .and_then(|options| options.between_bytes_timeout)
        .unwrap_or(DEFAULT_TIMEOUT)
        .max(Duration::from_nanos(1));
    let stream = connect(addresses, host, tls, connect_timeout).await?;
    let (mut sender, connection) = tokio::time::timeout(
        connect_timeout,
        hyper::client::conn::http1::Builder::new().handshake(TokioIo::new(stream)),
    )
    .await
    .map_err(|_elapsed| Error::ConnectionTimeout)??;
    // The request line carries the path only: scheme and authority go to
    // a proxy, and `hyper` does not drop them.
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_owned(), ToString::to_string);
    *request.uri_mut() = http::Uri::builder()
        .path_and_query(path)
        .build()
        .map_err(|_invalid| Error::HttpRequestUriInvalid)?;
    let response = async move {
        let response = tokio::time::timeout(first_byte_timeout, sender.send_request(request))
            .await
            .map_err(|_elapsed| Error::ConnectionReadTimeout)??;
        let mut timeout = tokio::time::interval(between_bytes_timeout);
        timeout.reset();
        Ok::<_, Error>(response.map(|incoming| Incoming { incoming, timeout }))
    };
    let mut response = pin!(response);
    let mut connection = Some(connection);
    // Drive the connection until the response head arrives.
    let response = poll_fn(|cx| match response.as_mut().poll(cx) {
        Poll::Ready(answered) => Poll::Ready(answered),
        Poll::Pending => {
            let Some(driving) = connection.as_mut() else {
                return Poll::Pending;
            };
            let ended = ready!(Pin::new(driving).poll(cx));
            connection = None;
            match ended {
                Ok(()) => response.as_mut().poll(cx),
                Err(error) => Poll::Ready(Err(Error::from(error))),
            }
        }
    })
    .await?;
    let connection: Connection = Box::new(async move {
        let Some(connection) = connection else {
            return Ok(());
        };
        connection.await.map_err(|error| {
            if error.is_timeout() {
                Error::HttpResponseTimeout
            } else {
                Error::from(error)
            }
        })
    });
    Ok((response.map(BodyExt::boxed_unsync), connection))
}
