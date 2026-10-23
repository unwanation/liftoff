//! Domain core of liftoff.
//!
//! The inside of the hexagon: domain types, use-case services and the
//! *ports* (traits) through which the services reach the outside world. No
//! framework, no runtime, no I/O beyond the clock and the random source
//! ([`system`], [`crypto`]).
//!
//! # Layout
//!
//! - [`domain`] – validated value types and the records.
//! - [`error`]   – the error vocabulary adapters map to their protocol.
//! - [`ports`]   – traits implemented by the adapters (driven ports).
//! - [`crypto`] – XChaCha20-Poly1305 under the master key.
//! - [`system`]  – the wall clock and the operating system's random
//!   source, the real [`Clock`](ports::Clock) and [`Rng`](ports::Rng): with `crypto` the only reads of the operating system.
//! - [`compose`] – compose validation and rendering.
//! - [`proxy_config`] – the whole proxy configuration.
//! - [`service`] – the use-case services (onwards).
//! - [`manifest`] – `liftoff.toml`, the project manifest.
//! - [`detect`]  – stack detection: the manifest proposed for a
//!   repository without one.
//! - [`dev`]     – `lof dev` run locally: the free port and the `--port`
//!   flag.
//! - [`snippet`] – the CI workflow that deploys a project, per forge.
//! - [`module`] – module manifests and the `core-call` bridge.
//! - `fakes`     – hand-written fakes of every port (feature
//!   `test-support`).
//!
//! The v1 tenants, targets, SSO gate and their services are gone: v2
//! rebuilds the services on the project and environment domain
//! (onwards).

pub mod compose;
pub mod crypto;
pub mod detect;
pub mod dev;
pub mod domain;
pub mod error;
#[cfg(any(test, feature = "test-support"))]
pub mod fakes;
pub mod manifest;
pub mod module;
pub mod ports;
pub mod proxy_config;
pub mod service;
pub mod snippet;
pub mod system;

pub use error::{Error, Problem, System, ValidationError};
