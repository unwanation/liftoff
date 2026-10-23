//! REST interface adapter for liftoff.
//!
//! An axum router with production middleware: request IDs, structured
//! tracing, timeouts, body limits, panic containment and hardening headers.
//! Errors are rendered as RFC 9457 `application/problem+json`.
//!
//! [`app_router`] serves the routes for the entities
//! under `/api/v1` over [`Services`] (login and sessions, forge
//! connections, projects, environments, bindings, domains, deploys and the
//! deploy token, secrets, administration), `/openapi.json`, the probes and
//! the loopback-only `GET /internal/tls-ask`. [`ApiError`] is the single
//! place where domain errors become HTTP.
//!
//! Tests drive the router in memory; only the WebSocket upgrade of `GET
//! /api/v1/tunnel` needs a real connection, on an ephemeral loopback port.
//!
//! # Endpoints
//!
//! See `GET /openapi.json` for the complete list with schemas.

mod config;
mod cors;
mod error;
mod extract;
mod middleware;
mod openapi;
pub mod pages;
mod probes;
mod ready;
mod router;
mod routes;
mod serve;
mod services;
mod state;
mod view;

pub use config::RestConfig;
pub use error::{ApiError, PROBLEM_JSON};
pub use ready::{Probed, Readiness};
pub use router::app_router;
pub use routes::gate::gate_router;
pub use routes::tunnel::{TunnelHandoff, UpgradedIo, UpgradedStream};
pub use serve::{ServeError, serve_app, serve_app_on, serve_gate_on, serve_router_on};
pub use services::Services;
pub use state::AppState;
