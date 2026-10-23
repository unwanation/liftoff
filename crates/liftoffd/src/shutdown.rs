//! Cooperative shutdown: one token shared by every listener, cancelled on
//! SIGINT/SIGTERM or when any listener fails.

use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub(crate) struct Shutdown {
    token: CancellationToken,
}

impl Shutdown {
    /// Creates the token and spawns the signal watcher.
    pub(crate) fn new() -> Self {
        let token = CancellationToken::new();
        let _watcher = tokio::spawn(watch_signals(token.clone()));
        Self { token }
    }

    /// A child token for one listener.
    pub(crate) fn token(&self) -> CancellationToken {
        self.token.child_token()
    }

    /// Cancels every listener.
    pub(crate) fn trigger(&self) {
        self.token.cancel();
    }
}

async fn watch_signals(token: CancellationToken) {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "cannot listen for SIGINT");
            core::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                let _received: Option<()> = sig.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "cannot listen for SIGTERM");
                core::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = core::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("SIGINT received"),
        () = terminate => tracing::info!("SIGTERM received"),
        () = token.cancelled() => return,
    }
    token.cancel();
}
