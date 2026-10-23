//! Which project and environment a command is about:
//! `--project`/`--env`, else `git remote get-url origin` resolved by the
//! server (`POST /projects/-/resolve`), with `.liftoff` overriding the
//! server and the default environment.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::error::CliError;

/// The environment of a command when neither `--env` nor `.liftoff` names
/// one.
pub(crate) const DEFAULT_ENV: &str = "prod";

/// The per-checkout overrides file, never committed.
pub(crate) const DOTFILE: &str = ".liftoff";

/// Reads the `origin` remote of a working tree.
pub trait GitRemote: Send {
    /// `git remote get-url origin` run in `dir`; `None` outside a
    /// repository or without an `origin`.
    fn origin(&self, dir: &Path) -> Option<String>;
}

/// The real `git` binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitCommand;

impl GitRemote for GitCommand {
    fn origin(&self, dir: &Path) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["remote", "get-url", "origin"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let url = String::from_utf8(output.stdout).ok()?;
        let url = url.trim();
        (!url.is_empty()).then(|| url.to_owned())
    }
}

/// A repository on a forge, as `git remote` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Repository {
    /// The forge host without user or port, e.g. `git.example.com`.
    pub(crate) host: String,
    /// `owner/name`.
    pub(crate) repo: String,
}

impl Repository {
    /// The last segment of the path: the default project name.
    pub(crate) fn name(&self) -> &str {
        self.repo.rsplit('/').next().unwrap_or(&self.repo)
    }

    /// Parses the ssh (`git@host:owner/repo.git`, `ssh://git@host:2222/…`)
    /// and http(s) forms of a remote URL. A local path names no forge.
    pub(crate) fn parse(url: &str) -> Option<Self> {
        let url = url.trim();
        let (authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
            if !matches!(scheme, "ssh" | "git" | "http" | "https" | "git+ssh") {
                return None;
            }
            rest.split_once('/')?
        } else {
            // scp-like: `[user@]host:path`, with no `/` before the colon.
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') || authority.is_empty() {
                return None;
            }
            (authority, path)
        };
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let host = host.split_once(':').map_or(host, |(host, _port)| host);
        if host.is_empty() {
            return None;
        }
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
            return None;
        }
        Some(Self {
            host: host.to_ascii_lowercase(),
            repo: segments.join("/"),
        })
    }
}

/// `.liftoff`: which server and environment this checkout uses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dotfile {
    /// A server name of the CLI config.
    #[serde(default)]
    pub(crate) server: Option<String>,
    /// The default environment.
    #[serde(default)]
    pub(crate) env: Option<String>,
}

impl Dotfile {
    /// The nearest `.liftoff` from `cwd` up to the root of its working tree;
    /// none at all is an empty one.
    pub(crate) fn find(cwd: &Path) -> Result<Self, CliError> {
        let Some(path) = locate(cwd) else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(&path).map_err(|error| {
            CliError::Config(format!("cannot read {}: {error}", path.display()))
        })?;
        toml::from_str(&text)
            .map_err(|error| CliError::Config(format!("bad {}: {error}", path.display())))
    }
}

/// The top of the working tree `cwd` is in: the nearest directory that
/// holds `.git`, else `cwd` itself.
pub(crate) fn work_tree_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// `.liftoff` in `cwd` or a parent, stopping at the directory that holds
/// `.git`.
fn locate(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let candidate = dir.join(DOTFILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir.join(".git").exists() {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    mod work_tree_root {
        use super::*;

        #[test]
        fn is_the_directory_holding_git_when_cwd_is_below_it() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join(".git")).unwrap();
            let sub = dir.path().join("a").join("b");
            std::fs::create_dir_all(&sub).unwrap();
            assert_eq!(work_tree_root(&sub), dir.path());
        }

        #[test]
        fn is_cwd_when_no_parent_holds_git() {
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(work_tree_root(dir.path()), dir.path());
        }
    }

    mod parse {
        use super::*;

        fn repo(host: &str, repo: &str) -> Repository {
            Repository {
                host: host.to_owned(),
                repo: repo.to_owned(),
            }
        }

        // TC: CLIX-04
        #[test]
        fn keeps_nested_groups_when_the_forge_has_them() {
            assert_eq!(
                Repository::parse("https://gitlab.com/group/sub/app.git"),
                Some(repo("gitlab.com", "group/sub/app"))
            );
        }

        // TC: CLIX-04
        #[test]
        fn is_none_when_the_path_has_one_segment() {
            assert_eq!(Repository::parse("git@git.example.com:liftoff.git"), None);
        }

        // TC: CLIX-04
        #[test]
        fn is_none_when_the_scheme_is_a_file() {
            assert_eq!(Repository::parse("file:///srv/git/owner/app.git"), None);
        }

        // TC: CLIX-04
        #[test]
        fn names_the_project_after_the_last_segment() {
            assert_eq!(
                Repository::parse("git@git.example.com:acme/shop.git")
                    .as_ref()
                    .map(Repository::name),
                Some("shop")
            );
        }
    }
}
