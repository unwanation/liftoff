//! Modules.
//!
//! A module's configuration never comes back in an answer: its secret
//! fields are sealed at rest and answered by no route.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::{IntoParams, ToSchema};

/// Body of `POST /modules`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModuleInstall {
    /// The module (`tenants`, `dns.cloudflare`).
    pub name: String,
    /// The release; absent picks the newest one the daemon can run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The configuration, checked against the module's `config_schema`.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: Map<String, Value>,
}

/// Body of `PATCH /modules/{name}`: what to change.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModulePatch {
    /// Enable (after a fix: the error is cleared) or disable it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// A new configuration, checked against the module's `config_schema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub config: Option<Map<String, Value>>,
}

/// Query of `DELETE /modules/{name}`.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ModuleDelete {
    /// Drop its tables and keys too (`lof module remove --purge`).
    #[serde(default)]
    pub purge: bool,
}

/// An installed module (`GET /modules`): what the CLI builds its commands
/// from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ModuleView {
    /// Name.
    pub name: String,
    /// Installed version.
    pub version: String,
    /// Whether it takes calls.
    pub enabled: bool,
    /// Why it is disabled.
    pub error: Option<String>,
    /// RFC 3339.
    pub installed_at: String,
    /// Its CLI commands (`lof <module> <command>`).
    pub commands: Vec<CommandView>,
    /// Its routes below `/api/v1/m/<name>`.
    pub routes: Vec<RouteView>,
    /// Its MCP tools.
    pub tools: Vec<ToolView>,
    /// The JSON Schema of its configuration.
    #[schema(value_type = Option<Object>)]
    pub config_schema: Option<Value>,
    /// The forge it serves, for a `git.*` module: its hosts
    /// and whether it has an OAuth application.
    #[serde(default)]
    pub git_provider: Option<GitProviderView>,
}

/// What a `git.*` module serves (its manifest's `git_provider`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct GitProviderView {
    /// The forge hosts it serves, e.g. `["github.com"]`.
    pub hosts: Vec<String>,
    /// Whether it has an OAuth application: `POST
    /// /git/connections/{host}/device` starts the device flow, else it
    /// answers `400` and `lof git connect` asks for a token.
    pub oauth: bool,
}

/// A CLI command of a module. It runs as `POST
/// /api/v1/m/<module>/<command>` with its arguments as a JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct CommandView {
    /// Name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// Its arguments, in order: each a `--<name>` flag.
    pub args: Vec<ArgView>,
}

/// An argument of a [`CommandView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ArgView {
    /// Name.
    pub name: String,
    /// `string`, `integer` or `boolean`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Whether it must be given (asked on a terminal, exit 64 without).
    pub required: bool,
    /// What the CLI asks with on a terminal.
    pub hint: Option<String>,
}

/// A route of a module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct RouteView {
    /// Method.
    pub method: String,
    /// Path below `/api/v1/m/<module>`.
    pub path: String,
    /// Reachable without a session.
    pub public: bool,
}

/// An MCP tool of a module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ToolView {
    /// Name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// The JSON Schema of its arguments.
    #[schema(value_type = Object)]
    pub schema: Value,
}

/// Body of `POST /modules/{name}/tools/{tool}`.
///
/// A call of an MCP tool of a module, which runs as the caller (the API puts the tools of modules in the MCP server, and `lof mcp` reaches them through this route).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModuleToolCall {
    /// The arguments, checked by the core against the tool's `schema`
    /// before the module sees them (a refusal is a `400`).
    #[serde(default)]
    #[schema(value_type = Object)]
    pub arguments: Map<String, Value>,
}

/// What a tool of a module answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ModuleToolAnswer {
    /// The module's JSON answer, as it gave it (any JSON value).
    pub result: Value,
}

/// A module of the registry (`GET /modules/available`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct AvailableModuleView {
    /// Name.
    pub name: String,
    /// The releases the daemon can run, newest first.
    pub releases: Vec<ReleaseView>,
}

/// One release of an [`AvailableModuleView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ReleaseView {
    /// Version.
    pub version: String,
    /// The WIT world it was built against.
    pub wit_version: String,
    /// The oldest `liftoffd` it runs on.
    pub liftoffd_min: String,
    /// The modules it requires.
    pub requires: Vec<String>,
    /// The JSON Schema of its configuration, which the CLI asks from.
    #[schema(value_type = Option<Object>)]
    pub config_schema: Option<Value>,
}
