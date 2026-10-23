//! The Caddy admin API adapter of liftoff.
//!
//! [`CaddyAdmin`] implements the core's `Proxy` port: it renders the
//! `ProxyConfig` the core computes from the store into Caddy's JSON
//! ([`render`]) and replaces the running configuration with `POST /load`
//! on the admin API (loopback only); its
//! [`Proxy::ready`](liftoff_core::ports::Proxy::ready) answers the readiness
//! probe with `GET /config/`. There are no files on disk: when
//! Caddy refuses a configuration it keeps the previous one, and the
//! refusal comes back as `Error::Upstream` carrying Caddy's text.

mod caddy;
mod json;

pub use caddy::CaddyAdmin;
pub use json::render;
