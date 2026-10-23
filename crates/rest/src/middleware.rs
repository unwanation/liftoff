//! Tower middleware stack shared by every route.

use core::any::Any;
use core::time::Duration;

use axum::body::Body;
use axum::extract::MatchedPath;
use axum::response::{IntoResponse, Response};
use http::{HeaderName, HeaderValue, Request, StatusCode, header};
use tower::ServiceBuilder;
use tower::layer::util::{Identity, Stack};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::error::ApiError;

/// Header carrying the request id (generated if the client did not send one).
pub(crate) const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

type SecurityHeaders = Stack<
    SetResponseHeaderLayer<HeaderValue>,
    Stack<
        SetResponseHeaderLayer<HeaderValue>,
        Stack<SetResponseHeaderLayer<HeaderValue>, Identity>,
    >,
>;

/// The mark of a response whose `Cache-Control` is its own: a file of an
/// artifact (`public, max-age=300` when shared).
/// A handler sets it in the response's extensions, which a module's
/// answer cannot reach; every other response is `no-store`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnCacheControl;

/// `Cache-Control: no-store` on `response`, whatever it carried, unless
/// it is marked [`OwnCacheControl`].
async fn no_store(mut response: Response) -> Response {
    if response.extensions().get::<OwnCacheControl>().is_none() {
        let _previous = response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Headers that make a JSON API inert in a browser context.
fn security_headers() -> ServiceBuilder<SecurityHeaders> {
    ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        // A page of the daemon's sets its own (it allows its stylesheet); everything else gets this one.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
}

/// The span of a request. Its `path` is the matched route
/// (`/api/v1/auth/links/{token}`), not the request's path, which may
/// carry a secret (a one-time link's token; secrets never reach the log). A request no route matched logs its path with every segment
/// that looks like one of liftoff's tokens masked.
fn make_span(request: &Request<Body>) -> tracing::Span {
    let request_id = request
        .headers()
        .get(&REQUEST_ID)
        .and_then(|v| v.to_str().ok());
    let path = request.extensions().get::<MatchedPath>().map_or_else(
        || masked(request.uri().path()),
        |matched| matched.as_str().to_owned(),
    );
    tracing::info_span!(
        "http.request",
        method = %request.method(),
        path = %path,
        request_id = request_id.unwrap_or_default(),
    )
}

/// `path` with every segment that starts like a token of liftoff's
/// (`lf` and a letter, then `_`: `lfs_`, `lfl_`, `lfc_`, …) replaced by
/// `***`.
fn masked(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            let bytes = segment.as_bytes();
            let token = bytes.len() > 4
                && bytes.starts_with(b"lf")
                && bytes.get(2).is_some_and(u8::is_ascii_lowercase)
                && bytes.get(3) == Some(&b'_');
            if token { "***" } else { segment }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn handle_panic(payload: Box<dyn Any + Send + 'static>) -> Response {
    let message = match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => payload.downcast::<&str>().map_or_else(
            |_| "unknown panic payload".to_owned(),
            |message| (*message).to_owned(),
        ),
    };
    tracing::error!(panic = %message, "handler panicked");
    ApiError::internal().into_response()
}

/// A request timeout answering `504`.
pub(crate) fn timeout(after: Duration) -> TimeoutLayer {
    TimeoutLayer::with_status_code(StatusCode::GATEWAY_TIMEOUT, after)
}

/// Applies the full production stack to `router`.
///
/// Order (outermost first): request id → trace → propagate id → panic guard →
/// security headers → `no-store` → sensitive-header redaction. The body limits are the
/// routes' own ([`app_router`](crate::app_router)): `POST /deploys` takes a
/// function's component.
///
/// The span reads the matched route, which only a router that matched
/// it knows: apply the stack to the router that holds the routes, never
/// around one that only dispatches to others (`by_host`).
pub(crate) fn apply(router: axum::Router) -> axum::Router {
    router.layer(
        ServiceBuilder::new()
            .layer(SetRequestIdLayer::new(REQUEST_ID, MakeRequestUuid))
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(make_span)
                    .on_response(DefaultOnResponse::new().level(Level::INFO)),
            )
            .layer(PropagateRequestIdLayer::new(REQUEST_ID))
            .layer(CatchPanicLayer::custom(handle_panic))
            .layer(security_headers())
            .layer(axum::middleware::map_response(no_store))
            .layer(SetSensitiveRequestHeadersLayer::new([
                header::AUTHORIZATION,
                header::COOKIE,
                header::PROXY_AUTHORIZATION,
            ])),
    )
}

#[cfg(test)]
mod tests {
    mod masked {
        use super::super::masked;

        // TC: OTL-06 (a path no route matched keeps no token)
        #[test]
        fn hides_token_segment_when_path_carries_one() {
            assert_eq!(
                masked("/api/v1/auth/links/lfl_0123456789/extra"),
                "/api/v1/auth/links/***/extra"
            );
        }

        // TC: OTL-06
        #[test]
        fn keeps_path_when_no_segment_is_token() {
            assert_eq!(
                masked("/api/v1/projects/lfx/lf_a"),
                "/api/v1/projects/lfx/lf_a"
            );
        }
    }
}
