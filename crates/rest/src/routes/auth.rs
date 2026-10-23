//! Single-user login over HTTP: the bearer layer that
//! turns a request into a [`Principal`], and the `/auth/*`, `/me` and
//! `/admin/token` handlers.
//!
//! The layer asks [`AuthService::authenticate`](liftoff_core::service::AuthService::authenticate), which asks the
//! `authenticate` hook first (a module may name the actor) and checks the
//! `Authorization: Bearer` session otherwise ([`credential`]: from the
//! panel `liftoff-web`'s origin, the browser session's cookie stands in
//! for a missing bearer). Without a principal every
//! route behind the layer answers `401`; the answer never says why a token
//! was refused and never repeats it.

use axum::Extension;
use axum::Json;
use axum::extract::{OriginalUri, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{StatusCode, header};
use liftoff_api::{LinkCreate, LinkIssued, SessionGranted, SetupTokenGranted, TokenExchange};
use liftoff_core::Error;
use liftoff_core::domain::{Bearer, DeployTokenSecret, LinkTarget, SessionSecret, SetupSecret};
use liftoff_core::ports::{HookRequest, Ports};
use liftoff_core::service::{self, Principal};

use super::gate::{SESSION_COOKIE, cookie};
use crate::cors::PanelRequest;
use crate::error::ApiError;
use crate::extract::ApiJson;
use crate::services::Services;
use crate::view;

/// The session name when the CLI does not send one.
const DEFAULT_MACHINE: &str = "cli";

/// The parts of `request` the `authenticate` hook sees: method, the path
/// and query as the client sent them, and every header that is text and
/// not [hidden](liftoff_core::service::hidden_from_hooks) (the core's one
/// rule).
pub(crate) fn hook_request(request: &Request) -> HookRequest {
    let uri = request
        .extensions()
        .get::<OriginalUri>()
        .map_or_else(|| request.uri(), |original| &original.0);
    let path = uri
        .path_and_query()
        .map_or_else(|| uri.path().to_owned(), ToString::to_string);
    service::hook_request(
        request.method().as_str(),
        &path,
        request
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
    )
}

/// The liftoff token of `request`'s bearer, if it has one of a liftoff
/// shape ([`bearer_of`](liftoff_core::service::bearer_of)). A token that
/// is not shaped like a liftoff token is passed on as no token: the hook
/// may still know it, and without the hook it is as unknown as any other.
pub(crate) fn bearer(request: &Request) -> Option<Bearer> {
    service::bearer_of(
        request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )
}

/// The credential of `request`: its [bearer](bearer); without an
/// `Authorization` header, on a request from the panel `liftoff-web`'s
/// origin ([`PanelRequest`]), the browser
/// session's cookie (`__Host-liftoff_session`). A request from any other
/// origin never authenticates with the cookie, which `SameSite=None`
/// sends with every cross-site request.
pub(crate) fn credential(request: &Request) -> Option<Bearer> {
    if request.headers().contains_key(header::AUTHORIZATION) {
        return bearer(request);
    }
    request.extensions().get::<PanelRequest>()?;
    let raw = cookie(request.headers(), SESSION_COOKIE)?;
    SessionSecret::parse(raw).ok().map(Bearer::Session)
}

/// The bearer layer; see the module documentation.
pub(crate) async fn authenticate<P: Ports>(
    State(services): State<Services<P>>,
    mut request: Request,
    next: Next,
) -> Response {
    let principal = match services
        .auth
        .authenticate(&hook_request(&request), credential(&request).as_ref())
        .await
    {
        Ok(principal) => principal,
        Err(error) => return ApiError::from(error).into_response(),
    };
    let _previous: Option<Principal> = request.extensions_mut().insert(principal);
    next.run(request).await
}

/// Who calls `POST /deploys`: CI with its project's deploy token, or a
/// person (deploy token or session).
#[derive(Debug, Clone)]
pub(crate) enum DeployCaller {
    /// A deploy token, live when the request came in.
    Token(DeployTokenSecret),
    /// A person, authenticated like every other route.
    User(Box<Principal>),
}

/// The layer of `POST /deploys`: a live deploy-token bearer passes as
/// [`DeployCaller::Token`] (any other deploy token answers `401` before
/// the body is read); anything else goes through the bearer layer's
/// check and answers `401` without a principal.
pub(crate) async fn deploy_caller<P: Ports>(
    State(services): State<Services<P>>,
    mut request: Request,
    next: Next,
) -> Response {
    let caller = match credential(&request) {
        // Checked before the body is read: the route takes a component
        // of up to 50 MiB, which a forged token must not make it buffer.
        Some(Bearer::Deploy(token)) => match services.deploys.check_deploy_token(&token).await {
            Ok(()) => DeployCaller::Token(token),
            Err(error) => return ApiError::from(error).into_response(),
        },
        bearer => match services
            .auth
            .authenticate(&hook_request(&request), bearer.as_ref())
            .await
        {
            Ok(principal) => DeployCaller::User(Box::new(principal)),
            Err(error) => return ApiError::from(error).into_response(),
        },
    };
    let _previous: Option<DeployCaller> = request.extensions_mut().insert(caller);
    next.run(request).await
}

/// `POST /auth/token`: the single-use token for a `cli` session.
///
/// A token of the wrong shape is refused like an unknown one, `401`.
pub(crate) async fn exchange<P: Ports>(
    State(services): State<Services<P>>,
    ApiJson(body): ApiJson<TokenExchange>,
) -> Result<Json<SessionGranted>, ApiError> {
    let token = SetupSecret::parse(&body.token)
        .map_err(|_malformed| Error::unauthenticated("unknown or revoked token"))?;
    let name = body.name.as_deref().unwrap_or(DEFAULT_MACHINE);
    let grant = services.auth.exchange(&token, name).await?;
    Ok(Json(SessionGranted {
        token: grant.token.expose().to_owned(),
        user: view::user(&grant.user),
    }))
}

/// `DELETE /auth/session`: ends the caller's session.
pub(crate) async fn logout<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<StatusCode, ApiError> {
    services.auth.logout(&caller).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /me`: who the caller is.
pub(crate) async fn me(Extension(caller): Extension<Principal>) -> Json<liftoff_api::UserView> {
    Json(view::user(caller.user()))
}

/// `POST /admin/token`: a new single-use token for another machine.
pub(crate) async fn admin_token<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<Response, ApiError> {
    let token = services.auth.create_admin_token(caller.user()).await?;
    Ok((
        StatusCode::CREATED,
        Json(SetupTokenGranted {
            token: token.expose().to_owned(),
        }),
    )
        .into_response())
}

/// `POST /auth/links`: a one-time link of the caller to `target`; `201` with the link, shown once.
pub(crate) async fn create_link<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<LinkCreate>,
) -> Result<Response, ApiError> {
    let target = LinkTarget::parse(&body.target).map_err(Error::from)?;
    let link = services.links.issue(caller.user(), &target).await?;
    Ok((
        StatusCode::CREATED,
        Json(LinkIssued {
            url: link.url,
            expires_at: view::timestamp(link.expires_at),
        }),
    )
        .into_response())
}
