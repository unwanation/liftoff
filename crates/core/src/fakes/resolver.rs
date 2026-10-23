//! [`FakeResolver`].

use core::future::ready;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use super::log::{Call, CallLog, Faults, ResolveCall, lock, scriptable};
use crate::domain::Host;
use crate::error::Error;
use crate::ports::Resolver;

#[derive(Debug, Default)]
struct Zone {
    cname: BTreeMap<Host, Vec<Host>>,
    a: BTreeMap<Host, Vec<Ipv4Addr>>,
    /// `*.<zone>`: the A records of any name directly under the zone
    /// that has none of its own.
    wildcard: BTreeMap<Host, Vec<Ipv4Addr>>,
}

impl Zone {
    fn a(&self, host: &Host) -> Vec<Ipv4Addr> {
        if let Some(addresses) = self.a.get(host) {
            return addresses.clone();
        }
        self.wildcard
            .iter()
            .find(|(zone, _)| host.label_in(zone).is_some())
            .map(|(_, addresses)| addresses.clone())
            .unwrap_or_default()
    }
}

/// DNS from a table: names without records answer `Ok(vec![])`.
#[derive(Debug, Clone, Default)]
pub struct FakeResolver {
    zone: Arc<Mutex<Zone>>,
    log: CallLog,
    faults: Faults,
}

scriptable!(FakeResolver);

impl FakeResolver {
    /// A fake with its own call log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake writing to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }

    /// Sets the CNAME of `host` (replacing earlier ones).
    pub fn set_cname(&self, host: &Host, targets: Vec<Host>) {
        let _previous: Option<Vec<Host>> = lock(&self.zone).cname.insert(host.clone(), targets);
    }

    /// Sets the A records of `host` (replacing earlier ones).
    pub fn set_a(&self, host: &Host, addresses: Vec<Ipv4Addr>) {
        let _previous: Option<Vec<Ipv4Addr>> = lock(&self.zone).a.insert(host.clone(), addresses);
    }

    /// Sets the A records of `*.<zone>` (replacing earlier ones).
    pub fn set_wildcard_a(&self, zone: &Host, addresses: Vec<Ipv4Addr>) {
        let _previous: Option<Vec<Ipv4Addr>> =
            lock(&self.zone).wildcard.insert(zone.clone(), addresses);
    }

    /// Removes every record of `host`.
    pub fn remove(&self, host: &Host) {
        let mut zone = lock(&self.zone);
        let _cname: Option<Vec<Host>> = zone.cname.remove(host);
        let _a: Option<Vec<Ipv4Addr>> = zone.a.remove(host);
    }
}

impl Resolver for FakeResolver {
    fn cname(&self, host: &Host) -> impl Future<Output = Result<Vec<Host>, Error>> + Send {
        self.log
            .push(Call::Resolve(ResolveCall::Cname(host.clone())));
        ready(self.faults.check("cname").map(|()| {
            lock(&self.zone)
                .cname
                .get(host)
                .cloned()
                .unwrap_or_default()
        }))
    }

    fn a(&self, host: &Host) -> impl Future<Output = Result<Vec<Ipv4Addr>, Error>> + Send {
        self.log.push(Call::Resolve(ResolveCall::A(host.clone())));
        ready(self.faults.check("a").map(|()| lock(&self.zone).a(host)))
    }
}
