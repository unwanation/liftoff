//! The routes of the commands (see the [crate documentation](crate)).

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::bindings::exports::liftoff::module::module::{Request, Response};
use crate::bindings::liftoff::module::host::Error;
use crate::core::User;
use crate::db::{Quota, Tenant};
use crate::{core, db, ulid};

/// The route of the command `name`: its words joined by `/`.
pub(crate) fn path_of(name: &str) -> String {
    format!("/{}", name.replace(' ', "/"))
}

/// A response with a JSON body.
pub(crate) fn json(status: u16, body: &Value) -> Response {
    Response {
        status,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("cache-control".to_owned(), "no-store".to_owned()),
        ],
        body: body.to_string().into_bytes(),
    }
}

/// A problem document (RFC 9457) of `status` with `detail`.
pub(crate) fn problem(status: u16, detail: &str) -> Response {
    let title = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    Response {
        status,
        headers: vec![(
            "content-type".to_owned(),
            "application/problem+json".to_owned(),
        )],
        body: json!({"type": "about:blank", "title": title, "status": status, "detail": detail})
            .to_string()
            .into_bytes(),
    }
}

/// The core's refusal as the route's answer, with its status.
pub(crate) fn failure(error: &Error) -> Response {
    let status = match error.code.as_str() {
        "validation" => 400,
        "unauthenticated" => 401,
        "forbidden" => 403,
        "not_found" => 404,
        "conflict" => 409,
        "upstream" => 502,
        "unavailable" => 503,
        _ => 500,
    };
    if status == 500 {
        return problem(500, "internal error");
    }
    problem(status, &error.message)
}

/// `1.5` for 150 hundredths of a CPU.
pub(crate) fn cpus_text(hundredths: u64) -> String {
    let whole = hundredths.checked_div(100).unwrap_or(0);
    let frac = hundredths.checked_rem(100).unwrap_or(0);
    if frac == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{frac:02}")
    }
}

/// `0.5` as hundredths of a CPU, within the core's 0.01..=64.
fn parse_cpus(raw: &str) -> Option<u32> {
    let (whole, frac) = raw.split_once('.').unwrap_or((raw, ""));
    let digits = |text: &str| text.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() || whole.len() > 2 || frac.len() > 2 || !digits(whole) || !digits(frac) {
        return None;
    }
    let whole: u32 = whole.parse().ok()?;
    let frac: u32 = format!("{frac:0<2}").parse().ok()?;
    let hundredths = whole.checked_mul(100)?.checked_add(frac)?;
    (1..=6_400).contains(&hundredths).then_some(hundredths)
}

/// Whether `slug` is a tenant slug (as project names:
/// `^[a-z0-9][a-z0-9-]{0,31}$`).
fn is_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    slug.len() <= 32
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// What a route answers, or the response that ends it early.
type Answer = Result<Response, Response>;

/// Turns a core or store failure into the early response.
fn or_fail<T>(result: Result<T, Error>) -> Result<T, Response> {
    result.map_err(|error| failure(&error))
}

/// The body as `T`, or `400`.
fn body<T: DeserializeOwned>(req: &Request) -> Result<T, Response> {
    let raw: &[u8] = if req.body.is_empty() {
        b"{}"
    } else {
        &req.body
    };
    serde_json::from_slice(raw).map_err(|why| problem(400, &format!("the body: {why}")))
}

/// The tenant of slug `slug`, or `404`.
fn tenant(slug: &str) -> Result<Tenant, Response> {
    or_fail(db::tenant_by_slug(slug))?
        .ok_or_else(|| problem(404, &format!("tenant {slug} not found")))
}

/// `403` unless `actor` is the admin or an owner of `tenant`.
fn ensure_owner(actor: &User, tenant: &Tenant) -> Result<(), Response> {
    if actor.is_admin() || or_fail(db::role(&tenant.id, &actor.id))?.as_deref() == Some("owner") {
        return Ok(());
    }
    Err(problem(
        403,
        &format!(
            "{} is not an owner of the tenant {}",
            actor.login, tenant.slug
        ),
    ))
}

/// A tenant as the routes answer it, with the actor's role.
fn tenant_json(tenant: &Tenant, role: Option<&str>) -> Value {
    json!({
        "id": tenant.id,
        "slug": tenant.slug,
        "name": tenant.name,
        "max_projects": tenant.max_projects,
        "env_cpus": tenant.env_cpus.map(|cpus| cpus_text(u64::from(cpus))),
        "env_memory_mb": tenant.env_memory_mb,
        "role": role,
    })
}

/// A request to a route.
pub(crate) fn handle(req: &Request) -> Response {
    let Some(actor_id) = req.actor.as_deref() else {
        return problem(401, "a session is required");
    };
    let actor = match core::user_by_id(actor_id) {
        Ok(Some(actor)) => actor,
        Ok(None) => return problem(401, "a session is required"),
        Err(error) => return failure(&error),
    };
    if req.method != "POST" {
        return problem(404, &format!("no route {} {}", req.method, req.path));
    }
    let answered = match req.path.as_str() {
        "/tenant/create" => create(&actor, req),
        "/tenant/ls" => list(&actor),
        "/tenant/rm" => remove(&actor, req),
        "/tenant/members/add" => add_member(&actor, req),
        "/tenant/members/rm" => remove_member(&actor, req),
        "/tenant/members/ls" => members(&actor, req),
        "/protect" => protect(&actor, req, true),
        "/unprotect" => protect(&actor, req, false),
        "/admin/tenant/set" => set_quotas(&actor, req),
        _ => Err(problem(404, &format!("no route POST {}", req.path))),
    };
    answered.unwrap_or_else(|early| early)
}

/// `{"tenant"}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TenantBody {
    tenant: String,
}

/// The quotas of a new tenant until the admin changes them (the v1
/// defaults): three projects, environments of one CPU and 1024 MiB. A
/// tenant anyone may create must not lift the `max_projects` the admin
/// set on their others (TEN-04).
const NEW_TENANT_QUOTAS: [(Quota, u32); 3] = [
    (Quota::MaxProjects, 3),
    (Quota::EnvCpus, 100),
    (Quota::EnvMemoryMb, 1024),
];

/// `lof tenant create`: the tenant gets [`NEW_TENANT_QUOTAS`].
fn create(actor: &User, req: &Request) -> Answer {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Create {
        slug: String,
        #[serde(default)]
        name: Option<String>,
    }
    let Create { slug, name } = body(req)?;
    if !is_slug(&slug) {
        return Err(problem(
            400,
            &format!("slug {slug:?}: expected ^[a-z0-9][a-z0-9-]{{0,31}}$"),
        ));
    }
    let name = name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| slug.clone());
    if or_fail(db::tenant_by_slug(&slug))?.is_some() {
        return Err(problem(409, &format!("tenant {slug} already exists")));
    }
    let id = ulid::new().ok_or_else(|| problem(500, "internal error"))?;
    or_fail(db::insert_tenant(&id, &slug, &name, &actor.id))?;
    for (quota, value) in NEW_TENANT_QUOTAS {
        or_fail(db::set_quota(&id, quota, Some(value)))?;
    }
    or_fail(db::put_member(&id, &actor.id, "owner"))?;
    let created = tenant(&slug)?;
    Ok(json(201, &tenant_json(&created, Some("owner"))))
}

/// `lof tenant ls`.
fn list(actor: &User) -> Answer {
    let listed: Vec<Value> = if actor.is_admin() {
        let roles: Vec<(Tenant, String)> = or_fail(db::tenants_of(&actor.id))?;
        or_fail(db::tenants())?
            .iter()
            .map(|tenant| {
                let role = roles
                    .iter()
                    .find(|(mine, _)| mine.id == tenant.id)
                    .map(|(_, role)| role.as_str());
                tenant_json(tenant, role)
            })
            .collect()
    } else {
        or_fail(db::tenants_of(&actor.id))?
            .iter()
            .map(|(tenant, role)| tenant_json(tenant, Some(role)))
            .collect()
    };
    Ok(json(200, &Value::Array(listed)))
}

/// `lof tenant rm` (TEN-10).
fn remove(actor: &User, req: &Request) -> Answer {
    let TenantBody { tenant: slug } = body(req)?;
    let found = tenant(&slug)?;
    ensure_owner(actor, &found)?;
    let projects = or_fail(core::project_count(&found.id))?;
    if projects > 0 {
        return Err(problem(
            409,
            &format!("tenant {slug} still holds {projects} projects; remove them first"),
        ));
    }
    or_fail(db::delete_tenant(&found.id))?;
    Ok(json(200, &json!({"removed": slug})))
}

/// `lof tenant members add` (TEN-03).
fn add_member(actor: &User, req: &Request) -> Answer {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Add {
        tenant: String,
        login: String,
        #[serde(default)]
        owner: bool,
    }
    let Add {
        tenant: slug,
        login,
        owner,
    } = body(req)?;
    let found = tenant(&slug)?;
    ensure_owner(actor, &found)?;
    let user = or_fail(core::user(&login))?;
    let role = if owner { "owner" } else { "member" };
    or_fail(db::put_member(&found.id, &user.id, role))?;
    Ok(json(200, &json!({"login": user.login, "role": role})))
}

/// `lof tenant members rm` (TEN-03): the last owner stays.
fn remove_member(actor: &User, req: &Request) -> Answer {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Remove {
        tenant: String,
        login: String,
    }
    let Remove {
        tenant: slug,
        login,
    } = body(req)?;
    let found = tenant(&slug)?;
    ensure_owner(actor, &found)?;
    let user = or_fail(core::user(&login))?;
    let members = or_fail(db::members(&found.id))?;
    let owners = members
        .iter()
        .filter(|member| member.role == "owner")
        .count();
    let is_owner = members
        .iter()
        .any(|member| member.user_id == user.id && member.role == "owner");
    if is_owner && owners == 1 {
        return Err(problem(
            409,
            &format!("{login} is the last owner of the tenant {slug}"),
        ));
    }
    or_fail(db::delete_member(&found.id, &user.id))?;
    Ok(json(200, &json!({"removed": login})))
}

/// `lof tenant members ls`, by login.
fn members(actor: &User, req: &Request) -> Answer {
    let TenantBody { tenant: slug } = body(req)?;
    let found = tenant(&slug)?;
    if !actor.is_admin() && or_fail(db::role(&found.id, &actor.id))?.is_none() {
        return Err(problem(
            403,
            &format!("{} is not a member of the tenant {slug}", actor.login),
        ));
    }
    let users = or_fail(core::users())?;
    let mut listed: Vec<(String, String)> = or_fail(db::members(&found.id))?
        .into_iter()
        .filter_map(|member| {
            users
                .iter()
                .find(|user| user.id == member.user_id)
                .map(|user| (user.login.clone(), member.role))
        })
        .collect();
    listed.sort();
    let listed: Vec<Value> = listed
        .into_iter()
        .map(|(login, role)| json!({"login": login, "role": role}))
        .collect();
    Ok(json(200, &Value::Array(listed)))
}

/// `lof protect` and `lof unprotect` (TEN-06, TEN-09).
fn protect(actor: &User, req: &Request, protected: bool) -> Answer {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Target {
        project: String,
        env: String,
    }
    let Target { project, env } = body(req)?;
    let tenant = or_fail(core::environment_tenant(&project, &env))?;
    let member = match &tenant {
        Some(tenant) => or_fail(db::role(tenant, &actor.id))?.is_some(),
        None => false,
    };
    if !actor.is_admin() && !member {
        return Err(problem(
            403,
            &format!("{} is not a member of the tenant of {project}", actor.login),
        ));
    }
    or_fail(core::set_protected(&project, &env, protected))?;
    Ok(json(
        200,
        &json!({"project": project, "env": env, "protected": protected}),
    ))
}

/// `lof admin tenant set` (TEN-04, TEN-05): admins only.
fn set_quotas(actor: &User, req: &Request) -> Answer {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Quotas {
        tenant: String,
        #[serde(default)]
        max_projects: Option<u32>,
        #[serde(default)]
        env_cpus: Option<String>,
        #[serde(default)]
        env_memory_mb: Option<u32>,
    }
    if !actor.is_admin() {
        return Err(problem(
            403,
            &format!("{} may not set the quotas of a tenant", actor.login),
        ));
    }
    let Quotas {
        tenant: slug,
        max_projects,
        env_cpus,
        env_memory_mb,
    } = body(req)?;
    let found = tenant(&slug)?;
    let cpus = match env_cpus {
        Some(raw) => Some(parse_cpus(&raw).ok_or_else(|| {
            problem(
                400,
                &format!("env_cpus {raw:?}: expected 0.01 to 64 with at most two decimals"),
            )
        })?),
        None => None,
    };
    if let Some(memory) = env_memory_mb
        && !(32..=262_144).contains(&memory)
    {
        return Err(problem(
            400,
            &format!("env_memory_mb {memory}: expected 32 to 262144"),
        ));
    }
    let changes = [
        (Quota::MaxProjects, max_projects),
        (Quota::EnvCpus, cpus),
        (Quota::EnvMemoryMb, env_memory_mb),
    ];
    for (quota, value) in changes {
        if value.is_some() {
            or_fail(db::set_quota(&found.id, quota, value))?;
        }
    }
    let updated = tenant(&slug)?;
    Ok(json(200, &tenant_json(&updated, None)))
}
