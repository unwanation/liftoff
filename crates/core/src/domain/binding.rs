//! Bindings.

use super::ids::{BindingId, DomainId, EnvironmentId};
use super::names::ServiceName;
use super::refs::{ContainerPort, HostPort};

/// Routes a domain to a service port of a compose environment.
/// `domain_id` and `host_port` are unique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Identifier.
    pub id: BindingId,
    /// The environment.
    pub environment_id: EnvironmentId,
    /// The domain (one binding per domain).
    pub domain_id: DomainId,
    /// Compose service receiving the traffic.
    pub service: ServiceName,
    /// Port inside the container.
    pub container_port: ContainerPort,
    /// Loopback port published on the host.
    pub host_port: HostPort,
}
