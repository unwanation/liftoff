//! The compose project `lf_system`: Caddy with
//! its admin API on loopback.
//!
//! Caddy runs on the host network: the upstreams it proxies to are
//! `127.0.0.1:<host_port>` bindings and the API on `[api] bind`, all on
//! the host's loopback, so nothing is published by compose. What Caddy
//! listens on comes from the configuration liftoff loads: `:80` and `:443`
//! with `tls = "acme"`, only `127.0.0.1:<edge_port>` with `tls = "edge"`.

use core::net::SocketAddr;

/// The compose project name.
pub const PROJECT: &str = "lf_system";

/// The Caddy release `lf_system` runs, pinned: a new one comes with a
/// liftoff release, never with a restart.
pub const CADDY_IMAGE: &str = "caddy:2.10.2";

/// `lf_system/compose.yaml`: one `caddy` service on [`CADDY_IMAGE`].
///
/// Host network, admin API on `admin` (`CADDY_ADMIN`), certificates and
/// the autosaved configuration kept in volumes at `/data` and `/config`.
///
/// Caddy starts with `caddy run --resume`: the last configuration liftoff
/// loaded, or none at all, never the image's default site on `:80`.
#[must_use]
pub fn render(admin: SocketAddr) -> String {
    format!(
        "# The compose project of liftoff's own services, written by `liftoffd setup`.\n\
         # Caddy is on the host network: every upstream is on the host's loopback.\n\
         # What it listens on comes from the configuration liftoffd loads.\n\
         name: {PROJECT}\n\
         services:\n\
         \x20 caddy:\n\
         \x20   image: {CADDY_IMAGE}\n\
         \x20   command: [\"caddy\", \"run\", \"--resume\"]\n\
         \x20   network_mode: host\n\
         \x20   restart: unless-stopped\n\
         \x20   environment:\n\
         \x20     CADDY_ADMIN: \"{admin}\"\n\
         \x20   volumes:\n\
         \x20     - caddy_data:/data\n\
         \x20     - caddy_config:/config\n\
         volumes:\n\
         \x20 caddy_data:\n\
         \x20 caddy_config:\n"
    )
}
