//! The module's `core-call`s: the core's `tenancy.*` and
//! `environments.set_protected`, which only `tenants` may call (they ask
//! no `authorize`: the module decides).

use serde::Deserialize;
use serde_json::{Value, json};

use crate::bindings::liftoff::module::host::{self, Error};

/// A user as the core answers it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct User {
    pub(crate) id: String,
    pub(crate) login: String,
    pub(crate) role: String,
}

impl User {
    /// Whether the user is the installation's admin.
    pub(crate) fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

fn call(service: &str, method: &str, args: &Value) -> Result<Value, Error> {
    let answer = host::core_call(service, method, &args.to_string())?;
    serde_json::from_str(&answer).map_err(|why| Error {
        code: "internal".to_owned(),
        message: format!("core-call {service}.{method} answered no JSON: {why}"),
    })
}

fn parsed<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|why| Error {
        code: "internal".to_owned(),
        message: format!("unexpected core answer: {why}"),
    })
}

/// Every user, by login.
pub(crate) fn users() -> Result<Vec<User>, Error> {
    parsed(call("tenancy", "users", &json!({}))?)
}

/// The user of id `id`, if there is one.
pub(crate) fn user_by_id(id: &str) -> Result<Option<User>, Error> {
    Ok(users()?.into_iter().find(|user| user.id == id))
}

/// The user `login`.
pub(crate) fn user(login: &str) -> Result<User, Error> {
    parsed(call("tenancy", "user", &json!({ "login": login }))?)
}

/// The tenant of the record `kind` (`project`, `environment`, `domain`,
/// `host`) `id`; `None` outside every tenant.
pub(crate) fn tenant_of(kind: &str, id: &str) -> Result<Option<String>, Error> {
    let answer = call("tenancy", "tenant_of", &json!({ "type": kind, "id": id }))?;
    Ok(answer
        .get("tenant")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// How many projects `tenant` holds.
pub(crate) fn project_count(tenant: &str) -> Result<u64, Error> {
    let answer = call("tenancy", "projects", &json!({ "tenant": tenant }))?;
    Ok(answer.get("count").and_then(Value::as_u64).unwrap_or(0))
}

/// The tenant of the environment `env` of `project`.
pub(crate) fn environment_tenant(project: &str, env: &str) -> Result<Option<String>, Error> {
    let answer = call(
        "tenancy",
        "environment",
        &json!({ "project": project, "env": env }),
    )?;
    Ok(answer
        .get("tenant")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// Puts `projects` and `domains` in `tenant`.
pub(crate) fn assign(tenant: &str, projects: &[String], domains: &[String]) -> Result<(), Error> {
    call(
        "tenancy",
        "assign",
        &json!({ "tenant": tenant, "projects": projects, "domains": domains }),
    )
    .map(drop)
}

/// Puts every project and domain outside a tenant in `tenant`; how many
/// projects and domains it changed.
pub(crate) fn adopt(tenant: &str) -> Result<(u64, u64), Error> {
    let answer = call("tenancy", "adopt", &json!({ "tenant": tenant }))?;
    let count = |field: &str| answer.get(field).and_then(Value::as_u64).unwrap_or(0);
    Ok((count("projects"), count("domains")))
}

/// Puts the environment `env` of `project` behind the gate or takes it
/// from there.
pub(crate) fn set_protected(project: &str, env: &str, protected: bool) -> Result<(), Error> {
    call(
        "environments",
        "set_protected",
        &json!({ "project": project, "env": env, "protected": protected }),
    )
    .map(drop)
}
