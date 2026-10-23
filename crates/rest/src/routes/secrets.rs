//! `/secrets`: a secret is named by its key, its scope by
//! `?scope=tenant|project|environment&id=<project>[/<environment>]`
//! (`tenant`: the tenant the project is in).

use axum::extract::{Path, State};
use axum::{Extension, Json};
use liftoff_api::{
    Deleted, EnvVar, ResolveQuery, ResolvedEnv, ScopeQuery, SecretPut, SecretRevisionView,
    SecretRollback, SecretSyncFailure, SecretValueView, SecretView, SecretsSync, SecretsSynced,
};
use liftoff_core::Error;
use liftoff_core::domain::{EnvName, ProjectName, ScopeRef, SecretKey, SecretValue, VersionNumber};
use liftoff_core::ports::Ports;
use liftoff_core::service::Principal;

use super::{deleted, detached};
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery};
use crate::services::Services;
use crate::view;

/// The scope `scope` and `id` name. `tenant` names the tenant of the
/// project `id`; the service answers `424` `tenants` for a project outside
/// every tenant.
fn scope_ref(scope: &str, id: &str) -> Result<ScopeRef, Error> {
    match scope {
        "tenant" => Ok(ScopeRef::Tenant(
            ProjectName::new(id).map_err(|e| e.with_field("id"))?,
        )),
        "project" => Ok(ScopeRef::Project(
            ProjectName::new(id).map_err(|e| e.with_field("id"))?,
        )),
        "environment" => {
            let (project, env) = id
                .split_once('/')
                .ok_or_else(|| Error::invalid("id", "an environment is <project>/<environment>"))?;
            Ok(ScopeRef::Environment(
                ProjectName::new(project).map_err(|e| e.with_field("id"))?,
                EnvName::new(env).map_err(|e| e.with_field("id"))?,
            ))
        }
        _ => Err(Error::invalid(
            "scope",
            "one of: tenant project environment",
        )),
    }
}

fn key(raw: &str) -> Result<SecretKey, Error> {
    Ok(SecretKey::new(raw).map_err(|e| e.with_field("key"))?)
}

/// `GET /secrets?scope=&id=`: keys, scopes and flags, never values.
pub(crate) async fn list<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiQuery(query): ApiQuery<ScopeQuery>,
) -> Result<Json<Vec<SecretView>>, ApiError> {
    let scope = scope_ref(&query.scope, &query.id)?;
    let entries = services.secrets.list(caller.user(), &scope).await?;
    Ok(Json(entries.iter().map(view::secret).collect()))
}

/// `PUT /secrets`: the next version of a key; the answer has no value.
pub(crate) async fn put<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<SecretPut>,
) -> Result<Json<SecretView>, ApiError> {
    let scope = scope_ref(&body.scope, &body.id)?;
    let key = key(&body.key)?;
    let entry = services
        .secrets
        .set(
            caller.user(),
            &scope,
            key,
            SecretValue::new(body.value),
            body.ci,
        )
        .await?;
    Ok(Json(view::secret(&entry)))
}

/// `GET /secrets/{key}?scope=&id=`: the current value, for the owner of
/// the scope only (SEC-10).
pub(crate) async fn get<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(raw): Path<String>,
    ApiQuery(query): ApiQuery<ScopeQuery>,
) -> Result<Json<SecretValueView>, ApiError> {
    let scope = scope_ref(&query.scope, &query.id)?;
    let key = key(&raw)?;
    let (value, version) = services
        .secrets
        .get_versioned(caller.user(), &scope, &key)
        .await?;
    Ok(Json(SecretValueView {
        key: key.to_string(),
        value: value.expose().to_owned(),
        version: version.get(),
    }))
}

/// `DELETE /secrets/{key}?scope=&id=`: the key with its history.
pub(crate) async fn delete<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(raw): Path<String>,
    ApiQuery(query): ApiQuery<ScopeQuery>,
) -> Result<Json<Deleted>, ApiError> {
    let scope = scope_ref(&query.scope, &query.id)?;
    let key = key(&raw)?;
    services.secrets.remove(caller.user(), &scope, &key).await?;
    Ok(deleted(&key))
}

/// `GET /secrets/{key}/versions?scope=&id=`: newest first, no values.
pub(crate) async fn versions<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(raw): Path<String>,
    ApiQuery(query): ApiQuery<ScopeQuery>,
) -> Result<Json<Vec<SecretRevisionView>>, ApiError> {
    let scope = scope_ref(&query.scope, &query.id)?;
    let key = key(&raw)?;
    let revisions = services
        .secrets
        .history(caller.user(), &scope, &key)
        .await?;
    Ok(Json(revisions.iter().map(view::revision).collect()))
}

/// `POST /secrets/{key}/rollback?scope=&id=`: the value of version `to`
/// as the next version.
pub(crate) async fn rollback<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(raw): Path<String>,
    ApiQuery(query): ApiQuery<ScopeQuery>,
    ApiJson(body): ApiJson<SecretRollback>,
) -> Result<Json<SecretView>, ApiError> {
    let scope = scope_ref(&query.scope, &query.id)?;
    let key = key(&raw)?;
    let to = VersionNumber::new(body.to).map_err(|e| e.with_field("to"))?;
    let entry = services
        .secrets
        .rollback(caller.user(), &scope, &key, to)
        .await?;
    Ok(Json(view::secret(&entry)))
}

/// `POST /secrets/sync`: the project's `ci = true` secrets to its
/// repository's CI.
pub(crate) async fn sync<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<SecretsSync>,
) -> Result<Json<SecretsSynced>, ApiError> {
    let project = ProjectName::new(&body.project).map_err(|e| e.with_field("project"))?;
    let report =
        detached(async move { services.secret_sync.sync(caller.user(), &project).await }).await?;
    Ok(Json(SecretsSynced {
        synced: report.synced.iter().map(ToString::to_string).collect(),
        unchanged: report.unchanged.iter().map(ToString::to_string).collect(),
        failed: report
            .failed
            .iter()
            .map(|failure| SecretSyncFailure {
                key: failure.key.to_string(),
                error: failure.error.clone(),
            })
            .collect(),
    }))
}

/// `GET /secrets/resolve?project=&env=`: the env the environment runs
/// with, in order (`lof secret pull`).
pub(crate) async fn resolve<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiQuery(query): ApiQuery<ResolveQuery>,
) -> Result<Json<ResolvedEnv>, ApiError> {
    let project = ProjectName::new(&query.project).map_err(|e| e.with_field("project"))?;
    let env = EnvName::new(&query.env).map_err(|e| e.with_field("env"))?;
    let resolved = services
        .secrets
        .resolve(caller.user(), &project, &env, None)
        .await?;
    Ok(Json(ResolvedEnv {
        vars: resolved
            .vars()
            .iter()
            .map(|(key, value)| EnvVar {
                key: key.to_string(),
                value: value.expose().to_owned(),
            })
            .collect(),
    }))
}
