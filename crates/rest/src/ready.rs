//! The readiness hook: what `/readyz` asks before it answers `200`.

use core::fmt;
use core::pin::Pin;
use std::sync::Arc;

use liftoff_core::Error;

/// One dependency's answer: its name (`docker`, `sqlite`) and its result.
#[derive(Debug)]
pub struct Probed {
    /// The name `GET /server` shows it under.
    pub name: &'static str,
    /// `Ok` when it answered.
    pub result: Result<(), Error>,
}

/// The dependency checks behind `GET /readyz` and `GET /server` (docker and `SQLite`).
///
/// The daemon implements it over its real adapters; without one the probe
/// answers `200`. Object-safe on purpose: [`AppState`](crate::AppState)
/// stores it without another type parameter.
pub trait Readiness: fmt::Debug + Send + Sync + 'static {
    /// Every dependency, each asked whatever the others answered. An
    /// error is logged, never shown to the caller.
    fn check(&self) -> Pin<Box<dyn Future<Output = Vec<Probed>> + Send + '_>>;
}

/// The installed check, as a router extension.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReadyCheck(pub(crate) Option<Arc<dyn Readiness>>);
