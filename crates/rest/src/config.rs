use core::net::{Ipv4Addr, SocketAddr};
use core::time::Duration;

use serde::{Deserialize, Serialize};

/// REST listener settings. `liftoffd` fills them from its `[api]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RestConfig {
    /// Address to bind. Defaults to loopback; put a reverse proxy in front for
    /// TLS and public exposure.
    pub bind: SocketAddr,
    /// Upper bound for a whole request/response cycle (e.g. `"10s"`).
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Upper bound for routes that wait on docker, Caddy or Forgejo
    /// (start, stop, remove, pin, domains, reconcile). Must
    /// cover the compose wait (120s) plus the upstream budget.
    #[serde(with = "humantime_serde")]
    pub long_request_timeout: Duration,
    /// Maximum accepted request body in bytes. Larger bodies get `413`.
    pub max_body_bytes: usize,
}

impl Default for RestConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 7000)),
            request_timeout: Duration::from_secs(10),
            long_request_timeout: Duration::from_secs(150),
            max_body_bytes: 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_loopback_only_when_default() {
        assert!(RestConfig::default().bind.ip().is_loopback());
    }

    // `POST /deploys` carries a compose file and an env file.
    #[test]
    fn allows_one_megabyte_when_default() {
        assert_eq!(RestConfig::default().max_body_bytes, 1_048_576);
    }

    #[test]
    fn listens_on_the_daemon_port_when_default() {
        assert_eq!(RestConfig::default().bind.port(), 7000);
    }

    #[test]
    fn rejects_when_field_unknown() {
        let err = serde_json::from_str::<RestConfig>(r#"{"bnd": "0.0.0.0:1"}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn parses_when_humantime_duration() {
        let cfg: RestConfig = serde_json::from_str(r#"{"request_timeout": "2s 500ms"}"#).unwrap();
        assert_eq!(cfg.request_timeout, Duration::from_millis(2500));
    }
}
