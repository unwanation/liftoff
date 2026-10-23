//! `/artifacts`: publish,
//! update, list, get, versions, rollback, delete, delete a version, share
//! and unshare. `POST /artifacts` and `PUT /artifacts/{id}` take
//! `multipart/form-data` (`html`, a `files` field per file named by its
//! `filename`, `title`) or the JSON [`liftoff_api::ArtifactBody`] with
//! base64 files ([`ArtifactForm`]).
//!
//! Every answer about one artifact is an [`ArtifactView`]; only the
//! publish of a private artifact carries `login_url`, a one-time link
//! that logs its owner's browser in (amendment of).
//! Without a session every route is `401`, another user's artifact
//! `403`, an unknown artifact or version `404` (ART-18).

use axum::extract::{Path, State};
use axum::response::Response;
use axum::{Extension, Json};
use liftoff_api::{ArtifactRollback, ArtifactVersionView, ArtifactView, ArtifactsQuery, Deleted};
use liftoff_core::Error;
use liftoff_core::domain::{Artifact, ArtifactId, VersionNumber};
use liftoff_core::ports::Ports;
use liftoff_core::service::{Principal, PublishedArtifact};

use super::{created, deleted, detached};
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, ArtifactForm};
use crate::services::Services;
use crate::view;

/// `GET /artifacts?all`.
pub(crate) async fn list<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiQuery(query): ApiQuery<ArtifactsQuery>,
) -> Result<Json<Vec<ArtifactView>>, ApiError> {
    let artifacts = services.artifacts.list(caller.user(), query.all).await?;
    Ok(Json(
        artifacts
            .iter()
            .map(|artifact| answer(&services, artifact))
            .collect(),
    ))
}

/// `POST /artifacts`: `201`, with `login_url` while it is private.
pub(crate) async fn publish<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ArtifactForm(content): ArtifactForm,
) -> Result<Response, ApiError> {
    let artifacts = services.artifacts.clone();
    let published =
        detached(async move { artifacts.publish(caller.user(), content).await }).await?;
    Ok(created(published_view(published)))
}

/// `GET /artifacts/{id}`.
pub(crate) async fn get<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<ArtifactView>, ApiError> {
    let artifact = services
        .artifacts
        .get(caller.user(), artifact_id(&id)?)
        .await?;
    Ok(Json(answer(&services, &artifact)))
}

/// `PUT /artifacts/{id}`: a new current version.
pub(crate) async fn update<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
    ArtifactForm(content): ArtifactForm,
) -> Result<Json<ArtifactView>, ApiError> {
    let id = artifact_id(&id)?;
    let artifacts = services.artifacts.clone();
    let published =
        detached(async move { artifacts.update(caller.user(), id, content).await }).await?;
    Ok(Json(published_view(published)))
}

/// `DELETE /artifacts/{id}`: every version and its files.
pub(crate) async fn delete<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let id = artifact_id(&id)?;
    let artifacts = services.artifacts.clone();
    detached(async move { artifacts.delete(caller.user(), id).await }).await?;
    Ok(deleted(&id))
}

/// `GET /artifacts/{id}/versions`: oldest first.
pub(crate) async fn versions<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ArtifactVersionView>>, ApiError> {
    let versions = services
        .artifacts
        .versions(caller.user(), artifact_id(&id)?)
        .await?;
    Ok(Json(versions.iter().map(view::artifact_version).collect()))
}

/// `POST /artifacts/{id}/rollback {to}`: version `to` copied as the new
/// current one.
pub(crate) async fn rollback<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ArtifactRollback>,
) -> Result<Json<ArtifactView>, ApiError> {
    let id = artifact_id(&id)?;
    let to = VersionNumber::new(body.to).map_err(|error| error.with_field("to"))?;
    let artifacts = services.artifacts.clone();
    let published =
        detached(async move { artifacts.rollback(caller.user(), id, to).await }).await?;
    Ok(Json(published_view(published)))
}

/// `DELETE /artifacts/{id}/versions/{n}`: one version that is not the
/// current one (`409` for it).
pub(crate) async fn delete_version<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((id, n)): Path<(String, String)>,
) -> Result<Json<Deleted>, ApiError> {
    let id = artifact_id(&id)?;
    let number = n
        .parse::<u32>()
        .ok()
        .and_then(|n| VersionNumber::new(n).ok())
        .ok_or_else(|| ApiError::from(Error::not_found(format!("version {n} of artifact {id}"))))?;
    let artifacts = services.artifacts.clone();
    detached(async move { artifacts.delete_version(caller.user(), id, number).await }).await?;
    Ok(deleted(&format!("{id}/v/{number}")))
}

/// `POST /artifacts/{id}/share`: open to anybody, no TTL.
pub(crate) async fn share<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<ArtifactView>, ApiError> {
    let artifact = services
        .artifacts
        .share(caller.user(), artifact_id(&id)?)
        .await?;
    Ok(Json(answer(&services, &artifact)))
}

/// `POST /artifacts/{id}/unshare`: private again, with a TTL from now.
pub(crate) async fn unshare<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<ArtifactView>, ApiError> {
    let artifact = services
        .artifacts
        .unshare(caller.user(), artifact_id(&id)?)
        .await?;
    Ok(Json(answer(&services, &artifact)))
}

/// The artifact `raw` names; a path segment that is no artifact id names
/// none (`404`).
fn artifact_id(raw: &str) -> Result<ArtifactId, ApiError> {
    ArtifactId::parse(raw)
        .map_err(|_not_an_id| ApiError::from(Error::not_found(format!("artifact {raw}"))))
}

/// `artifact` as answered, at its address, without a login link.
fn answer<P: Ports>(services: &Services<P>, artifact: &Artifact) -> ArtifactView {
    view::artifact(artifact, services.artifacts.url(artifact.id), None)
}

/// What publish, update and rollback made, with the login link publish
/// issued for a private artifact.
fn published_view(published: PublishedArtifact) -> ArtifactView {
    view::artifact(
        &published.artifact,
        published.url,
        published.login_url.map(|link| link.url),
    )
}
