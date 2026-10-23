//! The MCP interface of liftoff.
//!
//! A driving adapter like `liftoff-rest`, but on the client side of the
//! API: every [`liftoff_api::Operation`] is a tool `liftoff_<name>` whose
//! call becomes the REST request with the session from the CLI config;
//! the tools of the installed modules (`GET /modules`) join them as
//! `liftoff_<module>_<tool>`, called through
//! `POST /api/v1/modules/{name}/tools/{tool}`; the logs are the resources
//! `liftoff://projects/{p}/environments/{e}/log` and
//! `liftoff://deploys/{id}/log`. Beside the `liftoff_artifacts_*` tools,
//! the tools of the standalone artifacts server (`publish_artifact`, `update_artifact`,
//! `share_artifact`, `unshare_artifact`, `list_artifacts`,
//! `list_artifact_versions`, `delete_artifact`) are aliases with the
//! predecessor's arguments and answers. It depends on `liftoff-api`, never on
//! `liftoff-core` (the CLI rule).
//!
//! Transports: stdio ([`serve_stdio`], `lof mcp`) and Streamable HTTP
//! ([`http::StreamableHttp`], `/mcp` of the daemon with the modules `mcp` and `oidc`).

mod aliases;
mod api;
pub mod http;
mod server;
mod stdio;
mod tools;

pub use stdio::{serve, serve_stdio};

/// The installation the server talks to: what `lof mcp` reads from the
/// CLI config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Base URL of the daemon, without `/api/v1`.
    pub url: String,
    /// The session token, sent as `Authorization: Bearer`.
    pub token: String,
}

/// Why the server stopped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    /// The MCP connection could not start or broke.
    #[error("mcp transport: {0}")]
    Transport(String),
}
