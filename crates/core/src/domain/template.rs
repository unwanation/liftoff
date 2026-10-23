//! Template repositories: where
//! the catalogue of `lof create` reads a `liftoff.toml` from, and the
//! ten-minute cache of what it read.

use core::fmt;

use super::ids::UserId;
use super::refs::{Host, RepoRef};
use super::spelled::spelled_enum;
use super::timestamp::Timestamp;
use crate::error::{Problem, ValidationError};

/// A template repository as `[templates] repos` of `liftoffd.toml` and
/// `[local] templates` of the CLI's configuration name it:
/// `[<host>/]<owner>/<name>`.
///
/// Without a host (`acme/app`, the documented spelling) the entry
/// is read through each of the actor's connections in host order, and the
/// first that has a `liftoff.toml` lists it; when none has one, the
/// catalogue gives one warning naming every host it asked.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TemplateSource {
    /// The forge host, when the entry names one (`git.example.com`).
    pub host: Option<Host>,
    /// `owner/name` on that forge.
    pub repo: RepoRef,
}

impl TemplateSource {
    /// Human-readable shape for errors.
    pub const PATTERN: &'static str = "[host/]owner/name";

    /// Parses `[<host>/]<owner>/<name>`.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `repo` when `raw` is not two or
    /// three valid parts separated by slashes.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let pattern = || {
            ValidationError::new(
                "repo",
                Problem::Pattern {
                    expected: Self::PATTERN,
                },
            )
        };
        let parts: Vec<&str> = raw.split('/').collect();
        match parts.as_slice() {
            [_, _] => Ok(Self {
                host: None,
                repo: RepoRef::new(raw)?,
            }),
            [host, owner, name] => Ok(Self {
                host: Some(Host::new(host).map_err(|_invalid| pattern())?),
                repo: RepoRef::new(&format!("{owner}/{name}"))?,
            }),
            _ if raw.is_empty() => Err(ValidationError::new("repo", Problem::Empty)),
            _ => Err(pattern()),
        }
    }
}

impl fmt::Display for TemplateSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Some(host) => write!(f, "{host}/{}", self.repo),
            None => write!(f, "{}", self.repo),
        }
    }
}

/// `[template] group` of a `liftoff.toml`: the groups `lof
/// create` shows first, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TemplateGroup {
    /// A site.
    Web,
    /// An HTTP API.
    Api,
    /// A WASM function.
    Function,
    /// A compose service without a site of its own.
    Service,
}

spelled_enum!(TemplateGroup { Web => "web", Api => "api", Function => "function", Service => "service" });

/// What one user's connection read from a template repository, kept ten
/// minutes so the catalogue does not ask the forge on every call (TPL-05).
///
/// Unique by `(user_id, host, repo)`: a private template one user may read
/// is never answered to another from the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTemplate {
    /// Whose connection read it.
    pub user_id: UserId,
    /// The forge host.
    pub host: Host,
    /// `owner/name` on that host.
    pub repo: RepoRef,
    /// The `liftoff.toml` text on the default branch; `None` when the
    /// repository has none.
    pub manifest: Option<String>,
    /// When the forge was asked.
    pub fetched_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    mod template_source {
        use super::*;

        // TC: TPL-06 (`acme/app` names no host)
        #[test]
        fn no_host_when_owner_and_name() {
            assert_eq!(
                TemplateSource::new("Acme/app").unwrap(),
                TemplateSource {
                    host: None,
                    repo: RepoRef::new("acme/app").unwrap(),
                }
            );
        }

        // TC: TPL-06 (a repository on a named forge)
        #[test]
        fn host_when_three_parts() {
            assert_eq!(
                TemplateSource::new("git.example.com/acme/app").unwrap(),
                TemplateSource {
                    host: Some(Host::new("git.example.com").unwrap()),
                    repo: RepoRef::new("acme/app").unwrap(),
                }
            );
        }

        // TC: TPL-06
        #[test]
        fn written_back_as_parsed_when_displayed() {
            assert_eq!(
                TemplateSource::new("git.example.com/acme/app")
                    .unwrap()
                    .to_string(),
                "git.example.com/acme/app"
            );
        }

        // TC: TPL-06
        #[test]
        fn validation_on_repo_when_four_parts() {
            assert_eq!(
                TemplateSource::new("git.example.com/a/b/c")
                    .unwrap_err()
                    .field,
                "repo"
            );
        }

        // TC: TPL-06
        #[test]
        fn validation_on_repo_when_one_part() {
            assert_eq!(TemplateSource::new("acme").unwrap_err().field, "repo");
        }

        // TC: TPL-06
        #[test]
        fn validation_on_repo_when_host_invalid() {
            assert_eq!(
                TemplateSource::new("not a host/acme/app")
                    .unwrap_err()
                    .field,
                "repo"
            );
        }
    }
}
