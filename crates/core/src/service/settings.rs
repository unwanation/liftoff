//! [`Settings`]: the configuration values the services read.

use core::net::{Ipv4Addr, SocketAddr};
use core::ops::RangeInclusive;
use core::time::Duration;

use crate::domain::{AcmeEmail, Host, HostPort, Label, TemplateSource};
use crate::module::RegistryKey;

/// Configuration the services read; `liftoffd` fills it from
/// `liftoffd.toml`. Plain data: there are no compiled-in defaults for the
/// installation-specific values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Base wildcard domain of managed domains (`[domains] base`), e.g.
    /// `apps.example.com`.
    pub base: Host,
    /// The server's public address (`[domains] public_ip`): external
    /// domains may point an A record at it instead of a CNAME to `base`.
    pub public_ip: Ipv4Addr,
    /// Labels nobody may take as a managed domain (`[domains] reserved`).
    pub reserved: Vec<Label>,
    /// Host ports bindings are allocated from (`[domains] port_range`),
    /// passed to [`Store::free_host_port`](crate::ports::Store::free_host_port).
    pub port_range: RangeInclusive<HostPort>,
    /// `docker compose up --wait-timeout` (`[deploy] wait_timeout`).
    pub wait_timeout: Duration,
    /// How many deploys of an environment are kept (`[deploy] history`); the current one is never pruned.
    pub deploy_history: usize,
    /// How long a tunnel whose connection was lost waits for its client
    /// before it is `offline` (`[tunnel] offline_after`).
    pub tunnel_offline_after: Duration,
    /// What the proxy configuration needs beyond the store (`[api]`, `[proxy]`).
    pub proxy: ProxySettings,
    /// What installing modules checks releases against.
    pub modules: ModuleSettings,
    /// The template repositories of the catalogue (`[templates] repos`), in configuration order.
    pub templates: Vec<TemplateSource>,
    /// What artifacts are held to (`[artifacts]`).
    pub artifacts: ArtifactSettings,
}

/// `[artifacts]` as [`ArtifactsService`](crate::service::ArtifactsService)
/// reads it; its address base is [`ProxySettings::api_host`] unless
/// `host` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSettings {
    /// How long a private artifact lives after its latest version or its
    /// `unshare` (`ttl`, 5 hours by default).
    pub ttl: Duration,
    /// The largest sum of a version's bytes, `index.html` included
    /// (`max_size_mb` in MiB, 16 by default).
    pub max_size_bytes: u64,
    /// How many versions of an artifact are kept (`versions`, 50 by
    /// default); the current one is never dropped.
    pub versions: usize,
    /// A host of their own (`host`): `https://<host>/<id>` instead of
    /// `https://<api host>/a/<id>`.
    pub host: Option<Host>,
}

impl Default for ArtifactSettings {
    /// The defaults: `ttl = "5h"`, `max_size_mb = 16`,
    /// `versions = 50`, no host of their own.
    fn default() -> Self {
        Self {
            ttl: Duration::from_hours(5),
            max_size_bytes: 16 * 1024 * 1024,
            versions: 50,
            host: None,
        }
    }
}

/// What [`ModuleService`](crate::service::ModuleService) checks a release
/// of the registry against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSettings {
    /// The key the registry's releases must be signed with: the one
    /// pinned in the `liftoffd` binary (a test key in tests). Unpinned,
    /// every install is refused (REG-01).
    pub registry_key: RegistryKey,
    /// This daemon's version (semver), which a release's `liftoffd_min`
    /// must not exceed.
    pub liftoffd_version: String,
}

/// The settings [`ProxyConfig::from`](crate::proxy_config::ProxyConfig::from)
/// reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySettings {
    /// The host of `[api] public_url` (`liftoff.example.com`): the site of
    /// the API and the daemon's pages.
    pub api_host: Host,
    /// `[api] bind`, where that site's requests go (`127.0.0.1:7000`).
    pub api_bind: SocketAddr,
    /// `[proxy] tls`.
    pub tls: TlsMode,
    /// `[gate] bind`, the gate listener the `forward_auth` of a protected
    /// environment's domains asks (`127.0.0.1:7001`).
    pub gate_bind: SocketAddr,
    /// `[functions] bind`, the function listener the domains of a running
    /// function environment go to (`127.0.0.1:7003`).
    pub functions_bind: SocketAddr,
    /// `[tunnel] bind`, the tunnel listener the domains of a running
    /// tunnel environment go to (`127.0.0.1:7004`).
    pub tunnel_bind: SocketAddr,
}

/// How Caddy terminates TLS (`[proxy] tls`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsMode {
    /// `tls = "acme"`: Caddy listens on 80 and 443 and gets certificates
    /// itself, on demand (with `ask`) for external domains.
    Acme {
        /// `[proxy] acme_email`, the ACME account.
        email: AcmeEmail,
    },
    /// `tls = "edge"`: Caddy listens on `127.0.0.1:<edge_port>` without
    /// TLS behind the host's own proxy.
    Edge {
        /// `[proxy] edge_port`.
        port: u16,
    },
}
