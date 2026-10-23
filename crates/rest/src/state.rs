//! The state every handler shares.

use std::sync::Arc;

use crate::ready::{Readiness, ReadyCheck};
use crate::routes::tunnel::{TunnelHandoff, TunnelSlot};

/// What the router serves from besides the [`Services`](crate::Services).
///
/// The daemon's readiness check of docker and `SQLite`, and where `GET
/// /api/v1/tunnel` hands its upgraded connections.
///
/// Cheap to clone: every field is an [`Arc`] inside.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub(crate) ready: ReadyCheck,
    pub(crate) tunnels: TunnelSlot,
}

impl AppState {
    /// A state without a readiness check (`/readyz` always answers `200`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs the check `GET /readyz` runs (default: none, always ready).
    #[must_use]
    pub fn with_readiness(mut self, check: impl Readiness) -> Self {
        self.ready = ReadyCheck(Some(Arc::new(check)));
        self
    }

    /// Installs what `GET /api/v1/tunnel` hands its upgraded connections
    /// to (default: none, the route answers `503`).
    #[must_use]
    pub fn with_tunnels(mut self, tunnels: impl TunnelHandoff) -> Self {
        self.tunnels = TunnelSlot(Some(Arc::new(tunnels)));
        self
    }
}
