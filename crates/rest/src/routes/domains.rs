//! `/domains` and `/domains/{host}`.

use axum::extract::{Path, State};
use axum::response::Response;
use axum::{Extension, Json};
use liftoff_api::{Deleted, DomainCreate, DomainPatch, DomainView};
use liftoff_core::Error;
use liftoff_core::domain::{Host, Label, ProjectName};
use liftoff_core::ports::Ports;
use liftoff_core::service::Principal;

use super::{created, deleted, detached};
use crate::error::ApiError;
use crate::extract::ApiJson;
use crate::services::Services;
use crate::view;

/// `GET /domains`: every domain, by host.
pub(crate) async fn list<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<Json<Vec<DomainView>>, ApiError> {
    let domains = services.domains.list(caller.user()).await?;
    Ok(Json(domains.iter().map(view::domain_record).collect()))
}

/// `POST /domains`: a label under the base (`proxied` from the start
/// with `dns.cloudflare`), or a full host with `external`.
pub(crate) async fn create<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<DomainCreate>,
) -> Result<Response, ApiError> {
    let project = ProjectName::new(&body.project).map_err(|e| e.with_field("project"))?;
    let shown_project = project.clone();
    let domain = if body.external {
        if body.proxied {
            return Err(Error::invalid(
                "proxied",
                "only managed domains can be proxied, not an external one",
            )
            .into());
        }
        let host = Host::new(&body.name).map_err(|e| e.with_field("name"))?;
        detached(async move {
            services
                .domains
                .add_external(caller.user(), &project, host)
                .await
        })
        .await?
    } else {
        let label = Label::new(&body.name).map_err(|e| e.with_field("name"))?;
        detached(async move {
            services
                .domains
                .add_managed(caller.user(), &project, label, body.proxied)
                .await
        })
        .await?
    };
    Ok(created(view::domain(&shown_project, &domain)))
}

/// `PATCH /domains/{host}`: `proxied` needs a DNS module (`424`
/// without one).
pub(crate) async fn patch<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(host): Path<String>,
    ApiJson(body): ApiJson<DomainPatch>,
) -> Result<Json<DomainView>, ApiError> {
    let host = Host::new(&host)?;
    let record = services
        .domains
        .set_proxied(caller.user(), &host, body.proxied)
        .await?;
    Ok(Json(view::domain_record(&record)))
}

/// `DELETE /domains/{host}`: only without a binding.
pub(crate) async fn delete<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(host): Path<String>,
) -> Result<Json<Deleted>, ApiError> {
    let host = Host::new(&host)?;
    let shown = host.clone();
    detached(async move { services.domains.remove(caller.user(), &host).await }).await?;
    Ok(deleted(&shown))
}
