//! The browser's way in without OIDC (11): the page a one-time link opens and the login
//! page of the API host.
//!
//! | Route | Response |
//! |---|---|
//! | `GET /api/v1/auth/links/{token}` | [`LinksService::open_own`](liftoff_core::service::LinksService::open_own): `302` to `https://<target host>/.liftoff/callback?code=<code>` with `Set-Cookie: __Host-liftoff_session=<browser session>; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=<s>` (`SameSite=None` while the panel `liftoff-web` is kept, [`WebPanelService::same_site`](liftoff_core::service::WebPanelService::same_site)); for a link to an artifact, `302` to its address (to the gate's handover on `[artifacts] host`) with the same cookie |
//! | `GET /login?host&rd` | the module's `render_page(login)` when one replaces it ([`GateListener::login_page`](liftoff_core::service::GateListener::login_page)): its HTML (`oidc`) or `302` where it sends the browser (`web` with `take_pages`: the panel's login with `return=https://<host><rd>&host=<host>` for a host the daemon serves behind a session, without either for any other), else the daemon's [`pages::login`] |
//! | `GET /login?link&host&rd` | the pasted link opened for the address `host`, returning to `rd` there, as above |
//!
//! The environment's host cannot see the API host's cookie: the browser
//! carries the gate's one-time code there in the query (the v1 SSO
//! handover), and the listener of that host redeems it for the host's own
//! session (`__Host-liftoff_gate`). A link that is unknown, expired or
//! used is the login page with a notice and `410`; a link pasted on the
//! page of another address is the forbidden page, `403`, and stays
//! unused. Nothing is set on either. Every page carries [`pages::CSP`];
//! a module's page keeps the API's default CSP.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};
use liftoff_core::Error;
use liftoff_core::domain::{Host, LinkSecret, ReturnPath};
use liftoff_core::ports::{ModulePage, Ports};
use liftoff_core::service::{
    BrowserSession, OpenedLink, OpenedOwn, SameSite, browser_session_cookie,
};

use super::artifacts_view;
use super::gate::{query_value, redirect, return_path, with};
use crate::error::ApiError;
use crate::pages;
use crate::services::Services;

/// What the login page says of a link that cannot be opened any more.
const GONE: &str = "That link expired or was already used. A link works once, within ten \
                    minutes: get a new one.";

/// `GET /api/v1/auth/links/{token}`: a link to an environment hands
/// the session over to its address; a link to an artifact sets the API
/// host's cookie and lands on the artifact.
pub(crate) async fn open_link<P: Ports>(
    State(services): State<Services<P>>,
    Path(token): Path<String>,
) -> Response {
    let rd = ReturnPath::root();
    let Ok(token) = LinkSecret::parse(&token) else {
        return gone(None, &rd);
    };
    match services.links.open_own(&token).await {
        Ok(OpenedOwn::Address(opened)) => entered(&opened, services.web.same_site().await),
        Ok(OpenedOwn::Artifact { browser, artifact }) => with_session(
            redirect(&artifacts_view::landing(&services, artifact)),
            &browser,
            services.web.same_site().await,
        ),
        Err(error) => refused(error, None, &rd),
    }
}

/// `GET /login?host&rd`, and `GET /login?link&host&rd` from its form.
pub(crate) async fn page<P: Ports>(
    State(services): State<Services<P>>,
    request: Request,
) -> Response {
    let query = request.uri().query().unwrap_or_default();
    let host = query_value(query, "host").and_then(|raw| Host::new(&raw).ok());
    let rd = return_path(query_value(query, "rd").as_deref());
    if let Some(link) = query_value(query, "link") {
        let token = pasted_token(&link);
        return opened(&services, token.as_ref(), host.as_ref(), rd).await;
    }
    match services.gate.login_page(host.as_ref(), &rd).await {
        Some(ModulePage::Html(module_page)) => html(StatusCode::OK, module_page, None),
        Some(ModulePage::Redirect(location)) => redirect(location.as_str()),
        None => html(
            StatusCode::OK,
            pages::login(host.as_ref(), &rd, None),
            Some(pages::CSP),
        ),
    }
}

/// The token of a pasted link: the last segment of its path
/// (`https://<api host>/api/v1/auth/links/<token>`), or the token itself.
fn pasted_token(link: &str) -> Option<LinkSecret> {
    let link = link.trim();
    let path = link.split(['?', '#']).next().unwrap_or(link);
    let token = path.trim_end_matches('/').rsplit('/').next()?;
    LinkSecret::parse(token).ok()
}

/// The link `token` opened at the address `at` (or its own target),
/// returning to `rd` there (see the [module documentation](self)).
async fn opened<P: Ports>(
    services: &Services<P>,
    token: Option<&LinkSecret>,
    at: Option<&Host>,
    rd: ReturnPath,
) -> Response {
    let Some(token) = token else {
        return gone(at, &rd);
    };
    match services.links.open(token, at, rd.clone()).await {
        Ok(opened) => entered(&opened, services.web.same_site().await),
        Err(error) => refused(error, at, &rd),
    }
}

/// The page of a link that could not be opened at `at`.
fn refused(error: Error, at: Option<&Host>, rd: &ReturnPath) -> Response {
    match error {
        Error::Gone(_) => gone(at, rd),
        Error::Forbidden(reason) => html(
            StatusCode::FORBIDDEN,
            pages::forbidden(&reason),
            Some(pages::CSP),
        ),
        error => {
            let problem = ApiError::from(error);
            let reason = problem
                .detail()
                .map_or_else(|| problem.status().as_str().to_owned(), ToOwned::to_owned);
            html(
                problem.status(),
                pages::login(at, rd, Some(&reason)),
                Some(pages::CSP),
            )
        }
    }
}

/// `302` to the callback of the target's host with the gate's one-time
/// code, setting the `browser` session as the API host's cookie for as
/// long as it lives.
fn entered(opened: &OpenedLink, same_site: SameSite) -> Response {
    with_session(
        redirect(&format!(
            "https://{}/.liftoff/callback?code={}",
            opened.host,
            opened.code.expose()
        )),
        &opened.browser,
        same_site,
    )
}

/// `reply` setting `browser` as the API host's cookie for as long as it
/// lives, `SameSite=<same_site>` (`None` while the panel `liftoff-web` is kept).
fn with_session(reply: Response, browser: &BrowserSession, same_site: SameSite) -> Response {
    let session = &browser.session;
    let max_age = session
        .expires_at
        .unix_timestamp()
        .saturating_sub(session.created_at.unix_timestamp())
        .max(0);
    with(
        reply,
        header::SET_COOKIE,
        &browser_session_cookie(&browser.token, max_age, same_site),
    )
}

/// The login page with the notice of a used link, `410`.
fn gone(at: Option<&Host>, rd: &ReturnPath) -> Response {
    html(
        StatusCode::GONE,
        pages::login(at, rd, Some(GONE)),
        Some(pages::CSP),
    )
}

/// An HTML page of `status`, never cached, with `csp` (the API's default
/// without one).
pub(crate) fn html(status: StatusCode, body: String, csp: Option<&'static str>) -> Response {
    let mut reply = Response::new(Body::from(body));
    *reply.status_mut() = status;
    let headers = reply.headers_mut();
    let _previous = headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    let _previous = headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(csp) = csp {
        let _previous = headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(csp),
        );
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    mod pasted_token {
        use super::*;

        // TC: OTL-08 (the page takes the whole link or its token)
        #[test]
        fn takes_last_segment_when_link_is_url() {
            let token = "lfl_0123456789abcdefghijklmnopqrstuvwxyzABCD";
            let parsed = LinkSecret::parse(token).ok();
            assert_eq!(
                (
                    pasted_token(&format!(
                        " https://liftoff.test/api/v1/auth/links/{token}?x#y "
                    )),
                    pasted_token(token)
                ),
                (parsed.clone(), parsed)
            );
        }

        // TC: OTL-08
        #[test]
        fn none_when_text_is_no_link() {
            assert_eq!(pasted_token("hello"), None);
        }
    }
}
