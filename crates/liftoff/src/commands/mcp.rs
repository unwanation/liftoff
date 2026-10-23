//! `lof mcp`: the MCP server over
//! stdio, calling the API of the chosen server with its session.
//!
//! While it runs, stdout carries the JSON-RPC of MCP and nothing else; the
//! command prints nothing of its own there.

use liftoff_mcp::Session;
use tokio_util::sync::CancellationToken;

use crate::config::Chosen;
use crate::error::CliError;

/// Serves MCP on stdin and stdout for `chosen` until the client hangs up
/// or `cancel` (Ctrl-C).
pub(super) async fn run(
    chosen: Option<&Chosen>,
    cancel: CancellationToken,
) -> Result<(), CliError> {
    let chosen = chosen.ok_or(CliError::NotLoggedIn)?;
    let token = chosen.server.token.clone().ok_or(CliError::NotLoggedIn)?;
    let session = Session {
        url: chosen.server.url.clone(),
        token,
    };
    tokio::select! {
        served = liftoff_mcp::serve_stdio(session) => {
            served.map_err(|error| CliError::Local(error.to_string()))
        }
        () = cancel.cancelled() => Ok(()),
    }
}
