use crate::domain::{
    DeviceCode, GitProvider, Host, ImageTag, Login, RepoRef, SecretKey, SecretValue,
};
use crate::error::Error;

/// An opened git connection: what a [`Git`] call acts as. Built by a
/// service from a stored [`GitConnection`](crate::domain::GitConnection)
/// whose credential it unsealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitAccess {
    /// The forge.
    pub provider: GitProvider,
    /// The forge host, e.g. `git.example.com`.
    pub host: Host,
    /// The account on the forge.
    pub login: Login,
    /// The plaintext credential.
    pub token: SecretValue,
}

/// What an account may do on a repository, in increasing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RepoRole {
    /// Nothing (the repository is invisible).
    None,
    /// Read.
    Read,
    /// Push.
    Write,
    /// Admin: may manage CI secrets and pin environments.
    Admin,
}

/// A container image in the forge's registry: `<host>/<owner>/<name>:<tag>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PackageRef {
    /// User or organisation.
    pub owner: String,
    /// Package name.
    pub name: String,
    /// The tag.
    pub tag: ImageTag,
}

/// A template repository as a forge reads it (the catalogue of `lof create`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Template {
    /// `owner/name` of the template repository.
    pub repo: RepoRef,
    /// Its `liftoff.toml` on the default branch, as text; `None` when it
    /// has none (the catalogue skips it with a warning, TPL-04).
    pub manifest: Option<String>,
}

/// The repository [`Git::generate_from_template`] creates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NewRepo {
    /// Its `owner/name`, validated by the adapter that took the input.
    pub repo: RepoRef,
    /// Whether it is private (`lof create`'s default).
    pub private: bool,
}

/// An OAuth device flow a forge started (RFC 8628 3.2; `lof git connect github` when the module has an OAuth application).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStart {
    /// What the flow is polled with ([`Git::device_poll`]).
    pub device_code: DeviceCode,
    /// The code the person enters at the forge.
    pub user_code: String,
    /// The page the person opens, e.g. `https://github.com/login/device`.
    pub verification_uri: String,
    /// Seconds to wait between two polls.
    pub interval: u64,
    /// Seconds the device code stays valid.
    pub expires_in: u64,
}

/// One poll of a device flow ([`Git::device_poll`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceGrant {
    /// The person has not confirmed yet.
    Pending,
    /// The person has not confirmed yet, and the forge asks to poll less
    /// often (RFC 8628 3.5 `slow_down`): from now on wait `interval`
    /// seconds between two polls, or 5 seconds more than so far when the
    /// forge did not say.
    SlowDown {
        /// The interval the forge asks for, if it said.
        interval: Option<u64>,
    },
    /// Confirmed: the access token of the person's account.
    Granted {
        /// The access token.
        token: SecretValue,
        /// Seconds the token stays valid, when the forge says (GitLab's
        /// OAuth tokens: two hours); `None` for one without a lifetime.
        expires_in: Option<u64>,
    },
}

/// A git forge, always acting as a user's own connection.
///
/// Forgejo implemented it in the core; from the
/// `git.*` modules serve it through
/// [`ModuleGit`](crate::module::ModuleGit), which picks the module by the
/// connection's host.
///
/// # Errors
///
/// API failures are [`Error::Upstream`]; a revoked or rejected credential
/// is [`Error::Forbidden`]; a host no forge module serves is
/// [`Error::Validation`], a forge whose module is missing or disabled
/// [`Error::ModuleRequired`].
pub trait Git: Send + Sync {
    /// The forge `host` is: the provider whose `git.<provider>` module
    /// serves it. A project's CI dialect is a fact about its forge host,
    /// not about who asks.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on `host` when no `git.*` module serves it,
    /// or modules of two providers do; [`Error::ModuleRequired`] when the
    /// one serving it is disabled.
    fn provider_of(&self, host: &Host) -> impl Future<Output = Result<GitProvider, Error>> + Send;
    /// The account `token` logs in as on `host` (`GET /user`): how
    /// `POST /git/connections` checks a token before storing it.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when the forge rejects the token.
    fn account(
        &self,
        provider: GitProvider,
        host: &Host,
        token: &SecretValue,
    ) -> impl Future<Output = Result<Login, Error>> + Send;
    /// The role of `connection`'s account on `repo`.
    fn repo_role(
        &self,
        connection: &GitAccess,
        repo: &RepoRef,
    ) -> impl Future<Output = Result<RepoRole, Error>> + Send;
    /// Whether `connection` can read `image` in the forge's registry.
    fn image_exists(
        &self,
        connection: &GitAccess,
        image: &PackageRef,
    ) -> impl Future<Output = Result<bool, Error>> + Send;
    /// Sets the CI secret `key` of `repo`; [`Error::Forbidden`] without
    /// [`RepoRole::Admin`].
    fn put_ci_secret(
        &self,
        connection: &GitAccess,
        repo: &RepoRef,
        key: &SecretKey,
        value: &SecretValue,
    ) -> impl Future<Output = Result<(), Error>> + Send;
    /// The `liftoff.toml` of each of `repos` as `connection` reads it, in
    /// the order given (the template catalogue).
    fn list_templates(
        &self,
        connection: &GitAccess,
        repos: &[RepoRef],
    ) -> impl Future<Output = Result<Vec<Template>, Error>> + Send;
    /// Generates `target` from the template repository `template` as
    /// `connection`; the repository created.
    fn generate_from_template(
        &self,
        connection: &GitAccess,
        template: &RepoRef,
        target: &NewRepo,
    ) -> impl Future<Output = Result<RepoRef, Error>> + Send;
    /// Starts the OAuth device flow of the forge on `host`.
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] with a hint to connect with a token when the
    /// forge's module has no OAuth application (GIT-01).
    fn device_start(
        &self,
        provider: GitProvider,
        host: &Host,
    ) -> impl Future<Output = Result<DeviceStart, Error>> + Send;
    /// Polls the device flow `device_code` of [`Git::device_start`] once.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] when the person denied it, [`Error::Gone`]
    /// when the code expired.
    fn device_poll(
        &self,
        provider: GitProvider,
        host: &Host,
        device_code: &DeviceCode,
    ) -> impl Future<Output = Result<DeviceGrant, Error>> + Send;
}
