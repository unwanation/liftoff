//! `core-call(service, method, args)`: a module calls a core
//! service as the actor of its current call (MOD-11).
//!
//! [`CoreBridge`] holds the table of methods a module may call
//! ([`CoreBridge::METHODS`]); anything else is refused before a service
//! runs. An allowed method runs the core service with the call's actor,
//! so it asks `authorize` like every other operation of that service
//! (the single [`Authorizer`](crate::service::Authorizer)). A call without
//! an actor (a tick, a migration) is refused. Arguments and answers are
//! JSON.
//!
//! One method runs without an actor: `auth.create_session`, the end of a
//! login a module ran on its public routes (`oidc`), where nobody is logged in yet. A manifest is the module's own
//! claim, so it is not enough on its own: the caller must declare
//! `replaces: ["login"]` ([`Caller::replaces`]), be installed (the
//! install, enable and reconfiguration refuse a second module that
//! replaces `login`), be the only module of the registry that replaces
//! `login` (checked at every call), and name as `issuer` exactly the
//! `issuer` its admin configured, an `https://` one. A module can thus
//! only log in users of the provider the admin pointed it at, never
//! claim the users of another one.

use core::future::ready;
use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::domain::{
    ConfigValue, DomainId, EnvName, Environment, EnvironmentId, Host, Login, ModuleName,
    OidcIssuer, Project, ProjectId, ProjectName, SessionKind, TenantId, User, UserRole,
};
use crate::error::{Error, Problem, ValidationError};
use crate::ports::{ModuleRuntime as _, Ports, Store as _};
use crate::service::{
    AuthService, EnvironmentService, ProjectService, ProviderLogin, SecretsService, Settings,
    TenancyService, TenantOf,
};

/// Who makes a `core-call`: the module, what its manifest takes over,
/// and the actor of its call in progress.
#[derive(Debug, Clone, Copy)]
pub struct Caller<'a> {
    /// The module.
    pub module: &'a ModuleName,
    /// Its manifest's `replaces`: the core CLI commands it took over.
    pub replaces: &'a [String],
    /// The actor of the module call in progress; `None` on a public
    /// route, for a tick or a migration.
    pub actor: Option<&'a User>,
}

impl Caller<'_> {
    /// Whether the module took over `lof login`.
    #[must_use]
    pub fn logs_in(&self) -> bool {
        self.replaces.iter().any(|command| command == "login")
    }
}

/// Where the host sends a module's `core-call`.
pub trait CoreCalls: Send + Sync + 'static {
    /// Calls `service.method` with the JSON `args` for `caller`; the
    /// JSON answer.
    ///
    /// # Errors
    ///
    /// Whatever the core refuses with; the host hands it to the module
    /// as a `host.error` with the variant's code.
    fn call(
        &self,
        caller: Caller<'_>,
        service: &str,
        method: &str,
        args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send;
}

/// No core behind the host: every call answers [`Error::Unavailable`]
/// (the module host of, and hosts that need no core).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCoreCalls;

impl CoreCalls for NoCoreCalls {
    fn call(
        &self,
        _caller: Caller<'_>,
        service: &str,
        method: &str,
        _args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        ready(Err(Error::Unavailable(format!(
            "core-call {service}.{method} is not available"
        ))))
    }
}

/// The daemon's [`CoreCalls`]: the core services over `P`.
#[derive(Debug)]
pub struct CoreBridge<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
    /// Whether `auth.create_session` takes a plain-HTTP loopback issuer
    /// ([`OidcIssuer::loopback`]): only a bridge of the tests does.
    #[cfg(any(test, feature = "test-support"))]
    loopback_issuers: bool,
}

impl<P> Clone for CoreBridge<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
            #[cfg(any(test, feature = "test-support"))]
            loopback_issuers: self.loopback_issuers,
        }
    }
}

impl<P: Ports> CoreBridge<P> {
    /// The methods a module may call, as `service.method`. Each runs the
    /// core service of that name, which asks `authorize` for the actor.
    ///
    /// | Method | `args` | Answer |
    /// |---|---|---|
    /// | `projects.list` | `{}` | `[project]` |
    /// | `projects.get` | `{"project"}` | `project` |
    /// | `environments.list` | `{"project"}` | `[environment]` |
    /// | `environments.get` | `{"project","env"}` | `environment` |
    /// | `secrets.resolve` | `{"project","env"}` | `{"KEY": "value"}` |
    /// | `auth.create_session` | `{"issuer","subject","login","email","role","name","kind"}` | `{"token","user":{"id","login","email","role","source"}}` |
    /// | `system.api_url` | `{}` | `{"url"}`: `https://<api host>` of `[api] public_url` |
    /// | `environments.set_protected` | `{"project","env","protected"}` | `environment` (plus `"protected"`) |
    /// | `tenancy.users` | `{}` | `[user]` |
    /// | `tenancy.user` | `{"login"}` | `user` |
    /// | `tenancy.tenant_of` | `{"type":"project"\|"environment"\|"domain"\|"host","id"}` | `{"tenant"}` (`null` outside every tenant) |
    /// | `tenancy.projects` | `{"tenant"}` | `{"count"}` |
    /// | `tenancy.environment` | `{"project","env"}` | `{"id","project_id","tenant","protected"}` |
    /// | `tenancy.assign` | `{"tenant","projects":[id],"domains":[id]}` | `{"projects","domains"}` (how many changed) |
    /// | `tenancy.adopt` | `{"tenant"}` | `{"projects","domains"}` |
    ///
    /// `user` is `{"id","login","email","role"}`.
    ///
    /// `auth.create_session` needs no actor but a caller that
    /// [logs in](Caller::logs_in); see [`AuthService::open_session`].
    /// `system.api_url` needs no actor (the `oidc` module derives its browser callback from it).
    /// `environments.set_protected` and `tenancy.*` are the
    /// [`TenancyService`]'s, for the module named `tenants` alone, with
    /// or without an actor, and ask no `authorize`: the module decided.
    pub const METHODS: &'static [&'static str] = &[
        "projects.list",
        "projects.get",
        "environments.list",
        "environments.get",
        "secrets.resolve",
        "auth.create_session",
        "system.api_url",
        "environments.set_protected",
        "tenancy.users",
        "tenancy.user",
        "tenancy.tenant_of",
        "tenancy.projects",
        "tenancy.environment",
        "tenancy.assign",
        "tenancy.adopt",
    ];

    /// The methods of the `tenants` module alone.
    const TENANCY: &'static [&'static str] = &[
        "environments.set_protected",
        "tenancy.users",
        "tenancy.user",
        "tenancy.tenant_of",
        "tenancy.projects",
        "tenancy.environment",
        "tenancy.assign",
        "tenancy.adopt",
    ];

    /// The method that runs without an actor.
    const CREATE_SESSION: &'static str = "auth.create_session";

    /// The bridge to the services over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self {
            ports,
            settings,
            #[cfg(any(test, feature = "test-support"))]
            loopback_issuers: false,
        }
    }

    /// The bridge, accepting as `auth.create_session`'s issuer also a
    /// plain-HTTP one on loopback: the fake provider of a test, which
    /// serves no TLS (test support only; the daemon's bridge takes
    /// `https://` issuers alone).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn allowing_loopback_issuers(mut self) -> Self {
        self.loopback_issuers = true;
        self
    }

    /// `raw` as the issuer of a session.
    fn issuer(&self, raw: &str) -> Result<OidcIssuer, ValidationError> {
        #[cfg(any(test, feature = "test-support"))]
        if self.loopback_issuers {
            return OidcIssuer::loopback(raw);
        }
        OidcIssuer::new(raw)
    }

    /// `auth.create_session` for `caller`, which must log in.
    async fn create_session(&self, caller: Caller<'_>, args: &str) -> Result<Value, Error> {
        if !caller.logs_in() {
            return Err(Error::forbidden(format!(
                "core-call {} is for a module that replaces login; {} does not",
                Self::CREATE_SESSION,
                caller.module
            )));
        }
        let SessionArgs {
            issuer,
            subject,
            login,
            email,
            role,
            name,
            kind,
        } = parse(args)?;
        let issuer = self.issuer(&issuer)?;
        self.check_issuer(caller.module, &issuer).await?;
        self.check_sole_login(caller.module)?;
        if subject.is_empty() || subject.len() > MAX_SUBJECT_BYTES {
            return Err(ValidationError::new(
                "subject",
                Problem::Pattern {
                    expected: "1 to 255 bytes",
                },
            )
            .into());
        }
        let name = session_name(name)?;
        let login = ProviderLogin {
            issuer,
            subject,
            login: Login::new(&login)?,
            email,
            role: UserRole::parse(&role)?,
        };
        let kind = SessionKind::parse(&kind)?;
        let grant = AuthService::new(Arc::clone(&self.ports))
            .open_session(login, &name, kind)
            .await?;
        tracing::info!(module = %caller.module, login = %grant.user.login, "a module opened a session");
        Ok(serde_json::json!({
            "token": grant.token.expose(),
            "user": {
                "id": grant.user.id.to_string(),
                "login": grant.user.login.to_string(),
                "email": grant.user.email,
                "role": grant.user.role.to_string(),
                "source": grant.user.source.to_string(),
            },
        }))
    }

    /// Refuses `issuer` unless it is the `issuer` the admin configured
    /// for the installed `module`: the binding of `auth.create_session`
    /// to state the admin controls, not to the module's manifest.
    async fn check_issuer(&self, module: &ModuleName, issuer: &OidcIssuer) -> Result<(), Error> {
        let row = self.ports.store().module(module).await?.ok_or_else(|| {
            Error::forbidden(format!(
                "core-call {} is for an installed module; {module} is not installed",
                Self::CREATE_SESSION
            ))
        })?;
        let configured = match row.config.get("issuer") {
            Some(ConfigValue::Plain(Value::String(configured))) => Some(configured.as_str()),
            _ => None,
        };
        if configured == Some(issuer.as_str()) {
            return Ok(());
        }
        tracing::warn!(%module, %issuer, ?configured, "a module asked for a session of an issuer it is not configured with");
        Err(Error::forbidden(format!(
            "{module} may open sessions only for the issuer its configuration names"
        )))
    }

    /// Refuses while a loaded module other than `module` replaces `login`
    /// too. The install and every enable or reconfiguration refuse a
    /// second such module already (`ModuleService`); this is the check at
    /// use, so that no path into the registry (a restore, a manifest that
    /// changed with its configuration) leaves two modules minting
    /// sessions.
    fn check_sole_login(&self, module: &ModuleName) -> Result<(), Error> {
        let rival =
            self.ports
                .modules()
                .registry()
                .list()
                .into_iter()
                .find(|(name, manifest, _status)| {
                    name != module && manifest.replaces.iter().any(|command| command == "login")
                });
        match rival {
            None => Ok(()),
            Some((rival, _manifest, _status)) => {
                tracing::error!(%module, %rival, "two loaded modules replace login; neither opens a session");
                Err(Error::forbidden(format!(
                    "{module} and {rival} both replace login; remove one of them"
                )))
            }
        }
    }

    /// Runs the `tenants` module's `method` (see [`CoreBridge::METHODS`]).
    async fn tenancy(&self, caller: Caller<'_>, method: &str, args: &str) -> Result<Value, Error> {
        if caller.module.as_str() != TENANTS_MODULE {
            return Err(Error::forbidden(format!(
                "core-call {method} is for the {TENANTS_MODULE} module; {} is not it",
                caller.module
            )));
        }
        let tenancy = TenancyService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        match method {
            "environments.set_protected" => set_protected(&tenancy, args).await,
            "tenancy.users" => {
                let NoArgs {} = parse(args)?;
                let users = tenancy.users().await?;
                Ok(Value::Array(users.iter().map(user_json).collect()))
            }
            "tenancy.user" => {
                let LoginArgs { login } = parse(args)?;
                Ok(user_json(&tenancy.user(&Login::new(&login)?).await?))
            }
            "tenancy.tenant_of" => tenant_of(&tenancy, args).await,
            "tenancy.projects" => {
                let TenantArgs { tenant } = parse(args)?;
                let count = tenancy.project_count(TenantId::parse(&tenant)?).await?;
                Ok(serde_json::json!({ "count": count }))
            }
            "tenancy.environment" => {
                let (project, env) = environment_args(args)?;
                let (project, environment) = tenancy.environment(&project, &env).await?;
                Ok(serde_json::json!({
                    "id": environment.id.to_string(),
                    "project_id": project.id.to_string(),
                    "tenant": project.tenant_id.map(|id| id.to_string()),
                    "protected": environment.protected,
                }))
            }
            "tenancy.assign" => assign(&tenancy, args).await,
            "tenancy.adopt" => {
                let TenantArgs { tenant } = parse(args)?;
                let (projects, domains) = tenancy.adopt(TenantId::parse(&tenant)?).await?;
                Ok(serde_json::json!({ "projects": projects, "domains": domains }))
            }
            _ => Err(not_allowed(method)),
        }
    }

    /// Runs the allowed `method` as `actor`.
    async fn dispatch(&self, actor: &User, method: &str, args: &str) -> Result<Value, Error> {
        let projects = || ProjectService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        let environments =
            || EnvironmentService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        match method {
            "projects.list" => {
                let NoArgs {} = parse(args)?;
                let listed = projects().list(actor).await?;
                Ok(Value::Array(listed.iter().map(project_json).collect()))
            }
            "projects.get" => {
                let ProjectArgs { project } = parse(args)?;
                let project = ProjectName::new(&project)?;
                Ok(project_json(&projects().get(actor, &project).await?))
            }
            "environments.list" => {
                let ProjectArgs { project } = parse(args)?;
                let project = ProjectName::new(&project)?;
                let listed = environments().list(actor, &project).await?;
                Ok(Value::Array(
                    listed
                        .iter()
                        .map(|environment| environment_json(&project, environment))
                        .collect(),
                ))
            }
            "environments.get" => {
                let (project, env) = environment_args(args)?;
                let environment = environments().get(actor, &project, &env).await?;
                Ok(environment_json(&project, &environment))
            }
            "secrets.resolve" => {
                let (project, env) = environment_args(args)?;
                let resolved = SecretsService::new(Arc::clone(&self.ports))
                    .resolve(actor, &project, &env, None)
                    .await?;
                Ok(Value::Object(
                    resolved
                        .vars()
                        .iter()
                        .map(|(key, value)| (key.to_string(), Value::from(value.expose())))
                        .collect(),
                ))
            }
            _ => Err(not_allowed(method)),
        }
    }
}

/// `environments.set_protected`.
async fn set_protected<P: Ports>(tenancy: &TenancyService<P>, args: &str) -> Result<Value, Error> {
    let ProtectArgs {
        project,
        env,
        protected,
    } = parse(args)?;
    let project = ProjectName::new(&project)?;
    let environment = tenancy
        .set_protected(&project, &EnvName::new(&env)?, protected)
        .await?;
    let mut answer = environment_json(&project, &environment);
    if let Some(fields) = answer.as_object_mut() {
        let _previous = fields.insert("protected".to_owned(), Value::Bool(environment.protected));
    }
    Ok(answer)
}

/// `tenancy.tenant_of`.
async fn tenant_of<P: Ports>(tenancy: &TenancyService<P>, args: &str) -> Result<Value, Error> {
    let TenantOfArgs { kind, id } = parse(args)?;
    let of = match kind.as_str() {
        "project" => TenantOf::Project(ProjectId::parse(&id)?),
        "environment" => TenantOf::Environment(EnvironmentId::parse(&id)?),
        "domain" => TenantOf::Domain(DomainId::parse(&id)?),
        "host" => TenantOf::Host(Host::new(&id)?),
        _ => {
            return Err(Error::invalid(
                "type",
                "one of project, environment, domain, host",
            ));
        }
    };
    let tenant = tenancy.tenant_of(&of).await?;
    Ok(serde_json::json!({ "tenant": tenant.map(|id| id.to_string()) }))
}

/// `tenancy.assign`.
async fn assign<P: Ports>(tenancy: &TenancyService<P>, args: &str) -> Result<Value, Error> {
    let AssignArgs {
        tenant,
        projects,
        domains,
    } = parse(args)?;
    let projects = projects
        .iter()
        .map(|id| ProjectId::parse(id))
        .collect::<Result<Vec<_>, _>>()?;
    let domains = domains
        .iter()
        .map(|id| DomainId::parse(id))
        .collect::<Result<Vec<_>, _>>()?;
    let (projects, domains) = tenancy
        .assign(TenantId::parse(&tenant)?, &projects, &domains)
        .await?;
    Ok(serde_json::json!({ "projects": projects, "domains": domains }))
}

/// The module whose `core-call` reaches [`TenancyService`].
const TENANTS_MODULE: &str = "tenants";

/// `args` of `environments.set_protected`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectArgs {
    project: String,
    env: String,
    protected: bool,
}

/// `args` naming a user.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginArgs {
    login: String,
}

/// `args` naming a tenant.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantArgs {
    tenant: String,
}

/// `args` of `tenancy.tenant_of`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantOfArgs {
    #[serde(rename = "type")]
    kind: String,
    id: String,
}

/// `args` of `tenancy.assign`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignArgs {
    tenant: String,
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    domains: Vec<String>,
}

/// A user as the `tenants` module sees it.
fn user_json(user: &User) -> Value {
    serde_json::json!({
        "id": user.id.to_string(),
        "login": user.login.to_string(),
        "email": user.email,
        "role": user.role.as_str(),
    })
}

/// The refusal of a method outside [`CoreBridge::METHODS`].
fn not_allowed(method: &str) -> Error {
    Error::forbidden(format!("core-call {method} is not allowed"))
}

/// `args` of a method without arguments: `{}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "serde reads a unit struct from `null`, this one from `{}`"
)]
struct NoArgs {}

/// The longest OIDC subject a session is opened for (`OpenID` Connect Core
/// 2: at most 255 ASCII characters).
const MAX_SUBJECT_BYTES: usize = 255;

/// The session name when the module sends none (as `POST /auth/token`).
const DEFAULT_SESSION_NAME: &str = "cli";

/// The longest session name a module may give, in bytes.
const MAX_SESSION_NAME_BYTES: usize = 64;

/// The session name of `name` (the default when there is none), refused
/// when it is empty, longer than [`MAX_SESSION_NAME_BYTES`] or holds a
/// control character: it reaches the core from a public route.
fn session_name(name: Option<String>) -> Result<String, Error> {
    let Some(name) = name else {
        return Ok(DEFAULT_SESSION_NAME.to_owned());
    };
    if name.is_empty() || name.len() > MAX_SESSION_NAME_BYTES || name.chars().any(char::is_control)
    {
        return Err(ValidationError::new(
            "name",
            Problem::Pattern {
                expected: "1 to 64 bytes without control characters",
            },
        )
        .into());
    }
    Ok(name)
}

/// `args` of `auth.create_session`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionArgs {
    issuer: String,
    subject: String,
    login: String,
    #[serde(default)]
    email: String,
    role: String,
    #[serde(default)]
    name: Option<String>,
    kind: String,
}

/// `args` naming a project.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectArgs {
    project: String,
}

/// `args` naming an environment of a project.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentArgs {
    project: String,
    env: String,
}

/// The JSON `args` of a method, refused as [`Error::Validation`] of the
/// field `args`.
fn parse<T: DeserializeOwned>(args: &str) -> Result<T, Error> {
    serde_json::from_str(args).map_err(|why| Error::invalid("args", why.to_string()))
}

/// The project and environment of `{"project","env"}`, validated.
fn environment_args(args: &str) -> Result<(ProjectName, EnvName), Error> {
    let EnvironmentArgs { project, env } = parse(args)?;
    Ok((ProjectName::new(&project)?, EnvName::new(&env)?))
}

/// A time as REST has it: RFC 3339.
fn timestamp(at: crate::domain::Timestamp) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// A project as a module sees it: the fields of REST's project view
/// (`liftoff_api::ProjectView`, times in RFC 3339) but the manifest,
/// plus its tenant.
fn project_json(project: &Project) -> Value {
    serde_json::json!({
        "id": project.id.to_string(),
        "name": project.name.to_string(),
        "tenant_id": project.tenant_id.map(|id| id.to_string()),
        "git_host": project.git_host.to_string(),
        "repo": project.repo.to_string(),
        "created_by": project.created_by.to_string(),
        "created_at": timestamp(project.created_at),
    })
}

/// An environment of `project` as a module sees it: fields of REST's
/// environment view (`liftoff_api::EnvironmentView`), times in RFC 3339.
fn environment_json(project: &ProjectName, environment: &Environment) -> Value {
    serde_json::json!({
        "id": environment.id.to_string(),
        "project": project.to_string(),
        "name": environment.name.to_string(),
        "kind": environment.kind.to_string(),
        "state": environment.state.to_string(),
        "visibility": environment.visibility.to_string(),
        "branch": environment.pin.as_ref().map(|pin| pin.branch.to_string()),
        "owner_user_id": environment.owner_user_id.map(|id| id.to_string()),
        "created_at": timestamp(environment.created_at),
    })
}

impl<P: Ports> CoreCalls for CoreBridge<P> {
    fn call(
        &self,
        caller: Caller<'_>,
        service: &str,
        method: &str,
        args: &str,
    ) -> impl Future<Output = Result<String, Error>> + Send {
        let method = format!("{service}.{method}");
        async move {
            if !Self::METHODS.contains(&method.as_str()) {
                return Err(not_allowed(&method));
            }
            if method == Self::CREATE_SESSION {
                return Ok(self.create_session(caller, args).await?.to_string());
            }
            if method == "system.api_url" {
                let NoArgs {} = parse(args)?;
                return Ok(serde_json::json!({
                    "url": format!("https://{}", self.settings.proxy.api_host),
                })
                .to_string());
            }
            if Self::TENANCY.contains(&method.as_str()) {
                return Ok(self.tenancy(caller, &method, args).await?.to_string());
            }
            let actor = caller.actor.ok_or_else(|| {
                Error::unauthenticated(format!("core-call {method} needs the actor of a call"))
            })?;
            let answer = self.dispatch(actor, &method, args).await?;
            Ok(answer.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SecretScope;
    use crate::domain::UserRole;
    use crate::fakes::{FakePorts, World};
    use crate::ports::{Action, Clock as _, HookResult, Resource};

    fn bridge(world: &World) -> CoreBridge<FakePorts> {
        CoreBridge::new(Arc::clone(&world.ports), Arc::clone(&world.settings))
    }

    fn probe() -> ModuleName {
        ModuleName::new("probe").unwrap()
    }

    /// A call of the module `probe`, which replaces `replaces`, as `actor`.
    fn caller<'a>(
        module: &'a ModuleName,
        replaces: &'a [String],
        actor: Option<&'a User>,
    ) -> Caller<'a> {
        Caller {
            module,
            replaces,
            actor,
        }
    }

    async fn call_as(
        world: &World,
        actor: &User,
        service: &str,
        method: &str,
        args: &str,
    ) -> Result<String, Error> {
        world.log().clear();
        bridge(world)
            .call(caller(&probe(), &[], Some(actor)), service, method, args)
            .await
    }

    mod methods {
        use super::*;

        // TC: MOD-11 (the table holds the methods the host names)
        #[test]
        fn lists_spec_methods_when_table_is_read() {
            let table = CoreBridge::<FakePorts>::METHODS;
            assert!(
                ["projects.list", "environments.get", "secrets.resolve"]
                    .iter()
                    .all(|method| table.contains(method)),
                "{table:?}"
            );
        }
    }

    mod call {
        use super::*;

        // TC: MOD-11 (a method outside the table is an error)
        #[tokio::test]
        async fn refuses_method_when_not_in_table() {
            let world = World::new().await;
            let refused = call_as(&world, &world.owner, "store", "delete_user", "{}").await;
            assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
        }

        // TC: MOD-11 (refused before any service runs: nothing is asked)
        #[tokio::test]
        async fn asks_nothing_when_method_not_in_table() {
            let world = World::new().await;
            let _refused = call_as(&world, &world.owner, "projects", "remove", "{}").await;
            assert_eq!(world.log().authorized(), Vec::new());
        }

        // TC: MOD-11 (an allowed method passes `authorize` with the call's
        // actor: every question is about the actor, the first about the
        // list; how a list is narrowed per item is TEN-02's, see the
        // `Authorizer::visible` tests)
        #[tokio::test]
        async fn asks_authorize_with_call_actor_when_projects_listed() {
            let world = World::new().await;
            let carol = world.records.user("carol", UserRole::User).unwrap();
            world.ports.store.insert_user(&carol).await.unwrap();
            world
                .ports
                .hooks
                .answer_next("authorize", HookResult::Allow);
            let _listed = call_as(&world, &carol, "projects", "list", "{}").await;
            let asked = world.log().authorized();
            assert_eq!(
                (
                    asked.first().cloned(),
                    asked.iter().all(|(actor, _, _)| *actor == carol.id)
                ),
                (Some((carol.id, Action::ProjectList, Resource::Admin)), true),
                "{asked:?}"
            );
        }

        // TC: TEN-07 (the browser login's callback derives from `[api] public_url`; no actor needed)
        #[tokio::test]
        async fn answers_api_url_when_system_api_url_called_without_actor() {
            let world = World::new().await;
            let answer = bridge(&world)
                .call(caller(&probe(), &[], None), "system", "api_url", "{}")
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&answer).unwrap(),
                serde_json::json!({
                    "url": format!("https://{}", world.settings.proxy.api_host)
                })
            );
        }

        // TC: MOD-11
        #[tokio::test]
        async fn asks_authorize_for_environment_when_environment_got() {
            let world = World::new().await;
            let _got = call_as(
                &world,
                &world.owner,
                "environments",
                "get",
                r#"{"project":"shop","env":"prod"}"#,
            )
            .await;
            assert_eq!(
                world.log().authorized(),
                vec![(
                    world.owner.id,
                    Action::EnvironmentGet,
                    Resource::Environment(world.prod.id)
                )]
            );
        }

        // TC: MOD-11
        #[tokio::test]
        async fn asks_authorize_for_project_when_project_got() {
            let world = World::new().await;
            let _got = call_as(
                &world,
                &world.owner,
                "projects",
                "get",
                r#"{"project":"shop"}"#,
            )
            .await;
            assert_eq!(
                world.log().authorized(),
                vec![(
                    world.owner.id,
                    Action::ProjectGet,
                    Resource::Project(world.shop.id)
                )]
            );
        }

        // TC: MOD-11
        #[tokio::test]
        async fn asks_authorize_for_project_when_environments_listed() {
            let world = World::new().await;
            let _listed = call_as(
                &world,
                &world.owner,
                "environments",
                "list",
                r#"{"project":"shop"}"#,
            )
            .await;
            assert_eq!(
                world.log().authorized(),
                vec![(
                    world.owner.id,
                    Action::EnvironmentList,
                    Resource::Project(world.shop.id)
                )]
            );
        }

        // TC: MOD-11 (plaintext secrets reach a module only through the
        // actor's `secret.resolve`)
        #[tokio::test]
        async fn asks_authorize_for_environment_secrets_when_secrets_resolved() {
            let world = World::new().await;
            let _resolved = call_as(
                &world,
                &world.owner,
                "secrets",
                "resolve",
                r#"{"project":"shop","env":"prod"}"#,
            )
            .await;
            assert_eq!(
                world.log().authorized(),
                vec![(
                    world.owner.id,
                    Action::SecretResolve,
                    Resource::Secret(SecretScope::Environment(world.prod.id))
                )]
            );
        }

        // TC: MOD-11 (a user the core does not allow gets no values)
        #[tokio::test]
        async fn refuses_user_when_secrets_resolve_has_no_opinion() {
            let world = World::new().await;
            let carol = world.records.user("carol", UserRole::User).unwrap();
            world.ports.store.insert_user(&carol).await.unwrap();
            let refused = call_as(
                &world,
                &carol,
                "secrets",
                "resolve",
                r#"{"project":"shop","env":"prod"}"#,
            )
            .await;
            assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
        }

        // TC: MOD-11 (the answer maps each key to its plaintext value)
        #[tokio::test]
        async fn answers_secret_values_when_admin_resolves_them() {
            let world = World::new().await;
            let resolved = call_as(
                &world,
                &world.owner,
                "secrets",
                "resolve",
                r#"{"project":"shop","env":"prod"}"#,
            )
            .await
            .unwrap();
            let answer: Value = serde_json::from_str(&resolved).unwrap();
            assert!(
                answer
                    .as_object()
                    .is_some_and(|vars| !vars.is_empty() && vars.values().all(Value::is_string)),
                "{resolved}"
            );
        }

        // TC: MOD-11 (the answer is the service's result as JSON)
        #[tokio::test]
        async fn answers_projects_as_json_when_admin_lists_them() {
            let world = World::new().await;
            let listed = call_as(&world, &world.owner, "projects", "list", "{}")
                .await
                .unwrap();
            let rows: Value = serde_json::from_str(&listed).unwrap();
            assert_eq!(
                rows.as_array()
                    .map(|rows| rows.iter().map(|row| row["name"].clone()).collect()),
                Some(vec![serde_json::json!("shop")]),
                "{listed}"
            );
        }

        // TC: MOD-11 (times travel as REST's do: RFC 3339)
        #[tokio::test]
        async fn answers_created_at_as_rfc3339_when_project_got() {
            let world = World::new().await;
            let got = call_as(
                &world,
                &world.owner,
                "projects",
                "get",
                r#"{"project":"shop"}"#,
            )
            .await
            .unwrap();
            let project: Value = serde_json::from_str(&got).unwrap();
            assert_eq!(
                project["created_at"],
                Value::from(
                    world
                        .shop
                        .created_at
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap()
                ),
                "{got}"
            );
        }

        // TC: MOD-11 (the hook's deny reaches the module as `Forbidden`)
        #[tokio::test]
        async fn refuses_with_hook_reason_when_authorize_denies() {
            let world = World::new().await;
            world
                .ports
                .hooks
                .answer_next("authorize", HookResult::Deny("not your tenant".to_owned()));
            let refused = call_as(&world, &world.owner, "projects", "list", "{}").await;
            assert_eq!(
                refused.map_err(|error| error.to_string()),
                Err("not your tenant".to_owned())
            );
        }

        // TC: MOD-11 (without the admin role and without a module's
        // allow the core decides as for any other caller)
        #[tokio::test]
        async fn refuses_user_when_hook_has_no_opinion() {
            let world = World::new().await;
            let carol = world.records.user("carol", UserRole::User).unwrap();
            let refused = call_as(&world, &carol, "projects", "list", "{}").await;
            assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
        }

        // TC: MOD-11 (arguments are validated at the boundary)
        #[tokio::test]
        async fn rejects_args_when_they_do_not_fit_method() {
            let world = World::new().await;
            let refused = call_as(
                &world,
                &world.owner,
                "environments",
                "get",
                r#"{"project":"Shop!"}"#,
            )
            .await;
            assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
        }

        // TC: MOD-11 (a call without an actor, e.g. from a tick, is refused
        // and asks nothing)
        #[tokio::test]
        async fn refuses_call_when_it_has_no_actor() {
            let world = World::new().await;
            world.log().clear();
            let refused = bridge(&world)
                .call(caller(&probe(), &[], None), "projects", "list", "{}")
                .await;
            assert_eq!(
                (
                    matches!(refused, Err(Error::Unauthenticated(_))),
                    world.log().authorized()
                ),
                (true, Vec::new()),
                "{refused:?}"
            );
        }
    }

    mod create_session {
        use super::*;

        const ARGS: &str = r#"{"issuer":"https://id.example.com","subject":"z1","login":"bob","email":"bob@example.com","role":"user","name":"laptop","kind":"cli"}"#;

        /// `probe` installed with `issuer` as its configured issuer.
        async fn install_probe(world: &World, issuer: &str) {
            let mut config = crate::domain::ModuleConfigRecord::new();
            let _none = config.insert("issuer".to_owned(), ConfigValue::Plain(Value::from(issuer)));
            world
                .ports
                .store
                .insert_module(&crate::domain::InstalledModule {
                    name: probe(),
                    version: crate::domain::ModuleVersion::new("0.1.0").unwrap(),
                    sha256: "ab".repeat(32),
                    enabled: true,
                    config,
                    error: None,
                    installed_at: world.ports.clock.now(),
                })
                .await
                .unwrap();
        }

        /// `auth.create_session` of `probe` (installed for
        /// `https://id.example.com`), which replaces `replaces`.
        async fn create(world: &World, replaces: &[String], args: &str) -> Result<String, Error> {
            if world.ports.store.module(&probe()).await.unwrap().is_none() {
                install_probe(world, "https://id.example.com").await;
            }
            bridge(world)
                .call(
                    caller(&probe(), replaces, None),
                    "auth",
                    "create_session",
                    args,
                )
                .await
        }

        fn login() -> Vec<String> {
            vec!["login".to_owned()]
        }

        // TC: OIDC-01 (a module that replaces login opens a session
        // without an actor: nobody is logged in yet)
        #[tokio::test]
        async fn answers_token_and_user_when_caller_replaces_login() {
            let world = World::new().await;
            let answer: Value =
                serde_json::from_str(&create(&world, &login(), ARGS).await.unwrap()).unwrap();
            assert_eq!(
                (
                    answer["token"]
                        .as_str()
                        .is_some_and(|token| token.starts_with("lfs_")),
                    answer["user"]["login"].clone(),
                    answer["user"]["source"].clone()
                ),
                (
                    true,
                    serde_json::json!("bob"),
                    serde_json::json!("oidc:https://id.example.com")
                )
            );
        }

        // TC: OIDC-01 (any other module may not mint sessions)
        #[tokio::test]
        async fn refuses_when_caller_does_not_replace_login() {
            let world = World::new().await;
            let refused = create(&world, &[], ARGS).await;
            assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
        }

        // TC: OIDC-01 (nothing is created when refused)
        #[tokio::test]
        async fn creates_no_user_when_caller_does_not_replace_login() {
            let world = World::new().await;
            let _refused = create(&world, &[], ARGS).await;
            assert_eq!(world.ports.store.users().await.unwrap().len(), 1);
        }

        // TC: OIDC-01 (a module that declares `replaces: ["login"]` may
        // not claim the users of a provider its admin did not configure)
        #[tokio::test]
        async fn refuses_and_creates_no_user_when_issuer_is_not_configured_one() {
            let world = World::new().await;
            install_probe(&world, "https://other.example").await;
            let refused = create(&world, &login(), ARGS).await;
            assert_eq!(
                (
                    matches!(refused, Err(Error::Forbidden(_))),
                    world.ports.store.users().await.unwrap().len()
                ),
                (true, 1),
                "{refused:?}"
            );
        }

        // TC: OIDC-01 (a module that is not installed has no configured
        // issuer: refused)
        #[tokio::test]
        async fn refuses_when_caller_is_not_installed() {
            let world = World::new().await;
            let refused = bridge(&world)
                .call(
                    caller(&probe(), &login(), None),
                    "auth",
                    "create_session",
                    ARGS,
                )
                .await;
            assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
        }

        // TC: OIDC-01 (one module logs people in: while another loaded
        // module replaces login too, neither opens a session)
        #[tokio::test]
        async fn refuses_and_creates_no_user_when_another_module_replaces_login() {
            let world = World::new().await;
            world.ports.modules.registry().register(
                ModuleName::new("rogue").unwrap(),
                crate::module::ModuleManifest::parse(
                    r#"{"name":"rogue","version":"0.1.0","replaces":["login"]}"#,
                )
                .unwrap(),
            );
            let refused = create(&world, &login(), ARGS).await;
            assert_eq!(
                (
                    matches!(refused, Err(Error::Forbidden(_))),
                    world.ports.store.users().await.unwrap().len()
                ),
                (true, 1),
                "{refused:?}"
            );
        }

        // TC: OIDC-06 (an issuer is `https://`: the daemon's bridge
        // refuses a plain-HTTP loopback issuer even when it is the
        // configured one; only a test bridge accepts it)
        #[tokio::test]
        async fn rejects_loopback_http_issuer_unless_bridge_is_for_tests() {
            const LOOPBACK: &str = "http://127.0.0.1:8080";
            let world = World::new().await;
            install_probe(&world, LOOPBACK).await;
            let args = ARGS.replace("https://id.example.com", LOOPBACK);
            let refused = create(&world, &login(), &args).await;
            let accepted = bridge(&world)
                .allowing_loopback_issuers()
                .call(
                    caller(&probe(), &login(), None),
                    "auth",
                    "create_session",
                    &args,
                )
                .await;
            assert!(
                matches!(refused, Err(Error::Validation(_))) && accepted.is_ok(),
                "{refused:?} {accepted:?}"
            );
        }

        // TC: OIDC-04 (the session name comes from a public route: bounded
        // at the core's boundary)
        #[rstest::rstest]
        #[case::long(&"x".repeat(65))]
        #[case::empty("")]
        #[case::control("lap\ntop")]
        #[tokio::test]
        async fn rejects_session_name_when_it_is_not_short_text(#[case] name: &str) {
            let world = World::new().await;
            let args = serde_json::json!({"issuer": "https://id.example.com", "subject": "z1", "login": "bob", "role": "user", "name": name, "kind": "cli"});
            let refused = create(&world, &login(), &args.to_string()).await;
            assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
        }

        // TC: OIDC-06 (the module's claims are validated at the boundary)
        #[rstest::rstest]
        #[case::issuer(r#"{"issuer":"http://evil.example","subject":"z1","login":"bob","role":"user","kind":"cli"}"#)]
        #[case::login(r#"{"issuer":"https://id.example.com","subject":"z1","login":"Bob Smith!","role":"user","kind":"cli"}"#)]
        #[case::subject(r#"{"issuer":"https://id.example.com","subject":"","login":"bob","role":"user","kind":"cli"}"#)]
        #[case::role(r#"{"issuer":"https://id.example.com","subject":"z1","login":"bob","role":"root","kind":"cli"}"#)]
        #[case::kind(r#"{"issuer":"https://id.example.com","subject":"z1","login":"bob","role":"user","kind":"tty"}"#)]
        #[tokio::test]
        async fn rejects_args_when_a_claim_is_invalid(#[case] args: &str) {
            let world = World::new().await;
            let refused = create(&world, &login(), args).await;
            assert!(matches!(refused, Err(Error::Validation(_))), "{refused:?}");
        }
    }

    /// `environments.set_protected`: the flag
    /// `lof protect` sets, written by the `tenants` module alone.
    mod set_protected {
        use super::*;

        fn tenants() -> ModuleName {
            ModuleName::new("tenants").unwrap()
        }

        const PROTECT_PROD: &str = r#"{"project":"shop","env":"prod","protected":true}"#;

        async fn stored_flag(world: &World) -> bool {
            world
                .ports
                .store
                .environment(world.prod.id)
                .await
                .unwrap()
                .unwrap()
                .protected
        }

        // TC: TEN-06 (the core keeps the flag the tenants module sets, for
        // its member: the module decided, so the core does not ask
        // `authorize` again, which would skip the calling module and leave
        // only the admin)
        #[tokio::test]
        async fn stores_flag_when_tenants_sets_protected_for_member() {
            let world = World::new().await;
            let carol = world.add_user("carol", UserRole::User).await.unwrap();
            let set = bridge(&world)
                .call(
                    caller(&tenants(), &[], Some(&carol)),
                    "environments",
                    "set_protected",
                    PROTECT_PROD,
                )
                .await;
            assert_eq!(
                (
                    set.map(|_| ()).map_err(|error| error.to_string()),
                    stored_flag(&world).await
                ),
                (Ok(()), true)
            );
        }

        // TC: TEN-06 (only the tenants module writes `environments.protected`)
        #[tokio::test]
        async fn forbidden_naming_tenants_when_another_module_sets_protected() {
            let world = World::new().await;
            let refused = bridge(&world)
                .call(
                    caller(&probe(), &[], Some(&world.owner)),
                    "environments",
                    "set_protected",
                    PROTECT_PROD,
                )
                .await;
            assert!(
                matches!(&refused, Err(Error::Forbidden(why)) if why.contains("tenants")),
                "{refused:?}"
            );
        }

        /// Valid `args` of the `tenants`-only `method` over `world`: the
        /// ones the `tenants` module itself would send.
        fn tenancy_args(world: &World, method: &str) -> String {
            let tenant = ulid::Ulid::from_parts(9, 9).to_string();
            match method {
                "environments.set_protected" => PROTECT_PROD.to_owned(),
                "tenancy.users" => "{}".to_owned(),
                "tenancy.user" => format!(r#"{{"login":"{}"}}"#, world.owner.login),
                "tenancy.tenant_of" => format!(r#"{{"type":"project","id":"{}"}}"#, world.shop.id),
                "tenancy.environment" => r#"{"project":"shop","env":"prod"}"#.to_owned(),
                "tenancy.assign" => format!(
                    r#"{{"tenant":"{tenant}","projects":["{}"],"domains":["{}"]}}"#,
                    world.shop.id, world.domain.id
                ),
                _ => format!(r#"{{"tenant":"{tenant}"}}"#),
            }
        }

        // TC: TEN-02 (every `tenancy.*` call and `environments.set_protected`
        // is the tenants module's alone: another module, even acting for the
        // admin, is refused before anything is read or changed)
        #[rstest::rstest]
        #[tokio::test]
        async fn forbidden_naming_tenants_when_another_module_calls_tenancy(
            #[values(
                "environments.set_protected",
                "tenancy.users",
                "tenancy.user",
                "tenancy.tenant_of",
                "tenancy.projects",
                "tenancy.environment",
                "tenancy.assign",
                "tenancy.adopt"
            )]
            method: &str,
        ) {
            let world = World::new().await;
            let (interface, name) = method.split_once('.').unwrap();
            let before = (
                world.ports.store.projects().await.unwrap(),
                world.ports.store.domains().await.unwrap(),
                stored_flag(&world).await,
            );
            world.log().clear();
            let refused = bridge(&world)
                .call(
                    caller(&probe(), &[], Some(&world.owner)),
                    interface,
                    name,
                    &tenancy_args(&world, method),
                )
                .await;
            let after = (
                world.ports.store.projects().await.unwrap(),
                world.ports.store.domains().await.unwrap(),
                stored_flag(&world).await,
            );
            assert!(
                matches!(&refused, Err(Error::Forbidden(why)) if why.contains("tenants")),
                "{method}: {refused:?}"
            );
            assert_eq!(after, before, "{method} changed the store");
        }

        // TC: TEN-02 (the same args reach the core for the tenants module:
        // the refusal above is the caller's, not the args')
        #[rstest::rstest]
        #[tokio::test]
        async fn answers_when_tenants_calls_tenancy(
            #[values(
                "environments.set_protected",
                "tenancy.users",
                "tenancy.user",
                "tenancy.tenant_of",
                "tenancy.projects",
                "tenancy.environment",
                "tenancy.assign",
                "tenancy.adopt"
            )]
            method: &str,
        ) {
            let world = World::new().await;
            let (interface, name) = method.split_once('.').unwrap();
            let answered = bridge(&world)
                .call(
                    caller(&tenants(), &[], Some(&world.owner)),
                    interface,
                    name,
                    &tenancy_args(&world, method),
                )
                .await;
            assert!(answered.is_ok(), "{method}: {answered:?}");
        }

        // TC: TEN-06 (`protect` re-applies the whole proxy configuration,
        // the bound domain gets `forward_auth`)
        #[tokio::test]
        async fn applies_proxy_config_when_tenants_sets_protected() {
            let world = World::new().await;
            world
                .deployed(&world.prod, crate::domain::EnvState::Running)
                .await
                .unwrap();
            let _set = bridge(&world)
                .call(
                    caller(&tenants(), &[], Some(&world.owner)),
                    "environments",
                    "set_protected",
                    PROTECT_PROD,
                )
                .await;
            let gate = world
                .ports
                .proxy
                .applied()
                .and_then(|config| config.sites.into_iter().next())
                .and_then(|site| site.gate);
            assert_eq!(gate, Some(World::GATE_BIND));
        }
    }
}
