//! `GET /api/v1/tunnel`: the WebSocket upgrade of `lof dev`.
//!
//! The CLI sends its `cli` session as the bearer and the project in
//! `X-Liftoff-Project`. The route answers `401` without a live session
//! and `403` for a deploy token or a session that is no `cli` one
//! (DEVT-06), asks
//! [`TunnelsService::open`](liftoff_core::service::TunnelsService::open)
//! for the caller's tunnel environment, answers `101` (with
//! `X-Liftoff-Login-Url`, the one-time link of a private environment,
//! DEVT-07) and hands the upgraded connection to the installed
//! [`TunnelHandoff`]: the daemon's tunnel server.

use core::fmt;
use core::pin::Pin;
use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use hyper_util::rt::TokioIo;
use liftoff_core::domain::{Bearer, Environment, ProjectName, SessionKind};
use liftoff_core::ports::Ports;
use sha1::{Digest as _, Sha1};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::ApiError;
use crate::routes::auth::{bearer, hook_request};
use crate::services::Services;

/// The project the CLI opens its tunnel in.
const PROJECT_HEADER: HeaderName = HeaderName::from_static("x-liftoff-project");

/// The one-time link of a private tunnel, for `lof dev` to print
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
    let headers = request.headers();
    let project = headers
        .get(&PROJECT_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::bad_request("X-Liftoff-Project names the project"))?;
    let project = ProjectName::new(project)?;
    let accept = accept_key(headers)?;
    let Some(handoff) = slot.0 else {
        tracing::error!("GET /api/v1/tunnel has no tunnel server installed");
        return Err(ApiError::unavailable());
    };
    let opened = services.tunnels.open(principal.user(), &project).await?;
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
    if let Some(login_url) = login_url {
        let _previous = answer.insert(LOGIN_URL_HEADER, login_url);
    }
    Ok(response)
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
}
