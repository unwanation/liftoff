//! `/modules`, `/modules/available` and `/modules/{name}`: installing,
//! changing and removing modules,
//! and `/modules/{name}/tools/{tool}`: calling an MCP tool of a module. Every rule is the core's
//! `ModuleService` and `ModuleRouteService`; a module's configuration is
//! in no answer.

use axum::extract::{Path, State};
use axum::response::Response;
use axum::{Extension, Json};
use liftoff_api::{
    AvailableModuleView, Deleted, ModuleDelete, ModuleInstall, ModulePatch, ModuleToolAnswer,
    ModuleToolCall, ModuleView,
};
use liftoff_core::domain::{ModuleName, ModuleVersion};
use liftoff_core::ports::Ports;
use liftoff_core::service::{InstallRequest, ModuleChange, Principal};

use super::{created, deleted, detached};
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery};
use crate::services::Services;
use crate::view;

/// `GET /modules`: the installed modules with their commands, routes and
/// tools, in installation order.
pub(crate) async fn list<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<Json<Vec<ModuleView>>, ApiError> {
    let modules = services.modules.list(caller.user()).await?;
    Ok(Json(modules.iter().map(view::module).collect()))
}

/// `GET /modules/available`: the registry's modules this daemon can run.
pub(crate) async fn available<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
) -> Result<Json<Vec<AvailableModuleView>>, ApiError> {
    let available = services.modules.available(caller.user()).await?;
    Ok(Json(available.iter().map(view::available_module).collect()))
}

/// `POST /modules`: download, verify, configure, migrate and load a
/// module without a restart.
pub(crate) async fn install<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    ApiJson(body): ApiJson<ModuleInstall>,
) -> Result<Response, ApiError> {
    let request = InstallRequest {
        name: ModuleName::new(&body.name).map_err(|e| e.with_field("name"))?,
        version: body
            .version
            .as_deref()
            .map(ModuleVersion::new)
            .transpose()
            .map_err(|e| e.with_field("version"))?,
        config: body.config,
    };
    let record =
        detached(async move { services.modules.install(caller.user(), request).await }).await?;
    Ok(created(view::module(&record)))
}

/// `PATCH /modules/{name}`: enable, disable or configure it.
pub(crate) async fn patch<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<ModulePatch>,
) -> Result<Json<ModuleView>, ApiError> {
    let name = ModuleName::new(&name)?;
    let change = ModuleChange {
        enabled: body.enabled,
        config: body.config,
    };
    let record =
        detached(async move { services.modules.update(caller.user(), &name, change).await })
            .await?;
    Ok(Json(view::module(&record)))
}

/// `DELETE /modules/{name}?purge=`: remove it; its tables stay unless
/// `purge`.
pub(crate) async fn delete<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path(name): Path<String>,
    ApiQuery(query): ApiQuery<ModuleDelete>,
) -> Result<Json<Deleted>, ApiError> {
    let name = ModuleName::new(&name)?;
    let removed = name.clone();
    detached(async move {
        services
            .modules
            .remove(caller.user(), &removed, query.purge)
            .await
    })
    .await?;
    Ok(deleted(&name))
}

/// `POST /modules/{name}/tools/{tool}`: call an MCP tool of the module as
/// the caller; the module's JSON answer as `result`.
pub(crate) async fn call_tool<P: Ports>(
    State(services): State<Services<P>>,
    Extension(caller): Extension<Principal>,
    Path((name, tool)): Path<(String, String)>,
    ApiJson(body): ApiJson<ModuleToolCall>,
) -> Result<Json<ModuleToolAnswer>, ApiError> {
    let name = ModuleName::new(&name)?;
    let result = detached(async move {
        services
            .module_routes
            .call_tool(caller.user(), &name, &tool, &body.arguments)
            .await
    })
    .await?;
    Ok(Json(ModuleToolAnswer { result }))
}
