//! Layered configuration: compiled defaults, then the TOML file
//! (`/etc/liftoff/liftoffd.toml`), then `LIFTOFF_*` environment variables.
//!
//! Env var names use `__` as the section separator: `LIFTOFF_API__BIND`,
//! `LIFTOFF_DEPLOY__WAIT_TIMEOUT`. Unknown keys are rejected so typos fail
//! fast.
//!
//! Secrets never appear in the configuration: it names the files that hold
//! them (`paths.master_key`), which are loaded at start-up.
//!
//! The sections are those of `config/default.toml`, plus `[docker]` (liftoff's own
//! `DOCKER_CONFIG`) and the listener limits in `[api]`;
//! [`AppConfig::settings`] turns the values the services read into the
//! core's [`Settings`].

use core::net::{Ipv4Addr, SocketAddr};
use core::time::Duration;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format as _, Serialized, Toml};
use liftoff_core::Error;
use liftoff_core::domain::{AcmeEmail, Host, HostPort, Label, TemplateSource};
use liftoff_core::module::RegistryKey;
use liftoff_core::service::{ArtifactSettings, ModuleSettings, ProxySettings, Settings, TlsMode};
use serde::{Deserialize, Serialize};

/// Where the daemon looks for its file when `--config` is not given.
pub const DEFAULT_CONFIG_FILE: &str = "/etc/liftoff/liftoffd.toml";

/// Root configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AppConfig {
    /// Logging.
    pub log: LogConfig,
    /// `SQLite` database.
    pub store: StoreConfig,
    /// Directories and key files.
    pub paths: PathsConfig,
    /// Docker.
    pub docker: DockerConfig,
    /// Deploys.
    pub deploy: DeployConfig,
    /// The public face of the API.
    pub api: ApiConfig,
    /// Managed and external domains.
    pub domains: DomainsConfig,
    /// Caddy.
    pub proxy: ProxyConfig,
    /// Functions.
    pub functions: FunctionsConfig,
    /// Tunnels of `lof dev`.
    pub tunnel: TunnelConfig,
    /// The gate of protected environments.
    pub gate: GateConfig,
    /// Artifacts.
    pub artifacts: ArtifactsConfig,
    /// Templates of `lof create`.
    pub templates: TemplatesConfig,
    /// The module registry.
    pub modules: ModulesConfig,
}

/// `[api]`: where the API is reached and its listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ApiConfig {
    /// Where the API and the daemon's pages are reached, e.g.
    /// `https://liftoff.example.com`. Required.
    pub public_url: String,
    /// The listener; loopback, Caddy is in front of it.
    pub bind: SocketAddr,
    /// Upper bound of a whole request/response cycle.
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Upper bound of the routes that wait on docker, Caddy or a forge.
    #[serde(with = "humantime_serde")]
    pub long_request_timeout: Duration,
    /// The largest accepted request body; larger ones get `413`.
    pub max_body_bytes: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        let rest = liftoff_rest::RestConfig::default();
        Self {
            public_url: String::new(),
            bind: rest.bind,
            request_timeout: rest.request_timeout,
            long_request_timeout: rest.long_request_timeout,
            max_body_bytes: rest.max_body_bytes,
        }
    }
}

impl ApiConfig {
    /// The settings of the REST listener.
    #[must_use]
    pub const fn rest(&self) -> liftoff_rest::RestConfig {
        liftoff_rest::RestConfig {
            bind: self.bind,
            request_timeout: self.request_timeout,
            long_request_timeout: self.long_request_timeout,
            max_body_bytes: self.max_body_bytes,
        }
    }
}

/// `[functions]` (read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FunctionsConfig {
    /// The functions listener, loopback.
    pub bind: SocketAddr,
    /// Memory of one instance.
    pub max_memory_mb: u32,
    /// Upper bound of one invocation.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// Instances kept per environment.
    pub instances_per_env: u32,
    /// The key-value store of one environment.
    pub kv_max_mb: u32,
}

impl Default for FunctionsConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 7003)),
            max_memory_mb: 128,
            timeout: Duration::from_secs(30),
            instances_per_env: 4,
            kv_max_mb: 64,
        }
    }
}

/// `[tunnel]` (read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TunnelConfig {
    /// The tunnel listener, loopback.
    pub bind: SocketAddr,
    /// Streams of one tunnel at once.
    pub max_streams: u32,
    /// How long a tunnel without its client stays before it is offline.
    #[serde(with = "humantime_serde")]
    pub offline_after: Duration,
    /// The bandwidth of one tunnel environment, both directions together.
    pub bandwidth: TunnelBandwidth,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 7004)),
            max_streams: 64,
            offline_after: Duration::from_secs(30),
            bandwidth: TunnelBandwidth::default(),
        }
    }
}

/// `[tunnel] bandwidth`: the token bucket of a tunnel environment: `bytes_per_second` on average, up to `burst` bytes at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TunnelBandwidth {
    /// Bytes a second both directions of the connection share; not 0.
    pub bytes_per_second: u64,
    /// Bytes that may pass at once after a pause.
    pub burst: u64,
}

impl Default for TunnelBandwidth {
    /// 10 MiB a second with bursts of 1 MiB.
    fn default() -> Self {
        Self {
            bytes_per_second: 10 * 1024 * 1024,
            burst: 1024 * 1024,
        }
    }
}

/// `[gate]` (read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GateConfig {
    /// The `forward_auth` listener; loopback only (validated): the gate
    /// trusts the `X-Liftoff-Host` and `X-Forwarded-Uri` Caddy sets.
    pub bind: SocketAddr,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 7001)),
        }
    }
}

/// `[artifacts]` (read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ArtifactsConfig {
    /// How long an artifact lives.
    #[serde(with = "humantime_serde")]
    pub ttl: Duration,
    /// The largest artifact.
    pub max_size_mb: u32,
    /// Versions kept per artifact.
    pub versions: u32,
    /// Where artifacts are served; empty means `<api.public_url>/a/`.
    pub host: String,
}

impl ArtifactsConfig {
    /// What the core reads: `max_size_mb` in
    /// MiB, an empty `host` as none. The host must not be `api_host`:
    /// the artifacts' host answers with the artifacts alone (VIEW-08), so
    /// it would hide the whole API.
    fn settings(&self, api_host: &Host) -> Result<ArtifactSettings, Error> {
        let host = if self.host.is_empty() {
            None
        } else {
            let host = Host::new(&self.host).map_err(|error| error.with_field("artifacts.host"))?;
            if &host == api_host {
                return Err(Error::invalid(
                    "artifacts.host",
                    "must differ from the host of api.public_url",
                ));
            }
            Some(host)
        };
        Ok(ArtifactSettings {
            ttl: self.ttl,
            max_size_bytes: u64::from(self.max_size_mb).saturating_mul(1024 * 1024),
            versions: usize::try_from(self.versions)
                .map_err(|_too_many| Error::invalid("artifacts.versions", "is too large"))?,
            host,
        })
    }
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            ttl: Duration::from_hours(5),
            max_size_mb: 16,
            versions: 50,
            host: String::new(),
        }
    }
}

/// `[templates]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TemplatesConfig {
    /// The template repositories `lof create` offers, `[host/]owner/name`.
    pub repos: Vec<String>,
}

impl Default for TemplatesConfig {
    fn default() -> Self {
        Self {
            repos: vec!["github.com/unwanation/liftoff-fn-ts".to_owned()],
        }
    }
}

/// `[modules]` (read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModulesConfig {
    /// The releases of the module registry.
    pub registry: String,
    /// The longest one call into a module may run (MOD-08).
    #[serde(with = "humantime_serde")]
    pub call_timeout: Duration,
    /// The most linear memory one module may grow to, in MiB (MOD-10).
    pub max_memory_mb: u32,
}

impl ModulesConfig {
    /// The longest `call_timeout` accepted.
    const MAX_CALL_TIMEOUT: Duration = Duration::from_hours(1);
    /// The largest `max_memory_mb` accepted: a wasm32 memory holds 4 GiB.
    const MAX_MEMORY_MB: u32 = 4096;

    /// What one module may use: `max_memory_mb` and `call_timeout`
    /// (MOD-08, MOD-10), checked at the boundary.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] of `modules.call_timeout` when it is zero
    /// (every call would time out and every module be disabled) or over an
    /// hour, of `modules.max_memory_mb` when it is zero (no module would
    /// instantiate) or over 4096.
    pub fn limits(&self) -> Result<liftoff_wasm::Limits, Error> {
        const MIB: usize = 1024 * 1024;
        if self.call_timeout.is_zero() || self.call_timeout > Self::MAX_CALL_TIMEOUT {
            return Err(Error::invalid(
                "modules.call_timeout",
                "must be more than 0s and at most 1h",
            ));
        }
        if self.max_memory_mb == 0 || self.max_memory_mb > Self::MAX_MEMORY_MB {
            return Err(Error::invalid(
                "modules.max_memory_mb",
                "must be between 1 and 4096",
            ));
        }
        Ok(liftoff_wasm::Limits {
            max_memory_bytes: usize::try_from(self.max_memory_mb)
                .unwrap_or(usize::MAX)
                .saturating_mul(MIB),
            call_timeout: self.call_timeout,
        })
    }
}

impl Default for ModulesConfig {
    fn default() -> Self {
        Self {
            registry: "https://api.github.com/repos/unwanation/liftoff-modules/releases".to_owned(),
            call_timeout: Duration::from_secs(10),
            max_memory_mb: 64,
        }
    }
}

/// `[domains]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DomainsConfig {
    /// The wildcard base of managed domains, e.g. `apps.example.com`.
    /// Required.
    pub base: String,
    /// The server's public address, for external domains' A records.
    /// Required.
    pub public_ip: Option<Ipv4Addr>,
    /// Labels nobody may take as a managed domain.
    pub reserved: Vec<String>,
    /// The host ports bindings are allocated from, inclusive.
    pub port_range: [u16; 2],
}

impl Default for DomainsConfig {
    fn default() -> Self {
        Self {
            base: String::new(),
            public_ip: None,
            reserved: Vec::new(),
            port_range: [20_000, 29_999],
        }
    }
}

/// `[proxy] tls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum TlsKind {
    /// Caddy gets certificates itself on 80 and 443.
    #[default]
    Acme,
    /// Caddy listens on `127.0.0.1:<edge_port>` behind the host's proxy.
    Edge,
}

/// `[proxy]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProxyConfig {
    /// How Caddy terminates TLS.
    pub tls: TlsKind,
    /// The ACME account; required with `tls = "acme"`.
    pub acme_email: Option<String>,
    /// The loopback port Caddy listens on; required with `tls = "edge"`.
    pub edge_port: Option<u16>,
    /// Caddy's admin API, loopback only.
    pub admin: SocketAddr,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            tls: TlsKind::Acme,
            acme_email: None,
            edge_port: None,
            admin: SocketAddr::from((Ipv4Addr::LOCALHOST, 2019)),
        }
    }
}

/// Logging settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    /// `tracing` filter directive, e.g. `info` or `info,liftoff_rest=debug`.
    pub level: String,
    /// Output format.
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: LogFormat::Json,
        }
    }
}

/// Log line encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// One JSON object per line (production default).
    Json,
    /// Human-readable, coloured when attached to a terminal.
    Pretty,
}

/// `[store]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StoreConfig {
    /// The database file.
    pub path: PathBuf,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            path: "/var/lib/liftoff/liftoff.db".into(),
        }
    }
}

/// `[paths]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathsConfig {
    /// The daemon's data directory; compose projects live in `<data>/compose`.
    pub data: PathBuf,
    /// The key that seals secrets (32 raw bytes or 64 hex digits).
    pub master_key: PathBuf,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            data: "/var/lib/liftoff".into(),
            master_key: "/etc/liftoff/master.key".into(),
        }
    }
}

impl PathsConfig {
    /// Where the compose project directories live.
    #[must_use]
    pub fn compose(&self) -> PathBuf {
        self.data.join("compose")
    }

    /// Where compiled modules are cached (MOD-12).
    #[must_use]
    pub fn module_cache(&self) -> PathBuf {
        self.data.join("cache")
    }
}

/// `[docker]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DockerConfig {
    /// `DOCKER_CONFIG` of liftoff's own docker commands; `docker login`
    /// writes here.
    pub config_dir: PathBuf,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            config_dir: "/var/lib/liftoff/docker".into(),
        }
    }
}

/// `[deploy]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeployConfig {
    /// How many deploys of different environments run at once.
    pub parallel: usize,
    /// `docker compose up --wait-timeout`, and also how long shutdown waits
    /// for the running deploys.
    #[serde(with = "humantime_serde")]
    pub wait_timeout: Duration,
    /// Deploys kept per environment.
    pub history: usize,
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            parallel: 2,
            wait_timeout: Duration::from_secs(120),
            history: 20,
        }
    }
}

impl AppConfig {
    /// Environment variable prefix.
    pub const ENV_PREFIX: &'static str = "LIFTOFF_";

    /// The [`Settings`] of the services, validated.
    ///
    /// # Errors
    ///
    /// A [`liftoff_core::Error::Validation`] naming the key that is missing or
    /// malformed: `api.public_url`, `domains.base`, `domains.public_ip`,
    /// `domains.reserved`, `domains.port_range`, `proxy.acme_email`,
    /// `proxy.edge_port`, `proxy.admin`, `modules.call_timeout`,
    /// `modules.max_memory_mb`, `templates.repos`, `artifacts.host`.
    pub fn settings(&self) -> Result<Settings, Error> {
        let _limits = self.modules.limits()?;
        let base = Host::new(&self.domains.base).map_err(|e| e.with_field("domains.base"))?;
        let public_ip = self
            .domains
            .public_ip
            .ok_or_else(|| Error::invalid("domains.public_ip", "is required"))?;
        let reserved = self
            .domains
            .reserved
            .iter()
            .map(|label| Label::new(label).map_err(|e| e.with_field("domains.reserved")))
            .collect::<Result<_, _>>()?;
        let [low, high] = self.domains.port_range;
        let port_range =
            HostPort::range(low, high).map_err(|e| e.with_field("domains.port_range"))?;
        self.check_listeners()?;
        let tls = match self.proxy.tls {
            TlsKind::Acme => TlsMode::Acme {
                email: AcmeEmail::new(self.proxy.acme_email.as_deref().ok_or_else(|| {
                    Error::invalid("proxy.acme_email", "is required with tls = \"acme\"")
                })?)
                .map_err(|e| e.with_field("proxy.acme_email"))?,
            },
            TlsKind::Edge => TlsMode::Edge {
                port: self.proxy.edge_port.ok_or_else(|| {
                    Error::invalid("proxy.edge_port", "is required with tls = \"edge\"")
                })?,
            },
        };
        let templates = self
            .templates
            .repos
            .iter()
            .map(|repo| TemplateSource::new(repo).map_err(|e| e.with_field("templates.repos")))
            .collect::<Result<_, _>>()?;
        let api = api_host(&self.api.public_url)?;
        Ok(Settings {
            base,
            public_ip,
            reserved,
            port_range,
            wait_timeout: self.deploy.wait_timeout,
            deploy_history: self.deploy.history,
            tunnel_offline_after: self.tunnel.offline_after,
            proxy: ProxySettings {
                api_host: api.clone(),
                api_bind: self.api.bind,
                tls,
                gate_bind: self.gate.bind,
                functions_bind: self.functions.bind,
                tunnel_bind: self.tunnel.bind,
            },
            modules: ModuleSettings {
                registry_key: RegistryKey::new(crate::modules::REGISTRY_PUBLIC_KEY),
                liftoffd_version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            templates,
            artifacts: self.artifacts.settings(&api)?,
        })
    }

    /// The loopback rule of the daemon's own listeners and the Caddy
    /// admin API, and a tunnel bandwidth that passes bytes.
    fn check_listeners(&self) -> Result<(), Error> {
        if !self.proxy.admin.ip().is_loopback() {
            return Err(Error::invalid(
                "proxy.admin",
                "the Caddy admin API must be on loopback",
            ));
        }
        if !self.gate.bind.ip().is_loopback() {
            return Err(Error::invalid(
                "gate.bind",
                "the gate trusts the headers of the local Caddy and must be on loopback",
            ));
        }
        if !self.functions.bind.ip().is_loopback() {
            return Err(Error::invalid(
                "functions.bind",
                "the function listener answers the local Caddy only and must be on loopback",
            ));
        }
        if !self.tunnel.bind.ip().is_loopback() {
            return Err(Error::invalid(
                "tunnel.bind",
                "the tunnel listener answers the local Caddy only and must be on loopback",
            ));
        }
        if self.tunnel.bandwidth.bytes_per_second == 0 {
            return Err(Error::invalid(
                "tunnel.bandwidth.bytes_per_second",
                "a tunnel needs some bandwidth: at least 1 byte a second",
            ));
        }
        Ok(())
    }

    /// Loads defaults, then `file` (if given), then environment overrides.
    ///
    /// # Errors
    ///
    /// The figment error: unreadable or malformed file, unknown key, wrong
    /// type.
    pub fn load(file: Option<&Path>) -> Result<Self, Box<figment::Error>> {
        Self::figment(file).extract().map_err(Box::new)
    }

    fn figment(file: Option<&Path>) -> Figment {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));
        if let Some(path) = file {
            figment = figment.merge(Toml::file_exact(path));
        }
        // `LIFTOFF_CONFIG` names the file (clap reads it); it is not a key.
        figment.merge(
            Env::prefixed(Self::ENV_PREFIX)
                .ignore(&["config"])
                .split("__"),
        )
    }
}

/// The host of `public_url` (`https://<host>[/]`).
fn api_host(public_url: &str) -> Result<Host, Error> {
    let invalid = || Error::invalid("api.public_url", "must be https://<host> or http://<host>");
    let rest = public_url
        .strip_prefix("https://")
        .or_else(|| public_url.strip_prefix("http://"))
        .ok_or_else(invalid)?;
    let host = rest.strip_suffix('/').unwrap_or(rest);
    if host.contains(['/', ':', '@', '?', '#']) {
        return Err(invalid());
    }
    Ok(Host::new(host).map_err(|e| e.with_field("api.public_url"))?)
}

#[cfg(test)]
#[expect(
    clippy::result_large_err,
    reason = "figment::Jail dictates the closure error type"
)]
mod tests {
    use figment::Jail;

    use super::*;

    mod load {
        use super::*;

        #[test]
        fn loads_defaults_when_no_source() {
            Jail::expect_with(|_| {
                assert_eq!(AppConfig::load(None).map_err(|e| *e)?, AppConfig::default());
                Ok(())
            });
        }

        #[test]
        fn defaults_match_when_nothing_is_set() {
            let config = AppConfig::default();
            assert_eq!(
                (
                    config.api.bind.to_string(),
                    config.store.path.to_str(),
                    config.paths.data.to_str(),
                    config.paths.master_key.to_str(),
                    config.docker.config_dir.to_str(),
                    config.deploy.parallel,
                    config.deploy.wait_timeout,
                    config.deploy.history,
                ),
                (
                    "127.0.0.1:7000".to_owned(),
                    Some("/var/lib/liftoff/liftoff.db"),
                    Some("/var/lib/liftoff"),
                    Some("/etc/liftoff/master.key"),
                    Some("/var/lib/liftoff/docker"),
                    2,
                    Duration::from_secs(120),
                    20,
                )
            );
        }

        #[test]
        fn file_wins_over_defaults_and_env_wins_over_file() {
            Jail::expect_with(|jail| {
                jail.create_file(
                    "app.toml",
                    "[deploy]\nparallel = 4\nwait_timeout = \"30s\"\n\
                     [log]\nlevel = \"debug\"\nformat = \"pretty\"",
                )?;
                jail.set_env("LIFTOFF_DEPLOY__PARALLEL", "3");
                jail.set_env("LIFTOFF_LOG__LEVEL", "warn");
                let cfg = AppConfig::load(Some(Path::new("app.toml"))).map_err(|e| *e)?;
                assert_eq!(
                    (
                        cfg.deploy.parallel,
                        cfg.deploy.wait_timeout,
                        cfg.log.level.as_str(),
                        cfg.log.format,
                        cfg.deploy.history
                    ),
                    (3, Duration::from_secs(30), "warn", LogFormat::Pretty, 20),
                    "env > file > defaults, per key"
                );
                Ok(())
            });
        }

        #[test]
        fn env_sets_nested_keys_when_prefixed() {
            Jail::expect_with(|jail| {
                jail.set_env("LIFTOFF_PATHS__DATA", "/srv/liftoff");
                jail.set_env("LIFTOFF_API__BIND", "127.0.0.1:7100");
                let cfg = AppConfig::load(None).map_err(|e| *e)?;
                assert_eq!(
                    (cfg.paths.data.to_str(), cfg.api.bind.port()),
                    (Some("/srv/liftoff"), 7100)
                );
                Ok(())
            });
        }

        #[test]
        fn ignores_the_config_path_variable_when_set() {
            Jail::expect_with(|jail| {
                jail.set_env("LIFTOFF_CONFIG", "/etc/liftoff/liftoffd.toml");
                assert!(AppConfig::load(None).is_ok(), "clap owns LIFTOFF_CONFIG");
                Ok(())
            });
        }

        #[test]
        fn rejects_when_key_unknown() {
            Jail::expect_with(|jail| {
                jail.create_file("app.toml", "[log]\nlevle = \"debug\"")?;
                assert!(AppConfig::load(Some(Path::new("app.toml"))).is_err());
                Ok(())
            });
        }

        #[test]
        fn rejects_when_section_unknown() {
            Jail::expect_with(|jail| {
                jail.create_file("app.toml", "[deploys]\nparallel = 4")?;
                assert!(AppConfig::load(Some(Path::new("app.toml"))).is_err());
                Ok(())
            });
        }

        #[test]
        fn rejects_when_env_key_unknown() {
            Jail::expect_with(|jail| {
                jail.set_env("LIFTOFF_DEPLOY__PARALLELISM", "4");
                assert!(AppConfig::load(None).is_err());
                Ok(())
            });
        }

        #[test]
        fn errors_when_file_missing() {
            Jail::expect_with(|_| {
                assert!(AppConfig::load(Some(Path::new("nope.toml"))).is_err());
                Ok(())
            });
        }
    }

    mod settings {
        use super::*;

        /// The keys the services need, set.
        pub(super) fn complete() -> AppConfig {
            let mut config = AppConfig::default();
            config.api.public_url = "https://liftoff.example.com/".to_owned();
            config.domains.base = "apps.example.com".to_owned();
            config.domains.public_ip = Some(Ipv4Addr::new(1, 2, 3, 4));
            config.domains.reserved = vec!["liftoff".to_owned()];
            config.proxy.acme_email = Some("ops@example.com".to_owned());
            config
        }

        /// The field a refused configuration names.
        fn refused(config: &AppConfig) -> Option<&'static str> {
            match config.settings() {
                Err(Error::Validation(error)) => Some(error.field),
                _ => None,
            }
        }

        // TC: SETUP-05 (the values the services read)
        #[test]
        fn carries_the_spec_9_values_when_complete() {
            let settings = complete().settings().unwrap();
            assert_eq!(
                (
                    settings.base.as_str(),
                    settings.public_ip,
                    settings.reserved.len(),
                    settings.port_range.start().get(),
                    settings.port_range.end().get(),
                    settings.proxy.api_host.as_str(),
                    settings.proxy.api_bind.port(),
                    settings.deploy_history,
                ),
                (
                    "apps.example.com",
                    Ipv4Addr::new(1, 2, 3, 4),
                    1,
                    20_000,
                    29_999,
                    "liftoff.example.com",
                    7000,
                    20
                )
            );
        }

        // TC: SETUP-05 (a configuration error is exit 78)
        #[test]
        fn names_the_key_when_the_base_is_missing() {
            let mut config = complete();
            config.domains.base = String::new();
            assert_eq!(refused(&config), Some("domains.base"));
        }

        // TC: SETUP-05
        #[test]
        fn names_the_key_when_the_public_url_has_a_path() {
            let mut config = complete();
            config.api.public_url = "https://liftoff.example.com/api".to_owned();
            assert_eq!(refused(&config), Some("api.public_url"));
        }

        // TC: SETUP-05
        #[test]
        fn names_the_key_when_acme_has_no_email() {
            let mut config = complete();
            config.proxy.acme_email = None;
            assert_eq!(refused(&config), Some("proxy.acme_email"));
        }

        // TC: SETUP-05 (`tls = edge` on a loopback port)
        #[test]
        fn edge_on_its_port_when_edge_is_chosen() {
            Jail::expect_with(|jail| {
                jail.create_file(
                    "app.toml",
                    "[api]\npublic_url = \"https://liftoff.example.com\"\n\
                     [domains]\nbase = \"apps.example.com\"\npublic_ip = \"1.2.3.4\"\n\
                     [proxy]\ntls = \"edge\"\nedge_port = 7002",
                )?;
                let config = AppConfig::load(Some(Path::new("app.toml"))).map_err(|e| *e)?;
                assert_eq!(
                    config.settings().map(|s| s.proxy.tls).ok(),
                    Some(TlsMode::Edge { port: 7002 })
                );
                Ok(())
            });
        }

        // TC: SETUP-05 (the Caddy admin API only on loopback)
        #[test]
        fn names_the_key_when_the_caddy_admin_is_not_loopback() {
            let mut config = complete();
            config.proxy.admin = "0.0.0.0:2019".parse().unwrap();
            assert_eq!(refused(&config), Some("proxy.admin"));
        }

        // TC: SETUP-05, TEN-07 (the gate trusts `X-Liftoff-Host` because only the local Caddy reaches it)
        #[test]
        fn names_the_key_when_the_gate_is_not_loopback() {
            let mut config = complete();
            config.gate.bind = "0.0.0.0:7001".parse().unwrap();
            assert_eq!(refused(&config), Some("gate.bind"));
        }

        // the function listener takes loopback peers only.
        #[test]
        fn names_the_key_when_the_function_listener_is_not_loopback() {
            let mut config = complete();
            config.functions.bind = "0.0.0.0:7003".parse().unwrap();
            assert_eq!(refused(&config), Some("functions.bind"));
        }

        // the tunnel listener takes loopback peers only.
        #[test]
        fn names_the_key_when_the_tunnel_listener_is_not_loopback() {
            let mut config = complete();
            config.tunnel.bind = "0.0.0.0:7004".parse().unwrap();
            assert_eq!(refused(&config), Some("tunnel.bind"));
        }

        // a tunnel without bandwidth would never pass a byte.
        #[test]
        fn names_the_key_when_the_tunnel_bandwidth_is_zero() {
            let mut config = complete();
            config.tunnel.bandwidth.bytes_per_second = 0;
            assert_eq!(refused(&config), Some("tunnel.bandwidth.bytes_per_second"));
        }

        // TC: TPL-06 (the server's half of the catalogue: `[templates]
        // repos`, in order, with or without a host)
        #[test]
        fn carries_the_template_repos_when_configured() {
            let mut config = complete();
            config.templates.repos = vec![
                "acme/app".to_owned(),
                "git.example.com/acme/fn-ts".to_owned(),
            ];
            let templates: Vec<String> = config
                .settings()
                .unwrap()
                .templates
                .iter()
                .map(ToString::to_string)
                .collect();
            assert_eq!(templates, ["acme/app", "git.example.com/acme/fn-ts"]);
        }

        // TC: TPL-06 (a configuration error names the key, exit 78)
        #[test]
        fn names_the_key_when_a_template_repo_is_invalid() {
            let mut config = complete();
            config.templates.repos = vec!["not a repo".to_owned()];
            assert_eq!(refused(&config), Some("templates.repos"));
        }

        // TC: ART-01, ART-03 (the TTL as it is, `max_size_mb` in MiB, an empty `host` as none)
        #[test]
        fn carries_the_artifact_limits_when_defaults() {
            let artifacts = complete().settings().unwrap().artifacts;
            assert_eq!(
                (
                    artifacts.ttl,
                    artifacts.max_size_bytes,
                    artifacts.versions,
                    artifacts.host
                ),
                (Duration::from_hours(5), 16 * 1024 * 1024, 50, None)
            );
        }

        // TC: ART-01 (`https://<artifacts.host>/<id>` when it is set)
        #[test]
        fn carries_the_artifact_host_when_set() {
            let mut config = complete();
            config.artifacts.host = "art.example.com".to_owned();
            assert_eq!(
                config
                    .settings()
                    .ok()
                    .and_then(|settings| settings.artifacts.host)
                    .map(|host| host.to_string()),
                Some("art.example.com".to_owned())
            );
        }

        // a configuration error names the key (exit 78).
        #[test]
        fn names_the_key_when_the_artifact_host_is_invalid() {
            let mut config = complete();
            config.artifacts.host = "not a host".to_owned();
            assert_eq!(refused(&config), Some("artifacts.host"));
        }

        // TC: VIEW-08 (the artifacts' host answers with the artifacts
        // alone: the API host as that host would hide the whole API)
        #[test]
        fn names_the_key_when_the_artifact_host_is_the_api_host() {
            let mut config = complete();
            config.artifacts.host = "liftoff.example.com".to_owned();
            assert_eq!(refused(&config), Some("artifacts.host"));
        }
    }

    mod spec_9 {
        use super::*;

        /// The documented example, one key per line.
        const EXAMPLE: &str = r#"
[api]
public_url = "https://liftoff.example.com"
bind = "127.0.0.1:7000"

[domains]
base = "apps.example.com"
public_ip = "1.2.3.4"
reserved = []
port_range = [20000, 29999]

[proxy]
tls = "acme"
acme_email = "ops@example.com"

[store]
path = "/var/lib/liftoff/liftoff.db"

[paths]
data = "/var/lib/liftoff"
master_key = "/etc/liftoff/master.key"

[deploy]
parallel = 2
wait_timeout = "120s"
history = 20

[functions]
bind = "127.0.0.1:7003"
max_memory_mb = 128
timeout = "30s"
instances_per_env = 4
kv_max_mb = 64

[tunnel]
bind = "127.0.0.1:7004"
max_streams = 64
offline_after = "30s"
bandwidth = { bytes_per_second = 10485760, burst = 1048576 }

[gate]
bind = "127.0.0.1:7001"

[artifacts]
ttl = "5h"
max_size_mb = 16
versions = 50
host = ""

[templates]
repos = ["acme/app", "acme/fn-ts"]

[modules]
registry = "https://api.github.com/repos/unwanation/liftoff-modules/releases"
call_timeout = "10s"
max_memory_mb = 64

[log]
format = "json"
level = "info"
"#;

        // TC: SETUP-05 (every documented section is a known key)
        #[test]
        fn loads_when_the_spec_example_is_given() {
            Jail::expect_with(|jail| {
                jail.create_file("liftoffd.toml", EXAMPLE)?;
                let loaded = AppConfig::load(Some(Path::new("liftoffd.toml")));
                assert!(loaded.is_ok(), "{loaded:?}");
                Ok(())
            });
        }

        // TC: SETUP-05 (the listener is `[api] bind`)
        #[test]
        fn listens_on_the_api_bind_when_it_is_set() {
            Jail::expect_with(|jail| {
                jail.create_file(
                    "liftoffd.toml",
                    "[api]\npublic_url = \"https://liftoff.example.com\"\nbind = \"127.0.0.1:7100\"\n\
                     [domains]\nbase = \"apps.example.com\"\npublic_ip = \"1.2.3.4\"\n\
                     [proxy]\nacme_email = \"ops@example.com\"",
                )?;
                let config = AppConfig::load(Some(Path::new("liftoffd.toml"))).map_err(|e| *e)?;
                assert_eq!(
                    config.settings().map(|s| s.proxy.api_bind.to_string()).ok(),
                    Some("127.0.0.1:7100".to_owned())
                );
                Ok(())
            });
        }

        // TC: SETUP-05
        #[test]
        fn env_sets_the_api_bind_when_prefixed() {
            Jail::expect_with(|jail| {
                jail.set_env("LIFTOFF_API__BIND", "127.0.0.1:7200");
                jail.set_env("LIFTOFF_API__PUBLIC_URL", "https://liftoff.example.com");
                jail.set_env("LIFTOFF_DOMAINS__BASE", "apps.example.com");
                jail.set_env("LIFTOFF_DOMAINS__PUBLIC_IP", "1.2.3.4");
                jail.set_env("LIFTOFF_PROXY__ACME_EMAIL", "ops@example.com");
                let config = AppConfig::load(None).map_err(|e| *e)?;
                assert_eq!(
                    config.settings().map(|s| s.proxy.api_bind.port()).ok(),
                    Some(7200)
                );
                Ok(())
            });
        }

        // TC: SETUP-05 (unknown keys stay an error inside the new sections)
        #[test]
        fn rejects_when_a_key_of_a_spec_section_is_unknown() {
            Jail::expect_with(|jail| {
                jail.create_file("liftoffd.toml", "[functions]\nmax_memory = 128")?;
                assert!(AppConfig::load(Some(Path::new("liftoffd.toml"))).is_err());
                Ok(())
            });
        }
    }

    mod modules {
        use super::*;

        fn loaded(toml: &str) -> AppConfig {
            let mut config = None;
            Jail::expect_with(|jail| {
                jail.create_file("liftoffd.toml", toml)?;
                config = Some(AppConfig::load(Some(Path::new("liftoffd.toml"))).map_err(|e| *e)?);
                Ok(())
            });
            config.unwrap()
        }

        // TC: MOD-10 (`[modules] max_memory_mb` caps a module's memory)
        #[test]
        fn caps_module_memory_when_max_memory_mb_is_set() {
            let config = loaded("[modules]\nmax_memory_mb = 32");
            assert_eq!(
                config
                    .modules
                    .limits()
                    .ok()
                    .map(|limits| limits.max_memory_bytes),
                Some(32 * 1024 * 1024)
            );
        }

        /// The field `[modules]` of `toml` is refused with, if any.
        fn refused(toml: &str) -> Option<&'static str> {
            match loaded(toml).modules.limits() {
                Err(Error::Validation(error)) => Some(error.field),
                _ => None,
            }
        }

        // TC: MOD-08 (a zero timeout would time out, and disable, every module)
        #[test]
        fn rejects_call_timeout_when_it_is_zero() {
            assert_eq!(
                refused("[modules]\ncall_timeout = \"0s\""),
                Some("modules.call_timeout")
            );
        }

        // TC: MOD-08
        #[test]
        fn rejects_call_timeout_when_it_is_over_an_hour() {
            assert_eq!(
                refused("[modules]\ncall_timeout = \"2h\""),
                Some("modules.call_timeout")
            );
        }

        // TC: MOD-10 (no module instantiates without memory)
        #[test]
        fn rejects_max_memory_mb_when_it_is_zero() {
            assert_eq!(
                refused("[modules]\nmax_memory_mb = 0"),
                Some("modules.max_memory_mb")
            );
        }

        // TC: MOD-10 (a wasm32 memory holds 4 GiB at most)
        #[test]
        fn rejects_max_memory_mb_when_it_is_over_4096() {
            assert_eq!(
                refused("[modules]\nmax_memory_mb = 4097"),
                Some("modules.max_memory_mb")
            );
        }

        // TC: MOD-08 (the daemon refuses to start on a bad `[modules]`)
        #[test]
        fn refuses_settings_when_modules_limits_are_invalid() {
            let mut config = settings::complete();
            config.modules.call_timeout = Duration::ZERO;
            assert!(matches!(
                config.settings(),
                Err(Error::Validation(error)) if error.field == "modules.call_timeout"
            ));
        }

        // TC: MOD-08 (`[modules] call_timeout` bounds one call)
        #[test]
        fn bounds_module_call_when_call_timeout_is_set() {
            let config = loaded("[modules]\ncall_timeout = \"2s\"");
            assert_eq!(
                config
                    .modules
                    .limits()
                    .ok()
                    .map(|limits| limits.call_timeout),
                Some(Duration::from_secs(2))
            );
        }

        // TC: MOD-12 (compiled components live in `paths.data/cache/`)
        #[test]
        fn caches_modules_below_data_when_data_is_set() {
            let config = loaded("[paths]\ndata = \"/srv/liftoff\"");
            assert_eq!(
                config.paths.module_cache(),
                PathBuf::from("/srv/liftoff/cache")
            );
        }
    }

    mod default_toml {
        use super::*;

        // TC: SETUP-05 (`config/default.toml` documents the defaults)
        #[test]
        fn equals_the_compiled_defaults_when_loaded() {
            Jail::expect_with(|jail| {
                jail.create_file("default.toml", include_str!("../../../config/default.toml"))?;
                let config = AppConfig::load(Some(Path::new("default.toml"))).map_err(|e| *e)?;
                assert_eq!(config, AppConfig::default());
                Ok(())
            });
        }
    }

    mod env_example {
        use super::*;

        // `.env.example` at the repository root documents the variables.
        #[test]
        fn loads_and_keeps_the_defaults_when_the_example_is_used() {
            Jail::expect_with(|jail| {
                let example = include_str!("../../../.env.example");
                for line in example.lines().map(str::trim) {
                    if let Some((key, value)) = line.split_once('=') {
                        jail.set_env(key, value);
                    }
                }
                let config = AppConfig::load(None).map_err(|e| *e)?;
                assert_eq!(config.api.bind, AppConfig::default().api.bind);
                Ok(())
            });
        }
    }

    mod dockerfile {
        use super::*;

        /// The `KEY=value` pairs of the `ENV` instructions of the image,
        /// continuation lines included.
        fn image_env() -> Vec<(String, String)> {
            let dockerfile = include_str!("../../../Dockerfile");
            let mut pairs = Vec::new();
            let mut in_env = false;
            for line in dockerfile.lines().map(str::trim) {
                let rest = if let Some(rest) = line.strip_prefix("ENV ") {
                    rest
                } else if in_env {
                    line
                } else {
                    continue;
                };
                in_env = rest.ends_with('\\');
                for word in rest.trim_end_matches('\\').split_whitespace() {
                    if let Some((key, value)) = word.split_once('=') {
                        pairs.push((key.to_owned(), value.to_owned()));
                    }
                }
            }
            pairs
        }

        // The image's own environment is a valid configuration (unknown keys are an error) that binds every interface.
        #[test]
        fn loads_and_binds_every_interface_when_the_image_env_is_used() {
            Jail::expect_with(|jail| {
                let pairs = image_env();
                assert!(!pairs.is_empty(), "the Dockerfile sets no ENV");
                for (key, value) in &pairs {
                    jail.set_env(key, value);
                }
                let config = AppConfig::load(None).map_err(|e| *e)?;
                assert_eq!(config.api.bind.to_string(), "0.0.0.0:8080");
                Ok(())
            });
        }
    }
}
