//! The `wasi:http` hooks of a module (its `http_allow`, MOD-07) and of a
//! function (any host, with a timeout; FN-08).

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use core::time::Duration;
use std::sync::Arc;

use liftoff_core::domain::ModuleName;
use liftoff_core::module::HttpAllow;
use tokio::sync::Semaphore;
use wasmtime_wasi_http::{RequestOptions, WasiBody, WasiHttpHooks};

use crate::limits::MAX_HTTP_REQUESTS_IN_FLIGHT;
use crate::send::{Connection, send};

/// The `wasi:http` hooks of a module or function instance: a request of
/// a module whose host and port are not in the manifest's `http_allow`
/// is refused
/// (`HTTP-request-denied`) before any connection is made (MOD-07); unless
/// private networks are allowed, so is one whose host is, or resolves to,
/// an address that is not public (`destination-IP-prohibited`). An
/// allowed one is sent to the addresses that were checked
/// ([`send`](crate::send)), never resolved again. At most
/// [`MAX_HTTP_REQUESTS_IN_FLIGHT`] requests of the instance are open at
/// once; one more is refused (`connection-limit-reached`). A function
/// has no allow-list, but its requests wait at most its outgoing timeout
/// for a connection, the first byte and every next one.
#[derive(Debug, Clone)]
pub(crate) struct AllowedHttp {
    /// Who sends the requests, for the log: `module <name>` or
    /// `function of environment <id>`.
    caller: Arc<str>,
    allow: Allow,
    private_networks: bool,
    resolver: Resolver,
    /// The longest a request of the caller may wait, at each step.
    timeout: Option<Duration>,
    /// One permit per open request of the instance.
    in_flight: Arc<Semaphore>,
}

/// The hosts a caller may reach.
#[derive(Debug, Clone)]
enum Allow {
    /// A module's `http_allow`.
    List(Arc<[HttpAllow]>),
    /// Any host: a function.
    Any,
}

impl AllowedHttp {
    /// The hooks of `module` with its `http_allow`; `private_networks`
    /// lets it reach addresses that are not public.
    pub(crate) fn new(
        module: &ModuleName,
        allow: Arc<[HttpAllow]>,
        private_networks: bool,
        resolver: Resolver,
    ) -> Self {
        Self {
            caller: Arc::from(format!("module {module}")),
            allow: Allow::List(allow),
            private_networks,
            resolver,
            timeout: None,
            in_flight: Arc::new(Semaphore::new(MAX_HTTP_REQUESTS_IN_FLIGHT)),
        }
    }

    /// The hooks of a function (`caller` names it for the log): any host,
    /// each step of a request within `timeout` (FN-08); `private_networks`
    /// lets it reach addresses that are not public.
    pub(crate) fn any_host(caller: String, private_networks: bool, timeout: Duration) -> Self {
        Self {
            caller: Arc::from(caller),
            allow: Allow::Any,
            private_networks,
            resolver: Resolver::System,
            timeout: Some(timeout),
            in_flight: Arc::new(Semaphore::new(MAX_HTTP_REQUESTS_IN_FLIGHT)),
        }
    }

    /// Whether `host` on `port` is in the allow-list, `default_port` being
    /// the port of the request's scheme.
    fn allows(&self, host: &str, port: u16, default_port: u16) -> bool {
        match &self.allow {
            Allow::Any => true,
            Allow::List(allow) => allow
                .iter()
                .any(|allowed| allowed.allows(host, port, default_port)),
        }
    }

    /// `options` with every timeout at most the caller's.
    fn capped(&self, options: Option<RequestOptions>) -> Option<RequestOptions> {
        let Some(cap) = self.timeout else {
            return options;
        };
        let options = options.unwrap_or_default();
        let at_most = |asked: Option<Duration>| Some(asked.map_or(cap, |asked| asked.min(cap)));
        Some(RequestOptions {
            connect_timeout: at_most(options.connect_timeout),
            first_byte_timeout: at_most(options.first_byte_timeout),
            between_bytes_timeout: at_most(options.between_bytes_timeout),
        })
    }
}

/// How a host name becomes addresses: the system's resolver, or (in
/// tests) answers scripted one lookup at a time, or (feature
/// `test-support`) the system's with some `https` authorities routed to
/// fakes on loopback.
#[derive(Debug, Clone, Default)]
pub(crate) enum Resolver {
    /// `getaddrinfo` through tokio.
    #[default]
    System,
    /// The system's resolver, except for requests to `https://<authority>`
    /// of one of the [`HttpsRoute`]s, which go over plain HTTP to its
    /// address ([`ModuleHost::routing_https_to`](crate::ModuleHost::routing_https_to)).
    #[cfg(feature = "test-support")]
    Routed(Arc<[HttpsRoute]>),
    /// Each lookup takes the next answer; the queries are counted.
    #[cfg(test)]
    Scripted(Arc<std::sync::Mutex<Script>>),
}

/// Where [`Resolver::Routed`] sends `https://<authority>`.
#[cfg(feature = "test-support")]
#[derive(Debug, Clone)]
pub(crate) struct HttpsRoute {
    /// `host`, or `host:port` for a port other than 443.
    pub(crate) authority: String,
    /// The plain HTTP server the requests go to.
    pub(crate) to: SocketAddr,
}

/// The answers of [`Resolver::Scripted`] and how many lookups it saw.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Script {
    answers: std::collections::VecDeque<Vec<IpAddr>>,
    lookups: usize,
}

impl Resolver {
    /// The addresses of `name` on `port`.
    async fn lookup(&self, name: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
        match self {
            #[cfg(feature = "test-support")]
            Self::Routed(_) => Ok(tokio::net::lookup_host((name, port))
                .await?
                .map(|address| address.ip())
                .collect()),
            Self::System => Ok(tokio::net::lookup_host((name, port))
                .await?
                .map(|address| address.ip())
                .collect()),
            #[cfg(test)]
            Self::Scripted(script) => {
                let mut script = script
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                script.lookups = script.lookups.saturating_add(1);
                Ok(script.answers.pop_front().unwrap_or_default())
            }
        }
    }

    /// The address an `https` request to `host` on `port` is routed to
    /// over plain HTTP, if this is the [`Resolver::Routed`] of its
    /// authority.
    #[cfg(feature = "test-support")]
    fn https_route(&self, host: &str, port: u16) -> Option<SocketAddr> {
        let Self::Routed(routes) = self else {
            return None;
        };
        routes
            .iter()
            .find(|route| {
                if port == 443 {
                    route.authority == host
                } else {
                    route.authority == format!("{host}:{port}")
                }
            })
            .map(|route| route.to)
    }
}

/// `request` with its scheme `http`: what a routed `https` request is
/// sent as (no TLS to the fake).
#[cfg(feature = "test-support")]
fn plain(
    mut request: http::Request<WasiBody>,
) -> Result<http::Request<WasiBody>, wasmtime_wasi_http::Error> {
    let mut parts = request.uri().clone().into_parts();
    parts.scheme = Some(http::uri::Scheme::HTTP);
    *request.uri_mut() = http::Uri::from_parts(parts)
        .map_err(|_invalid| wasmtime_wasi_http::Error::HttpRequestUriInvalid)?;
    Ok(request)
}

/// Whether `ip` is a public address: not loopback, private, link-local,
/// shared (`100.64.0.0/10`), unspecified, broadcast, multicast,
/// documentation, `0.0.0.0/8`, unique local (`fc00::/7`) or an IPv4
/// address of one of these mapped into IPv6.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map_or_else(|| is_public_v6(ip), is_public_v4),
    }
}

/// [`is_public`] of an IPv4 address.
const fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    let shared = first == 100 && (second & 0xc0) == 64;
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || shared
        || first == 0)
}

/// [`is_public`] of an IPv6 address.
const fn is_public_v6(ip: Ipv6Addr) -> bool {
    let [first, ..] = ip.segments();
    let unique_local = (first & 0xfe00) == 0xfc00;
    let link_local = (first & 0xffc0) == 0xfe80;
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || unique_local || link_local)
}

/// The addresses `host` (a name or an IP literal) on `port` is reached
/// at, resolved once: unless `private_networks`, refused when any is not
/// public.
async fn vetted(
    caller: &str,
    resolver: &Resolver,
    host: &str,
    port: u16,
    private_networks: bool,
) -> Result<Vec<SocketAddr>, wasmtime_wasi_http::Error> {
    let addresses: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_name) => resolver.lookup(host, port).await.map_err(|error| {
            tracing::warn!(%caller, %host, %error, "an outgoing HTTP request names a host that does not resolve");
            wasmtime_wasi_http::Error::DestinationNotFound
        })?,
    };
    if addresses.is_empty() {
        return Err(wasmtime_wasi_http::Error::DestinationNotFound);
    }
    if !private_networks && let Some(private) = addresses.iter().find(|ip| !is_public(**ip)) {
        tracing::warn!(%caller, %host, address = %private, "an outgoing HTTP request is refused: the host is not a public address");
        return Err(wasmtime_wasi_http::Error::DestinationIpProhibited);
    }
    Ok(addresses
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect())
}

/// What [`WasiHttpHooks::send_request`] returns.
type Sent = Box<
    dyn Future<Output = wasmtime_wasi_http::Result<(http::Response<WasiBody>, Connection)>> + Send,
>;

impl WasiHttpHooks for AllowedHttp {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _fut: Connection,
    ) -> Sent {
        let options = self.capped(options);
        let uri = request.uri();
        let host = uri.host().unwrap_or_default().to_owned();
        let https = uri.scheme() == Some(&http::uri::Scheme::HTTPS);
        let default_port = if https { 443 } else { 80 };
        let port = uri.port_u16().unwrap_or(default_port);
        if !self.allows(&host, port, default_port) {
            tracing::warn!(caller = %self.caller, %host, port, "an outgoing HTTP request is refused: the host and port are not in http_allow");
            return Box::new(async { Err(wasmtime_wasi_http::Error::HttpRequestDenied) });
        }
        let Ok(permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
            tracing::warn!(caller = %self.caller, max = MAX_HTTP_REQUESTS_IN_FLIGHT, "an outgoing HTTP request is refused: too many requests are open");
            return Box::new(async { Err(wasmtime_wasi_http::Error::ConnectionLimitReached) });
        };
        let caller = Arc::clone(&self.caller);
        let resolver = self.resolver.clone();
        let private_networks = self.private_networks;
        Box::new(async move {
            let bare = host
                .strip_prefix('[')
                .and_then(|inner| inner.strip_suffix(']'))
                .unwrap_or(&host);
            #[cfg(feature = "test-support")]
            let (request, addresses) = match resolver.https_route(bare, port).filter(|_| https) {
                Some(to) => (plain(request)?, vec![to]),
                None => (
                    request,
                    vetted(&caller, &resolver, bare, port, private_networks).await?,
                ),
            };
            #[cfg(not(feature = "test-support"))]
            let addresses = vetted(&caller, &resolver, bare, port, private_networks).await?;
            let (response, connection) = send(request, options, &addresses, bare).await?;
            // The request is open until its connection ends.
            let connection: Connection = Box::new(async move {
                let _permit = permit;
                Box::into_pin(connection).await
            });
            Ok((response, connection))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the tests of `send_request` and `routed` share.
    mod send_request_support {
        use http_body_util::{BodyExt as _, Empty};

        use super::*;

        /// `GET url` without a body.
        pub(super) fn get(url: &str) -> http::Request<WasiBody> {
            http::Request::get(url)
                .body(
                    Empty::<bytes::Bytes>::new()
                        .map_err(|never| match never {})
                        .boxed_unsync(),
                )
                .unwrap()
        }

        /// The hooks of `badhost` allowing `allow`.
        pub(super) fn hooks(
            allow: &str,
            private_networks: bool,
            resolver: Resolver,
        ) -> AllowedHttp {
            AllowedHttp::new(
                &ModuleName::new("badhost").unwrap(),
                Arc::from(vec![HttpAllow::new(allow).unwrap()]),
                private_networks,
                resolver,
            )
        }

        /// Sends `url` through `hooks`.
        pub(super) fn sent(hooks: &mut AllowedHttp, url: &str) -> core::pin::Pin<Sent> {
            Box::into_pin(hooks.send_request(get(url), None, Box::new(async { Ok(()) })))
        }
    }

    mod send_request {
        use core::time::Duration;

        use super::send_request_support::*;
        use super::*;
        use crate::testing::Listener;

        /// A resolver answering `answers`, one per lookup, and its script.
        fn scripted(answers: &[&[&str]]) -> (Resolver, Arc<std::sync::Mutex<Script>>) {
            let script = Arc::new(std::sync::Mutex::new(Script {
                answers: answers
                    .iter()
                    .map(|ips| ips.iter().map(|ip| ip.parse().unwrap()).collect())
                    .collect(),
                lookups: 0,
            }));
            (Resolver::Scripted(Arc::clone(&script)), script)
        }

        // TC: MOD-07 (DNS rebinding: the name is resolved once, and the
        // request goes to the address that was checked)
        #[tokio::test]
        async fn connects_to_checked_address_when_name_would_resolve_elsewhere() {
            let listener = Listener::start().await.unwrap();
            let (resolver, script) = scripted(&[&["127.0.0.1"], &["127.0.0.2"]]);
            let mut hooks = hooks(&format!("rebind.test:{}", listener.port), true, resolver);
            let answered = sent(
                &mut hooks,
                &format!("http://rebind.test:{}/", listener.port),
            )
            .await;
            let lookups = script.lock().unwrap().lookups;
            assert_eq!(
                (
                    answered
                        .map(|(response, _)| response.status().as_u16())
                        .ok(),
                    lookups,
                    listener.accepted()
                ),
                (Some(200), 1, 1)
            );
        }

        // TC: MOD-07
        #[tokio::test]
        async fn refuses_name_when_one_of_its_addresses_is_private() {
            let (resolver, _script) = scripted(&[&["1.1.1.1", "127.0.0.1"]]);
            let mut hooks = hooks("mixed.test", false, resolver);
            let refused = sent(&mut hooks, "http://mixed.test/").await;
            assert!(matches!(
                refused,
                Err(wasmtime_wasi_http::Error::DestinationIpProhibited)
            ));
        }

        // TC: MOD-10 (a module cannot hold the daemon's sockets)
        #[tokio::test]
        async fn refuses_request_when_too_many_are_open() {
            let silent = Listener::silent().await.unwrap();
            let url = format!("http://127.0.0.1:{}/", silent.port);
            let mut hooks = hooks(
                &format!("127.0.0.1:{}", silent.port),
                true,
                Resolver::System,
            );
            let open: Vec<_> = (0..MAX_HTTP_REQUESTS_IN_FLIGHT)
                .map(|_| sent(&mut hooks, &url))
                .collect();
            let refused =
                tokio::time::timeout(Duration::from_secs(2), sent(&mut hooks, &url)).await;
            assert!(matches!(
                refused,
                Ok(Err(wasmtime_wasi_http::Error::ConnectionLimitReached))
            ));
            drop(open);
        }

        // TC: MOD-10 (a request that ended frees its place)
        #[tokio::test]
        async fn sends_request_again_when_an_open_one_is_dropped() {
            let silent = Listener::silent().await.unwrap();
            let url = format!("http://127.0.0.1:{}/", silent.port);
            let mut hooks = hooks(
                &format!("127.0.0.1:{}", silent.port),
                true,
                Resolver::System,
            );
            let mut open: Vec<_> = (0..MAX_HTTP_REQUESTS_IN_FLIGHT)
                .map(|_| sent(&mut hooks, &url))
                .collect();
            drop(open.pop());
            let waiting =
                tokio::time::timeout(Duration::from_millis(200), sent(&mut hooks, &url)).await;
            assert!(
                waiting.is_err(),
                "the request is sent and waits on the server"
            );
            drop(open);
        }
    }

    mod routed {
        use super::send_request_support::*;
        use super::*;
        use crate::testing::Listener;

        /// The resolver routing `https://<authority>` to `listener`.
        fn routing(authority: &str, listener: &Listener) -> Resolver {
            Resolver::Routed(Arc::from([HttpsRoute {
                authority: authority.to_owned(),
                to: SocketAddr::from(([127, 0, 0, 1], listener.port)),
            }]))
        }

        // TC: DNS-01 (the seam the fake Cloudflare sits behind)
        #[tokio::test]
        async fn sends_https_over_plain_http_to_route_when_authority_allowed() {
            let listener = Listener::start().await.unwrap();
            let mut hooks = hooks("api.test", false, routing("api.test", &listener));
            let answered = sent(&mut hooks, "https://api.test/zones").await;
            assert_eq!(
                (
                    answered
                        .map(|(response, _)| response.status().as_u16())
                        .ok(),
                    listener.accepted()
                ),
                (Some(200), 1)
            );
        }

        // TC: MOD-07 (routing never widens http_allow)
        #[tokio::test]
        async fn refused_when_routed_authority_not_in_http_allow() {
            let listener = Listener::start().await.unwrap();
            let mut hooks = hooks("other.test", false, routing("api.test", &listener));
            let refused = sent(&mut hooks, "https://api.test/zones").await;
            assert!(
                matches!(refused, Err(wasmtime_wasi_http::Error::HttpRequestDenied))
                    && listener.accepted() == 0
            );
        }

        // TC: MOD-07 (only `https` is routed)
        #[tokio::test]
        async fn not_routed_when_scheme_http() {
            let listener = Listener::start().await.unwrap();
            let mut hooks = hooks("api.test", false, routing("api.test", &listener));
            let outcome = sent(&mut hooks, "http://api.test/zones").await;
            assert!(
                outcome.is_err() && listener.accepted() == 0,
                "{:?}",
                outcome.map(|(response, _)| response.status())
            );
        }

        // TC: MOD-07 (`host:port` routes that port only)
        #[tokio::test]
        async fn routed_only_on_port_of_authority() {
            let listener = Listener::start().await.unwrap();
            let route = routing("api.test:8443", &listener);
            let mut default_port = hooks("api.test", false, route.clone());
            let mut own_port = hooks("api.test:8443", false, route);
            let elsewhere = sent(&mut default_port, "https://api.test/zones").await;
            let accepted_elsewhere = listener.accepted();
            let routed = sent(&mut own_port, "https://api.test:8443/zones").await;
            assert_eq!(
                (
                    elsewhere.is_err(),
                    accepted_elsewhere,
                    routed.map(|(response, _)| response.status().as_u16()).ok(),
                    listener.accepted()
                ),
                (true, 0, Some(200), 1)
            );
        }
    }

    mod is_public {
        use super::*;

        fn public(ip: &str) -> bool {
            is_public(ip.parse().unwrap())
        }

        // TC: MOD-07
        #[test]
        fn refuses_address_when_it_is_not_public() {
            for ip in [
                "127.0.0.1",
                "10.1.2.3",
                "172.16.0.1",
                "192.168.1.1",
                "169.254.169.254",
                "100.64.0.1",
                "0.0.0.0",
                "0.1.2.3",
                "255.255.255.255",
                "224.0.0.1",
                "::1",
                "::",
                "fd00::1",
                "fe80::1",
                "::ffff:127.0.0.1",
                "::ffff:10.0.0.1",
            ] {
                assert!(!public(ip), "{ip}");
            }
        }

        // TC: MOD-07
        #[test]
        fn allows_address_when_it_is_public() {
            for ip in ["1.1.1.1", "93.184.216.34", "100.128.0.1", "2606:4700::1111"] {
                assert!(public(ip), "{ip}");
            }
        }
    }
}
