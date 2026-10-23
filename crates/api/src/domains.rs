//! Domains.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Body of `POST /domains`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainCreate {
    /// The project the domain belongs to.
    pub project: String,
    /// A label under the base domain, or a full host with `external`.
    pub name: String,
    /// `name` is a host outside the base domain, checked by CNAME or A.
    #[serde(default)]
    pub external: bool,
    /// The DNS module proxies the managed domain's record from the start
    /// (needs `dns.cloudflare`: `424`; not with `external`).
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub proxied: bool,
}

/// Body of `PATCH /domains/{host}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainPatch {
    /// Whether the DNS module proxies it (needs `dns.cloudflare`: `424`).
    pub proxied: bool,
}

/// A domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
pub struct DomainView {
    /// FQDN.
    pub host: String,
    /// `managed` or `external`.
    pub kind: String,
    /// Name of the project.
    pub project: String,
    /// RFC 3339; external domains only.
    pub verified_at: Option<String>,
    /// RFC 3339: the daily check lost the record (a warning).
    pub record_lost_at: Option<String>,
    /// Whether a DNS module proxies it.
    pub proxied: bool,
}

/// Query of `GET /internal/tls-ask` (Caddy's on-demand TLS permission).
/// Caddy sends `domain` alone; anything else is refused like on every
/// other input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, IntoParams, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TlsAsk {
    /// The host Caddy wants a certificate for.
    pub domain: String,
}
