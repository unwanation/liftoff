//! `GET /api/v1/tunnel`: the WebSocket upgrade of `lof expose`.
//!
//! The CLI sends its `cli` session as the bearer, the port in
//! `X-Liftoff-Expose` and, for `--share`, `X-Liftoff-Share: 1`. The
//! route answers `401` without a live session and `403` for a deploy
//! token or a session that is no `cli` one (DEVT-06), asks
//! [`TunnelsService::expose`](liftoff_core::service::TunnelsService::expose)
//! for the caller's tunnel of the port with the client's address (the
//! peer, or for the local proxy's request the address it recorded last
//! in `X-Forwarded-For`), answers `101` with `X-Liftoff-Address` (the
//! tunnel's `https://` address) and `X-Liftoff-Login-Url` (the one-time
//! link of a private tunnel, DEVT-07), and hands the upgraded connection
//! to the installed [`TunnelHandoff`]: the daemon's tunnel server.

use core::fmt;
use core::net::{IpAddr, SocketAddr};
use core::pin::Pin;
use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use hyper_util::rt::TokioIo;
use liftoff_core::domain::{Bearer, ContainerPort, Environment, SessionKind};
use liftoff_core::ports::Ports;
use liftoff_core::service::Exposure;
use sha1::{Digest as _, Sha1};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::ApiError;
use crate::routes::auth::{bearer, hook_request};
use crate::services::Services;

/// The port `lof expose` exposes.
const EXPOSE_HEADER: HeaderName = HeaderName::from_static("x-liftoff-expose");

/// `1` for `lof expose --share`.
const SHARE_HEADER: HeaderName = HeaderName::from_static("x-liftoff-share");

/// The tunnel's address, `https://<name>.localhost.<base>`, for `lof
/// expose` to print.
const ADDRESS_HEADER: HeaderName = HeaderName::from_static("x-liftoff-address");

/// What the local proxy records of the client it forwards.
const FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The one-time link of a private tunnel, for `lof expose` to print
/// (DEVT-07).
const LOGIN_URL_HEADER: HeaderName = HeaderName::from_static("x-liftoff-login-url");

/// The GUID RFC 6455 (4.2.2) appends to the client's key.
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The raw connection after the `101` of `GET /api/v1/tunnel`.
pub trait UpgradedStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> UpgradedStream for T {}

/// The upgraded connection, as [`TunnelHandoff::serve`] gets it.
pub type UpgradedIo = Box<dyn UpgradedStream>;

/// What `GET /api/v1/tunnel` hands the upgraded connection to: the
/// daemon's tunnel server (`crates/liftoffd`), a fake in tests.
///
/// Object-safe on purpose: [`AppState`](crate::AppState) stores it
/// without another type parameter.
pub trait TunnelHandoff: fmt::Debug + Send + Sync + 'static {
    /// Runs `io` as the tunnel of `environment` until the connection
    /// ends.
    fn serve(
        &self,
        environment: Environment,
        io: UpgradedIo,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
}

/// The installed handoff, as a router extension.
#[derive(Debug, Clone, Default)]
pub(crate) struct TunnelSlot(pub(crate) Option<Arc<dyn TunnelHandoff>>);

/// `GET /api/v1/tunnel` (see the [module documentation](self)).
pub(crate) async fn upgrade<P: Ports>(
    State(services): State<Services<P>>,
    Extension(slot): Extension<TunnelSlot>,
    mut request: Request,
) -> Result<Response, ApiError> {
    let bearer = bearer(&request);
    if matches!(bearer, Some(Bearer::Deploy(_))) {
        return Err(refused("a deploy token opens no tunnel; run `lof login`"));
    }
    let principal = services
        .auth
        .authenticate(&hook_request(&request), bearer.as_ref())
        .await?;
    if !principal
        .session()
        .is_some_and(|session| session.kind == SessionKind::Cli)
    {
        return Err(refused("a tunnel needs the session of `lof login`"));
    }
    let exposure = exposure_of(&request)?;
    let accept = accept_key(request.headers())?;
    let Some(handoff) = slot.0 else {
        tracing::error!("GET /api/v1/tunnel has no tunnel server installed");
        return Err(ApiError::unavailable());
    };
    let opened = services.tunnels.expose(principal.user(), exposure).await?;
    let address =
        HeaderValue::try_from(format!("https://{}", opened.host)).map_err(|_invalid| {
            tracing::error!("the address of a tunnel is no header value");
            ApiError::internal()
        })?;
    let login_url = opened
        .login_url
        .as_deref()
        .map(HeaderValue::try_from)
        .transpose()
        .map_err(|_invalid| {
            tracing::error!("the login link of a tunnel is no header value");
            ApiError::internal()
        })?;
    let on_upgrade = hyper::upgrade::on(&mut request);
    let environment = opened.environment;
    drop(tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                handoff
                    .serve(environment, Box::new(TokioIo::new(upgraded)))
                    .await;
            }
            Err(error) => {
                tracing::warn!(%error, environment = %environment.name, "a tunnel upgrade failed");
            }
        }
    }));
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let answer = response.headers_mut();
    let _previous = answer.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    let _previous = answer.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    let _previous = answer.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    let _previous = answer.insert(ADDRESS_HEADER, address);
    if let Some(login_url) = login_url {
        let _previous = answer.insert(LOGIN_URL_HEADER, login_url);
    }
    Ok(response)
}

/// What the upgrade `request` asks to expose: the port of
/// `X-Liftoff-Expose` (`400` without one of 1–65535), `X-Liftoff-Share:
/// 1`, and the client's address ([`client_of`]).
fn exposure_of(request: &Request) -> Result<Exposure, ApiError> {
    let headers = request.headers();
    let port = headers
        .get(&EXPOSE_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u16>().ok())
        .ok_or_else(|| ApiError::bad_request("X-Liftoff-Expose names the port, 1–65535"))?;
    let port = ContainerPort::new(port).map_err(|e| e.with_field("port"))?;
    let share = headers
        .get(&SHARE_HEADER)
        .is_some_and(|value| value.as_bytes() == b"1");
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    Ok(Exposure {
        port,
        client: client_of(headers, peer),
        share,
    })
}

/// The client of a request whose peer is `peer`: the peer itself, but
/// for a loopback peer (the local proxy) that forwarded the request the
/// address the proxy recorded last in `X-Forwarded-For`; `None` when
/// the peer is unknown or the header is no address.
fn client_of(headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<IpAddr> {
    let peer = peer?.ip();
    if !peer.is_loopback() {
        return Some(peer);
    }
    let Some(forwarded) = headers.get(&FORWARDED_FOR) else {
        return Some(peer);
    };
    forwarded
        .to_str()
        .ok()?
        .rsplit(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// `403` with `detail`: the CLI's client does not retry it.
fn refused(detail: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, Some(detail.to_owned()))
}

/// The `Sec-WebSocket-Accept` of a WebSocket upgrade request (RFC 6455
/// 4.2.1, 4.2.2); `400` for a request that is no such upgrade.
fn accept_key(headers: &HeaderMap) -> Result<HeaderValue, ApiError> {
    let text = |name: &HeaderName| headers.get(name)?.to_str().ok();
    let has_token = |name: &HeaderName, token: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|item| item.trim().eq_ignore_ascii_case(token))
    };
    let upgrade =
        has_token(&header::UPGRADE, "websocket") && has_token(&header::CONNECTION, "upgrade");
    if !upgrade || text(&header::SEC_WEBSOCKET_VERSION) != Some("13") {
        return Err(ApiError::bad_request(
            "GET /api/v1/tunnel is a WebSocket upgrade (version 13)",
        ));
    }
    let key = text(&header::SEC_WEBSOCKET_KEY)
        .ok_or_else(|| ApiError::bad_request("Sec-WebSocket-Key is missing"))?;
    let mut digest = Sha1::new();
    digest.update(key.trim().as_bytes());
    digest.update(WEBSOCKET_GUID.as_bytes());
    HeaderValue::try_from(STANDARD.encode(digest.finalize()))
        .map_err(|_invalid| ApiError::internal())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod accept_key {
        use super::*;

        fn upgrade_headers() -> HeaderMap {
            let mut headers = HeaderMap::new();
            let _previous = headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
            let _previous = headers.insert(
                header::CONNECTION,
                HeaderValue::from_static("keep-alive, Upgrade"),
            );
            let _previous = headers.insert(
                header::SEC_WEBSOCKET_VERSION,
                HeaderValue::from_static("13"),
            );
            let _previous = headers.insert(
                header::SEC_WEBSOCKET_KEY,
                HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="), // gitleaks:allow
            );
            headers
        }

        // TC: DEVT-01 (RFC 6455 4.2.2: the sample key)
        #[test]
        fn rfc_sample_accept_when_upgrade_names_key() {
            assert_eq!(
                accept_key(&upgrade_headers()).ok(),
                Some(HeaderValue::from_static("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="))
            );
        }

        // TC: DEVT-01 (a request that is no upgrade opens no tunnel)
        #[test]
        fn bad_request_when_no_upgrade() {
            let mut headers = upgrade_headers();
            drop(headers.remove(header::UPGRADE));
            assert_eq!(
                accept_key(&headers).map_err(|error| error.status()),
                Err(StatusCode::BAD_REQUEST)
            );
        }
    }

    mod client_of {
        use super::*;

        fn forwarded(value: &'static str) -> HeaderMap {
            let mut headers = HeaderMap::new();
            let _previous = headers.insert(FORWARDED_FOR, HeaderValue::from_static(value));
            headers
        }

        fn at(raw: &str) -> SocketAddr {
            raw.parse().unwrap()
        }

        // TC: EXP-01 (a CLI that reached the API listener itself)
        #[test]
        fn loopback_when_local_peer_was_not_forwarded() {
            assert_eq!(
                client_of(&HeaderMap::new(), Some(at("127.0.0.1:50000"))),
                Some(IpAddr::from([127, 0, 0, 1]))
            );
        }

        // TC: EXP-01 (the local proxy's record, its last entry)
        #[test]
        fn last_proxy_entry_when_local_peer_forwarded() {
            assert_eq!(
                client_of(
                    &forwarded("10.0.0.1, 198.51.100.7"),
                    Some(at("127.0.0.1:50000"))
                ),
                Some(IpAddr::from([198, 51, 100, 7]))
            );
        }

        // TC: EXP-02 (a header from outside the machine counts for nothing)
        #[test]
        fn peer_when_peer_is_outside_machine() {
            assert_eq!(
                client_of(&forwarded("127.0.0.1"), Some(at("198.51.100.7:50000"))),
                Some(IpAddr::from([198, 51, 100, 7]))
            );
        }

        #[test]
        fn unknown_when_forwarded_for_is_no_address() {
            assert_eq!(
                client_of(&forwarded("somewhere"), Some(at("127.0.0.1:50000"))),
                None
            );
        }

        #[test]
        fn unknown_when_peer_unknown() {
            assert_eq!(client_of(&HeaderMap::new(), None), None);
        }
    }
}
