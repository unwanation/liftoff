use crate::error::Error;
use crate::proxy_config::ProxyConfig;

/// The reverse proxy (Caddy over its admin API in production).
///
/// The configuration is always pushed whole: every change of domains,
/// bindings or environment states calls [`Proxy::apply`] with a fresh
/// [`ProxyConfig`].
///
/// # Errors
///
/// [`Error::Upstream`] (system `caddy`) with Caddy's text when it rejects
/// the configuration.
pub trait Proxy: Send + Sync {
    /// Replaces the running configuration with `cfg`.
    fn apply(&self, cfg: ProxyConfig) -> impl Future<Output = Result<(), Error>> + Send;
    /// Whether the proxy answers its admin API (`GET /config/` on Caddy):
    /// what `GET /readyz` asks besides docker and the store.
    ///
    /// # Errors
    ///
    /// [`Error::Upstream`] (system `caddy`) when the admin API cannot be
    /// reached or answers anything but 2xx.
    fn ready(&self) -> impl Future<Output = Result<(), Error>> + Send;
}
