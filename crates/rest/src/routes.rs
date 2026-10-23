//! Route handlers, one file per route group. Each one parses
//! the request into domain types at the boundary, calls one service and
//! renders the answer; every rule lives in `liftoff-core`.

pub(crate) mod admin;
pub(crate) mod artifacts;
pub(crate) mod artifacts_view;
pub(crate) mod auth;
pub(crate) mod bindings;
pub(crate) mod deploys;
pub(crate) mod domains;
pub(crate) mod environments;
pub(crate) mod gate;
pub(crate) mod git;
pub(crate) mod internal;
pub(crate) mod login;
pub(crate) mod mcp;
pub(crate) mod modules;
pub(crate) mod projects;
pub(crate) mod registry;
pub(crate) mod secrets;
pub(crate) mod tunnel;

use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use liftoff_api::Deleted;
use serde::Serialize;

use crate::error::ApiError;

/// Runs `work` as its own task and waits for it.
///
/// For calls that act outside (docker, systemd, Caddy, the forge) and
/// record afterwards: when the client disconnects hyper drops the request
/// future, which would cancel the call half way and break "check, act
/// outside, record, compensate on failure". The task finishes
/// whatever the client does.
pub(crate) async fn detached<T, F>(work: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, liftoff_core::Error>> + Send + 'static,
{
    match tokio::spawn(work).await {
        Ok(result) => result.map_err(ApiError::from),
        Err(error) => {
            tracing::error!(%error, "detached request task failed");
            Err(ApiError::internal())
        }
    }
}

/// `201 Created` with `body`.
pub(crate) fn created<T: Serialize>(body: T) -> Response {
    (StatusCode::CREATED, Json(body)).into_response()
}

/// `200` with what was deleted.
pub(crate) fn deleted(what: &impl ToString) -> Json<Deleted> {
    Json(Deleted {
        deleted: what.to_string(),
    })
}
