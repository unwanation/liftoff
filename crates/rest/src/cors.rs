//! CORS of `/api/v1` for the panel `liftoff-web` (the `web` row).
//!
//! The layer asks the core's [`WebPanelService::origin`](liftoff_core::service::WebPanelService::origin)
//! on each request with an `Origin`, so installing or removing the `web`
//! module takes effect without a restart; a request without `Origin`
//! asks nothing. While a panel is kept:
//!
//! - a request whose `Origin` is the panel's gets
//!   `Access-Control-Allow-Origin: <origin>`,
//!   `Access-Control-Allow-Credentials: true` and `Vary: Origin` on
//!   whatever the route answers (a `401` problem too, so the panel can
//!   read it), and is marked [`PanelRequest`]: the bearer layer then
//!   takes the browser session's cookie as its credential;
//! - a preflight (`OPTIONS` with `Access-Control-Request-Method`) from
//!   it is answered here, `204` without a session, allowing the
//!   requested method (one of [`METHODS`]) and the requested headers;
//! - any other origin gets no CORS header, only `Vary: Origin`.
//!
//! Without a panel (or when the module fails to answer) nothing is
//! added. Only `/api/v1` carries the layer: never the viewer of
//! artifacts (`/a/`), the root pages or the listeners.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use liftoff_core::ports::Ports;
use liftoff_core::service::PanelOrigin;

use crate::services::Services;

/// The mark of a request from the panel's origin, in its extensions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PanelRequest;

/// The methods a preflight may be allowed.
const METHODS: [Method; 5] = [
    Method::GET,
    Method::POST,
    Method::PUT,
    Method::PATCH,
    Method::DELETE,
];

/// How long a browser may keep a preflight's answer, in seconds.
const PREFLIGHT_MAX_AGE: &str = "600";

/// The CORS layer of `/api/v1` (see the [module documentation](self)).
pub(crate) async fn cors<P: Ports>(
    State(services): State<Services<P>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
    else {
        return next.run(request).await;
    };
    match services.web.origin(&origin).await {
        PanelOrigin::NoPanel => next.run(request).await,
        PanelOrigin::Other => vary(next.run(request).await, false),
        PanelOrigin::Panel(_) => {
            if request.method() == Method::OPTIONS
                && request
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
            {
                return preflight(request.headers(), &origin);
            }
            let _previous = request.extensions_mut().insert(PanelRequest);
            allow(vary(next.run(request).await, false), &origin)
        }
    }
}

/// The answer to the panel's preflight with `headers` from `origin`.
fn preflight(headers: &HeaderMap, origin: &str) -> Response {
    let mut reply = Response::new(Body::empty());
    *reply.status_mut() = StatusCode::NO_CONTENT;
    let method = headers
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| Method::from_bytes(raw.trim().as_bytes()).ok())
        .filter(|method| METHODS.contains(method));
    let requested: Vec<HeaderName> = headers
        .get_all(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    let out = reply.headers_mut();
    if let Some(method) = method {
        insert(out, header::ACCESS_CONTROL_ALLOW_METHODS, method.as_str());
    }
    if !requested.is_empty() {
        let names: Vec<&str> = requested.iter().map(HeaderName::as_str).collect();
        insert(out, header::ACCESS_CONTROL_ALLOW_HEADERS, &names.join(", "));
    }
    insert(out, header::ACCESS_CONTROL_MAX_AGE, PREFLIGHT_MAX_AGE);
    allow(vary(reply, true), origin)
}

/// `reply` allowing `origin` with credentials.
fn allow(mut reply: Response, origin: &str) -> Response {
    let headers = reply.headers_mut();
    insert(headers, header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    insert(headers, header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true");
    reply
}

/// `reply` varying on `Origin` (and on the preflight's request headers
/// when `preflight`), whatever else it varies on.
fn vary(mut reply: Response, preflight: bool) -> Response {
    let headers = reply.headers_mut();
    let _appended = headers.append(header::VARY, HeaderValue::from_static("origin"));
    if preflight {
        let _appended = headers.append(
            header::VARY,
            HeaderValue::from_static(
                "access-control-request-method, access-control-request-headers",
            ),
        );
    }
    reply
}

/// Sets `name: value`; a value that is not a valid header value (the
/// origin was one, the rest is built of tokens) is left out.
fn insert(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::try_from(value) {
        let _previous = headers.insert(name, value);
    }
}
