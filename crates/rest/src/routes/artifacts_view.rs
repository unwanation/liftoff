//! The viewer of artifacts: the
//! wrapper page with its top bar and the version's files in an opaque
//! origin.
//!
//! On the API host, unless `[artifacts] host` is set (then the API host
//! answers `/a/…` with `404` and the artifacts' host serves the same
//! routes without the `/a` prefix: `/{id}`, `/{id}/v/{n}`, …):
//!
//! | Route | Response |
//! |---|---|
//! | `GET /a/{id}` | the wrapper of the current version ([`pages::artifact`](crate::pages::artifact)) |
//! | `GET /a/{id}/v/{n}` | the wrapper of version `n` |
//! | `GET /a/{id}/v/{n}/index.html`, `/a/{id}/v/{n}/{file}` | the file, with `Content-Security-Policy: sandbox allow-scripts allow-forms allow-popups allow-modals; frame-ancestors 'self'`, `nosniff`, the type by extension (`application/octet-stream` for an unknown one) and `Cache-Control: private, no-store` (`private`) or `public, max-age=300` (`shared`) |
//! | `POST /a/{id}/share`, `POST /a/{id}/unshare` | the owner's buttons on the wrapper: `303` back to it |
//! | `GET /.liftoff/callback?code=` (on `[artifacts] host` alone) | the gate's one-time code redeemed for the host's session: `302` to the wrapper with `Set-Cookie: __Host-liftoff_gate=…` |
//!
//! Every request asks [`Access::check`](liftoff_core::service::Access::check)
//! with the target `artifact:<id>` first: `shared` passes without a
//! session, `private` needs the API host's `browser` session
//! (`__Host-liftoff_session`) of its owner or an admin, or on
//! `[artifacts] host`, where that cookie never comes, the host's own
//! session (`__Host-liftoff_gate`), which the gate's handover gives as
//! for a private address (`/gate/start?host=<artifacts host>`). A
//! browser without one gets `302` to the login page (to `/gate/start` on
//! `[artifacts] host`) returning to the wrapper (never to a file), a
//! JSON client `401`, someone else `403`. An unknown artifact, version or
//! file is `404`.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse as _, Response};
use http::{HeaderValue, StatusCode, header};
use liftoff_core::domain::{
    ArtifactId, ArtifactPath, GateCodeSecret, GateSecret, Host, LinkTarget, SessionSecret, User,
    VersionNumber, Visibility,
};
use liftoff_core::ports::Ports;
use liftoff_core::service::{AccessDecision, AccessRequest};

use super::auth::{bearer, hook_request};
use super::gate::{GATE_COOKIE, SESSION_COOKIE, cookie, encode, query_value, redirect, render};
use super::login::html;
use crate::error::ApiError;
use crate::middleware::OwnCacheControl;
use crate::pages::{self, ArtifactPage};
use crate::services::Services;

/// The `Content-Security-Policy` of every file of a version: an opaque
/// origin (no cookies, no storage, no calls as the viewer), framed by
/// its own host's wrapper alone, as the standalone artifacts server.
pub(crate) const FILE_CSP: &str =
    "sandbox allow-scripts allow-forms allow-popups allow-modals; frame-ancestors 'self'";

/// `GET /a/{id}`.
pub(crate) async fn wrapper<P: Ports>(
    State(services): State<Services<P>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let access = access_request(&services, &request);
    wrapped(&services, &id, None, &access).await
}

/// `GET /a/{id}/v/{n}`.
pub(crate) async fn version<P: Ports>(
    State(services): State<Services<P>>,
    Path((id, n)): Path<(String, String)>,
    request: Request,
) -> Response {
    let access = access_request(&services, &request);
    wrapped(&services, &id, Some(&n), &access).await
}

/// `GET /a/{id}/v/{n}/{file}` (`index.html` among them).
pub(crate) async fn file<P: Ports>(
    State(services): State<Services<P>>,
    Path((id, n, name)): Path<(String, String, String)>,
    request: Request,
) -> Response {
    let Some(id) = artifact_id(&id) else {
        return not_found();
    };
    let access = access_request(&services, &request);
    drop(request);
    if let Err(refused) = viewer(&services, id, &access).await {
        return *refused;
    }
    let (Some(number), Ok(name)) = (version_number(&n), ArtifactPath::stored(&name)) else {
        return not_found();
    };
    match services.artifacts.served(id, number, &name).await {
        Ok(served) => file_reply(&name, served.visibility, served.bytes),
        Err(error) => ApiError::from(error).into_response(),
    }
}

/// `POST /a/{id}/share`: the owner's button on the wrapper.
pub(crate) async fn share<P: Ports>(
    State(services): State<Services<P>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (access, same) = (
        access_request(&services, &request),
        same_origin(&services, &request),
    );
    button(&services, &id, &access, same, Button::Share).await
}

/// `POST /a/{id}/unshare`: the owner's button on the wrapper.
pub(crate) async fn unshare<P: Ports>(
    State(services): State<Services<P>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (access, same) = (
        access_request(&services, &request),
        same_origin(&services, &request),
    );
    button(&services, &id, &access, same, Button::Unshare).await
}

/// `GET /.liftoff/callback?code=` on `[artifacts] host`: the gate's
/// one-time code becomes the host's session (the v1 SSO handover), `404` anywhere else.
pub(crate) async fn callback<P: Ports>(
    State(services): State<Services<P>>,
    request: Request,
) -> Response {
    let Some(host) = services.settings.artifacts.host.clone() else {
        return not_found();
    };
    let code = query_value(request.uri().query().unwrap_or_default(), "code")
        .and_then(|raw| GateCodeSecret::parse(&raw).ok());
    let answer = services.gate.redeem(&host, code.as_ref()).await;
    render(services.gate.api_host(), answer)
}

/// Which of the owner's buttons was pressed.
#[derive(Debug, Clone, Copy)]
enum Button {
    Share,
    Unshare,
}

/// The wrapper of version `n` of `id` (the current one without `n`).
async fn wrapped<P: Ports>(
    services: &Services<P>,
    id: &str,
    n: Option<&str>,
    access: &AccessRequest,
) -> Response {
    let Some(id) = artifact_id(id) else {
        return not_found();
    };
    let user = match viewer(services, id, access).await {
        Ok(user) => user,
        Err(refused) => return *refused,
    };
    let view = match services.artifacts.view(id, user.as_ref()).await {
        Ok(view) => view,
        Err(error) => return ApiError::from(error).into_response(),
    };
    let shown = match n {
        None => view.artifact.current_version,
        Some(n) => match version_number(n) {
            Some(number) => number,
            None => return not_found(),
        },
    };
    if !view.versions.iter().any(|version| version.number == shown) {
        return not_found();
    }
    let base = base_of(services, id);
    let page = pages::artifact(&ArtifactPage {
        view: &view,
        shown,
        base: &base,
    });
    html(StatusCode::OK, page, Some(pages::ARTIFACT_CSP))
}

/// The owner's `button` on `id`, posted from a page of the viewer's
/// own host when `same_origin`: `303` back to the wrapper once done.
async fn button<P: Ports>(
    services: &Services<P>,
    id: &str,
    access: &AccessRequest,
    same_origin: bool,
    button: Button,
) -> Response {
    let Some(id) = artifact_id(id) else {
        return not_found();
    };
    if !same_origin {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            Some("the form was not sent from this site".to_owned()),
        )
        .into_response();
    }
    let user = match viewer(services, id, access).await {
        Ok(Some(user)) => user,
        Ok(None) => return redirect(&login_url(services, id)),
        Err(refused) => return *refused,
    };
    let done = match button {
        Button::Share => services.artifacts.share(&user, id).await,
        Button::Unshare => services.artifacts.unshare(&user, id).await,
    };
    match done {
        Ok(_artifact) => {
            let mut reply = redirect(&base_of(services, id));
            *reply.status_mut() = StatusCode::SEE_OTHER;
            reply
        }
        Err(error) => ApiError::from(error).into_response(),
    }
}

/// Who views `id` once [`Access::check`](liftoff_core::service::Access::check)
/// lets `access` through (`None` for a `shared` artifact without a
/// session), or the answer that refuses it: `302` to the login page
/// returning to the wrapper, `401` for a JSON client, `403`, `404`.
async fn viewer<P: Ports>(
    services: &Services<P>,
    id: ArtifactId,
    access: &AccessRequest,
) -> Result<Option<User>, Box<Response>> {
    let decision = services
        .access
        .check(access, &LinkTarget::Artifact(id))
        .await;
    let refused = match decision {
        Ok(AccessDecision::Allow { user }) => return Ok(user),
        Ok(AccessDecision::Login) => redirect(&login_url(services, id)),
        Ok(AccessDecision::Unauthenticated) => ApiError::new(
            StatusCode::UNAUTHORIZED,
            Some(format!("artifact {id} is private: sign in to open it")),
        )
        .into_response(),
        Ok(AccessDecision::Forbidden(reason)) => html(
            StatusCode::FORBIDDEN,
            pages::forbidden(&reason),
            Some(pages::CSP),
        ),
        Err(error) => ApiError::from(error).into_response(),
    };
    Err(Box::new(refused))
}

/// What [`Access::check`](liftoff_core::service::Access::check) is told
/// of `request`: its host (the API host when it names none), the host's
/// session and the API host's `browser` session in its cookies, its
/// bearer and `Accept`.
fn access_request<P: Ports>(services: &Services<P>, request: &Request) -> AccessRequest {
    let headers = request.headers();
    AccessRequest {
        host: host_of(request).unwrap_or_else(|| services.settings.proxy.api_host.clone()),
        cookie: cookie(headers, GATE_COOKIE).and_then(|raw| GateSecret::parse(raw).ok()),
        session: cookie(headers, SESSION_COOKIE).and_then(|raw| SessionSecret::parse(raw).ok()),
        bearer: bearer(request),
        accept: headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned),
        request: hook_request(request),
    }
}

/// The host `request` is for: its URI's (HTTP/2) or its `Host` header's,
/// without the port.
pub(crate) fn host_of(request: &Request) -> Option<Host> {
    let raw = if let Some(host) = request.uri().host() {
        host
    } else {
        let value = request.headers().get(header::HOST)?.to_str().ok()?;
        value.rsplit_once(':').map_or(value, |(host, port)| {
            if port.bytes().all(|byte| byte.is_ascii_digit()) {
                host
            } else {
                value
            }
        })
    };
    Host::new(raw).ok()
}

/// The host whose pages the viewer is: `[artifacts] host`, else the API
/// host.
fn own_host<P: Ports>(services: &Services<P>) -> &Host {
    let settings = &services.settings;
    settings
        .artifacts
        .host
        .as_ref()
        .unwrap_or(&settings.proxy.api_host)
}

/// Whether the form of `request` came from a page of the viewer's own
/// host: its `Origin` is `https://<host>` (a browser sends one with every form it posts; a form of another site never acts with the owner's cookie).
fn same_origin<P: Ports>(services: &Services<P>, request: &Request) -> bool {
    let expected = format!("https://{}", own_host(services));
    request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin.eq_ignore_ascii_case(&expected))
}

/// The wrapper's path of `id` on the viewer's host: `/a/<id>`, or
/// `/<id>` on `[artifacts] host`.
fn base_of<P: Ports>(services: &Services<P>, id: ArtifactId) -> String {
    if services.settings.artifacts.host.is_some() {
        format!("/{id}")
    } else {
        format!("/a/{id}")
    }
}

/// Where a browser without a session signs in, returning to the wrapper
/// of `id` (a file returns to its wrapper, never to itself):
/// `https://<api host>/login?rd=/a/<id>` on the API host, whose cookie
/// it gets there; on `[artifacts] host`, which never sees that cookie,
/// `https://<api host>/gate/start?host=<artifacts host>&rd=/<id>`, the
/// gate's handover (the login first when there is no session yet).
fn login_url<P: Ports>(services: &Services<P>, id: ArtifactId) -> String {
    let settings = &services.settings;
    let rd = encode(&base_of(services, id));
    let api = &settings.proxy.api_host;
    settings.artifacts.host.as_ref().map_or_else(
        || format!("https://{api}/login?rd={rd}"),
        |host| {
            format!(
                "https://{api}/gate/start?host={}&rd={rd}",
                encode(host.as_str())
            )
        },
    )
}

/// Where the one-time link of an artifact lands once it set the API
/// host's cookie: the artifact's address on the
/// API host; on `[artifacts] host`, which never sees that cookie, the
/// gate's handover to it ([`login_url`]).
pub(crate) fn landing<P: Ports>(services: &Services<P>, id: ArtifactId) -> String {
    if services.settings.artifacts.host.is_some() {
        login_url(services, id)
    } else {
        services.artifacts.url(id)
    }
}

/// The id `raw` names, if it is one.
fn artifact_id(raw: &str) -> Option<ArtifactId> {
    ArtifactId::parse(raw).ok()
}

/// The version `raw` names: a decimal number from 1.
fn version_number(raw: &str) -> Option<VersionNumber> {
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = raw.parse::<u32>().ok()?;
    VersionNumber::new(number).ok()
}

/// `404` of an artifact, version or file that is not there.
fn not_found() -> Response {
    ApiError::not_found().into_response()
}

/// The bytes of the file `name` in an opaque origin, typed by its
/// extension, kept by a browser according to `visibility`.
fn file_reply(name: &ArtifactPath, visibility: Visibility, bytes: Vec<u8>) -> Response {
    let mut reply = Response::new(Body::from(bytes));
    let headers = reply.headers_mut();
    let _previous = headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(name.as_str())),
    );
    let _previous = headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(FILE_CSP),
    );
    let _previous = headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let cache = if visibility == Visibility::Shared {
        "public, max-age=300"
    } else {
        "private, no-store"
    };
    let _previous = headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    let _previous = reply.extensions_mut().insert(OwnCacheControl);
    reply
}

/// The media type of the file `name` by its extension;
/// `application/octet-stream` for an unknown one or none.
fn content_type(name: &str) -> &'static str {
    let file = name.rsplit('/').next().unwrap_or(name);
    let Some((_, extension)) = file.rsplit_once('.') else {
        return "application/octet-stream";
    };
    match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod content_type {
        use super::*;

        // TC: VIEW-04 (the extension of the last segment, in any case)
        #[test]
        fn types_by_last_extension_when_name_nested() {
            assert_eq!(
                [
                    content_type("a.b/site.CSS"),
                    content_type("img.d/logo"),
                    content_type("archive.tar.gz"),
                ],
                [
                    "text/css; charset=utf-8",
                    "application/octet-stream",
                    "application/octet-stream"
                ]
            );
        }
    }

    mod version_number {
        use super::*;

        // TC: VIEW-05 (a version is a decimal number from 1)
        #[test]
        fn none_when_not_a_positive_decimal() {
            assert_eq!(
                ["0", "+1", "latest", "2"].map(|raw| version_number(raw).map(VersionNumber::get)),
                [None, None, None, Some(2)]
            );
        }
    }
}
