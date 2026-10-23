//! Deploys and deploy tokens.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Fields of `POST /deploys`, as JSON or as `multipart/form-data` fields.
///
/// A deploy token sends `repo`, `ref`, `tag` and `compose`; `env` is
/// deprecated (answered with a `Deprecation` header, stored as environment
/// secrets). A session sends `project`, `environment`, `tag` and
/// optionally `compose`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeployBody {
    /// `owner/name` of the repository (must be the token's project's).
    #[serde(default)]
    pub repo: Option<String>,
    /// Branch that was pushed.
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    /// Image tag to deploy.
    #[serde(default)]
    pub tag: Option<String>,
    /// The compose file.
    #[serde(default)]
    pub compose: Option<String>,
    /// Deprecated: `.env` lines.
    #[serde(default)]
    pub env: Option<String>,
    /// Project of a manual deploy (sessions only).
    #[serde(default)]
    pub project: Option<String>,
    /// Environment of a manual deploy (sessions only).
    #[serde(default)]
    pub environment: Option<String>,
}

/// A deploy that was created and queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct QueuedView {
    /// Name of the project.
    pub project: String,
    /// Name of the environment.
    pub environment: String,
    /// ULID of the deploy.
    pub deploy_id: String,
    /// Number within the environment.
    pub number: u32,
}

/// Answer of `POST /deploys` and `.../redeploy`: `202` with the deploys,
/// `200` with none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct DeploysQueued {
    /// The deploys that were queued.
    pub deploys: Vec<QueuedView>,
}

/// Body of `POST .../redeploy`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RedeployBody {
    /// Number of the deploy to copy; the current one when absent.
    #[serde(default)]
    pub to: Option<u32>,
}

/// A deploy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct DeployView {
    /// ULID.
    pub id: String,
    /// Number within the environment.
    pub number: u32,
    /// Name of the project.
    pub project: String,
    /// Name of the environment.
    pub environment: String,
    /// `ci`, `cli` or `redeploy`.
    pub source: String,
    /// Branch the image came from.
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// Image tag.
    pub tag: String,
    /// `queued`, `running`, `succeeded` or `failed`.
    pub status: String,
    /// Short reason of a failure.
    pub error: Option<String>,
    /// For rollbacks: the number of the deploy that was redeployed.
    pub rolled_back_to: Option<u32>,
    /// ULID of the user or deploy token that triggered it.
    pub triggered_by: String,
    /// RFC 3339.
    pub started_at: Option<String>,
    /// RFC 3339.
    pub finished_at: Option<String>,
}

/// Answer of `POST /projects/{p}/token/rotate`. The new value is never
/// returned: it lives in the project secret `LIFTOFF_TOKEN` (`ci = true`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct TokenView {
    /// Name of the project.
    pub project: String,
    /// ULID of the live token.
    pub id: String,
    /// RFC 3339.
    pub created_at: String,
}

/// Answer of `GET /projects/{p}/snippet`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SnippetView {
    /// The file of the repository the workflow belongs at, relative to
    /// its root: `.forgejo/workflows/deploy.yml`,
    /// `.github/workflows/deploy.yml` or `.gitlab-ci.yml` by the project's
    /// forge.
    pub path: String,
    /// The CI workflow that deploys the project with `LIFTOFF_TOKEN`.
    pub snippet: String,
}
