//! The gate in front of protected environments:
//! `GET /gate/start` on the API, and the router of the listener on
//! `[gate] bind` that Caddy's `forward_auth` asks.
//!
//! [`GateListener`] holds the rules; this adapter reads the request into
//! domain types (the host of `X-Liftoff-Host`, the cookies, the query),
//! hands them over and renders the [`GateAnswer`] as a short
//! `text/plain` page for a browser, with the redirects and cookies:
//!
//! | Answer | Response |
//! |---|---|
//! | `Start` | `302` to `https://<api host>/gate/start?host=<host>&rd=<rd>` |
//! | `Login` | `302` to `https://<api host>/api/v1/m/oidc/login?rd=<that /gate/start>` |
//! | `LoginPage` | `302` to `https://<api host>/login?host=<host>&rd=<rd>` (the daemon's login page) |
//! | `Callback` | `302` to `https://<host>/.liftoff/callback?code=<code>` |
//! | `Enter` | `302` to `rd` with `Set-Cookie: __Host-liftoff_gate=<secret>; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=<s>` |
//! | `Pass` | `200` with `X-Liftoff-User: <login>` and `X-Liftoff-Email: <email>` (when known) |
//! | `Refuse` | `403` with the reason |
//! | `SignedOut` | `200` clearing `__Host-liftoff_gate` |
//! | `Stale` | `400` |
//!
//! A failure renders with the status of the API's one error mapping
//! ([`ApiError`]). The listener answers only loopback peers (the local
//! Caddy, whose headers it trusts; `[gate] bind` is loopback too) and
//! `400` without `X-Liftoff-Host`; a path other than `/gate/check`,
//! `/.liftoff/callback` and `/.liftoff/logout` is `404`.

use core::fmt::Write as _;
use core::net::SocketAddr;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use liftoff_core::Error;
use liftoff_core::domain::{GateCodeSecret, GateSecret, Host, ReturnPath, SessionSecret};
use liftoff_core::ports::{ModuleRequest, Ports};
use liftoff_core::service::{
    GateAnswer, GateListener, HOST_SESSION_COOKIE, cookie_value, host_session_cleared,
    host_session_cookie,
};

use super::auth::hook_request;
use crate::error::ApiError;
use crate::services::Services;

/// The cookie of the browser session on the API host (set by the `oidc`
/// module's browser login).
pub(crate) const SESSION_COOKIE: &str = ModuleRequest::SESSION_COOKIE;

/// The cookie of the session on one protected host (the core's, which
/// `Access::check` reads too).
pub(crate) const GATE_COOKIE: &str = HOST_SESSION_COOKIE;

/// The path of the `oidc` module's browser login on the API.
const LOGIN_PATH: &str = "/api/v1/m/oidc/login";

/// The answer when a code is not right: one text for every case.
const STALE: &str = "the sign-in link is stale; open the site again";

/// `GET /gate/start?host&rd` on the API host.
pub(crate) async fn start<P: Ports>(
    State(services): State<Services<P>>,
    request: Request,
) -> Response {
    let query = request.uri().query().unwrap_or_default();
    let host = query_value(query, "host").and_then(|raw| Host::new(&raw).ok());
    let rd = return_path(query_value(query, "rd").as_deref());
    let session =
        cookie(request.headers(), SESSION_COOKIE).and_then(|raw| SessionSecret::parse(raw).ok());
    let answer = services
        .gate
        .start(host.as_ref(), rd, session.as_ref(), &hook_request(&request))
        .await;
    render(services.gate.api_host(), answer)
}

/// The router of the gate listener.
///
/// It answers `/gate/check`, `/.liftoff/callback` and `/.liftoff/logout`,
/// any other path `404`. Serve it with `ConnectInfo<SocketAddr>`: a request without a known
/// loopback peer is refused.
pub fn gate_router<P: Ports>(gate: GateListener<P>) -> Router {
    Router::new().fallback(handle::<P>).with_state(gate)
}

/// A request to the gate listener (see the [module
/// documentation](self)).
async fn handle<P: Ports>(
    State(gate): State<GateListener<P>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    request: Request,
) -> Response {
    if !peer.is_some_and(|Extension(ConnectInfo(peer))| peer.ip().is_loopback()) {
        tracing::warn!("refused a gate request from outside the machine");
        return text(
            StatusCode::FORBIDDEN,
            "only the local proxy may ask the gate",
        );
    }
    let headers = request.headers();
    let Some(host) = header_text(headers, "x-liftoff-host").and_then(|raw| Host::new(raw).ok())
    else {
        return text(StatusCode::BAD_REQUEST, "the gate needs X-Liftoff-Host");
    };
    let host_session = cookie(headers, GATE_COOKIE).and_then(|raw| GateSecret::parse(raw).ok());
    let answer = match request.uri().path() {
        "/gate/check" => {
            gate.check(
                &host,
                host_session.as_ref(),
                return_path(header_text(headers, "x-forwarded-uri")),
                &hook_request(&request),
            )
            .await
        }
        "/.liftoff/callback" => {
            let code = query_value(request.uri().query().unwrap_or_default(), "code")
                .and_then(|raw| GateCodeSecret::parse(&raw).ok());
            gate.redeem(&host, code.as_ref()).await
        }
        "/.liftoff/logout" => gate.logout(&host, host_session.as_ref()).await,
        _ => return text(StatusCode::NOT_FOUND, "not found"),
    };
    render(gate.api_host(), answer)
}

/// The HTTP response of what the gate decided (see the [module
/// documentation](self)).
pub(crate) fn render(api_host: &Host, answer: Result<GateAnswer, Error>) -> Response {
    match answer {
        Ok(GateAnswer::Start { host, rd }) => redirect(&start_url(api_host, &host, &rd)),
        Ok(GateAnswer::Login { host, rd }) => {
            let back = format!(
                "/gate/start?host={}&rd={}",
                encode(host.as_str()),
                encode(rd.as_str())
            );
            redirect(&format!(
                "https://{api_host}{LOGIN_PATH}?rd={}",
                encode(&back)
            ))
        }
        Ok(GateAnswer::LoginPage { host, rd }) => redirect(&login_page_url(api_host, &host, &rd)),
        Ok(GateAnswer::Callback { host, code }) => redirect(&format!(
            "https://{host}/.liftoff/callback?code={}",
            code.expose()
        )),
        Ok(GateAnswer::Enter {
            secret,
            rd,
            max_age,
        }) => with(
            redirect(rd.as_str()),
            header::SET_COOKIE,
            &host_session_cookie(&secret, max_age),
        ),
        Ok(GateAnswer::Pass { login, email }) => {
            let passed = with(
                text(StatusCode::OK, ""),
                header::HeaderName::from_static("x-liftoff-user"),
                login.as_str(),
            );
            if email.is_empty() {
                passed
            } else {
                with(
                    passed,
                    header::HeaderName::from_static("x-liftoff-email"),
                    &email,
                )
            }
        }
        Ok(GateAnswer::Refuse(reason)) => text(StatusCode::FORBIDDEN, &reason),
        Ok(GateAnswer::SignedOut) => with(
            text(StatusCode::OK, "signed out"),
            header::SET_COOKIE,
            &host_session_cleared(),
        ),
        Ok(GateAnswer::Stale) => text(StatusCode::BAD_REQUEST, STALE),
        Err(error) => {
            let problem = ApiError::from(error);
            let reason = problem
                .detail()
                .map_or_else(|| problem.status().as_str().to_owned(), ToOwned::to_owned);
            text(problem.status(), &reason)
        }
    }
}

/// `https://<api host>/gate/start?host=<host>&rd=<rd>`.
fn start_url(api_host: &Host, host: &Host, rd: &ReturnPath) -> String {
    format!(
        "https://{api_host}/gate/start?host={}&rd={}",
        encode(host.as_str()),
        encode(rd.as_str())
    )
}

/// `https://<api host>/login?host=<host>&rd=<rd>`: the daemon's login
/// page for `host`.
fn login_page_url(api_host: &Host, host: &Host, rd: &ReturnPath) -> String {
    format!(
        "https://{api_host}/login?host={}&rd={}",
        encode(host.as_str()),
        encode(rd.as_str())
    )
}

/// The path `raw` names on the same host, `/` for anything else (no
/// open redirect, v1 SSO-04).
pub(crate) fn return_path(raw: Option<&str>) -> ReturnPath {
    raw.and_then(|raw| ReturnPath::new(raw).ok())
        .unwrap_or_else(ReturnPath::root)
}

/// A `text/plain` page of `status` that is never cached.
fn text(status: StatusCode, body: &str) -> Response {
    let mut reply = Response::new(Body::from(body.to_owned()));
    *reply.status_mut() = status;
    let headers = reply.headers_mut();
    let _previous = headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    let _previous = headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    reply
}

/// `302` to `location`.
pub(crate) fn redirect(location: &str) -> Response {
    with(text(StatusCode::FOUND, ""), header::LOCATION, location)
}

/// `reply` with `name: value` added; a value that is not a valid header
/// value (it cannot be: every value here is built of URL-safe text) is
/// the gate's failure.
pub(crate) fn with(mut reply: Response, name: header::HeaderName, value: &str) -> Response {
    match HeaderValue::try_from(value) {
        Ok(value) => {
            let _appended = reply.headers_mut().append(name, value);
            reply
        }
        Err(_invalid) => {
            tracing::error!(header = %name, "the gate built an invalid header");
            text(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// The first value of the header `name` that is text.
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// The value of the cookie `name` in the `Cookie` headers.
pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    cookie_value(
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
        name,
    )
}

/// Every `name=value` pair of the `Cookie` headers.
pub(crate) fn cookies(headers: &HeaderMap) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
}

/// The decoded value of `name` in the query `query`.
pub(crate) fn query_value(query: &str, name: &str) -> Option<String> {
    let (_, value) = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)?;
    decode(value)
}

/// Percent-encodes everything but the unreserved characters (RFC 3986).
pub(crate) fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _written = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Percent-decodes `text` (`+` stays `+`); `None` when it is not valid
/// UTF-8 or a `%` is not followed by two hex digits.
fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let hex = bytes.get(index.saturating_add(1)..index.saturating_add(3))?;
            let hex = core::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index = index.saturating_add(3);
        } else {
            out.push(byte);
            index = index.saturating_add(1);
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    mod encoding {
        use super::*;

        #[test]
        fn decodes_what_it_encodes_when_round_tripped() {
            let text = "/gate/start?host=blog.apps.test&rd=%2F";
            assert_eq!(decode(&encode(text)).as_deref(), Some(text));
        }
    }
}
