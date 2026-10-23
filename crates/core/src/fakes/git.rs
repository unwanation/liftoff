//! [`FakeGit`].

use core::future::ready;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use super::log::{Call, CallLog, Faults, GitCall, lock, scriptable};
use crate::domain::{DeviceCode, GitProvider, Host, Login, RepoRef, SecretKey, SecretValue};
use crate::error::Error;
use crate::ports::{
    DeviceGrant, DeviceStart, Git, GitAccess, NewRepo, PackageRef, RepoRole, Template,
};

#[derive(Debug, Default)]
struct GitState {
    roles: BTreeMap<(Login, RepoRef), RepoRole>,
    images: BTreeSet<(Login, PackageRef)>,
    ci_secrets: BTreeMap<(RepoRef, SecretKey), SecretValue>,
    accounts: BTreeMap<(Host, String), Login>,
    manifests: BTreeMap<RepoRef, String>,
    /// Host → the device flow it offers and the answers of its next polls.
    devices: BTreeMap<Host, (DeviceStart, VecDeque<DeviceGrant>)>,
    /// Host → the provider whose module serves it.
    served: BTreeMap<Host, GitProvider>,
}

/// A forge with a role table instead of a server, keyed by the account
/// login of the [`GitAccess`] (its host and token are not checked).
///
/// - [`FakeGit::grant`] sets an account's role on a repository (default:
///   [`RepoRole::None`]).
/// - [`FakeGit::allow_image`] makes an image visible to an account
///   (default: not visible).
/// - [`FakeGit::accept_token`] makes `account` answer a login for a token
///   on a host (default: every token is rejected with [`Error::Forbidden`]).
/// - `put_ci_secret` needs [`RepoRole::Admin`], otherwise it is
///   [`Error::Forbidden`]; [`FakeGit::ci_secret`] shows stored values.
/// - [`FakeGit::template`] gives a repository a `liftoff.toml`, which
///   `list_templates` reads for an account that may read the repository
///   (default: none).
/// - `generate_from_template` needs [`RepoRole::Read`] on the template,
///   otherwise it is [`Error::NotFound`]; it makes the account admin of
///   the repository it answers, with the template's `liftoff.toml`.
/// - [`FakeGit::offer_device_flow`] gives a host a device flow (default:
///   none, so `device_start` and `device_poll` are [`Error::Validation`]
///   pointing at `--token`, as a module without an OAuth application);
///   each poll answers the next grant, the last one repeats.
/// - [`FakeGit::serve`] makes a provider's module serve a host, which
///   `provider_of` answers (default: none, so `provider_of` is
///   [`Error::Validation`] on `host`, as when no git module serves it).
#[derive(Debug, Clone, Default)]
pub struct FakeGit {
    state: Arc<Mutex<GitState>>,
    log: CallLog,
    faults: Faults,
}

scriptable!(FakeGit);

impl FakeGit {
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

    /// Sets the role of `login` on `repo`.
    pub fn grant(&self, login: &Login, repo: &RepoRef, role: RepoRole) {
        let _previous: Option<RepoRole> = lock(&self.state)
            .roles
            .insert((login.clone(), repo.clone()), role);
    }

    /// Makes `account` answer `login` for `token` on `host`.
    pub fn accept_token(&self, host: &Host, token: &str, login: &Login) {
        let _previous: Option<Login> = lock(&self.state)
            .accounts
            .insert((host.clone(), token.to_owned()), login.clone());
    }

    /// Makes `image` visible to `login`.
    pub fn allow_image(&self, login: &Login, image: &PackageRef) {
        let _inserted: bool = lock(&self.state)
            .images
            .insert((login.clone(), image.clone()));
    }

    /// Gives `repo` the `liftoff.toml` `toml`.
    pub fn template(&self, repo: &RepoRef, toml: &str) {
        let _previous: Option<String> = lock(&self.state)
            .manifests
            .insert(repo.clone(), toml.to_owned());
    }

    /// Makes `host` offer the device flow `start`, whose polls answer
    /// `grants` in order (the last one repeats; none: pending).
    pub fn offer_device_flow(&self, host: &Host, start: DeviceStart, grants: Vec<DeviceGrant>) {
        let _previous = lock(&self.state)
            .devices
            .insert(host.clone(), (start, grants.into()));
    }

    /// Makes the module of `provider` serve `host`.
    pub fn serve(&self, host: &Host, provider: GitProvider) {
        let _previous: Option<GitProvider> =
            lock(&self.state).served.insert(host.clone(), provider);
    }

    /// The value of a CI secret, if set.
    #[must_use]
    pub fn ci_secret(&self, repo: &RepoRef, key: &SecretKey) -> Option<SecretValue> {
        lock(&self.state)
            .ci_secrets
            .get(&(repo.clone(), key.clone()))
            .cloned()
    }

    /// Every CI secret of `repo`, by key.
    #[must_use]
    pub fn ci_secrets(&self, repo: &RepoRef) -> Vec<(SecretKey, SecretValue)> {
        lock(&self.state)
            .ci_secrets
            .iter()
            .filter(|((of, _), _)| of == repo)
            .map(|((_, key), value)| (key.clone(), value.clone()))
            .collect()
    }

    /// The git calls received so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<GitCall> {
        self.log
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Git(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    fn role(&self, login: &Login, repo: &RepoRef) -> RepoRole {
        lock(&self.state)
            .roles
            .get(&(login.clone(), repo.clone()))
            .copied()
            .unwrap_or(RepoRole::None)
    }
}

impl Git for FakeGit {
    fn provider_of(&self, host: &Host) -> impl Future<Output = Result<GitProvider, Error>> + Send {
        self.log
            .push(Call::Git(GitCall::ProviderOf { host: host.clone() }));
        let result = self.faults.check("provider_of").and_then(|()| {
            lock(&self.state)
                .served
                .get(host)
                .copied()
                .ok_or_else(|| Error::invalid("host", format!("no git module serves {host}")))
        });
        ready(result)
    }

    fn account(
        &self,
        _provider: GitProvider,
        host: &Host,
        token: &SecretValue,
    ) -> impl Future<Output = Result<Login, Error>> + Send {
        self.log
            .push(Call::Git(GitCall::Account { host: host.clone() }));
        let result = self.faults.check("account").and_then(|()| {
            lock(&self.state)
                .accounts
                .get(&(host.clone(), token.expose().to_owned()))
                .cloned()
                .ok_or_else(|| Error::forbidden(format!("{host} rejected the token")))
        });
        ready(result)
    }

    fn repo_role(
        &self,
        connection: &GitAccess,
        repo: &RepoRef,
    ) -> impl Future<Output = Result<RepoRole, Error>> + Send {
        self.log.push(Call::Git(GitCall::RepoRole {
            login: connection.login.clone(),
            repo: repo.clone(),
        }));
        ready(
            self.faults
                .check("repo_role")
                .map(|()| self.role(&connection.login, repo)),
        )
    }

    fn image_exists(
        &self,
        connection: &GitAccess,
        image: &PackageRef,
    ) -> impl Future<Output = Result<bool, Error>> + Send {
        self.log.push(Call::Git(GitCall::ImageExists {
            login: connection.login.clone(),
            image: image.clone(),
        }));
        ready(self.faults.check("image_exists").map(|()| {
            lock(&self.state)
                .images
                .contains(&(connection.login.clone(), image.clone()))
        }))
    }

    fn put_ci_secret(
        &self,
        connection: &GitAccess,
        repo: &RepoRef,
        key: &SecretKey,
        value: &SecretValue,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.log.push(Call::Git(GitCall::PutCiSecret {
            login: connection.login.clone(),
            repo: repo.clone(),
            key: key.clone(),
        }));
        let result = self.faults.check("put_ci_secret").and_then(|()| {
            if self.role(&connection.login, repo) < RepoRole::Admin {
                return Err(Error::forbidden(format!(
                    "{} may not manage the CI secrets of {repo}",
                    connection.login
                )));
            }
            let _previous: Option<SecretValue> = lock(&self.state)
                .ci_secrets
                .insert((repo.clone(), key.clone()), value.clone());
            Ok(())
        });
        ready(result)
    }

    fn list_templates(
        &self,
        connection: &GitAccess,
        repos: &[RepoRef],
    ) -> impl Future<Output = Result<Vec<Template>, Error>> + Send {
        self.log.push(Call::Git(GitCall::ListTemplates {
            login: connection.login.clone(),
            repos: repos.to_vec(),
        }));
        let result = self.faults.check("list_templates").map(|()| {
            repos
                .iter()
                .map(|repo| Template {
                    repo: repo.clone(),
                    manifest: (self.role(&connection.login, repo) >= RepoRole::Read)
                        .then(|| lock(&self.state).manifests.get(repo).cloned())
                        .flatten(),
                })
                .collect()
        });
        ready(result)
    }

    fn generate_from_template(
        &self,
        connection: &GitAccess,
        template: &RepoRef,
        target: &NewRepo,
    ) -> impl Future<Output = Result<RepoRef, Error>> + Send {
        self.log.push(Call::Git(GitCall::GenerateFromTemplate {
            login: connection.login.clone(),
            template: template.clone(),
            target: target.repo.to_string(),
        }));
        let result = self.faults.check("generate_from_template").and_then(|()| {
            if self.role(&connection.login, template) < RepoRole::Read {
                return Err(Error::not_found(format!("template {template}")));
            }
            let created = target.repo.clone();
            let mut state = lock(&self.state);
            let manifest = state.manifests.get(template).cloned();
            if let Some(manifest) = manifest {
                let _previous: Option<String> = state.manifests.insert(created.clone(), manifest);
            }
            let _previous: Option<RepoRole> = state
                .roles
                .insert((connection.login.clone(), created.clone()), RepoRole::Admin);
            drop(state);
            Ok(created)
        });
        ready(result)
    }

    fn device_start(
        &self,
        _provider: GitProvider,
        host: &Host,
    ) -> impl Future<Output = Result<DeviceStart, Error>> + Send {
        self.log
            .push(Call::Git(GitCall::DeviceStart { host: host.clone() }));
        let result = self.faults.check("device_start").and_then(|()| {
            lock(&self.state)
                .devices
                .get(host)
                .map(|(start, _grants)| start.clone())
                .ok_or_else(|| no_oauth(host))
        });
        ready(result)
    }

    fn device_poll(
        &self,
        _provider: GitProvider,
        host: &Host,
        device_code: &DeviceCode,
    ) -> impl Future<Output = Result<DeviceGrant, Error>> + Send {
        self.log.push(Call::Git(GitCall::DevicePoll {
            host: host.clone(),
            device_code: device_code.clone(),
        }));
        let result = self.faults.check("device_poll").and_then(|()| {
            let mut state = lock(&self.state);
            let Some((start, grants)) = state.devices.get_mut(host) else {
                return Err(no_oauth(host));
            };
            if start.device_code != *device_code {
                return Err(Error::invalid("device_code", "unknown device code"));
            }
            let grant = if grants.len() > 1 {
                grants.pop_front()
            } else {
                grants.front().cloned()
            };
            drop(state);
            Ok(grant.unwrap_or(DeviceGrant::Pending))
        });
        ready(result)
    }
}

/// What a forge without an OAuth application answers the device flow.
fn no_oauth(host: &Host) -> Error {
    Error::invalid(
        "provider",
        format!("{host} has no OAuth application: connect with --token"),
    )
}
