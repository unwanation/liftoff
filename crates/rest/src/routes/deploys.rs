//! `POST /deploys`, `GET /deploys/{id}[/log]`, and the deploy token of a
//! project (`POST /projects/{p}/token/rotate`, `GET /projects/{p}/snippet`).

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use http::{HeaderValue, StatusCode, header};
use liftoff_api::{DeployBody, DeployView, DeploysQueued, SnippetView, TokenView};
use liftoff_core::domain::{Branch, DeployId, EnvName, ImageTag, ProjectName, RepoRef};
use liftoff_core::ports::Ports;
use liftoff_core::service::{CiDeployRequest, CiFunctionRequest, Principal, QueuedDeploy};
use liftoff_core::{Error, ValidationError};

use super::auth::DeployCaller;
use super::detached;
use crate::error::ApiError;
use crate::extract::DeployForm;
use crate::services::Services;
use crate::view;

/// The `Deprecation` header (RFC 9745) of a CI call that still sends `env`
/// (DEP-04).
const DEPRECATION: header::HeaderName = header::HeaderName::from_static("deprecation");

/// `field` of the body, which this kind of caller must send.
fn required<'a>(value: Option<&'a str>, field: &'static str) -> Result<&'a str, Error> {
    value.ok_or_else(|| Error::invalid(field, "is required"))
}

/// The CI half of a body: `repo`, `ref`, `tag`, `compose` and the
/// deprecated `env`; `project` and `environment` are for sessions.
fn ci_request(body: DeployBody) -> Result<CiDeployRequest, Error> {
    if body.project.is_some() || body.environment.is_some() {
        return Err(Error::invalid(
            "project",
            "a deploy token deploys by repo and ref, not by project and environment",
        ));
    }
    Ok(CiDeployRequest {
        repo: RepoRef::new(required(body.repo.as_deref(), "repo")?)?,
        git_ref: Branch::new(required(body.git_ref.as_deref(), "ref")?)
            .map_err(|e: ValidationError| e.with_field("ref"))?,
        tag: ImageTag::new(required(body.tag.as_deref(), "tag")?)?,
        compose: required(body.compose.as_deref(), "compose")?.to_owned(),
        env: body.env,
    })
}

/// `202` with the queued deploys, or `200` with none (DEP-02).
fn queued(deploys: &[QueuedDeploy]) -> Response {
    let status = if deploys.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    let body = DeploysQueued {
        deploys: deploys.iter().map(view::queued).collect(),
    };
    (status, Json(body)).into_response()
}

/// The CI call of a function: `repo`, `ref` and the multipart field
/// `component`, nothing of a compose deploy.
fn ci_function_request(body: &DeployBody, component: Vec<u8>) -> Result<CiFunctionRequest, Error> {
    if body.project.is_some() || body.environment.is_some() {
        return Err(Error::invalid(
            "project",
            "a deploy token deploys by repo and ref, not by project and environment",
        ));
    }
    if body.tag.is_some() || body.compose.is_some() || body.env.is_some() {
        return Err(Error::invalid(
            "component",
            "a component is deployed without tag, compose and env",
        ));
    }
    Ok(CiFunctionRequest {
        repo: RepoRef::new(required(body.repo.as_deref(), "repo")?)?,
        git_ref: Branch::new(required(body.git_ref.as_deref(), "ref")?)
            .map_err(|e: ValidationError| e.with_field("ref"))?,
        component,
    })
}

/// `POST /deploys`: CI with its deploy token (`repo`, `ref`, `tag`, `compose`), or a person with a session (`project`,
/// `environment`, `tag`, optionally `compose`: `lof deploy`). JSON or
/// `multipart/form-data`; a function's deploy is multipart with the
/// binary field `component` instead of `tag` and `compose`.
pub(crate) async fn create<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<DeployCaller>,
    form: DeployForm,
) -> Result<Response, ApiError> {
    let DeployForm { body, component } = form;
    match caller {
        DeployCaller::Token(token) => {
            if let Some(component) = component {
                let request = ci_function_request(&body, component)?;
                let outcome =
                    detached(
                        async move { services.deploys.request_ci_function(&token, request).await },
                    )
                    .await?;
                return Ok(queued(&outcome.deploys));
            }
            let request = ci_request(body)?;
            let outcome = services.deploys.request_ci(&token, request).await?;
            let mut response = queued(&outcome.deploys);
            if outcome.deprecated_env {
                let _previous: Option<HeaderValue> = response
                    .headers_mut()
                    .insert(DEPRECATION, HeaderValue::from_static("true"));
            }
            Ok(response)
        }
        DeployCaller::User(principal) => {
            if body.repo.is_some() || body.git_ref.is_some() || body.env.is_some() {
                return Err(Error::invalid(
                    "repo",
                    "a session deploys by project and environment; repo, ref and env are for CI",
                )
                .into());
            }
            let project = ProjectName::new(required(body.project.as_deref(), "project")?)
                .map_err(|e| e.with_field("project"))?;
            let env = EnvName::new(required(body.environment.as_deref(), "environment")?)
                .map_err(|e| e.with_field("environment"))?;
            if let Some(component) = component {
                if body.tag.is_some() || body.compose.is_some() {
                    return Err(Error::invalid(
                        "component",
                        "a component is deployed without tag and compose",
                    )
                    .into());
                }
                let deploy = detached(async move {
                    services
                        .deploys
                        .request_function(principal.user(), &project, &env, component)
                        .await
                })
                .await?;
                return Ok(queued(&[deploy]));
            }
            let tag = ImageTag::new(required(body.tag.as_deref(), "tag")?)?;
            let deploy = services
                .deploys
                .request_cli(principal.user(), &project, &env, tag, body.compose)
                .await?;
            Ok(queued(&[deploy]))
        }
    }
}

/// `GET /deploys/{id}`.
pub(crate) async fn get<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<DeployView>, ApiError> {
    let record = services
        .deploys
        .get(caller.user(), DeployId::parse(&id)?)
        .await?;
    Ok(Json(view::deploy(&record)))
}

/// `GET /deploys/{id}/log`: the step output as plain text.
pub(crate) async fn log<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let record = services
        .deploys
        .get(caller.user(), DeployId::parse(&id)?)
        .await?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        record.deploy.log,
    )
        .into_response())
}

/// `POST /projects/{p}/token/rotate`: the new token's id; its value goes
/// to the project secret `LIFTOFF_TOKEN` and the repository's CI only.
pub(crate) async fn rotate<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(project): Path<String>,
) -> Result<Json<TokenView>, ApiError> {
    let project = ProjectName::new(&project)?;
    let shown = project.clone();
    let token =
        detached(async move { services.deploys.rotate_token(caller.user(), &project).await })
            .await?;
    Ok(Json(view::token(&shown, &token)))
}

/// `GET /projects/{p}/snippet`: the CI workflow that calls `POST /deploys`,
/// for the project's forge, and the file it belongs at.
pub(crate) async fn snippet<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(project): Path<String>,
) -> Result<Json<SnippetView>, ApiError> {
    let project = ProjectName::new(&project)?;
    let snippet = services.deploys.snippet(caller.user(), &project).await?;
    Ok(Json(SnippetView {
        path: snippet.path.as_str().to_owned(),
        snippet: snippet.content,
    }))
}
