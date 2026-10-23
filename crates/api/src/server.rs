//! The server's own state (`GET /server`): what `lof doctor` prints
//! besides the probes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The base domain, the public address and one row per dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ServerView {
    /// `[domains] base`, e.g. `example.com`.
    pub base_domain: String,
    /// `[domains] public_ip`.
    pub public_ip: String,
    /// `docker`, `sqlite`, `caddy` and `dns`, in this order.
    pub checks: Vec<ServerCheckView>,
}

/// One dependency of the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct ServerCheckView {
    /// `docker`, `sqlite`, `caddy` or `dns` (the wildcard of the base).
    pub name: String,
    /// `ok`, `warn` or `fail`.
    pub status: String,
    /// What a caller may read about it; the cause of a failure goes to
    /// the server's log only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
