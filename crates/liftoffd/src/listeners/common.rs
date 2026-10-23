//! What the function and tunnel listeners share: the loopback check of the peer, the host
//! of a request, what [`Access::check`] reads of it, the login redirect,
//! the headers of liftoff's own that never reach the environment's code,
//! the session handover's `/.liftoff/callback` and `/.liftoff/logout`
//! (the gate's [`GateListener::redeem`] and [`GateListener::logout`], rendered as the gate renders them), the
//! daemon's pages ([`liftoff_rest::pages`], or a module's
//! `render_page`) and the listeners' short plain answers.
//!
//! [`Access::check`]: liftoff_core::service::Access::check

use core::fmt::Write as _;
use core::net::SocketAddr;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use liftoff_core::Error;
use liftoff_core::domain::{GateCodeSecret, GateSecret, Host, User};
use liftoff_core::ports::{ModulePage, PageKind, PageRequest, Ports};
use liftoff_core::service::{
    AccessRequest, GateAnswer, GateListener, HOST_SESSION_COOKIE, cookie_value, forwarded_cookies,
    forwards_header, hook_request, host_session_cleared, host_session_cookie,
    is_liftoff_authorization,
};
use liftoff_rest::pages;

/// The paths of liftoff's own on every address (the gate's handover).
pub(super) const LIFTOFF_PATHS: &str = "/.liftoff/";

/// The user headers a listener sets for the person it let through.
const USER_HEADER: HeaderName = HeaderName::from_static("x-liftoff-user");
/// See [`USER_HEADER`].
const EMAIL_HEADER: HeaderName = HeaderName::from_static("x-liftoff-email");

/// Whether a connection of `peer` is served: only the local Caddy may
/// ask, so only a loopback peer.
pub(super) const fn admits(peer: SocketAddr) -> bool {
    peer.ip().is_loopback()
}

/// The host of the request: `Host` without a port.
pub(super) fn host_of(headers: &HeaderMap) -> Option<Host> {
    let raw = headers.get(header::HOST)?.to_str().ok()?;
    let name = match raw.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => raw,
    };
    Host::new(name).ok()
}

/// What [`Access::check`](liftoff_core::service::Access::check) reads of
/// `request` on `host` (the core's [`AccessRequest::read`] over the raw
/// header values).
pub(super) fn access_request(host: &Host, request: &Request<Incoming>) -> AccessRequest {
    let headers = request.headers();
    let value = |name: &HeaderName| headers.get(name)?.to_str().ok();
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(|| request.uri().path().to_owned(), ToString::to_string);
    AccessRequest::read(
        host.clone(),
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
        value(&header::AUTHORIZATION),
        value(&header::ACCEPT),
        hook_request(
            request.method().as_str(),
            &path,
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
        ),
    )
}

/// Removes what is liftoff's from `headers`: the client's `X-Liftoff-*`, liftoff's cookies and an
/// `Authorization` that carries a liftoff token; then names the person
/// `user` (when the request carried a session) in `X-Liftoff-User` and
/// `X-Liftoff-Email`, as the gate does.
pub(super) fn strip_liftoff(headers: &mut HeaderMap, user: Option<&User>) {
    let liftoff: Vec<HeaderName> = headers
        .keys()
        .filter(|name| !forwards_header(name.as_str()))
        .cloned()
        .collect();
    for name in liftoff {
        drop(headers.remove(name));
    }
    let liftoff_bearer = headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .any(|value| value.to_str().map_or(true, is_liftoff_authorization));
    if liftoff_bearer {
        drop(headers.remove(header::AUTHORIZATION));
    }
    let cookies = forwarded_cookies(
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    );
    drop(headers.remove(header::COOKIE));
    if let Some(value) = cookies.and_then(|cookies| HeaderValue::try_from(cookies).ok()) {
        let _previous = headers.insert(header::COOKIE, value);
    }
    if let Some(user) = user {
        if let Ok(login) = HeaderValue::try_from(user.login.as_str()) {
            let _previous = headers.insert(USER_HEADER, login);
        }
        if let Some(email) = Some(user.email.as_str())
            .filter(|email| !email.is_empty())
            .and_then(|email| HeaderValue::try_from(email).ok())
        {
            let _previous = headers.insert(EMAIL_HEADER, email);
        }
    }
}

/// The CSP of a module's page: nothing loads (the API's default).
const MODULE_PAGE_CSP: &str = "default-src 'none'; frame-ancestors 'none'";

/// The answer to `/.liftoff/<path>` on `host` (the session handover, v1
/// SSO): `callback?code=` redeems the gate's one-time code of `host` for
/// its session cookie and returns to the path the code carries (`302`), a
/// code that is not one of `host`'s is `400`; `logout` ends the host
/// session and clears the cookie (`200`); any other path is `404`. The
/// environment's code never sees any of it.
pub(super) async fn liftoff_path<P: Ports>(
    gate: &GateListener<P>,
    host: &Host,
    request: &Request<Incoming>,
    listener: &str,
) -> Response<Full<Bytes>> {
    let session = cookie_value(
        request
            .headers()
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
        HOST_SESSION_COOKIE,
    )
    .and_then(|raw| GateSecret::parse(raw).ok());
    let answer = match request.uri().path().strip_prefix(LIFTOFF_PATHS) {
        Some("callback") => {
            let code = request
                .uri()
                .query()
                .unwrap_or_default()
                .split('&')
                .find_map(|pair| pair.strip_prefix("code="))
                .and_then(|raw| GateCodeSecret::parse(raw).ok());
            gate.redeem(host, code.as_ref()).await
        }
        Some("logout") => gate.logout(host, session.as_ref()).await,
        _ => return text(StatusCode::NOT_FOUND, "not found"),
    };
    match answer {
        Ok(GateAnswer::Enter {
            secret,
            rd,
            max_age,
        }) => with_cookie(
            redirect(rd.as_str()),
            &host_session_cookie(&secret, max_age),
        ),
        Ok(GateAnswer::SignedOut) => {
            with_cookie(text(StatusCode::OK, "signed out"), &host_session_cleared())
        }
        Ok(_stale) => text(
            StatusCode::BAD_REQUEST,
            "the sign-in link is stale; open the site again",
        ),
        Err(error) => failure(listener, &error),
    }
}

/// `reply` with `Set-Cookie: cookie`.
fn with_cookie(mut reply: Response<Full<Bytes>>, cookie: &str) -> Response<Full<Bytes>> {
    match HeaderValue::try_from(cookie) {
        Ok(value) => {
            let _appended = reply.headers_mut().append(header::SET_COOKIE, value);
            reply
        }
        Err(_invalid) => {
            tracing::error!("a listener built an invalid cookie");
            text(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// The page of `kind` with `status` for `request` to `host`: a module's
/// `render_page` when one replaces it (its HTML, or `302` where it sends the browser with the address it asked for: the panel `liftoff-web`), else `daemon`'s own (one of
/// [`liftoff_rest::pages`]).
pub(super) async fn page<P: Ports>(
    gate: &GateListener<P>,
    status: StatusCode,
    kind: PageKind,
    (host, request): (&Host, &Request<Incoming>),
    daemon: impl FnOnce() -> String,
) -> Response<Full<Bytes>> {
    module_page(gate, status, kind, (host, request))
        .await
        .unwrap_or_else(|| html(status, daemon(), pages::CSP))
}

/// The page of `kind` with `status` for `request` to `host` when a
/// module renders it (see [`page`]); `None` keeps the listener's own
/// answer.
pub(super) async fn module_page<P: Ports>(
    gate: &GateListener<P>,
    status: StatusCode,
    kind: PageKind,
    (host, request): (&Host, &Request<Incoming>),
) -> Option<Response<Full<Bytes>>> {
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str());
    Some(match gate.page(&PageRequest::at(kind, host, path)).await? {
        ModulePage::Html(module_page) => html(status, module_page, MODULE_PAGE_CSP),
        ModulePage::Redirect(location) => redirect(location.as_str()),
    })
}

/// An HTML page of the listener's with the v1 headers: `nosniff`, `csp`,
/// `no-store`.
fn html(status: StatusCode, body: String, csp: &'static str) -> Response<Full<Bytes>> {
    let mut reply = text(status, "");
    *reply.body_mut() = Full::new(Bytes::from(body));
    let headers = reply.headers_mut();
    let _previous = headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    let _previous = headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(csp),
    );
    let _previous = headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    reply
}

/// `https://<api host>/gate/start?host=<host>&rd=<path and query>`: the
/// session handover of the gate (v1 SSO), which comes back to the
/// request's path with the session of `host`.
pub(super) fn login_url(api_host: &Host, host: &Host, request: &Request<Incoming>) -> String {
    let rd = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str());
    format!(
        "https://{api_host}/gate/start?host={}&rd={}",
        encode(host.as_str()),
        encode(rd)
    )
}

/// Percent-encodes everything but the unreserved characters (RFC 3986).
fn encode(text: &str) -> String {
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

/// The one mapping of the core's errors for the listener named
/// `listener`: a missing record is `404`, a refusal `403`; anything else
/// is an internal error whose cause goes to the log, never to the client.
pub(super) fn failure(listener: &str, error: &Error) -> Response<Full<Bytes>> {
    match error {
        Error::NotFound(_) => text(StatusCode::NOT_FOUND, "not found"),
        Error::Forbidden(reason) => text(StatusCode::FORBIDDEN, reason),
        _ => {
            tracing::error!(%error, listener, "a listener failed");
            text(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// A `text/plain` answer of the listener's own, never cached.
pub(super) fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    let mut reply = Response::new(Full::new(Bytes::from(body.to_owned())));
    *reply.status_mut() = status;
    let headers = reply.headers_mut();
    let _previous = headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    let _previous = headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let _previous = headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    reply
}

/// `302` to `location`.
pub(super) fn redirect(location: &str) -> Response<Full<Bytes>> {
    let mut reply = text(StatusCode::FOUND, "");
    match HeaderValue::try_from(location) {
        Ok(value) => {
            let _previous = reply.headers_mut().insert(header::LOCATION, value);
            reply
        }
        Err(_invalid) => {
            tracing::error!("a listener built an invalid redirect");
            text(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}
