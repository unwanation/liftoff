//! `/admin/users`, `/admin/sessions` and `/admin/reconcile`
//! (`/admin/token` is in [`auth`](super::auth)).

use axum::extract::State;
use axum::{Extension, Json};
use http::StatusCode;
use liftoff_api::{Deleted, SessionsRevoke, UserView};
use liftoff_core::domain::Login;
use liftoff_core::ports::Ports;
use liftoff_core::service::Principal;

use super::{deleted, detached};
use crate::error::ApiError;
use crate::extract::ApiQuery;
use crate::services::Services;
use crate::view;

/// `GET /admin/users`: every user, by login.
pub(crate) async fn users<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<Json<Vec<UserView>>, ApiError> {
    let users = services.auth.users(caller.user()).await?;
    Ok(Json(users.iter().map(view::user).collect()))
}

/// `DELETE /admin/sessions?login=`: every session of that user ends.
pub(crate) async fn revoke_sessions<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiQuery(query): ApiQuery<SessionsRevoke>,
) -> Result<Json<Deleted>, ApiError> {
    let login = Login::new(&query.login)?;
    let _revoked: usize = services.auth.revoke_sessions(caller.user(), &login).await?;
    Ok(deleted(&login))
}

/// `POST /admin/reconcile`: environment states from docker, slices and
/// the whole proxy configuration, without deploys.
pub(crate) async fn reconcile<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<StatusCode, ApiError> {
    detached(async move { services.reconciler.reconcile(caller.user()).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}
