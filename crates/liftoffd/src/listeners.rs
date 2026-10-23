//! The daemon's own listeners behind Caddy:
//! [`functions`] on `[functions] bind`, [`tunnel`] on `[tunnel] bind`.

mod common;
pub mod functions;
pub mod tunnel;
