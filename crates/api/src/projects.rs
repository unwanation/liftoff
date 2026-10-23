//! Projects.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Body of `POST /projects`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectCreate {
    /// `^[a-z0-9][a-z0-9-]{0,31}$`.
    pub name: String,
    /// The forge host of the repository.
    pub git_host: String,
    /// `owner/name` of the repository.
    pub repo: String,
    /// The last `liftoff.toml` the CLI read, as text.
    #[serde(default)]
    pub manifest: Option<String>,
    /// The slug of the tenant the project goes to (the `tenants` module). Without it the module puts it in the caller's
    /// only tenant; without the module naming one is `424`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The template the project is made from (`lof create`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<ProjectTemplate>,
}

/// The template a project is made from: its `[template] group` and
/// `name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectTemplate {
    /// `web`, `api`, `function` or `service`.
    pub group: String,
    /// `[template] name`.
    pub name: String,
}

/// Body of `PATCH /projects/{p}`: the fields to change.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectPatch {
    /// The last `liftoff.toml` the CLI read, as text.
    #[serde(default)]
    pub manifest: Option<String>,
}

/// Body of `POST /projects/{p}/resolve`: the CLI's `git remote`. `{p}` is
/// `-` when the CLI does not know the name yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectResolve {
    /// The forge host, e.g. `git.example.com`.
    pub git_host: String,
    /// `owner/name`.
    pub repo: String,
}

/// A project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ProjectView {
    /// ULID.
    pub id: String,
    /// Name.
    pub name: String,
    /// The forge host.
    pub git_host: String,
    /// `owner/name`.
    pub repo: String,
    /// The last `liftoff.toml` read, as text.
    pub manifest: Option<String>,
    /// ULID of the creator.
    pub created_by: String,
    /// RFC 3339.
    pub created_at: String,
}
