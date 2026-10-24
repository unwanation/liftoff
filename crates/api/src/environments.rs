//! Environments and their bindings.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Body of `POST /projects/{p}/environments`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentCreate {
    /// Name, unique within the project.
    pub name: String,
    /// The branch CI deploys from; pinned by the caller.
    pub branch: String,
    /// `compose` (the default) or `function`; `tunnel` is refused
    /// (`400` on `kind`): `lof expose` creates tunnels through `GET /tunnel`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// Body of `PATCH /projects/{p}/environments/{e}`: the fields to change;
/// an absent field (never sent as `null`) keeps its value.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentPatch {
    /// A new branch, pinned by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// CPU allowance (`lof admin env set --cpus`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<f64>,
    /// Memory allowance in MiB (`lof admin env set --memory`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
}

/// Query of `DELETE /projects/{p}/environments/{e}`.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDelete {
    /// Must be `true` (`lof env rm --yes`).
    #[serde(default)]
    pub confirm: bool,
}

/// An environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct EnvironmentView {
    /// ULID.
    pub id: String,
    /// Name of the project.
    pub project: String,
    /// Name.
    pub name: String,
    /// `compose`, `function` or `tunnel`.
    pub kind: String,
    /// The pinned branch.
    pub branch: Option<String>,
    /// ULID of who pinned it.
    pub pinned_by: Option<String>,
    /// ULID of the user whose tunnel it is (`tunnel` only).
    #[serde(default)]
    pub owner: Option<String>,
    /// `empty`, `running`, `failed`, `stopped` or `offline`.
    pub state: String,
    /// CPU allowance.
    pub cpus: f64,
    /// Memory allowance in MiB.
    pub memory_mb: u32,
    /// ULID of the deploy that runs.
    pub current_deploy_id: Option<String>,
    /// `private` or `shared`.
    pub visibility: String,
    /// RFC 3339.
    pub created_at: String,
}

/// Query of `GET /projects/{p}/environments/{e}/logs`.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LogsQuery {
    /// One compose service; every service when absent.
    #[serde(default)]
    pub service: Option<String>,
    /// The last lines only.
    #[serde(default)]
    pub tail: Option<u32>,
    /// Keep streaming (the default).
    #[serde(default)]
    pub follow: Option<bool>,
}

/// Body of `POST /projects/{p}/environments/{e}/bindings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BindingCreate {
    /// A domain of the project.
    pub host: String,
    /// The compose service that listens; absent (with `port`) for a
    /// function environment, which has no services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The port it listens on inside its container; given with `service`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// A binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct BindingView {
    /// The domain.
    pub host: String,
    /// Name of the project.
    pub project: String,
    /// Name of the environment.
    pub environment: String,
    /// The compose service.
    pub service: String,
    /// Its port inside the container.
    pub container_port: u16,
    /// The loopback port Caddy proxies to.
    pub host_port: u16,
}
