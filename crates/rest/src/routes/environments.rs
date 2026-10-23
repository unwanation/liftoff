//! `/projects/{p}/environments[/{e}]`: the environments of a project, their
//! lifecycle, deploys, logs and bindings.

use core::convert::Infallible;
use core::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures_util::StreamExt as _;
use http::StatusCode;
use liftoff_api::{
    BindingCreate, Deleted, DeployView, DeploysQueued, EnvironmentCreate, EnvironmentDelete,
    EnvironmentPatch, EnvironmentView, LogsQuery, RedeployBody,
};
use liftoff_core::domain::{
    Branch, ContainerPort, Cpus, DeployNumber, EnvKind, EnvName, Host, MemoryMb, ProjectName,
    ServiceName,
};
use liftoff_core::error::Error;
use liftoff_core::ports::Ports;
use liftoff_core::service::{BindRequest, DeployRecord, EnvironmentChange, LogsRequest, Principal};

use super::{created, deleted, detached};
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiJsonOrDefault, ApiQuery};
use crate::services::Services;
use crate::view;

/// How often an idle log stream says it is still there.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// `{p}` of the path.
fn project_name(raw: &str) -> Result<ProjectName, ApiError> {
    Ok(ProjectName::new(raw)?)
}

/// `{p}` and `{e}` of the path.
fn names(project: &str, env: &str) -> Result<(ProjectName, EnvName), ApiError> {
    Ok((ProjectName::new(project)?, EnvName::new(env)?))
}

/// `GET /projects/{p}/environments`.
pub(crate) async fn list<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(project): Path<String>,
) -> Result<Json<Vec<EnvironmentView>>, ApiError> {
    let project = project_name(&project)?;
    let environments = services.environments.list(caller.user(), &project).await?;
    Ok(Json(
        environments
            .iter()
            .map(|environment| view::environment(&project, environment))
            .collect(),
    ))
}

/// `POST /projects/{p}/environments`: an empty environment pinned to a
/// branch by the caller.
pub(crate) async fn create<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(project): Path<String>,
    ApiJson(body): ApiJson<EnvironmentCreate>,
) -> Result<Response, ApiError> {
    let project = project_name(&project)?;
    let name = EnvName::new(&body.name).map_err(|e| e.with_field("name"))?;
    let branch = Branch::new(&body.branch).map_err(|e| e.with_field("branch"))?;
    let kind = match body.kind.as_deref() {
        None => EnvKind::Compose,
        Some(kind) => EnvKind::parse(kind).map_err(|e| e.with_field("kind"))?,
    };
    let shown = project.clone();
    let environment = detached(async move {
        services
            .environments
            .add(caller.user(), &project, name, branch, kind)
            .await
    })
    .await?;
    Ok(created(view::environment(&shown, &environment)))
}

/// `GET /projects/{p}/environments/{e}`.
pub(crate) async fn get<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
) -> Result<Json<EnvironmentView>, ApiError> {
    let (project, env) = names(&project, &env)?;
    let environment = services
        .environments
        .get(caller.user(), &project, &env)
        .await?;
    Ok(Json(view::environment(&project, &environment)))
}

/// `PATCH /projects/{p}/environments/{e}`: the pin and the limits.
pub(crate) async fn patch<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
    ApiJson(body): ApiJson<EnvironmentPatch>,
) -> Result<Json<EnvironmentView>, ApiError> {
    let (project, env) = names(&project, &env)?;
    let change = EnvironmentChange {
        branch: body
            .branch
            .as_deref()
            .map(Branch::new)
            .transpose()
            .map_err(|e| e.with_field("branch"))?,
        cpus: body
            .cpus
            .map(|cpus| Cpus::parse(&cpus.to_string()))
            .transpose()?,
        memory_mb: body.memory_mb.map(MemoryMb::new).transpose()?,
    };
    let shown = project.clone();
    let environment = detached(async move {
        services
            .environments
            .update(caller.user(), &project, &env, change)
            .await
    })
    .await?;
    Ok(Json(view::environment(&shown, &environment)))
}

/// `DELETE /projects/{p}/environments/{e}?confirm=true`.
pub(crate) async fn delete<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
    ApiQuery(query): ApiQuery<EnvironmentDelete>,
) -> Result<Json<Deleted>, ApiError> {
    let (project, env) = names(&project, &env)?;
    let shown = env.clone();
    detached(async move {
        services
            .environments
            .remove(caller.user(), &project, &env, query.confirm)
            .await
    })
    .await?;
    Ok(deleted(&shown))
}

/// `POST /projects/{p}/environments/{e}/start`.
pub(crate) async fn start<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
) -> Result<Json<EnvironmentView>, ApiError> {
    let (project, env) = names(&project, &env)?;
    let shown = project.clone();
    let environment = detached(async move {
        services
            .environments
            .start(caller.user(), &project, &env)
            .await
    })
    .await?;
    Ok(Json(view::environment(&shown, &environment)))
}

/// `POST /projects/{p}/environments/{e}/stop`.
pub(crate) async fn stop<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
) -> Result<Json<EnvironmentView>, ApiError> {
    let (project, env) = names(&project, &env)?;
    let shown = project.clone();
    let environment = detached(async move {
        services
            .environments
            .stop(caller.user(), &project, &env)
            .await
    })
    .await?;
    Ok(Json(view::environment(&shown, &environment)))
}

/// `POST /projects/{p}/environments/{e}/share`.
pub(crate) async fn share<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
) -> Result<Json<EnvironmentView>, ApiError> {
    let (project, env) = names(&project, &env)?;
    let environment = services
        .visibility
        .share(caller.user(), &project, &env)
        .await?;
    Ok(Json(view::environment(&project, &environment)))
}

/// `POST /projects/{p}/environments/{e}/unshare`.
pub(crate) async fn unshare<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
) -> Result<Json<EnvironmentView>, ApiError> {
    let (project, env) = names(&project, &env)?;
    let environment = services
        .visibility
        .unshare(caller.user(), &project, &env)
        .await?;
    Ok(Json(view::environment(&project, &environment)))
}

/// `POST /projects/{p}/environments/{e}/redeploy`: `202` with the queued
/// copy of deploy `to` (the current one without a body).
pub(crate) async fn redeploy<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
    ApiJsonOrDefault(body): ApiJsonOrDefault<RedeployBody>,
) -> Result<Response, ApiError> {
    let (project, env) = names(&project, &env)?;
    let to = body
        .to
        .map(DeployNumber::new)
        .transpose()
        .map_err(|e| e.with_field("to"))?;
    let queued = services
        .deploys
        .redeploy(caller.user(), &project, &env, to)
        .await?;
    let body = DeploysQueued {
        deploys: vec![view::queued(&queued)],
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// `GET /projects/{p}/environments/{e}/deploys`: newest first.
pub(crate) async fn deploys<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
) -> Result<Json<Vec<DeployView>>, ApiError> {
    let (project, env) = names(&project, &env)?;
    let deploys = services
        .environments
        .deploys(caller.user(), &project, &env)
        .await?;
    Ok(Json(
        deploys
            .into_iter()
            .map(|deploy| {
                view::deploy(&DeployRecord {
                    project: project.clone(),
                    environment: env.clone(),
                    deploy,
                })
            })
            .collect(),
    ))
}

/// `GET /projects/{p}/environments/{e}/logs`: one server-sent event per
/// line; a failing stream ends with an `error` event.
pub(crate) async fn logs<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
    ApiQuery(query): ApiQuery<LogsQuery>,
) -> Result<Response, ApiError> {
    let (project, env) = names(&project, &env)?;
    let request = LogsRequest {
        service: query
            .service
            .as_deref()
            .map(ServiceName::new)
            .transpose()
            .map_err(|e| e.with_field("service"))?,
        tail: query.tail,
        follow: query.follow.unwrap_or(true),
    };
    let lines = services
        .environments
        .logs(caller.user(), &project, &env, &request)
        .await?;
    let events = lines.map(|line| {
        Ok::<_, Infallible>(match line {
            Ok(text) => Event::default().data(text),
            Err(error) => {
                tracing::error!(%error, "log stream failed");
                Event::default()
                    .event("error")
                    .data("the log stream failed")
            }
        })
    });
    Ok(Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response())
}

/// `POST /projects/{p}/environments/{e}/bindings`: `service:port` of a
/// compose environment, or only the host for a function environment.
pub(crate) async fn bind<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((project, env)): Path<(String, String)>,
    ApiJson(body): ApiJson<BindingCreate>,
) -> Result<Response, ApiError> {
    let (project, env) = names(&project, &env)?;
    let host = Host::new(&body.host)?;
    let request = match (body.service, body.port) {
        (Some(service), Some(port)) => Some(BindRequest {
            service: ServiceName::new(&service).map_err(|e| e.with_field("service"))?,
            port: ContainerPort::new(port)?,
            host: host.clone(),
        }),
        (None, None) => None,
        (Some(_), None) => {
            return Err(Error::invalid("port", "a service is bound with its port").into());
        }
        (None, Some(_)) => {
            return Err(Error::invalid("service", "a port is bound with its service").into());
        }
    };
    let (shown_project, shown_env, shown_host) = (project.clone(), env.clone(), host.clone());
    let binding = detached(async move {
        match request {
            Some(request) => {
                services
                    .bindings
                    .bind(caller.user(), &project, &env, request)
                    .await
            }
            None => {
                services
                    .bindings
                    .bind_function(caller.user(), &project, &env, &host)
                    .await
            }
        }
    })
    .await?;
    Ok(created(view::binding(
        &shown_project,
        &shown_env,
        &shown_host,
        &binding,
    )))
}
