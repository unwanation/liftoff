use core::net::SocketAddr;
use std::io;

use liftoff_core::ports::Ports;
use liftoff_core::service::GateListener;
use tokio::net::TcpListener;

use crate::config::RestConfig;
use crate::router::app_router;
use crate::routes::gate::gate_router;
use crate::services::Services;
use crate::state::AppState;

/// Failure to start or run the REST listener.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// Binding the configured address failed.
    #[error("failed to bind REST listener on {addr}")]
    Bind {
        /// The address that could not be bound.
        addr: SocketAddr,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The accept loop failed.
    #[error("REST server error")]
    Io(#[from] io::Error),
}

/// Binds `config.bind` and serves the whole API over `state` and
/// `services` until `shutdown` resolves.
///
/// # Errors
///
/// Returns [`ServeError::Bind`] if the address is unavailable, or
/// [`ServeError::Io`] if the accept loop fails.
pub async fn serve_app<P: Ports>(
    state: AppState,
    services: Services<P>,
    config: RestConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let listener = TcpListener::bind(config.bind)
        .await
        .map_err(|source| ServeError::Bind {
            addr: config.bind,
            source,
        })?;
    serve_app_on(listener, state, services, config, shutdown).await
}

/// Serves the whole API on an already-bound listener. Useful for tests
/// (`127.0.0.1:0`) and socket activation. Every request carries its peer
/// address, which `GET /internal/tls-ask` checks.
///
/// # Errors
///
/// Returns [`ServeError::Io`] if the accept loop fails.
pub async fn serve_app_on<P: Ports>(
    listener: TcpListener,
    state: AppState,
    services: Services<P>,
    config: RestConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    serve_router_on(listener, app_router(state, services, &config), shutdown).await
}

/// Serves `app` (the [`app_router`], with what the daemon mounts beside it: `/mcp`) on an already-bound listener as
/// [`serve_app_on`] does.
///
/// # Errors
///
/// Returns [`ServeError::Io`] if the accept loop fails.
pub async fn serve_router_on(
    listener: TcpListener,
    app: axum::Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "REST listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    tracing::info!(%addr, "REST stopped");
    Ok(())
}

/// Serves the gate listener (`[gate] bind`) on a bound listener until
/// `shutdown` resolves.
///
/// Caddy's `forward_auth` asks `GET /gate/check`
/// there, and the protected hosts' `/.liftoff/*` paths go there. Every
/// request carries its peer address: the gate answers loopback peers
/// alone.
///
/// # Errors
///
/// Returns [`ServeError::Io`] if the accept loop fails.
pub async fn serve_gate_on<P: Ports>(
    listener: TcpListener,
    gate: GateListener<P>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "gate listening");
    axum::serve(
        listener,
        gate_router(gate).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    tracing::info!(%addr, "gate stopped");
    Ok(())
}
