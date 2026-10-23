//! `/projects`, `/projects/{p}` and `/projects/{p}/resolve`.

use axum::extract::{Path, State};
use axum::response::Response;
use axum::{Extension, Json};
use liftoff_api::{
    Deleted, ProjectCreate, ProjectPatch, ProjectResolve, ProjectTemplate, ProjectView,
};
use liftoff_core::domain::{
    Host, NewProject, ProjectName, RepoRef, TemplateGroup, TemplateRef, TenantSlug,
};
use liftoff_core::ports::Ports;
use liftoff_core::service::Principal;

use super::{created, deleted, detached};
use crate::error::ApiError;
use crate::extract::ApiJson;
use crate::services::Services;
use crate::view;

/// `GET /projects`.
pub(crate) async fn list<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<Json<Vec<ProjectView>>, ApiError> {
    let projects = services.projects.list(caller.user()).await?;
    Ok(Json(projects.iter().map(view::project).collect()))
}

/// `POST /projects`.
pub(crate) async fn create<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<ProjectCreate>,
) -> Result<Response, ApiError> {
    let new = NewProject {
        tenant: body
            .tenant
            .as_deref()
            .map(TenantSlug::new)
            .transpose()
            .map_err(|e| e.with_field("tenant"))?,
        name: ProjectName::new(&body.name).map_err(|e| e.with_field("name"))?,
        git_host: Host::new(&body.git_host).map_err(|e| e.with_field("git_host"))?,
        repo: RepoRef::new(&body.repo)?,
        manifest: body.manifest,
    };
    let template = body.template.as_ref().map(template_ref).transpose()?;
    let project = detached(async move {
        services
            .projects
            .create_from_template(caller.user(), new, template)
            .await
    })
    .await?;
    Ok(created(view::project(&project)))
}

/// `GET /projects/{p}`.
pub(crate) async fn get<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Json<ProjectView>, ApiError> {
    let project = services
        .projects
        .get(caller.user(), &ProjectName::new(&name)?)
        .await?;
    Ok(Json(view::project(&project)))
}

/// `PATCH /projects/{p}`.
pub(crate) async fn patch<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<ProjectPatch>,
) -> Result<Json<ProjectView>, ApiError> {
    let project = services
        .projects
        .set_manifest(caller.user(), &ProjectName::new(&name)?, body.manifest)
        .await?;
    Ok(Json(view::project(&project)))
}

/// `DELETE /projects/{p}`.
pub(crate) async fn delete<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let name = ProjectName::new(&name)?;
    services.projects.remove(caller.user(), &name).await?;
    Ok(deleted(&name))
}

/// `POST /projects/{p}/resolve`: the project of the CLI's `git remote`.
/// `{p}` is a placeholder (the CLI sends `-`): the body names the
/// repository.
pub(crate) async fn resolve<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(_placeholder): Path<String>,
    ApiJson(body): ApiJson<ProjectResolve>,
) -> Result<Json<ProjectView>, ApiError> {
    let git_host = Host::new(&body.git_host).map_err(|e| e.with_field("git_host"))?;
    let repo = RepoRef::new(&body.repo)?;
    let project = services
        .projects
        .resolve(caller.user(), &git_host, &repo)
        .await?;
    Ok(Json(view::project(&project)))
}

/// `{group, name}` of `POST /projects {template}`: a group of the
/// catalogue and a name that is not blank.
pub(crate) fn template_ref(body: &ProjectTemplate) -> Result<TemplateRef, ApiError> {
    let group = TemplateGroup::parse(&body.group).map_err(|e| e.with_field("template.group"))?;
    Ok(TemplateRef::new(group, &body.name)?)
}
