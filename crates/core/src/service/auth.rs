//! `AuthService` (test cases LOGN-01..LOGN-05).
//!
//! Login and sessions of the single-user mode:
//!
//! - a single-use [`SetupSecret`] (printed by `liftoffd setup`, minted
//!   again by `lof admin token`) is stored as a hash in the settings and
//!   traded once for a `cli` session (LOGN-01, LOGN-02);
//! - `cli` sessions live 30 days and every authenticated use moves the
//!   expiry to 30 days after it (LOGN-03);
//! - `logout` deletes the session it was called with (LOGN-04);
//! - [`Hooks::authenticate`](crate::ports::Hooks::authenticate) is asked
//!   first and may name the actor (the `oidc` module); only when
//!   it has no opinion is the bearer session checked (LOGN-05);
//! - a login module (`oidc`) hands over a person
//!   its provider vouched for ([`ProviderLogin`]):
//!   [`AuthService::open_session`] creates or refreshes the user of that
//!   `(source, subject)` and opens a session the core keeps like any
//!   other (OIDC-01…03, OIDC-07).

use std::sync::Arc;

use time::Duration;

use super::Authorizer;
use crate::domain::{
    Bearer, Login, ModuleName, OidcIssuer, Session, SessionId, SessionKind, SessionSecret,
    SetupSecret, SetupToken, User, UserId, UserRole, UserSource,
};
use crate::error::Error;
use crate::ports::{
    Action, AuthResult, Clock, HookRequest, Hooks as _, ModuleRuntime as _, Ports, Resource, Rng,
    Store,
};

#[cfg(test)]
mod tests;

/// Who a request is (what [`AuthService::authenticate`] found).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Principal {
    /// A person.
    User {
        /// The user.
        user: User,
        /// The session the request carried, as prolonged by this request;
        /// `None` when a hook named the actor (LOGN-05).
        session: Option<Session>,
    },
}

impl Principal {
    /// The person behind the request.
    #[must_use]
    pub const fn user(&self) -> &User {
        match self {
            Self::User { user, .. } => user,
        }
    }

    /// The session the request carried, if any.
    #[must_use]
    pub const fn session(&self) -> Option<&Session> {
        match self {
            Self::User { session, .. } => session.as_ref(),
        }
    }
}

/// A finished login: the person and the session secret, shown once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginGrant {
    /// Who logged in.
    pub user: User,
    /// The `cli` session secret; only its hash is stored.
    pub token: SessionSecret,
}

/// A person an OIDC provider vouched for, as a login module hands it
/// over: the provider, the account and what its ID token said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLogin {
    /// The provider: users of it are `oidc:<issuer>`.
    pub issuer: OidcIssuer,
    /// The account at the provider (`sub`).
    pub subject: String,
    /// The login (`preferred_username`).
    pub login: Login,
    /// The verified e-mail address, empty when the provider did not
    /// verify one.
    pub email: String,
    /// The role the module read from the provider's roles claim.
    pub role: UserRole,
}

/// The login of the admin `liftoffd setup` creates (one `local` row in single-user mode).
pub const SETUP_ADMIN_LOGIN: &str = "admin";

/// `liftoffd setup`: the `local` admin and its new setup token.
///
/// The admin is created as [`SETUP_ADMIN_LOGIN`] when the store has no
/// `local` admin; a second run keeps the one there is and replaces its
/// token.
///
/// It takes the ports one by one: setup runs before the daemon and has
/// no docker, proxy or forge of its own to bundle into [`Ports`].
///
/// # Errors
///
/// Whatever the store reports.
pub async fn bootstrap_admin<S: Store, C: Clock, R: Rng>(
    store: &S,
    clock: &C,
    rng: &R,
) -> Result<(User, SetupSecret), Error> {
    let existing = store
        .users()
        .await?
        .into_iter()
        .find(|user| user.is_admin() && user.source == UserSource::Local);
    let admin = if let Some(admin) = existing {
        admin
    } else {
        let admin = User {
            id: UserId::new(rng.ulid()),
            login: Login::new(SETUP_ADMIN_LOGIN)?,
            email: String::new(),
            role: UserRole::Admin,
            source: UserSource::Local,
            subject: None,
            created_at: clock.now(),
        };
        store.insert_user(&admin).await?;
        tracing::info!(login = %admin.login, "created the local admin");
        admin
    };
    let secret = mint_setup_token(store, clock, rng, &admin).await?;
    Ok((admin, secret))
}

/// Stores a new live setup token of `user` and returns its secret.
async fn mint_setup_token<S: Store, C: Clock, R: Rng>(
    store: &S,
    clock: &C,
    rng: &R,
    user: &User,
) -> Result<SetupSecret, Error> {
    let secret = SetupSecret::from_body(&rng.token_secret());
    let token = SetupToken {
        token_hash: secret.hash(),
        user_id: user.id,
        created_at: clock.now(),
    };
    store.put_setup_token(&token).await?;
    tracing::info!(login = %user.login, "issued a setup token");
    Ok(secret)
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct AuthService<P> {
    ports: Arc<P>,
}

impl<P> Clone for AuthService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
        }
    }
}

impl<P: Ports> AuthService<P> {
    /// A service over `ports`.
    #[must_use]
    pub const fn new(ports: Arc<P>) -> Self {
        Self { ports }
    }

    /// How long a `cli` session lives after its last use.
    pub const CLI_SESSION_TTL: Duration = Duration::days(30);

    /// How long a `browser` session lives.
    pub const BROWSER_SESSION_TTL: Duration = Duration::hours(24);

    /// A login module's finished login (`core-call auth.create_session`):
    /// the user of `login`'s issuer and subject is created, or its login,
    /// e-mail and role are replaced by what the provider said now
    /// (OIDC-01…03), and a new session of `kind` named `name` is opened
    /// for it beside any it has (`cli` 30 days, prolonged on use;
    /// `browser` 24 hours).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] for a `mcp` session (opened by the MCP transport); [`Error::Conflict`] when another user has the
    /// login; whatever the store reports.
    pub async fn open_session(
        &self,
        login: ProviderLogin,
        name: &str,
        kind: SessionKind,
    ) -> Result<LoginGrant, Error> {
        let ttl = match kind {
            SessionKind::Cli => Self::CLI_SESSION_TTL,
            SessionKind::Browser => Self::BROWSER_SESSION_TTL,
            SessionKind::Mcp => {
                return Err(Error::invalid(
                    "kind",
                    "a login module opens cli or browser sessions",
                ));
            }
        };
        let user = self.provider_user(login).await?;
        let store = self.ports.store();
        let now = self.ports.clock().now();
        let secret = SessionSecret::from_body(&self.ports.rng().token_secret());
        let session = Session {
            id: SessionId::new(self.ports.rng().ulid()),
            user_id: user.id,
            token_hash: secret.hash(),
            name: name.to_owned(),
            kind,
            expires_at: now.saturating_add(ttl),
            last_used_at: now,
            created_at: now,
        };
        store.insert_session(&session).await?;
        tracing::info!(login = %user.login, session = %session.id, %kind, "opened a session for a provider login");
        Ok(LoginGrant {
            user,
            token: secret,
        })
    }

    /// The user of `login`'s issuer and subject, created, or with its
    /// login, e-mail and role replaced by what the provider said now
    /// (OIDC-01…03); shared by [`AuthService::open_session`] and the
    /// `mcp` sessions of [`McpAuthService`](super::McpAuthService).
    ///
    /// # Errors
    ///
    /// [`Error::Conflict`] when another user has the login; whatever the
    /// store reports.
    pub(crate) async fn provider_user(&self, login: ProviderLogin) -> Result<User, Error> {
        let store = self.ports.store();
        let source = UserSource::Oidc(login.issuer);
        let now = self.ports.clock().now();
        let user = if let Some(known) = store.user_by_subject(&source, &login.subject).await? {
            let user = User {
                login: login.login,
                email: login.email,
                role: login.role,
                ..known
            };
            store.update_user(&user).await?;
            user
        } else {
            let user = User {
                id: UserId::new(self.ports.rng().ulid()),
                login: login.login,
                email: login.email,
                role: login.role,
                source,
                subject: Some(login.subject),
                created_at: now,
            };
            store.insert_user(&user).await?;
            tracing::info!(login = %user.login, source = %user.source, "created a user of a provider");
            user
        };
        Ok(user)
    }

    /// Mints the live single-use login token of `user`, replacing any
    /// earlier one (`liftoffd setup`; no session exists yet, so nothing is
    /// checked).
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn issue_setup_token(&self, user: &User) -> Result<SetupSecret, Error> {
        mint_setup_token(
            self.ports.store(),
            self.ports.clock(),
            self.ports.rng(),
            user,
        )
        .await
    }

    /// `lof admin token`: mints a new single-use login token of `actor`
    /// for another machine (LOGN-02).
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` is an admin; whatever the store
    /// reports.
    pub async fn create_admin_token(&self, actor: &User) -> Result<SetupSecret, Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::AdminToken, &Resource::Admin)
            .await?;
        self.issue_setup_token(actor).await
    }

    /// `POST /auth/token`: trades the live setup token for a `cli` session
    /// named `name`; the setup token is gone afterwards (LOGN-01).
    ///
    /// # Errors
    ///
    /// [`Error::Unauthenticated`] for a token that is not the live one
    /// (unknown or already exchanged); whatever the store reports.
    pub async fn exchange(&self, token: &SetupSecret, name: &str) -> Result<LoginGrant, Error> {
        let store = self.ports.store();
        let hash = token.hash();
        let setup = store.setup_token(&hash).await?.ok_or_else(rejected)?;
        let user = store.user(setup.user_id).await?.ok_or_else(rejected)?;
        let secret = SessionSecret::from_body(&self.ports.rng().token_secret());
        let now = self.ports.clock().now();
        let session = Session {
            id: SessionId::new(self.ports.rng().ulid()),
            user_id: user.id,
            token_hash: secret.hash(),
            name: name.to_owned(),
            kind: SessionKind::Cli,
            expires_at: now.saturating_add(Self::CLI_SESSION_TTL),
            last_used_at: now,
            created_at: now,
        };
        // One transaction: a failed session write leaves the token live, so
        // the owner is never locked out by it (LOGN-01).
        store
            .exchange_setup_token(&hash, &session)
            .await?
            .ok_or_else(rejected)?;
        tracing::info!(login = %user.login, session = %session.id, "opened a cli session");
        Ok(LoginGrant {
            user,
            token: secret,
        })
    }

    /// Who `request` is: the actor the `authenticate` hook names, else the
    /// user of the `bearer` session, which is prolonged (LOGN-03, LOGN-05).
    ///
    /// # Errors
    ///
    /// [`Error::Unauthenticated`] when the hook denies, or when it has no
    /// opinion and the bearer is missing, unknown, expired or not a
    /// session, or is an `mcp` session while the module `mcp` is not
    /// installed and enabled (MCPH-06).
    pub async fn authenticate(
        &self,
        request: &HookRequest,
        bearer: Option<&Bearer>,
    ) -> Result<Principal, Error> {
        let store = self.ports.store();
        match self.ports.hooks().authenticate(request).await {
            AuthResult::Actor(id) => {
                let user = store.user(id).await?.ok_or_else(|| {
                    tracing::error!(user = %id, "the authenticate hook named an unknown user");
                    rejected()
                })?;
                return Ok(Principal::User {
                    user,
                    session: None,
                });
            }
            AuthResult::Deny(why) => return Err(Error::unauthenticated(why)),
            AuthResult::Continue => {}
        }
        let Some(Bearer::Session(secret)) = bearer else {
            return Err(Error::unauthenticated("a session is required"));
        };
        let mut session = store
            .session_by_hash(&secret.hash())
            .await?
            .ok_or_else(rejected)?;
        let now = self.ports.clock().now();
        if session.expires_at <= now {
            tracing::warn!(session = %session.id, "rejected an expired session");
            return Err(Error::unauthenticated("session expired"));
        }
        if session.kind == SessionKind::Mcp && !self.mcp_enabled() {
            // The transport of `/mcp` opened it; it goes with the module.
            tracing::warn!(session = %session.id, "rejected an mcp session: the module mcp is off");
            return Err(Error::unauthenticated("the module mcp is not enabled"));
        }
        let user = store.user(session.user_id).await?.ok_or_else(rejected)?;
        session.last_used_at = now;
        if session.kind == SessionKind::Cli {
            session.expires_at = now.saturating_add(Self::CLI_SESSION_TTL);
        }
        store.update_session(&session).await?;
        Ok(Principal::User {
            user,
            session: Some(session),
        })
    }

    /// Whether the module `mcp` is installed and enabled.
    fn mcp_enabled(&self) -> bool {
        ModuleName::new(super::mcp_auth::MCP_MODULE).is_ok_and(|mcp| {
            self.ports
                .modules()
                .registry()
                .status(&mcp)
                .is_some_and(|status| status.enabled)
        })
    }

    /// `lof admin users`: every user, by login.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` is an admin; whatever the store
    /// reports.
    pub async fn users(&self, actor: &User) -> Result<Vec<User>, Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::AdminUsers, &Resource::Admin)
            .await?;
        self.ports.store().users().await
    }

    /// `lof admin sessions revoke <login>`: ends every session of `login`;
    /// returns how many there were.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` is an admin (checked before the
    /// lookup); [`Error::NotFound`] naming the login for an unknown user;
    /// whatever the store reports.
    pub async fn revoke_sessions(&self, actor: &User, login: &Login) -> Result<usize, Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::AdminRevokeSessions, &Resource::Admin)
            .await?;
        let store = self.ports.store();
        let user = store
            .user_by_login(login)
            .await?
            .ok_or_else(|| Error::not_found(format!("user {login}")))?;
        let revoked = store.delete_sessions_of_user(user.id).await?;
        tracing::info!(%login, revoked, by = %actor.login, "revoked the sessions of a user");
        Ok(revoked)
    }

    /// `DELETE /auth/session`: ends the session `principal` carried
    /// (LOGN-04).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] when the request carried no session (a hook
    /// named the actor); whatever the store reports.
    pub async fn logout(&self, principal: &Principal) -> Result<(), Error> {
        let Some(session) = principal.session() else {
            return Err(Error::invalid(
                "session",
                "the request carried no session to end",
            ));
        };
        self.ports.store().delete_session(session.id).await
    }
}

/// The refusal for a token that does not match a live credential. Says
/// nothing about why and never repeats the token.
fn rejected() -> Error {
    tracing::warn!("rejected an unknown or revoked token");
    Error::unauthenticated("unknown or revoked token")
}
