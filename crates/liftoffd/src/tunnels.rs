//! [`DaemonTunnels`]: what `GET /api/v1/tunnel` hands its upgraded
//! connections to.
//!
//! It runs each connection on the daemon's [`TunnelServer`] as the
//! tunnel of its environment and tells the core's [`TunnelsService`]
//! when the client's handshake ended (`running`) and how the connection
//! ended (`Shutdown`: `offline` at once; lost: `offline` after
//! `tunnel.offline_after`; evicted: nothing), naming each connection by
//! the number the server registered it under, so the end of a replaced
//! connection changes nothing. [`DaemonTunnels::activity`] counts what it
//! did, for whoever waits on it (the daemon's tests).

use core::fmt;
use core::pin::Pin;
use core::time::Duration;

use liftoff_core::domain::Environment;
use liftoff_core::error::Error;
use liftoff_core::ports::Ports;
use liftoff_core::service::{TunnelEnd, TunnelsService};
use liftoff_rest::{TunnelHandoff, UpgradedIo};
use liftoff_tunnel::{Disconnect, TunnelServer};
use tokio::sync::{oneshot, watch};

/// How much later than `tunnel.offline_after` a lost tunnel is checked:
/// the core's clock and tokio's timer need not agree to the millisecond.
const EXPIRY_SLACK: Duration = Duration::from_millis(250);

/// What [`DaemonTunnels`] did so far: each count goes up once the core
/// was told (whether or not it could record it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TunnelActivity {
    /// Connections whose handshake ended and that were reported
    /// connected.
    pub connected: u64,
    /// Connections whose end was reported.
    pub ended: u64,
    /// Checks for lost tunnels that ran `tunnel.offline_after` after a
    /// loss.
    pub expiries: u64,
}

/// See the [module documentation](self).
pub struct DaemonTunnels<P> {
    server: TunnelServer,
    tunnels: TunnelsService<P>,
    activity: watch::Sender<TunnelActivity>,
}

impl<P> fmt::Debug for DaemonTunnels<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DaemonTunnels")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl<P> Clone for DaemonTunnels<P> {
    fn clone(&self) -> Self {
        Self {
            server: self.server.clone(),
            tunnels: self.tunnels.clone(),
            activity: self.activity.clone(),
        }
    }
}

impl<P: Ports> DaemonTunnels<P> {
    /// Connections run on `server`; their lifecycle goes to `tunnels`,
    /// and a lost one is taken offline after its
    /// [`offline_after`](TunnelsService::offline_after) unless its client
    /// came back.
    #[must_use]
    pub fn new(server: TunnelServer, tunnels: TunnelsService<P>) -> Self {
        Self {
            server,
            tunnels,
            activity: watch::Sender::new(TunnelActivity::default()),
        }
    }

    /// The service connections are reported to; the REST services take
    /// a clone of it ([`Services::with_tunnels`](liftoff_rest::Services::with_tunnels)).
    #[must_use]
    pub const fn service(&self) -> &TunnelsService<P> {
        &self.tunnels
    }

    /// What this and its clones did so far, updated as it happens.
    #[must_use]
    pub fn activity(&self) -> watch::Receiver<TunnelActivity> {
        self.activity.subscribe()
    }

    fn count(&self, step: impl FnOnce(&mut TunnelActivity)) {
        self.activity.send_modify(step);
    }
}

impl<P: Ports + 'static> TunnelHandoff for DaemonTunnels<P> {
    fn serve(
        &self,
        environment: Environment,
        io: UpgradedIo,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let this = self.clone();
        Box::pin(async move { this.run(environment, io).await })
    }
}

impl<P: Ports + 'static> DaemonTunnels<P> {
    /// Runs one connection of `environment` to its end (see the [module
    /// documentation](self)).
    async fn run(self, environment: Environment, io: UpgradedIo) {
        let id = environment.id;
        let key = id.to_string();
        let (ready, handshaken) = oneshot::channel::<u64>();
        let served = self
            .server
            .serve_as(&key, environment.name.as_str(), io, move |connection| {
                let _gone = ready.send(connection);
            });
        let marked = async {
            let connection = handshaken.await.ok()?;
            if let Err(error) = self.tunnels.connected(id, connection).await {
                log_failure(
                    &error,
                    &environment,
                    "a connected tunnel could not be marked running",
                );
            }
            self.count(|activity| activity.connected = activity.connected.saturating_add(1));
            Some(connection)
        };
        let (ended, connection) = tokio::join!(served, marked);
        let end = match ended {
            Ok(Disconnect::Shutdown) => TunnelEnd::Shutdown,
            Ok(Disconnect::Evicted) => TunnelEnd::Evicted,
            Ok(Disconnect::Lost) => TunnelEnd::Lost,
            Err(error) => {
                // The handshake failed: the connection never was the
                // tunnel, the environment's state is untouched.
                tracing::info!(%error, environment = %environment.name, "a tunnel handshake failed");
                return;
            }
        };
        // A finished handshake registered the connection and sent its
        // number before the end.
        let Some(connection) = connection else {
            return;
        };
        if let Err(error) = self.tunnels.disconnected(id, connection, end).await {
            log_failure(&error, &environment, "a tunnel's end could not be recorded");
        }
        if end == TunnelEnd::Lost {
            drop(tokio::spawn(self.clone().expire_after_offline()));
        }
        self.count(|activity| activity.ended = activity.ended.saturating_add(1));
    }

    /// Waits `tunnel.offline_after` (and a moment, so the clock of the
    /// core agrees) and takes the tunnels whose client did not come back
    /// offline.
    async fn expire_after_offline(self) {
        let after = self.tunnels.offline_after();
        tokio::time::sleep(after.saturating_add(EXPIRY_SLACK)).await;
        match self.tunnels.expire().await {
            Ok(expired) if !expired.is_empty() => {
                tracing::info!(
                    count = expired.len(),
                    "tunnels without their client went offline"
                );
            }
            Ok(_none) => {}
            Err(error) => tracing::error!(%error, "taking lost tunnels offline failed"),
        }
        self.count(|activity| activity.expiries = activity.expiries.saturating_add(1));
    }
}

/// Logs that reporting a connection of `environment` failed: at `info`
/// when the environment was removed meanwhile (`lof env rm` of a
/// connected tunnel), else at `error`.
fn log_failure(error: &Error, environment: &Environment, what: &str) {
    if matches!(error, Error::NotFound(_)) {
        tracing::info!(%error, environment = %environment.name, "{what}: the tunnel is gone");
    } else {
        tracing::error!(%error, environment = %environment.name, "{what}");
    }
}
