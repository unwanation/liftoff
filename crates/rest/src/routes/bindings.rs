//! `DELETE /bindings/{host}` (binding is under the environment routes).

use axum::extract::{Path, State};
use axum::{Extension, Json};
use liftoff_api::Deleted;
use liftoff_core::domain::Host;
use liftoff_core::ports::Ports;
use liftoff_core::service::Principal;

use super::{deleted, detached};
use crate::error::ApiError;
use crate::services::Services;

/// `DELETE /bindings/{host}`: the domain answers the 503 page again.
pub(crate) async fn unbind<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(host): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let host = Host::new(&host)?;
    let shown = host.clone();
    detached(async move { services.bindings.unbind(caller.user(), &host).await }).await?;
    Ok(deleted(&shown))
}
