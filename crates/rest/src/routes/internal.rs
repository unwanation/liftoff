//! What only the machine itself may ask, at the root and never under
//! `/api/v1`: `GET /internal/tls-ask?domain=`, Caddy's on-demand TLS
//! permission.

use core::net::SocketAddr;

use axum::Extension;
use axum::extract::{ConnectInfo, State};
use http::{HeaderMap, StatusCode, header};
use liftoff_api::TlsAsk;
use liftoff_core::Error;
use liftoff_core::domain::Host;
use liftoff_core::ports::Ports;

use crate::error::ApiError;
use crate::extract::ApiQuery;
use crate::services::Services;

/// Headers a proxy adds to what it forwards (Caddy's `reverse_proxy`
/// sets `X-Forwarded-For` and `Via`); Caddy's own permission call carries
/// none of them.
const FORWARDING: [header::HeaderName; 3] = [
    header::HeaderName::from_static("x-forwarded-for"),
    header::FORWARDED,
    header::VIA,
];

/// `GET /internal/tls-ask?domain=`: `200` only for a known, verified
/// external host and only to a loopback peer that was not itself
/// forwarded by a proxy (the API site's Caddy reaches the daemon from loopback too); a request whose peer is not known is refused
/// like a foreign one.
pub(crate) async fn tls_ask<P: Ports>(
    State(services): State<Services<P>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<TlsAsk>,
) -> Result<StatusCode, ApiError> {
    let loopback = peer.is_some_and(|Extension(ConnectInfo(peer))| peer.ip().is_loopback());
    let forwarded = FORWARDING.iter().any(|name| headers.contains_key(name));
    if !loopback || forwarded {
        tracing::warn!("refused a tls-ask from outside the machine");
        return Err(Error::forbidden("only the local proxy may ask").into());
    }
    let host = Host::new(&query.domain).map_err(|e| e.with_field("domain"))?;
    services.domains.allow_certificate(&host).await?;
    Ok(StatusCode::OK)
}
