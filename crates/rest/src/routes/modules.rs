//! `/api/v1/m/{module}/...`: the routes of modules.
//!
//! The handler turns the request into a
//! [`ModuleRequest`](liftoff_core::ports::ModuleRequest) below the
//! module's prefix and hands it to
//! [`ModuleRouteService::handle`](liftoff_core::service::ModuleRouteService::handle)
//! with what authenticates it (the hook's view of the request and the
//! bearer), then answers what the module answered. These routes sit
//! outside the bearer layer: a route the module marks `public` needs no
//! session (OIDC callbacks, the login page); the service asks for one on
//! every other route.

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest as _, Request, State};
use axum::response::Response;
use http::{HeaderName, HeaderValue, StatusCode};
use liftoff_core::Error;
use liftoff_core::domain::ModuleName;
use liftoff_core::ports::{ModuleRequest, ModuleResponse, Ports};

use super::auth::{credential, hook_request};
use super::gate::cookies;
use crate::error::ApiError;
use crate::services::Services;

/// Any method on `/m/{module}` and `/m/{module}/{*path}`.
///
/// The module sees the path below `/m/<module>` (`/` for the prefix
/// itself) with the query, and the headers the hooks see (never a
/// liftoff credential or a cookie) plus the values of its login cookies
/// ([`module_headers`]; the core shows them to the browser login module
/// alone and drops the core's cookies a module answers). A module name that is not a valid name is a
/// module that is not installed (404).
pub(crate) async fn handle<P: Ports>(
    State(services): State<Services<P>>,
    request: Request,
) -> Result<Response, ApiError> {
    let hook = hook_request(&request);
    let bearer = credential(&request);
    let login_states: Vec<String> = cookies(request.headers())
        .filter(|(name, _)| name.starts_with(ModuleRequest::LOGIN_STATE_COOKIE_PREFIX))
        .map(|(_, value)| value.to_owned())
        .collect();
    let (module, path) = below_prefix(request.uri())?;
    let method = request.method().as_str().to_owned();
    let body = Bytes::from_request(request, &())
        .await
        .map_err(|rejection| ApiError::new(rejection.status(), Some(rejection.body_text())))?;
    let module_request = ModuleRequest {
        method,
        path,
        headers: module_headers(&hook.headers, &login_states),
        body: body.to_vec(),
    };
    let response = services
        .module_routes
        .handle(&module, module_request, &hook, bearer.as_ref())
        .await?;
    answer(&module, response)
}

/// The headers a module's route sees: those the hooks see (no cookie, no
/// liftoff credential) without a client's own
/// [`ModuleRequest::LOGIN_STATE_HEADER`], plus that header with the
/// values of the browser's login cookies, when it sends any.
fn module_headers(shown: &[(String, String)], login_states: &[String]) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = shown
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case(ModuleRequest::LOGIN_STATE_HEADER))
        .cloned()
        .collect();
    if !login_states.is_empty() {
        headers.push((
            ModuleRequest::LOGIN_STATE_HEADER.to_owned(),
            login_states.join(", "),
        ));
    }
    headers
}

/// The module of `/m/<module>/<rest>` and `/<rest>` with the query.
fn below_prefix(uri: &http::Uri) -> Result<(ModuleName, String), ApiError> {
    let raw = uri.path().strip_prefix("/m/").unwrap_or_default();
    let (name, rest) = raw.split_once('/').unwrap_or((raw, ""));
    let module = ModuleName::new(name)
        .map_err(|_invalid| ApiError::from(Error::not_found(format!("module {name}"))))?;
    let mut path = format!("/{rest}");
    if let Some(query) = uri.query() {
        path.push('?');
        path.push_str(query);
    }
    Ok((module, path))
}

/// The HTTP response of what `module` answered. A header that is not a
/// valid HTTP header is dropped; a status outside 100..=999 is the
/// module's failure.
fn answer(module: &ModuleName, response: ModuleResponse) -> Result<Response, ApiError> {
    let status = StatusCode::from_u16(response.status).map_err(|_invalid| {
        tracing::error!(%module, status = response.status, "a module answered an invalid status");
        ApiError::from(Error::internal_msg(format!(
            "module {module} answered status {}",
            response.status
        )))
    })?;
    let mut reply = Response::new(Body::from(response.body));
    *reply.status_mut() = status;
    for (name, value) in response.headers {
        match (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            (Ok(name), Ok(value)) => {
                let _appended = reply.headers_mut().append(name, value);
            }
            _ => {
                tracing::warn!(%module, header = %name, "dropped an invalid header a module answered");
            }
        }
    }
    Ok(reply)
}
