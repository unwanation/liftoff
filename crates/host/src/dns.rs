//! `DnsResolver`: the [`Resolver`] port over public DNS.
//!
//! It asks Cloudflare and Quad9 directly and caches nothing: under systemd-resolved `/etc/resolv.conf` names the caching
//! stub at `127.0.0.53`, whose answers can lag behind a record the user has
//! just changed, and `/etc/hosts` must not answer for a customer's domain.

use core::fmt;
use core::time::Duration;
use std::net::Ipv4Addr;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{CLOUDFLARE, QUAD9, ResolveHosts, ResolverConfig, ResolverOpts};
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{RData, RecordType};
use liftoff_core::domain::Host;
use liftoff_core::ports::Resolver;
use liftoff_core::{Error, System};

/// Public DNS through public name servers, without a cache.
#[derive(Clone)]
pub struct DnsResolver {
    resolver: TokioResolver,
}

impl fmt::Debug for DnsResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DnsResolver").finish_non_exhaustive()
    }
}

impl DnsResolver {
    /// A resolver asking Cloudflare and Quad9, without a cache.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when the resolver cannot be built.
    pub fn public() -> Result<Self, Error> {
        let (config, options) = settings();
        let resolver = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
            .with_options(options)
            .build()
            .map_err(|error| Error::internal_msg(format!("DNS resolver: {error}")))?;
        Ok(Self { resolver })
    }

    /// The answers of type `record_type` for `host`; none when the name
    /// has no such record or does not exist.
    async fn answers(&self, host: &Host, record_type: RecordType) -> Result<Vec<RData>, Error> {
        // A trailing dot makes it one fully qualified query, without the
        // search domains of the host.
        let name = format!("{host}.");
        match self.resolver.lookup(name.as_str(), record_type).await {
            Ok(lookup) => Ok(lookup
                .answers()
                .iter()
                .map(|record| record.data.clone())
                .collect()),
            Err(error) if error.is_no_records_found() => Ok(Vec::new()),
            Err(error) => Err(upstream(&error)),
        }
    }
}

/// The name servers and options of [`DnsResolver::public`]: public
/// resolvers over UDP and TCP, no response cache (zero entries, every TTL
/// capped at zero), and never `/etc/hosts`.
fn settings() -> (ResolverConfig, ResolverOpts) {
    let servers = CLOUDFLARE
        .udp_and_tcp()
        .chain(QUAD9.udp_and_tcp())
        .collect();
    let mut options = ResolverOpts::default();
    options.cache_size = 0;
    options.positive_max_ttl = Some(Duration::ZERO);
    options.negative_max_ttl = Some(Duration::ZERO);
    options.use_hosts_file = ResolveHosts::Never;
    (ResolverConfig::from_name_servers(servers), options)
}

/// A resolver failure, by what went wrong and without the name servers.
fn upstream(error: &NetError) -> Error {
    tracing::warn!(%error, "DNS lookup failed");
    Error::upstream(System::Dns, "DNS lookup failed")
}

impl Resolver for DnsResolver {
    async fn cname(&self, host: &Host) -> Result<Vec<Host>, Error> {
        Ok(self
            .answers(host, RecordType::CNAME)
            .await?
            .into_iter()
            .filter_map(|data| match data {
                RData::CNAME(target) => {
                    let text = target.0.to_ascii().to_ascii_lowercase();
                    Host::new(text.trim_end_matches('.')).ok()
                }
                _ => None,
            })
            .collect())
    }

    async fn a(&self, host: &Host) -> Result<Vec<Ipv4Addr>, Error> {
        Ok(self
            .answers(host, RecordType::A)
            .await?
            .into_iter()
            .filter_map(|data| match data {
                RData::A(address) => Some(address.0),
                _ => None,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod settings {
        use super::*;

        // TC: DOM-03 (plan 3: CNAME answers come from public resolvers,
        // not from the host's caching stub at 127.0.0.53)
        #[test]
        fn asks_public_name_servers_when_built() {
            let (config, _options) = settings();
            let servers: Vec<_> = config
                .name_servers()
                .iter()
                .map(|server| server.ip)
                .collect();
            assert!(
                !servers.is_empty() && servers.iter().all(|ip| !ip.is_loopback()),
                "{servers:?}"
            );
        }

        // TC: DOM-07 (plan 3: no cache, so reverify sees a changed record)
        #[test]
        fn caches_nothing_when_built() {
            let (_config, options) = settings();
            assert_eq!(
                (
                    options.cache_size,
                    options.positive_max_ttl,
                    options.negative_max_ttl,
                    options.use_hosts_file,
                ),
                (
                    0,
                    Some(Duration::ZERO),
                    Some(Duration::ZERO),
                    ResolveHosts::Never
                )
            );
        }
    }

    // TC: DOM-03 [live] (a real CNAME lookup)
    #[tokio::test]
    #[ignore = "live: needs public DNS"]
    async fn finds_a_records_when_name_exists() {
        let resolver = DnsResolver::public().unwrap();
        let addresses = resolver.a(&Host::new("one.one.one.one").unwrap()).await;
        assert!(
            addresses
                .as_ref()
                .is_ok_and(|found| found.contains(&Ipv4Addr::new(1, 1, 1, 1))),
            "{addresses:?}"
        );
    }

    // TC: DOM-05 [live]
    #[tokio::test]
    #[ignore = "live: needs public DNS"]
    async fn no_records_when_name_does_not_exist() {
        let resolver = DnsResolver::public().unwrap();
        let addresses = resolver
            .a(&Host::new("does-not-exist.liftoff.invalid").unwrap())
            .await;
        assert_eq!(addresses.ok(), Some(Vec::new()));
    }
}
