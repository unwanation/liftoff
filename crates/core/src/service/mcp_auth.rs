//! [`McpAuthService`]: the OAuth resource server of MCP over HTTP.
//!
//! Both questions need the module `mcp` installed and enabled, else
//! [`Error::ModuleRequired`] naming `mcp` (424):
//!
//! - [`McpAuthService::resource_metadata`] is the document of `GET
//!   /.well-known/oauth-protected-resource` (RFC 9728): the `resource`
//!   the `mcp` module keeps ([`Hooks::mcp_resource`](crate::ports::Hooks::mcp_resource)),
//!   as `authorization_servers` the issuer the admin configured for
//!   `oidc`, and as `scopes_supported` the scopes `oidc` asks for (its
//!   `scopes`, else its defaults) with `offline_access`: an MCP client
//!   asks the provider for exactly these, and a provider such as Zitadel
//!   refuses an authorization request without a scope;
//! - [`McpAuthService::verify_oauth`] takes the bearer of a request to
//!   `/mcp`: the `oidc` module verifies it as an access token for one of
//!   the `mcp` module's audiences ([`Hooks::verify_token`](crate::ports::Hooks::verify_token));
//!   the core accepts only the issuer configured for `oidc`, creates or
//!   refreshes the user of `(oidc:<issuer>, sub)` as a login does, and
//!   opens (or reuses, for the same token) a session of kind `mcp` that
//!   ends with the token. A refused or missing token is
//!   [`Error::Unauthenticated`] (401).

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use time::Duration;

use super::auth::{AuthService, ProviderLogin};
use crate::domain::{
    ConfigValue, ModuleName, OidcIssuer, SecretBody, Session, SessionId, SessionKind,
    SessionSecret, Timestamp, User,
};
use crate::error::{Error, ValidationError};
use crate::ports::{
    Clock as _, Hooks as _, ModuleRuntime as _, OAuthIdentity, Ports, Rng as _, Secrets, Store as _,
};

/// The name of the module that serves `/mcp`.
pub const MCP_MODULE: &str = "mcp";

/// The protected-resource metadata of `/mcp` (RFC 9728).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedResource {
    /// `resource`: the URL of `/mcp` the `mcp` module is configured with.
    pub resource: String,
    /// `authorization_servers`: the issuer of `oidc`.
    pub authorization_servers: Vec<String>,
    /// `scopes_supported`: the scopes of `oidc` with `offline_access`.
    pub scopes_supported: Vec<String>,
}

/// The `mcp` session of a verified access token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpGrant {
    /// The user of the token.
    pub user: User,
    /// The secret of the `mcp` session: the bearer the transport calls the
    /// API with as that user. Never logged.
    pub token: SessionSecret,
    /// When the session ends: the token's `exp`.
    pub expires_at: Timestamp,
    /// How long it lasts from now (by the core's clock).
    pub valid_for: Duration,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct McpAuthService<P> {
    ports: Arc<P>,
    loopback_issuers: bool,
}

impl<P> Clone for McpAuthService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            loopback_issuers: self.loopback_issuers,
        }
    }
}

impl<P: Ports> McpAuthService<P> {
    /// A service over `ports`.
    #[must_use]
    pub const fn new(ports: Arc<P>) -> Self {
        Self {
            ports,
            loopback_issuers: false,
        }
    }

    /// The service, accepting as the token's issuer also a plain-HTTP
    /// loopback one (feature `test-support`: the fake provider of tests,
    /// as [`CoreBridge::allowing_loopback_issuers`](crate::module::CoreBridge::allowing_loopback_issuers)).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn allowing_loopback_issuers(mut self) -> Self {
        self.loopback_issuers = true;
        self
    }

    /// The document of `GET /.well-known/oauth-protected-resource`.
    ///
    /// # Errors
    ///
    /// [`Error::ModuleRequired`] without the module `mcp` (or without
    /// `oidc`, which it requires); [`Error::Unavailable`] when a module
    /// could not answer.
    pub async fn resource_metadata(&self) -> Result<ProtectedResource, Error> {
        let (resource, _audiences) = self.resource().await?;
        let oidc = self.oidc_config().await?;
        Ok(ProtectedResource {
            resource,
            authorization_servers: vec![configured_issuer(&oidc)?],
            scopes_supported: configured_scopes(&oidc),
        })
    }

    /// The `mcp` session of the access token `bearer` (`None` when the
    /// request carried none; see the [module documentation](self)).
    ///
    /// # Errors
    ///
    /// [`Error::ModuleRequired`] without the module `mcp` (checked
    /// first, so a client without a token learns that `/mcp` is off);
    /// [`Error::Unauthenticated`] without a token, for a token nobody
    /// verifies or `oidc` refuses, of another issuer or expired;
    /// [`Error::Conflict`] when another user has the login; whatever the
    /// store reports.
    pub async fn verify_oauth(&self, bearer: Option<&str>) -> Result<McpGrant, Error> {
        let (_resource, audiences) = self.resource().await?;
        let token = bearer
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Error::unauthenticated("no access token"))?;
        let identity = self
            .ports
            .hooks()
            .verify_token(token, &audiences)
            .await?
            .ok_or_else(|| Error::unauthenticated("no module verifies access tokens"))?;
        let issuer = self.oidc_issuer().await?;
        if identity.issuer != issuer.as_str() {
            tracing::warn!(issuer = %identity.issuer, configured = %issuer, "an access token of another issuer");
            return Err(Error::unauthenticated("the token's issuer is not oidc's"));
        }
        let now = self.ports.clock().now();
        if identity.expires_at <= now {
            return Err(Error::unauthenticated("the access token expired"));
        }
        let secret = session_secret(self.ports.secrets(), token)?;
        let store = self.ports.store();
        match self.reused(&secret, &identity.subject, now).await? {
            Reuse::Live(grant) => return Ok(grant),
            Reuse::Foreign => {
                tracing::warn!("an access token names another account than its mcp session");
                return Err(Error::unauthenticated(
                    "the token names another account than its session",
                ));
            }
            Reuse::Expired(id) => store.delete_session(id).await?,
            Reuse::None => {}
        }
        let subject = identity.subject.clone();
        let expires_at = identity.expires_at;
        let user = AuthService::new(Arc::clone(&self.ports))
            .provider_user(provider_login(identity, issuer)?)
            .await?;
        self.delete_expired_sessions(&user, now).await?;
        let session = Session {
            id: SessionId::new(self.ports.rng().ulid()),
            user_id: user.id,
            token_hash: secret.hash(),
            name: SESSION_NAME.to_owned(),
            kind: SessionKind::Mcp,
            expires_at,
            last_used_at: now,
            created_at: now,
        };
        if let Err(error) = store.insert_session(&session).await {
            // A request of the same token opened it meanwhile.
            return match self.reused(&secret, &subject, now).await? {
                Reuse::Live(grant) => Ok(grant),
                Reuse::Foreign | Reuse::Expired(_) | Reuse::None => Err(error),
            };
        }
        tracing::info!(login = %user.login, session = %session.id, "opened an mcp session for an access token");
        Ok(McpGrant {
            user,
            token: secret,
            expires_at,
            valid_for: until(now, expires_at),
        })
    }

    /// What the store keeps for `secret`: the live `mcp` session of the
    /// account `subject`, a live session of someone else, an expired
    /// one, or nothing.
    async fn reused(
        &self,
        secret: &SessionSecret,
        subject: &str,
        now: Timestamp,
    ) -> Result<Reuse, Error> {
        let store = self.ports.store();
        let Some(session) = store.session_by_hash(&secret.hash()).await? else {
            return Ok(Reuse::None);
        };
        if session.expires_at <= now {
            return Ok(Reuse::Expired(session.id));
        }
        if session.kind != SessionKind::Mcp {
            return Ok(Reuse::Foreign);
        }
        let Some(user) = store.user(session.user_id).await? else {
            return Ok(Reuse::Expired(session.id));
        };
        if user.subject.as_deref() != Some(subject) {
            return Ok(Reuse::Foreign);
        }
        Ok(Reuse::Live(McpGrant {
            user,
            token: secret.clone(),
            expires_at: session.expires_at,
            valid_for: until(now, session.expires_at),
        }))
    }

    /// Deletes the `mcp` sessions of `user` that ended by `now`: each
    /// refreshed token opens a session, and the old ones would pile up.
    async fn delete_expired_sessions(&self, user: &User, now: Timestamp) -> Result<(), Error> {
        let store = self.ports.store();
        for session in store.sessions_of_user(user.id).await? {
            if session.kind == SessionKind::Mcp && session.expires_at <= now {
                store.delete_session(session.id).await?;
            }
        }
        Ok(())
    }

    /// The `resource` and audiences of the module `mcp`, which must be
    /// installed and enabled.
    async fn resource(&self) -> Result<(String, Vec<String>), Error> {
        let mcp = ModuleName::new(MCP_MODULE)?;
        let enabled = self
            .ports
            .modules()
            .registry()
            .status(&mcp)
            .is_some_and(|status| status.enabled);
        if !enabled {
            return Err(Error::module_required(MCP_MODULE));
        }
        let resource = self
            .ports
            .hooks()
            .mcp_resource()
            .await?
            .ok_or_else(|| Error::module_required(MCP_MODULE))?;
        Ok((resource.resource, resource.audiences))
    }

    /// The configuration of the installed `oidc`.
    async fn oidc_config(&self) -> Result<BTreeMap<String, ConfigValue>, Error> {
        let oidc = ModuleName::new(OIDC_MODULE)?;
        let row = self
            .ports
            .store()
            .module(&oidc)
            .await?
            .ok_or_else(|| Error::module_required(OIDC_MODULE))?;
        Ok(row.config)
    }

    /// The `issuer` the admin configured for the installed `oidc`, as
    /// configured.
    async fn configured_issuer(&self) -> Result<String, Error> {
        configured_issuer(&self.oidc_config().await?)
    }

    /// [`McpAuthService::configured_issuer`] as the issuer of users.
    async fn oidc_issuer(&self) -> Result<OidcIssuer, Error> {
        let issuer = self.configured_issuer().await?;
        self.issuer(&issuer).map_err(|invalid| {
            tracing::error!(%invalid, "the issuer configured for oidc is no issuer");
            Error::unavailable("the issuer of oidc is not usable")
        })
    }

    /// `raw` as an issuer.
    fn issuer(&self, raw: &str) -> Result<OidcIssuer, ValidationError> {
        #[cfg(any(test, feature = "test-support"))]
        if self.loopback_issuers {
            return OidcIssuer::loopback(raw);
        }
        OidcIssuer::new(raw)
    }
}

/// The name of the module whose issuer the tokens are of.
const OIDC_MODULE: &str = "oidc";

/// The scopes `oidc` asks for when its `scopes` is not set (the module's
/// own default: `openid`, `profile`, `email` and Zitadel's roles claim).
const DEFAULT_OIDC_SCOPES: [&str; 4] = [
    "openid",
    "profile",
    "email",
    "urn:zitadel:iam:org:project:roles",
];

/// The scope an MCP client needs on top: a refresh token, so its session
/// outlives one access token.
const OFFLINE_ACCESS: &str = "offline_access";

/// The `issuer` of `oidc`'s configuration `config`.
fn configured_issuer(config: &BTreeMap<String, ConfigValue>) -> Result<String, Error> {
    match config.get("issuer") {
        Some(ConfigValue::Plain(Value::String(issuer))) => Ok(issuer.clone()),
        _ => Err(Error::module_required(OIDC_MODULE)),
    }
}

/// The `scopes` of `oidc`'s configuration `config` (a JSON array of
/// strings, or words separated by spaces or commas, as the module reads
/// them), else [`DEFAULT_OIDC_SCOPES`], with [`OFFLINE_ACCESS`] once.
fn configured_scopes(config: &BTreeMap<String, ConfigValue>) -> Vec<String> {
    let mut scopes: Vec<String> = match config.get("scopes") {
        Some(ConfigValue::Plain(Value::Array(items))) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(ConfigValue::Plain(Value::String(words))) => words
            .split([' ', ','])
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    if scopes.is_empty() {
        scopes = DEFAULT_OIDC_SCOPES.map(str::to_owned).to_vec();
    }
    if !scopes.iter().any(|scope| scope == OFFLINE_ACCESS) {
        scopes.push(OFFLINE_ACCESS.to_owned());
    }
    scopes
}

/// The name of every `mcp` session.
const SESSION_NAME: &str = "mcp";

/// The domain separation of [`session_secret`].
const SESSION_DERIVATION: &[u8] = b"liftoff mcp session v1\0";

/// What the store keeps for the secret of an access token.
enum Reuse {
    /// The live `mcp` session of the token's account.
    Live(McpGrant),
    /// A live session of another account (or of another kind).
    Foreign,
    /// An ended session, or one whose user is gone: deleted, then opened
    /// anew.
    Expired(SessionId),
    /// Nothing.
    None,
}

/// The secret of the `mcp` session of the access token `token`, derived
/// from it under the installation's key ([`Secrets::mac`]): the session
/// is found again for the same token (by the hash of this secret, as
/// every session) with nothing kept in memory, and the token alone does
/// not name it, so whoever holds the token has `/mcp` but not a bearer of
/// the REST API. 40 base62 characters from two MACs.
fn session_secret(secrets: &impl Secrets, token: &str) -> Result<SessionSecret, Error> {
    /// Characters taken from each of the two blocks.
    const PER_BLOCK: usize = 20;
    const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut body = String::with_capacity(SecretBody::LEN);
    for block in [0_u8, 1] {
        let mut message = Vec::with_capacity(
            SESSION_DERIVATION
                .len()
                .saturating_add(1)
                .saturating_add(token.len()),
        );
        message.extend_from_slice(SESSION_DERIVATION);
        message.push(block);
        message.extend_from_slice(token.as_bytes());
        let digest = secrets.mac(&message)?;
        for byte in digest.iter().take(PER_BLOCK) {
            let index = usize::from(byte.checked_rem(62).unwrap_or(0));
            body.push(char::from(BASE62.get(index).copied().unwrap_or(b'0')));
        }
    }
    Ok(SessionSecret::from_body(&SecretBody::new(&body)?))
}

/// How long from `now` to `then`, in whole seconds (`exp` has no
/// finer grain).
const fn until(now: Timestamp, then: Timestamp) -> Duration {
    Duration::seconds(then.unix_timestamp().saturating_sub(now.unix_timestamp()))
}

/// What `identity` says, as a login of `issuer`.
fn provider_login(identity: OAuthIdentity, issuer: OidcIssuer) -> Result<ProviderLogin, Error> {
    if identity.subject.is_empty() || identity.subject.len() > MAX_SUBJECT_BYTES {
        return Err(Error::unauthenticated(
            "the access token has no usable subject",
        ));
    }
    Ok(ProviderLogin {
        issuer,
        subject: identity.subject,
        login: identity.login,
        email: identity.email,
        role: identity.role,
    })
}

/// The longest `sub` kept (as `auth.create_session`).
const MAX_SUBJECT_BYTES: usize = 255;
