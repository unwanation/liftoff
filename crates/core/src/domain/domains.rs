//! Domains.

use super::ids::{DomainId, ProjectId, TenantId};
use super::refs::Host;
use super::spelled::spelled_enum;
use super::timestamp::Timestamp;

/// Who answers for a domain's DNS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DomainKind {
    /// `<label>.<base>` under the base wildcard.
    Managed,
    /// Somebody else's name, verified by CNAME to the base or A to the
    /// public IP.
    External,
}

spelled_enum!(DomainKind { Managed => "managed", External => "external" });

/// A host name routed by liftoff; `host` is globally unique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    /// Identifier.
    pub id: DomainId,
    /// Owning tenant; `None` without the `tenants` module.
    pub tenant_id: Option<TenantId>,
    /// The project it was added to (BND-01: a domain binds only to an
    /// environment of its own project).
    pub project_id: ProjectId,
    /// The FQDN.
    pub host: Host,
    /// Managed or external.
    pub kind: DomainKind,
    /// When the DNS record was last verified (external domains).
    pub verified_at: Option<Timestamp>,
    /// Since when the daily re-verification finds the record of an
    /// external domain gone; `None` while it resolves. A warning in the
    /// domain's status only: the route stays (DOM-07).
    pub record_lost_at: Option<Timestamp>,
    /// Whether a DNS module proxies the record.
    pub proxied: bool,
    /// Creation time.
    pub created_at: Timestamp,
}
