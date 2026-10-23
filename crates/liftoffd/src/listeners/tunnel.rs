//! The tunnel listener on `[tunnel] bind`.
//!
//! Caddy sends it every request for the dev address of a running tunnel
//! environment, `Host` unchanged. The listener:
//!
//! - takes connections from loopback only (the peer is checked);
//! - finds the environment by `Host` → domain → binding → tunnel
//!   environment ([`ListenerRoute`]); any other host is `404`;
//! - asks [`Access::check`] before a single byte reaches the tunnel
//!   (DEVT-05): a `private` environment without a session sends a
//!   browser to the login (`302`) and a JSON client gets `401`; a session
//!   of someone who may not open it is `403`, the forbidden page;
//! - hands the request to the environment's tunnel
//!   ([`TunnelServer::forward`], keyed by the environment's id) without
//!   liftoff's cookies, a liftoff bearer, the browser's `X-Forwarded-*`
//!   and `X-Liftoff-*` (it sets its own `X-Forwarded-For`, `-Proto`,
//!   `-Host`, and `X-Liftoff-User`, `-Email` of the person it let
//!   through), and answers `503 dev offline` when no client is connected
//!   or the environment is `offline` (for the latter, the daemon's
//!   offline page of or a module's `render_page`);
//! - a path under `/.liftoff/` is liftoff's on every address, never
//!   passed to the tunnel: the session handover's `/.liftoff/callback`
//!   and `/.liftoff/logout`, any other `404`;
//! - an application's own WebSocket upgrade crosses the tunnel as it
//!   came (the connection is served with upgrades).
//!
//! [`Access::check`]: liftoff_core::service::Access::check
//! [`ListenerRoute`]: liftoff_core::service::ListenerRoute

use core::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use liftoff_core::domain::{EnvKind, EnvState, Host, LinkTarget, User};
use liftoff_core::ports::{PageKind, Ports};
use liftoff_core::service::{Access, AccessDecision, GateListener, ListenerRoute, Settings};
use liftoff_rest::pages;
use liftoff_tunnel::{ResponseBody, TunnelServer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use super::common::{
    LIFTOFF_PATHS, access_request, admits, failure, host_of, liftoff_path, login_url, page,
    redirect, strip_liftoff, text,
};

/// The listener's name in the log.
const LISTENER: &str = "tunnel";

/// The headers of this connection that mean nothing to the developer's
/// app; `Connection` and `Upgrade` pass, so an application's WebSocket
/// upgrade crosses the tunnel. `Proxy-Authorization` is a credential for
/// a proxy.
const HOP_BY_HOP: [HeaderName; 4] = [
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::PROXY_AUTHORIZATION,
    header::TE,
];

/// The prefix of the forwarding headers the listener sets itself.
const FORWARDED_PREFIX: &str = "x-forwarded-";

/// See [`FORWARDED_PREFIX`].
const FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
/// See [`FORWARDED_PREFIX`].
const FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
/// See [`FORWARDED_PREFIX`].
const FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

/// See the [module documentation](self).
#[derive(Debug)]
pub struct TunnelListener<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
    server: TunnelServer,
}

impl<P> Clone for TunnelListener<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
            server: self.server.clone(),
        }
    }
}

impl<P: Ports + 'static> TunnelListener<P> {
    /// The listener over `ports` (the store it routes by, the sessions
    /// and hooks [`Access::check`](liftoff_core::service::Access::check)
    /// reads) configured by `settings`, forwarding into the tunnels of
    /// `server`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>, server: TunnelServer) -> Self {
        Self {
            ports,
            settings,
            server,
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
        tracing::info!(%addr, "tunnel listener listening");
        // Each connection holds a receiver: `closed` resolves once the
        // last one finished its request after the stop (hyper-util's
        // `GracefulShutdown` does not take upgradeable connections).
        let (graceful, unused) = watch::channel(false);
        drop(unused);
        let mut stop = core::pin::pin!(stop);
        loop {
            let accepted = tokio::select! {
                () = &mut stop => break,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, peer)) => self.connection(stream, peer, graceful.subscribe()),
                // A failed accept ends that connection, never the listener.
                Err(error) => tracing::warn!(%error, "the tunnel listener could not accept"),
            }
        }
        let _no_connections = graceful.send(true);
        graceful.closed().await;
        tracing::info!(%addr, "tunnel listener stopped");
        Ok(())
    }

    /// Serves one connection of `peer` in its own task, until `stop`
    /// turns `true` and its current request ended.
    fn connection(&self, stream: TcpStream, peer: SocketAddr, mut stop: watch::Receiver<bool>) {
        if !admits(peer) {
            tracing::warn!(%peer, "refused a tunnel request from outside the machine");
            return;
        }
        let listener = self.clone();
        let service = service_fn(move |request: Request<Incoming>| {
            let listener = listener.clone();
            async move { Ok::<_, core::convert::Infallible>(listener.handle(request, peer).await) }
        });
        let connection = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades();
        drop(tokio::spawn(async move {
            let mut connection = core::pin::pin!(connection);
            let ended = tokio::select! {
                ended = connection.as_mut() => ended,
                () = stopped(&mut stop) => {
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(error) = ended {
                tracing::debug!(%error, %peer, "a tunnel connection ended with an error");
            }
            drop(stop);
        }));
    }

    /// One request of `peer` (see the [module documentation](self)).
    async fn handle(
        &self,
        mut request: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<ResponseBody> {
        let Some(host) = host_of(request.headers()) else {
            return own(text(StatusCode::NOT_FOUND, "not found"));
        };
        let gate = GateListener::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        if request.uri().path().starts_with(LIFTOFF_PATHS) {
            return own(liftoff_path(&gate, &host, &request, LISTENER).await);
        }
        let route = ListenerRoute::new(Arc::clone(&self.ports));
        let environment = match route.environment_of(&host, EnvKind::Tunnel).await {
            Ok(Some(environment)) => environment,
            Ok(None) => return own(text(StatusCode::NOT_FOUND, "not found")),
            Err(error) => return own(failure(LISTENER, &error)),
        };
        if environment.state != EnvState::Running {
            return own(page(
                &gate,
                StatusCode::SERVICE_UNAVAILABLE,
                PageKind::Offline,
                (&host, &request),
                pages::offline,
            )
            .await);
        }
        let access = Access::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        let decision = access
            .check(
                &access_request(&host, &request),
                &LinkTarget::Environment(environment.id),
            )
            .await;
        let user = match decision {
            Ok(AccessDecision::Allow { user }) => user,
            Ok(AccessDecision::Login) => {
                return own(redirect(&login_url(
                    &self.settings.proxy.api_host,
                    &host,
                    &request,
                )));
            }
            Ok(AccessDecision::Unauthenticated) => {
                return own(text(
                    StatusCode::UNAUTHORIZED,
                    "sign in to open this address",
                ));
            }
            Ok(AccessDecision::Forbidden(reason)) => {
                return own(page(
                    &gate,
                    StatusCode::FORBIDDEN,
                    PageKind::Forbidden,
                    (&host, &request),
                    || pages::forbidden(&reason),
                )
                .await);
            }
            Err(error) => return own(failure(LISTENER, &error)),
        };
        // Running but without its client: lost and waiting for it within
        // `offline_after` (the browser gets the offline page).
        if !self.server.is_connected(&environment.id.to_string()) {
            return own(page(
                &gate,
                StatusCode::SERVICE_UNAVAILABLE,
                PageKind::Offline,
                (&host, &request),
                pages::offline,
            )
            .await);
        }
        forwarded_headers(request.headers_mut(), &host, peer, user.as_ref());
        self.server
            .forward(&environment.id.to_string(), request)
            .await
    }
}

/// Resolves once `stop` turned `true` (or its sender is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _stopped = stop.wait_for(|stopped| *stopped).await.map(|_seen| ());
}

/// An answer of the listener's own as the body type of the tunnel's.
fn own(reply: Response<Full<bytes::Bytes>>) -> Response<ResponseBody> {
    reply.map(|body| body.map_err(|never| match never {}).boxed())
}

/// The headers of the request as the developer's app gets them: the
/// hop-by-hop headers of this connection but `Connection` and `Upgrade`
/// removed, what is liftoff's removed ([`strip_liftoff`]), and the
/// forwarding headers the listener's own (DEVT-13): whatever the browser
/// sent as `X-Forwarded-*` or `Forwarded` is dropped, `X-Forwarded-For` is
/// the client Caddy reported ([`client_of`]), `X-Forwarded-Proto` is
/// `https` (a dev address is served over TLS only) and
/// `X-Forwarded-Host` is the dev address.
fn forwarded_headers(headers: &mut HeaderMap, host: &Host, peer: SocketAddr, user: Option<&User>) {
    let client = client_of(headers, peer);
    for name in &HOP_BY_HOP {
        drop(headers.remove(name));
    }
    let forwarded: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with(FORWARDED_PREFIX) || *name == header::FORWARDED)
        .cloned()
        .collect();
    for name in forwarded {
        drop(headers.remove(name));
    }
    strip_liftoff(headers, user);
    if let Ok(client) = HeaderValue::try_from(client.to_string()) {
        let _previous = headers.insert(FORWARDED_FOR, client);
    }
    let _previous = headers.insert(FORWARDED_PROTO, HeaderValue::from_static("https"));
    if let Ok(host) = HeaderValue::try_from(host.as_str()) {
        let _previous = headers.insert(FORWARDED_HOST, host);
    }
}

/// The client of the request: the last address of `X-Forwarded-For`,
/// which the local Caddy (the only peer the listener serves, and one that
/// trusts no proxy before it) sets to the address it was asked from; the
/// peer itself without one.
fn client_of(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    headers
        .get_all(&FORWARDED_FOR)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
        .next_back()
        .unwrap_or_else(|| peer.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod admits {
        use super::*;

        // TC: DEVT-05 (the listener takes only the local Caddy)
        #[test]
        fn admitted_when_peer_is_loopback() {
            assert!(admits(SocketAddr::from(([127, 0, 0, 1], 50000))));
        }

        // TC: DEVT-05
        #[test]
        fn admitted_when_peer_is_ipv6_loopback() {
            assert!(admits("[::1]:50000".parse().unwrap()));
        }

        // TC: DEVT-05
        #[test]
        fn refused_when_peer_is_outside_the_machine() {
            assert!(!admits(SocketAddr::from(([203, 0, 113, 7], 50000))));
        }
    }

    mod forwarded_headers {
        use super::*;

        const PEER: SocketAddr = SocketAddr::new(IpAddr::V4(core::net::Ipv4Addr::LOCALHOST), 50000);

        fn host() -> Host {
            Host::new("shop-dev.apps.test").unwrap()
        }

        /// What the app gets of `sent`, as `(name, value)` pairs.
        fn forwarded(sent: &[(&'static str, &'static str)]) -> Vec<(String, String)> {
            let mut headers = HeaderMap::new();
            for (name, value) in sent {
                let _previous = headers.append(*name, HeaderValue::from_static(value));
            }
            forwarded_headers(&mut headers, &host(), PEER, None);
            let mut pairs: Vec<(String, String)> = headers
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap().to_owned()))
                .collect();
            pairs.sort();
            pairs
        }

        // TC: DEVT-13 (the browser's forged forwarding headers do not
        // reach the app: it sees the listener's)
        #[test]
        fn listeners_forwarding_headers_when_browser_forged_them() {
            let pairs = forwarded(&[
                ("x-forwarded-for", "10.9.9.9, 198.51.100.4"),
                ("x-forwarded-proto", "gopher"),
                ("x-forwarded-host", "evil.example"),
                ("x-forwarded-port", "1"),
                ("forwarded", "for=10.9.9.9"),
            ]);
            assert_eq!(
                pairs,
                vec![
                    ("x-forwarded-for".to_owned(), "198.51.100.4".to_owned()),
                    (
                        "x-forwarded-host".to_owned(),
                        "shop-dev.apps.test".to_owned()
                    ),
                    ("x-forwarded-proto".to_owned(), "https".to_owned()),
                ]
            );
        }

        // TC: DEVT-13 (`X-Liftoff-*` of the browser never arrives)
        #[test]
        fn no_liftoff_headers_when_browser_sent_them() {
            let pairs = forwarded(&[("x-liftoff-user", "admin"), ("x-custom", "kept")]);
            assert_eq!(
                pairs
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .filter(|name| !name.starts_with(FORWARDED_PREFIX))
                    .collect::<Vec<_>>(),
                vec!["x-custom"]
            );
        }

        // TC: DEVT-13 (liftoff's bearer and cookies stay with liftoff)
        #[test]
        fn no_liftoff_credentials_when_request_carried_them() {
            let pairs = forwarded(&[
                ("authorization", "Bearer lfs_secret"),
                ("cookie", "__Host-liftoff_gate=x; app=1"),
            ]);
            assert_eq!(
                pairs
                    .into_iter()
                    .filter(|(name, _)| !name.starts_with(FORWARDED_PREFIX))
                    .collect::<Vec<_>>(),
                vec![("cookie".to_owned(), "app=1".to_owned())]
            );
        }

        // TC: DEVT-13 (the peer is the client without Caddy's header)
        #[test]
        fn peer_is_client_when_no_forwarded_for() {
            let pairs = forwarded(&[]);
            assert!(
                pairs.contains(&("x-forwarded-for".to_owned(), "127.0.0.1".to_owned())),
                "{pairs:?}"
            );
        }

        // An application's WebSocket upgrade crosses the tunnel.
        #[test]
        fn upgrade_headers_pass_when_app_upgrades() {
            let pairs = forwarded(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
            assert!(
                pairs.contains(&("connection".to_owned(), "Upgrade".to_owned()))
                    && pairs.contains(&("upgrade".to_owned(), "websocket".to_owned())),
                "{pairs:?}"
            );
        }
    }
}
