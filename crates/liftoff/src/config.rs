//! `~/.config/liftoff/config.toml`: the installations the CLI
//! knows, each with its URL and session token, and which one is the
//! default.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// One installation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Server {
    /// Base URL of the daemon.
    pub(crate) url: String,
    /// Session token from `liftoff login`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token: Option<String>,
    /// The tenant `create` names without `--tenant` (`lof tenant use`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tenant: Option<String>,
}

/// The saved settings. A missing file is an empty config.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// The server used without `--server` or `.liftoff`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_server: Option<String>,
    /// Every installation by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) servers: BTreeMap<String, Server>,
    /// Local mode; kept as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) local: Option<toml::Table>,
    /// Settings of the local dev loop; kept as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dev: Option<toml::Table>,
}

/// A server picked for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chosen {
    /// Its name in the config.
    pub(crate) name: String,
    /// Its settings.
    pub(crate) server: Server,
}

impl Config {
    /// Reads the file; `None` when there is none.
    pub(crate) fn read(path: &Path) -> Result<Option<Self>, CliError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CliError::Config(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        toml::from_str(&text).map(Some).map_err(|error| {
            CliError::Config(format!(
                "bad config {}: {error}; fix it, or delete it and run `liftoff login <url> --token <t>`",
                path.display()
            ))
        })
    }

    /// Reads the file; a missing file is an empty config.
    pub(crate) fn load(path: &Path) -> Result<Self, CliError> {
        Ok(Self::read(path)?.unwrap_or_default())
    }

    /// The server named `wanted`, else the default one. `None` when the
    /// config names no server at all.
    pub(crate) fn choose(&self, wanted: Option<&str>) -> Result<Option<Chosen>, CliError> {
        let Some(name) = wanted.or(self.default_server.as_deref()) else {
            return Ok(None);
        };
        self.servers
            .get(name)
            .map(|server| {
                Some(Chosen {
                    name: name.to_owned(),
                    server: server.clone(),
                })
            })
            .ok_or_else(|| {
                CliError::Config(format!(
                    "no server named {name:?} in the config; see `liftoff server ls`"
                ))
            })
    }

    /// `[local] templates`: the template repositories the
    /// catalogue adds to the server's, as written (`[host/]owner/name`).
    pub(crate) fn local_templates(&self) -> Vec<String> {
        self.local
            .as_ref()
            .and_then(|local| local.get("templates"))
            .and_then(toml::Value::as_array)
            .map(|repos| {
                repos
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The name under which `url` is saved.
    pub(crate) fn name_of(&self, url: &str) -> Option<String> {
        self.servers
            .iter()
            .find(|(_, server)| server.url == url)
            .map(|(name, _)| name.clone())
    }

    /// A free name for a new server at `url`: its host's first meaningful
    /// label (`liftoff.example.com` → `primary`), with a number when taken.
    pub(crate) fn new_name(&self, url: &str) -> String {
        let base = suggested_name(url);
        if !self.servers.contains_key(&base) {
            return base;
        }
        (2_u32..=u32::MAX)
            .map(|number| format!("{base}-{number}"))
            .find(|name| !self.servers.contains_key(name))
            .unwrap_or(base)
    }

    /// Writes the file, readable by its owner only.
    pub(crate) fn save(&self, path: &Path) -> Result<(), CliError> {
        let text = toml::to_string(self)
            .map_err(|error| CliError::Local(format!("cannot encode the config: {error}")))?;
        let failed = |error: std::io::Error| {
            CliError::Local(format!("cannot write {}: {error}", path.display()))
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(failed)?;
        }
        // Write beside the file and rename over it, so an interrupted write
        // or a full disk never leaves a truncated config behind.
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = std::path::PathBuf::from(temporary);
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            // `mode` only applies to a file that is created here.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)
        })();
        if written.is_err() {
            let _ignored: Result<(), _> = std::fs::remove_file(&temporary);
        }
        written.map_err(failed)
    }
}

/// `https://liftoff.example.com` → `primary`; an address or `localhost` → `local`.
fn suggested_name(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split_once(':').map_or(host, |(host, _port)| host);
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    let numeric = labels
        .iter()
        .all(|label| label.bytes().all(|byte| byte.is_ascii_digit()));
    if labels.is_empty() || numeric || host.starts_with('[') || host == "localhost" {
        return "local".to_owned();
    }
    let generic = ["liftoff", "api", "www", "deploy"];
    let label = labels
        .iter()
        .find(|label| !generic.contains(label))
        .or_else(|| labels.first())
        .copied()
        .unwrap_or("main");
    label.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    mod new_name {
        use super::*;

        // TC: CLIX-08
        #[test]
        fn takes_the_meaningful_label_when_the_host_is_named() {
            assert_eq!(
                Config::default().new_name("https://liftoff.example.com"),
                "example"
            );
        }

        // TC: CLIX-08
        #[test]
        fn is_local_when_the_host_is_an_address() {
            assert_eq!(Config::default().new_name("http://127.0.0.1:7000"), "local");
        }

        // TC: CLIX-08
        #[test]
        fn adds_a_number_when_the_name_is_taken() {
            let mut config = Config::default();
            let _previous: Option<Server> = config.servers.insert(
                "example".to_owned(),
                Server {
                    url: "https://other.example.com".to_owned(),
                    token: None,
                    tenant: None,
                },
            );
            assert_eq!(config.new_name("https://liftoff.example.com"), "example-2");
        }
    }
}
