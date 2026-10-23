//! `GET /.well-known/oauth-protected-resource`: the protected-resource metadata (RFC 9728) of `/mcp`, which
//! an MCP client reads from the `resource_metadata` of a `401` to find the
//! authorization server and the scopes to ask it for. Open, at the root of the API host; `424` without
//! the module `mcp`. `/mcp` itself is the transport of `liftoff-mcp`,
//! which the daemon mounts beside this router.

use axum::Json;
use axum::extract::State;
use liftoff_core::ports::Ports;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::services::Services;

/// `GET /.well-known/oauth-protected-resource`.
pub(crate) async fn protected_resource<P: Ports>(
    State(services): State<Services<P>>,
) -> Result<Json<Value>, ApiError> {
    let metadata = services.mcp_auth.resource_metadata().await?;
    Ok(Json(json!({
        "resource": metadata.resource,
        "authorization_servers": metadata.authorization_servers,
        "scopes_supported": metadata.scopes_supported,
    })))
}
