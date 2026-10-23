//! `/git/connections`: the caller's own forge connections;
//! `/git/templates`: the template catalogue they read;
//! `/git/create-plan`: the plan of `lof create`;
//! `/git/generate`: its forge step.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use axum::{Extension, Json};
use liftoff_api::{
    ConnectionCreate, ConnectionView, CreatePlanRequest, CreatePlanView, Deleted,
    GeneratedRepoView, GitDevicePending, GitDevicePoll, GitDeviceStart, GitDeviceView,
    TemplateCatalogView, TemplatesQuery,
};
use liftoff_core::Error;
use liftoff_core::domain::{
    DeviceCode, GitProvider, Host, Login, ProjectName, RepoRef, SecretValue, TemplateGroup,
    TemplateSource, TenantSlug,
};
use liftoff_core::manifest::ProjectManifest;
use liftoff_core::ports::Ports;
use liftoff_core::service::{
    CreateOptions, CreatePlan, DevicePolled, ExistingManifest, ExistingRepo, Principal,
};

use super::{created, deleted, detached};
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery};
use crate::services::Services;
use crate::view;

/// `GET /git/connections`.
pub(crate) async fn list<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<Json<Vec<ConnectionView>>, ApiError> {
    let connections = services.git.list(caller.user()).await?;
    Ok(Json(connections.iter().map(view::connection).collect()))
}

/// `POST /git/connections`: checks the token with the forge, then stores
/// it sealed.
pub(crate) async fn create<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<ConnectionCreate>,
) -> Result<Response, ApiError> {
    let provider = GitProvider::parse(&body.provider).map_err(|e| e.with_field("provider"))?;
    let host = Host::new(&body.host)?;
    let token = SecretValue::new(body.token);
    let connection = detached(async move {
        services
            .git
            .connect(caller.user(), provider, host, token)
            .await
    })
    .await?;
    Ok(created(view::connection(&connection)))
}

/// `POST /git/connections/{host}/device` [`GitDeviceStart`]: starts the
/// forge's OAuth device flow, `200` [`GitDeviceView`]. A forge module without an OAuth application answers
/// `400` with a hint to connect with `--token` (GIT-01).
pub(crate) async fn device_start<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(host): Path<String>,
    ApiJson(body): ApiJson<GitDeviceStart>,
) -> Result<Json<GitDeviceView>, ApiError> {
    let provider = GitProvider::parse(&body.provider).map_err(|e| e.with_field("provider"))?;
    let host = Host::new(&host)?;
    let started = detached(async move {
        services
            .git
            .device_start(caller.user(), provider, host)
            .await
    })
    .await?;
    Ok(Json(GitDeviceView {
        device_code: started.device_code.expose().to_owned(),
        user_code: started.user_code,
        verification_uri: started.verification_uri,
        interval: started.interval,
        expires_in: started.expires_in,
    }))
}

/// `POST /git/connections/{host}/device/poll` [`GitDevicePoll`]: `202`
/// [`GitDevicePending`] until the person confirmed (`slow_down` with the
/// forge's new interval when it asks to poll less often), then `201`
/// [`ConnectionView`] stored as `POST /git/connections` stores a token
/// (kind `oauth`).
pub(crate) async fn device_poll<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(host): Path<String>,
    ApiJson(body): ApiJson<GitDevicePoll>,
) -> Result<Response, ApiError> {
    let provider = GitProvider::parse(&body.provider).map_err(|e| e.with_field("provider"))?;
    let host = Host::new(&host)?;
    let device_code = DeviceCode::new(&body.device_code)?;
    let polled = detached(async move {
        services
            .git
            .device_poll(caller.user(), provider, host, &device_code)
            .await
    })
    .await?;
    let waiting = |status: &str, interval| {
        let body = GitDevicePending {
            status: status.to_owned(),
            interval,
        };
        (StatusCode::ACCEPTED, Json(body)).into_response()
    };
    Ok(match polled {
        DevicePolled::Pending => waiting("pending", None),
        DevicePolled::SlowDown { interval } => waiting("slow_down", interval),
        DevicePolled::Connected(connection) => created(view::connection(&connection)),
    })
}

/// `DELETE /git/connections/{host}`.
pub(crate) async fn delete<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(host): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let host = Host::new(&host)?;
    services.git.disconnect(caller.user(), &host).await?;
    Ok(deleted(&host))
}

/// `GET /git/templates?group=&repos=`: the catalogue of the server's
/// template repositories plus `repos` (comma-separated
/// `[host/]owner/name`, the CLI's `[local] templates`), read as the
/// caller's own connections.
pub(crate) async fn templates<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiQuery(query): ApiQuery<TemplatesQuery>,
) -> Result<Json<TemplateCatalogView>, ApiError> {
    let group = query
        .group
        .as_deref()
        .map(|raw| TemplateGroup::parse(raw).map_err(|e| e.with_field("group")))
        .transpose()?;
    let repos = query
        .repos
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| TemplateSource::new(raw).map_err(|e| e.with_field("repos")))
        .collect::<Result<Vec<_>, _>>()?;
    let catalog = services
        .templates
        .list(caller.user(), &repos, group)
        .await?;
    Ok(Json(view::catalog(&catalog)))
}

/// `POST /git/create-plan` [`CreatePlanRequest`]: `200` [`CreatePlanView`],
/// the plan of `lof create` built and validated against the store (a taken name, repository or domain is `409`).
/// Nothing is created: the CLI runs the steps.
pub(crate) async fn create_plan<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<CreatePlanRequest>,
) -> Result<Json<CreatePlanView>, ApiError> {
    let plan = plan_of(services, caller, body).await?;
    Ok(Json(view::create_plan(&plan)))
}

/// `POST /git/generate` [`CreatePlanRequest`] of a template: `201`
/// [`GeneratedRepoView`].
/// The plan's `generate_repo` step, which the CLI cannot run with a
/// server: the forge connection is the caller's on the server.
/// `CreateService::generate_from_template` plans the request again (a
/// name taken since `create-plan` is still `409`) and generates the
/// repository through the caller's connection. A body of an existing
/// repository has nothing to generate: `400` on `template`.
pub(crate) async fn generate<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<CreatePlanRequest>,
) -> Result<Response, ApiError> {
    let (name, options) = request_of(&body)?;
    let (Some(raw), None) = (body.template, body.existing) else {
        return Err(Error::invalid("template", "generate needs a template").into());
    };
    let template = TemplateSource::new(&raw).map_err(|e| e.with_field("template"))?;
    let generated = detached(async move {
        services
            .create
            .generate_from_template(caller.user(), name, template, options)
            .await
    })
    .await?;
    Ok(created(GeneratedRepoView {
        git_host: generated.git_host.to_string(),
        repo: generated.repo.to_string(),
    }))
}

/// The project name and the options of `body`.
fn request_of(body: &CreatePlanRequest) -> Result<(ProjectName, CreateOptions), Error> {
    let name = ProjectName::new(&body.name).map_err(|e| e.with_field("name"))?;
    let options = CreateOptions {
        domain: body
            .domain
            .as_deref()
            .map(Host::new)
            .transpose()
            .map_err(|e| e.with_field("domain"))?,
        public: body.public,
        owner: body
            .owner
            .as_deref()
            .map(Login::new)
            .transpose()
            .map_err(|e| e.with_field("owner"))?,
        tenant: body
            .tenant
            .as_deref()
            .map(TenantSlug::new)
            .transpose()
            .map_err(|e| e.with_field("tenant"))?,
    };
    Ok((name, options))
}

/// The plan of `body` as `caller`: from its template, or of its existing
/// repository (exactly one of them).
async fn plan_of<P: Ports>(
    services: Services<P>,
    caller: Principal,
    body: CreatePlanRequest,
) -> Result<CreatePlan, ApiError> {
    let (name, options) = request_of(&body)?;
    match (body.template, body.existing) {
        (Some(raw), None) => {
            let template = TemplateSource::new(&raw).map_err(|e| e.with_field("template"))?;
            detached(async move {
                services
                    .create
                    .plan_from_template(caller.user(), name, template, options)
                    .await
            })
            .await
        }
        (None, Some(existing)) => {
            let manifest = ProjectManifest::parse(&existing.manifest)?;
            let existing = ExistingRepo {
                git_host: Host::new(&existing.git_host)
                    .map_err(|e| e.with_field("existing.git_host"))?,
                repo: RepoRef::new(&existing.repo).map_err(|e| e.with_field("existing.repo"))?,
                manifest: if existing.detected {
                    ExistingManifest::Detected(manifest)
                } else {
                    ExistingManifest::Committed(manifest)
                },
            };
            detached(async move {
                services
                    .create
                    .plan_for_existing(caller.user(), name, existing, options)
                    .await
            })
            .await
        }
        _ => Err(Error::invalid("template", "give exactly one of template and existing").into()),
    }
}
