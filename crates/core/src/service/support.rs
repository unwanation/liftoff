//! Steps the services share: lookups by name, the owner and
//! repository-admin checks,
//! docker exit codes, the deploy-in-flight guard, the compose override and
//! the whole-config proxy push.

use crate::compose::{ComposeFile, PublishedPort, render_override};
use crate::domain::{
    ComposeProject, DeployStatus, Domain, EnvKind, EnvName, EnvState, Environment, GitConnection,
    Host, Project, ProjectName, SecretValue, SliceName, User,
};
use crate::error::{Error, System};
use crate::ports::{
    Action, Clock as _, CommandOutput, Docker as _, Git as _, GitAccess, HookRequest, HookResult,
    Hooks as _, Ports, ProjectFile, ProjectFiles as _, Proxy as _, RepoRole, Resource,
    Secrets as _, Store as _,
};
use crate::proxy_config::ProxyConfig;

use super::authorizer::Authorizer;
use super::settings::Settings;

/// The repository (on the base host, which no forge is) of the
/// installation's own project of exposed ports
/// ([`TunnelsService::expose`](super::TunnelsService::expose)).
pub(super) const EXPOSURES_REPO: &str = "liftoff/localhost";

/// Whether `project` is the installation's own project of exposed ports,
/// which no list shows and no user creates.
#[must_use]
pub fn is_exposures(project: &Project, settings: &Settings) -> bool {
    project.git_host == settings.base && project.repo.to_string() == EXPOSURES_REPO
}

/// The project `name`, in whichever tenant it is.
///
/// A project name is unique across tenants: [`ProjectService::create`]
/// refuses one that any tenant took, since the compose project and the
/// slice of an environment (`lf_<project>_<env>`) are named after it on
/// one host.
///
/// [`ProjectService::create`]: super::ProjectService::create
pub(super) async fn project_by_name<P: Ports>(
    ports: &P,
    name: &ProjectName,
) -> Result<Project, Error> {
    ports
        .store()
        .projects()
        .await?
        .into_iter()
        .find(|project| &project.name == name)
        .ok_or_else(|| Error::not_found(format!("project {name}")))
}

/// The project `project` and its environment `env`.
pub(super) async fn environment_by_name<P: Ports>(
    ports: &P,
    project: &ProjectName,
    env: &EnvName,
) -> Result<(Project, Environment), Error> {
    let project = project_by_name(ports, project).await?;
    let environment = ports
        .store()
        .environment_by_name(project.id, env)
        .await?
        .ok_or_else(|| Error::not_found(format!("environment {env} of {}", project.name)))?;
    Ok((project, environment))
}

/// [`Error::Forbidden`] unless `actor` may do `action` on `resource`: the
/// one question [`Authorizer::check`] asks (MOD-04).
pub(super) async fn authorize<P: Ports>(
    ports: &P,
    actor: &User,
    action: Action,
    resource: &Resource,
) -> Result<(), Error> {
    Authorizer::new(ports).check(actor, action, resource).await
}

/// What `found` looked up for `action`, once `actor` may do `action` on
/// the resource `resource` names in it.
///
/// The lookup runs first because the hook names resources by id; it only
/// reads, so nothing is written or sent before the check. A lookup that
/// found nothing asks about [`Resource::Admin`] instead and keeps the
/// [`Error::NotFound`] for a caller who passes: a refused caller gets
/// [`Error::Forbidden`] either way and cannot tell an unknown name from
/// one it may not see. Either way the hook is asked exactly once.
pub(super) async fn authorized<P: Ports, T>(
    ports: &P,
    actor: &User,
    action: Action,
    found: Result<T, Error>,
    resource: impl FnOnce(&T) -> Resource,
) -> Result<T, Error> {
    match found {
        Ok(value) => {
            authorize(ports, actor, action, &resource(&value)).await?;
            Ok(value)
        }
        Err(Error::NotFound(what)) => {
            authorize(ports, actor, action, &Resource::Admin).await?;
            Err(Error::NotFound(what))
        }
        Err(other) => Err(other),
    }
}

/// The project `name`, once `actor` may do `action` on it.
pub(super) async fn authorized_project<P: Ports>(
    ports: &P,
    actor: &User,
    action: Action,
    name: &ProjectName,
) -> Result<Project, Error> {
    let found = project_by_name(ports, name).await;
    authorized(ports, actor, action, found, |project| {
        Resource::Project(project.id)
    })
    .await
}

/// The project `project` and its environment `env`, once `actor` may do
/// `action` on the environment.
pub(super) async fn authorized_environment<P: Ports>(
    ports: &P,
    actor: &User,
    action: Action,
    project: &ProjectName,
    env: &EnvName,
) -> Result<(Project, Environment), Error> {
    let found = environment_by_name(ports, project, env).await;
    authorized(ports, actor, action, found, |(_, environment)| {
        Resource::Environment(environment.id)
    })
    .await
}

/// The domain `host`, once `actor` may do `action` on it.
pub(super) async fn authorized_domain<P: Ports>(
    ports: &P,
    actor: &User,
    action: Action,
    host: &Host,
) -> Result<Domain, Error> {
    let found = ports
        .store()
        .domain_by_host(host)
        .await
        .and_then(|domain| domain.ok_or_else(|| Error::not_found(format!("domain {host}"))));
    authorized(ports, actor, action, found, |domain| {
        Resource::Domain(domain.id)
    })
    .await
}

/// `actor`'s own connection on the project's forge host, opened; the
/// message of [`Error::Forbidden`] without one says to run `lof git
/// connect`.
pub(super) async fn actor_access<P: Ports>(
    ports: &P,
    actor: &User,
    project: &Project,
) -> Result<GitAccess, Error> {
    let connection = ports
        .store()
        .git_connection(actor.id, &project.git_host)
        .await?
        .ok_or_else(|| {
            Error::forbidden(format!(
                "no connection to {}: run `lof git connect {}` first",
                project.git_host, project.git_host
            ))
        })?;
    open_connection(ports, &connection)
}

/// [`Error::Forbidden`] unless `actor`'s own connection on the project's
/// forge host administers its repository (ENV-02, ENV-04).
pub(super) async fn ensure_repo_admin<P: Ports>(
    ports: &P,
    actor: &User,
    project: &Project,
) -> Result<(), Error> {
    let access = actor_access(ports, actor, project).await?;
    let role = ports.git().repo_role(&access, &project.repo).await?;
    if role < RepoRole::Admin {
        return Err(Error::forbidden(format!(
            "{} is not an admin of {} on {}",
            access.login, project.repo, project.git_host
        )));
    }
    Ok(())
}

/// Turns a docker command that ran but exited non-zero into
/// [`Error::Upstream`] carrying its last stderr line.
pub(super) fn docker_ok(output: &CommandOutput) -> Result<(), Error> {
    if output.success() {
        return Ok(());
    }
    let detail = output.last_stderr_line().map_or_else(
        || format!("exited with {:?}", output.exit_code),
        str::to_owned,
    );
    Err(Error::upstream(System::Docker, detail))
}

/// Pushes the whole configuration of the store to the proxy.
pub(super) async fn apply_proxy<P: Ports>(ports: &P, settings: &Settings) -> Result<(), Error> {
    let config = ProxyConfig::from_store(ports.store(), settings).await?;
    ports.proxy().apply(config).await
}

/// [`Error::Conflict`] naming the deploy while one of `environment` is
/// queued or running (ENV-08): a queued deploy would undo a stop, and a
/// running one writes the same files and runs the same compose project.
pub(super) async fn ensure_no_deploy_in_flight<P: Ports>(
    ports: &P,
    environment: &Environment,
) -> Result<(), Error> {
    let in_flight = ports
        .store()
        .deploys_of_environment(environment.id)
        .await?
        .into_iter()
        .find(|deploy| matches!(deploy.status, DeployStatus::Queued | DeployStatus::Running));
    match in_flight {
        Some(deploy) => Err(Error::conflict(format!(
            "deploy #{} of {} is in progress",
            deploy.number.get(),
            environment.name
        ))),
        None => Ok(()),
    }
}

/// The compose file of the environment's current deploy, if it has one.
pub(super) async fn current_compose<P: Ports>(
    ports: &P,
    environment: &Environment,
) -> Result<Option<ComposeFile>, Error> {
    let Some(id) = environment.current_deploy_id else {
        return Ok(None);
    };
    let deploy = ports
        .store()
        .deploy(id)
        .await?
        .ok_or_else(|| Error::internal_msg(format!("current deploy {id} is missing")))?;
    ComposeFile::parse(&deploy.compose)
        .map(Some)
        .map_err(|error| Error::internal_msg(format!("stored compose of {id}: {error}")))
}

/// Regenerates `liftoff.override.yaml` from the stored bindings of
/// `environment` when it has a current deploy and, when it runs, recreates
/// it with `docker compose up` without pull (BND-02, BND-03).
pub(super) async fn apply_override<P: Ports>(
    ports: &P,
    settings: &Settings,
    project: &Project,
    environment: &Environment,
) -> Result<(), Error> {
    let Some(compose) = current_compose(ports, environment).await? else {
        return Ok(());
    };
    let published: Vec<PublishedPort> = ports
        .store()
        .bindings_of_environment(environment.id)
        .await?
        .into_iter()
        .map(|binding| PublishedPort {
            service: binding.service,
            container_port: binding.container_port,
            host_port: binding.host_port,
        })
        .collect();
    let compose_project = ComposeProject::new(&project.name, &environment.name);
    let slice = SliceName::new(&project.name, &environment.name);
    ports
        .files()
        .write(
            &compose_project,
            ProjectFile::Override,
            &render_override(&compose, &slice, &published),
        )
        .await?;
    if environment.state != EnvState::Running {
        return Ok(());
    }
    let output = ports
        .docker()
        .compose_up(&compose_project, settings.wait_timeout)
        .await?;
    docker_ok(&output)
}

/// The [`GitAccess`] of a stored connection, its credential unsealed.
///
/// A connection past its `expires_at` (an OAuth one) is
/// [`Error::Forbidden`] with the command that renews it, before the forge
/// is asked (GITF-05); a token the forge itself refuses gets the same
/// hint from its `git.*` module.
pub(super) fn open_connection<P: Ports>(
    ports: &P,
    connection: &GitConnection,
) -> Result<GitAccess, Error> {
    if connection
        .expires_at
        .is_some_and(|expires_at| expires_at <= ports.clock().now())
    {
        let host = &connection.host;
        return Err(Error::forbidden(format!(
            "the connection to {host} expired: run `lof git connect {} --host {host}` again",
            connection.provider.as_str()
        )));
    }
    Ok(GitAccess {
        provider: connection.provider,
        host: connection.host.clone(),
        login: connection.account_login.clone(),
        token: SecretValue::new(ports.secrets().decrypt(&connection.credential)?),
    })
}

/// The project whose `environment` it is.
pub(super) async fn project_of<P: Ports>(
    ports: &P,
    environment: &Environment,
) -> Result<Project, Error> {
    ports
        .store()
        .project(environment.project_id)
        .await?
        .ok_or_else(|| {
            Error::internal_msg(format!("environment {} has no project", environment.id))
        })
}

/// Why `user` may not open the private address `host` of `environment`: the admin and the
/// environment's owner (a tunnel's `owner_user_id`) may; anybody else
/// only when the `gate` hook lets them through ([`gate_refusal`]).
/// [`Access::check`](super::Access::check) and `/gate/start` decide
/// alike. `None` when they may.
pub(super) async fn private_refusal<P: Ports>(
    ports: &P,
    environment: &Environment,
    host: &Host,
    user: &User,
    request: &HookRequest,
) -> Option<String> {
    if user.is_admin() || environment.owner_user_id == Some(user.id) {
        return None;
    }
    gate_refusal(ports, host, user, request).await
}

/// Why the `gate` hook does not let `user` through to `host`: it denies,
/// or it has no opinion about someone who is not the admin (the phase-2
/// gate and the private environments decide alike). `None`
/// when it lets them through.
///
/// The hook sees `request` without its credentials: modules never see a
/// cookie or a bearer.
pub(super) async fn gate_refusal<P: Ports>(
    ports: &P,
    host: &Host,
    user: &User,
    request: &HookRequest,
) -> Option<String> {
    let shown = HookRequest {
        method: request.method.clone(),
        path: request.path.clone(),
        headers: request
            .headers
            .iter()
            .filter(|(name, _)| {
                !HookRequest::CREDENTIAL_HEADERS
                    .iter()
                    .any(|credential| name.eq_ignore_ascii_case(credential))
            })
            .cloned()
            .collect(),
    };
    let reason = match ports.hooks().gate(host, user, &shown).await {
        HookResult::Allow => return None,
        HookResult::Continue if user.is_admin() => return None,
        HookResult::Deny(reason) => reason,
        HookResult::Continue => format!("{} may not open {host}", user.login),
    };
    Some(format!(
        "signed in as {}; no access to {host}: {reason}",
        user.login
    ))
}

/// `Ok` when `actor` may act on the private address of `environment`
/// for someone: the admin, or the owner of a tunnel. An environment without an owner (compose,
/// function) leaves it to the `authorize` hook asked before. Another
/// user's tunnel is refused even when the hook allowed (VIS-06): a
/// member of the tenant opens it, but does not share it or hand out
/// links to it.
pub(super) fn owner_or_admin(
    actor: &User,
    environment: &Environment,
    action: Action,
) -> Result<(), Error> {
    match environment.owner_user_id {
        Some(owner) if owner != actor.id && !actor.is_admin() => Err(Error::forbidden(format!(
            "{} may not {action}: {} is another user's tunnel",
            actor.login, environment.name
        ))),
        _ => Ok(()),
    }
}

/// `Ok` for a compose environment; a function or a tunnel has no compose
/// project and no slice, so `what`
/// (a step only compose has) is a validation error on `kind` for them
/// until their own task gives it a branch.
pub(super) fn compose_only(environment: &Environment, what: &str) -> Result<(), Error> {
    match environment.kind {
        EnvKind::Compose => Ok(()),
        kind => Err(Error::invalid(
            "kind",
            format!(
                "{} is a {kind} environment: {what} is for compose environments",
                environment.name
            ),
        )),
    }
}
