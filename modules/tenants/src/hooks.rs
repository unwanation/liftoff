//! The hooks: `authorize`, `before_deploy`, `gate` and the data hook
//! `tenant_quotas` (see the [crate documentation](crate)).

use serde_json::{Value, json};

use crate::bindings::exports::liftoff::module::module::{HookResult, Response};
use crate::bindings::liftoff::module::host::Error;
use crate::{core, db, routes};

/// The text field `field` of `value`.
fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or_default()
}

/// Whether `user` is a member (or owner) of `tenant`.
fn member(tenant: &str, user: &str) -> Result<bool, Error> {
    Ok(db::role(tenant, user)?.is_some())
}

/// The answer about something of `tenant` (`None`: outside every tenant)
/// for the actor `user` (`login` for the reason).
fn by_membership(tenant: Option<&str>, user: &str, login: &str) -> Result<HookResult, Error> {
    let Some(tenant) = tenant else {
        // Outside every tenant: the core's rule (the admin only).
        return Ok(HookResult::Continue);
    };
    if member(tenant, user)? {
        return Ok(HookResult::Allow);
    }
    let slug = db::tenant_by_id(tenant)?.map_or_else(|| tenant.to_owned(), |found| found.slug);
    Ok(HookResult::Deny(format!(
        "{login} is not a member of the tenant {slug}"
    )))
}

/// The actions anyone may ask about the installation as a whole: the
/// lists the core then narrows item by item, the commands of modules,
/// one's own forge connections and the template catalogue read through
/// them, and one's own artifacts (publishing one, listing one's own; the
/// core keeps `ls --all` to the admin).
const OPEN_ACTIONS: [&str; 9] = [
    "project.list",
    "domain.list",
    "module.list",
    "git.list",
    "git.connect",
    "git.disconnect",
    "git.templates",
    "artifact.publish",
    "artifact.list",
];

/// `authorize(actor, action, resource)`.
pub(crate) fn authorize(payload: &Value) -> Result<HookResult, Error> {
    let actor = payload.get("actor").unwrap_or(&Value::Null);
    if text(actor, "role") == "admin" {
        return Ok(HookResult::Allow);
    }
    let (user, login) = (text(actor, "id"), text(actor, "login"));
    let action = text(payload, "action");
    let resource = payload.get("resource").unwrap_or(&Value::Null);
    let id = text(resource, "id");
    match text(resource, "type") {
        "admin" if OPEN_ACTIONS.contains(&action) => Ok(HookResult::Allow),
        // Artifacts belong to a user, not a tenant: anyone passes here and
        // the core lets the owner (or the admin) through only (ART-11).
        "artifact" => Ok(HookResult::Allow),
        "tenant" => by_membership(Some(id), user, login),
        kind @ ("project" | "environment" | "domain") => {
            let tenant = core::tenant_of(kind, id)?;
            by_membership(tenant.as_deref(), user, login)
        }
        "secret" => {
            let scope = resource.get("scope").unwrap_or(&Value::Null);
            let scope_id = text(scope, "id");
            let tenant = match text(scope, "type") {
                "tenant" => Some(scope_id.to_owned()),
                kind @ ("project" | "environment") => core::tenant_of(kind, scope_id)?,
                _ => return Ok(HookResult::Continue),
            };
            by_membership(tenant.as_deref(), user, login)
        }
        // The installation as a whole, and anything else: the core's
        // rule (the admin only).
        _ => Ok(HookResult::Continue),
    }
}

/// `before_deploy`: an environment above its tenant's limits is not
/// deployed (TEN-05: the tenant was shrunk, or the admin raised the
/// environment).
pub(crate) fn before_deploy(payload: &Value) -> Result<HookResult, Error> {
    let environment = payload.get("environment").unwrap_or(&Value::Null);
    let Some(tenant) = core::tenant_of("project", text(environment, "project"))? else {
        return Ok(HookResult::Continue);
    };
    let Some(tenant) = db::tenant_by_id(&tenant)? else {
        return Ok(HookResult::Continue);
    };
    let limits = environment.get("limits").unwrap_or(&Value::Null);
    let used = |field: &str| limits.get(field).and_then(Value::as_u64).unwrap_or(0);
    let cpus = used("cpus_hundredths");
    let memory = used("memory_mb");
    let mut over = Vec::new();
    if let Some(max) = tenant.env_cpus
        && cpus > u64::from(max)
    {
        over.push(format!(
            "{} CPUs above the tenant's {}",
            routes::cpus_text(cpus),
            routes::cpus_text(u64::from(max))
        ));
    }
    if let Some(max) = tenant.env_memory_mb
        && memory > u64::from(max)
    {
        over.push(format!("{memory} MiB above the tenant's {max} MiB"));
    }
    if over.is_empty() {
        return Ok(HookResult::Continue);
    }
    Ok(HookResult::Deny(format!(
        "environment {} exceeds the limits of the tenant {}: {}",
        text(environment, "name"),
        tenant.slug,
        over.join(", ")
    )))
}

/// `gate(host, request)`: the admin and the members of the host's tenant
/// pass (TEN-07); a member removed is refused at the next request
/// (TEN-08).
pub(crate) fn gate(payload: &Value) -> Result<HookResult, Error> {
    let actor = payload.get("actor").unwrap_or(&Value::Null);
    if text(actor, "role") == "admin" {
        return Ok(HookResult::Allow);
    }
    let tenant = core::tenant_of("host", text(payload, "host"))?;
    match by_membership(tenant.as_deref(), text(actor, "id"), text(actor, "login"))? {
        HookResult::Continue => Ok(HookResult::Deny(format!(
            "{} is in no tenant",
            text(payload, "host")
        ))),
        decided => Ok(decided),
    }
}

/// `POST /.hook/tenant_quotas` `{"tenant"}`: the tenant's quotas, `404`
/// for a tenant the module does not know.
pub(crate) fn tenant_quotas(body: &[u8]) -> Response {
    let tenant = serde_json::from_slice::<Value>(body).ok().and_then(|body| {
        body.get("tenant")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let found = match tenant.map(|tenant| db::tenant_by_id(&tenant)) {
        Some(Ok(Some(found))) => found,
        Some(Err(error)) => return routes::failure(&error),
        _ => return routes::problem(404, "no such tenant"),
    };
    routes::json(
        200,
        &json!({
            "max_projects": found.max_projects,
            "env_cpus_hundredths": found.env_cpus,
            "env_memory_mb": found.env_memory_mb,
        }),
    )
}

/// `POST /.hook/project_tenant` `{"actor":{"id","login","role"},"tenant"}`:
/// the tenant of a new project of the actor: the tenant of the
/// slug `tenant` (`404` for an unknown one), else the actor's only tenant;
/// `400` naming the slugs when they are in several, `403` when they are
/// in none (the admin in none gets `default`, when it exists). Membership
/// is not checked here: the core asks `authorize` about the tenant next.
pub(crate) fn project_tenant(body: &[u8]) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(body) else {
        return routes::problem(400, "the body must be {\"actor\", \"tenant\"}");
    };
    let actor = body.get("actor").unwrap_or(&Value::Null);
    let chosen = match body.get("tenant").and_then(Value::as_str) {
        Some(slug) => match db::tenant_by_slug(slug) {
            Ok(Some(tenant)) => tenant,
            Ok(None) => return routes::problem(404, &format!("no tenant {slug}")),
            Err(error) => return routes::failure(&error),
        },
        None => match only_tenant(actor) {
            Ok(tenant) => tenant,
            Err(refused) => return refused,
        },
    };
    routes::json(200, &json!({ "tenant": chosen.id }))
}

/// The one tenant of `actor`, or the refusal.
fn only_tenant(actor: &Value) -> Result<db::Tenant, Response> {
    let held = db::tenants_of(text(actor, "id")).map_err(|error| routes::failure(&error))?;
    let mut tenants: Vec<db::Tenant> = held.into_iter().map(|(tenant, _role)| tenant).collect();
    if tenants.len() > 1 {
        let slugs: Vec<&str> = tenants.iter().map(|tenant| tenant.slug.as_str()).collect();
        return Err(routes::problem(
            400,
            &format!(
                "{} is in several tenants ({}); name one with --tenant",
                text(actor, "login"),
                slugs.join(", ")
            ),
        ));
    }
    if let Some(tenant) = tenants.pop() {
        return Ok(tenant);
    }
    if text(actor, "role") == "admin"
        && let Ok(Some(default)) = db::tenant_by_slug(crate::migrate::DEFAULT)
    {
        return Ok(default);
    }
    Err(routes::problem(
        403,
        &format!(
            "{} is in no tenant; an owner adds you with lof tenant members add",
            text(actor, "login")
        ),
    ))
}
