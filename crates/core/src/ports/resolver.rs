use std::net::Ipv4Addr;

use crate::domain::Host;
use crate::error::Error;

/// Public DNS, for verifying external domains.
///
/// A name with no record of the asked type is `Ok(vec![])`, not an error.
///
/// # Errors
///
/// [`Error::Upstream`] when the resolver itself fails.
pub trait Resolver: Send + Sync {
    /// The CNAME targets of `host`.
    fn cname(&self, host: &Host) -> impl Future<Output = Result<Vec<Host>, Error>> + Send;
    /// The A records of `host`.
    fn a(&self, host: &Host) -> impl Future<Output = Result<Vec<Ipv4Addr>, Error>> + Send;
}
