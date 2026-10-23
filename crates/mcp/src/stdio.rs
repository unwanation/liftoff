//! The stdio transport: newline-delimited JSON-RPC over a reader and a
//! writer (`lof mcp` passes its stdin and stdout).

use rmcp::ServiceExt as _;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::api::Api;
use crate::server::Server;
use crate::{McpError, Session};

/// Serves MCP over `input` and `output` until the client hangs up,
/// calling the API of `session`.
///
/// # Errors
///
/// [`McpError::Transport`] when the connection cannot start or breaks.
pub async fn serve<R, W>(session: Session, input: R, output: W) -> Result<(), McpError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let api = Api::new(session).map_err(McpError::Transport)?;
    let running = Server::new(api)
        .serve((input, output))
        .await
        .map_err(|error| McpError::Transport(error.to_string()))?;
    running
        .waiting()
        .await
        .map_err(|error| McpError::Transport(error.to_string()))?;
    Ok(())
}

/// [`serve`] over this process's stdin and stdout.
///
/// # Errors
///
/// As [`serve`].
pub async fn serve_stdio(session: Session) -> Result<(), McpError> {
    serve(session, tokio::io::stdin(), tokio::io::stdout()).await
}
