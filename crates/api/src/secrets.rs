//! Secrets.
//!
//! A scope travels as `scope` (`tenant`, `project`, `environment`) and
//! `id`: the project name, or `<project>/<environment>`. `tenant` is the
//! tenant the project `id` is in; a project outside every tenant (no
//! `tenants` module) answers `424`. Secret routes name a secret by its key
//! (`/secrets/{key}?scope=&id=`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query of every route that names a scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeQuery {
    /// `tenant`, `project` or `environment`.
    pub scope: String,
    /// `<project>` (also for `tenant`) or `<project>/<environment>`.
    pub id: String,
}

/// Body of `PUT /secrets`: writes the next version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretPut {
    /// `tenant`, `project` or `environment`.
    pub scope: String,
    /// `<project>` (also for `tenant`) or `<project>/<environment>`.
    pub id: String,
    /// `^[A-Z][A-Z0-9_]{0,63}$`.
    pub key: String,
    /// The value.
    pub value: String,
    /// Whether `POST /secrets/sync` sends it to the forge's CI.
    #[serde(default)]
    pub ci: bool,
}

/// A secret without its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SecretView {
    /// The env variable.
    pub key: String,
    /// `tenant`, `project` or `environment`.
    pub scope: String,
    /// Current version.
    pub version: u32,
    /// Whether it goes to CI.
    pub ci: bool,
    /// ULID of the user, or `ci`.
    pub updated_by: String,
    /// RFC 3339.
    pub updated_at: String,
}

/// Answer of `GET /secrets/{key}`: the current value, for the scope's
/// owner only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SecretValueView {
    /// The env variable.
    pub key: String,
    /// The value.
    pub value: String,
    /// Its version.
    pub version: u32,
}

/// One version, without the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SecretRevisionView {
    /// Its number.
    pub version: u32,
    /// ULID of the user, or `ci`.
    pub updated_by: String,
    /// RFC 3339.
    pub updated_at: String,
}

/// Body of `POST /secrets/{key}/rollback`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretRollback {
    /// The version whose value becomes a new current version.
    pub to: u32,
}

/// Body of `POST /secrets/sync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretsSync {
    /// The project whose `ci = true` secrets go to its repository.
    pub project: String,
}

/// Answer of `POST /secrets/sync`: every
/// `ci = true` secret of the project in one of the three lists, by key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SecretsSynced {
    /// The keys that were sent now.
    pub synced: Vec<String>,
    /// The keys not sent: the forge already holds their current version.
    pub unchanged: Vec<String>,
    /// The keys the forge refused or failed on; the other keys were still
    /// sent.
    pub failed: Vec<SecretSyncFailure>,
}

/// One key `POST /secrets/sync` could not send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct SecretSyncFailure {
    /// The key.
    pub key: String,
    /// Why, as recorded with the secret.
    pub error: String,
}

/// Query of `GET /secrets/resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveQuery {
    /// Name of the project.
    pub project: String,
    /// Name of the environment.
    pub env: String,
}

/// One variable of a resolved env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct EnvVar {
    /// The variable.
    pub key: String,
    /// Its value.
    pub value: String,
}

/// Answer of `GET /secrets/resolve`: the env the environment runs with,
/// in the documented order (what `lof secret pull` prints).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ResolvedEnv {
    /// The variables.
    pub vars: Vec<EnvVar>,
}
