//! MOD-04, MOD-05: every operation of a core service asks the single
//! [`Authorizer`] exactly once, and without modules only the admin passes.

use std::sync::Arc;

use super::*;
use crate::domain::{
    ArtifactId, Branch, ContainerPort, DeployStatus, EnvKind, EnvName, EnvState, GitProvider, Host,
    ImageTag, Label, LinkTarget, Login, ModuleName, NewProject, ProjectName, RepoRef, ScopeRef,
    SecretKey, SecretScope, SecretValue, ServiceName, UserRole, VersionNumber,
};
use crate::fakes::FakeRelease;
use crate::fakes::World;
use crate::ports::{HookResult, RepoRole, Store as _};
use crate::service::{
    ArtifactContent, BindRequest, EnvironmentChange, GitService, InstallRequest, LogsRequest,
    ModuleChange, Reconciler, SecretsService, TemplateCatalog,
};

/// Arranges, as the admin, what the operation of `action` needs to run to
/// its end; the host [`Action::BindingBind`] binds.
async fn arrange(world: &World, action: Action) -> Result<Host, Error> {
    let free = Host::new("blog.apps.test")?;
    match action {
        Action::EnvironmentStop | Action::DeployRedeploy | Action::DeployRequest => {
            world.deployed(&world.prod, EnvState::Running).await?;
        }
        Action::EnvironmentStart => {
            world.deployed(&world.prod, EnvState::Stopped).await?;
        }
        Action::ProjectRemove => {
            for environment in [&world.prod, &world.qa] {
                world
                    .environments()
                    .remove(&world.owner, &world.shop.name, &environment.name, true)
                    .await?;
            }
            world
                .domains()
                .remove(&world.owner, &world.domain.host)
                .await?;
        }
        Action::DomainAddExternal => world
            .ports
            .resolver
            .set_a(&Host::new("shop.example.org")?, vec![World::PUBLIC_IP]),
        Action::DomainRemove => world.ports.store.delete_binding(world.binding.id).await?,
        Action::ModuleList
        | Action::ModuleAvailable
        | Action::ModuleInstall
        | Action::ModuleUpdate
        | Action::ModuleRemove => {
            world.ports.registry.publish(FakeRelease::probe())?;
            if matches!(action, Action::ModuleUpdate | Action::ModuleRemove) {
                world
                    .modules()
                    .install(&world.owner, probe_install()?)
                    .await?;
            }
        }
        Action::BindingBind => {
            world.add_domain(&world.shop, free.as_str()).await?;
            world.deployed(&world.prod, EnvState::Running).await?;
        }
        _ => {}
    }
    Ok(free)
}

/// The install of the fake `probe` with its required `greeting`.
fn probe_install() -> Result<InstallRequest, Error> {
    let mut config = serde_json::Map::new();
    let _none = config.insert("greeting".to_owned(), serde_json::Value::from("hi"));
    Ok(InstallRequest {
        name: ModuleName::new("probe")?,
        version: None,
        config,
    })
}

/// Whether `result` is the end of the whole operation of `action`:
/// success, or for [`Action::DomainSetProxied`] the `dns.cloudflare`
/// module it needs.
fn ran_to_end(action: Action, result: &Result<(), Error>) -> bool {
    match action {
        Action::DomainSetProxied => matches!(result, Err(Error::ModuleRequired(_))),
        _ => result.is_ok(),
    }
}

/// An artifact page for the operations of artifacts.
fn page(text: &str) -> ArtifactContent {
    ArtifactContent {
        html: format!("<title>{text}</title>").into_bytes(),
        files: Vec::new(),
        title: Some(text.to_owned()),
    }
}

/// The artifact the operation of `action` acts on, published by the
/// admin (with a second version for [`Action::ArtifactRemoveVersion`],
/// which may not remove the current one); `None` for the operations
/// without one.
async fn arrange_artifact(world: &World, action: Action) -> Result<Option<ArtifactId>, Error> {
    match action {
        Action::ArtifactUpdate
        | Action::ArtifactShare
        | Action::ArtifactUnshare
        | Action::ArtifactRollback
        | Action::ArtifactRemove
        | Action::ArtifactRemoveVersion
        | Action::ArtifactVersions
        | Action::ArtifactGet => {
            let artifacts = world.artifacts();
            let id = artifacts
                .publish(&world.owner, page("v1"))
                .await?
                .artifact
                .id;
            if action == Action::ArtifactRemoveVersion {
                artifacts.update(&world.owner, id, page("v2")).await?;
            }
            Ok(Some(id))
        }
        _ => Ok(None),
    }
}

/// The artifact [`arrange_artifact`] published.
fn arranged(artifact: Option<ArtifactId>) -> Result<ArtifactId, Error> {
    artifact.ok_or_else(|| Error::internal_msg("no artifact was arranged"))
}

/// Runs the operation of `action` as `actor` ([`run`]) after publishing
/// the artifact it needs ([`arrange_artifact`]): its result without the
/// value, and that artifact.
async fn perform(
    world: &World,
    actor: &User,
    action: Action,
) -> (Result<(), Error>, Option<ArtifactId>) {
    match arrange_artifact(world, action).await {
        Ok(artifact) => (run(world, actor, action, artifact).await, artifact),
        Err(error) => (Err(error), None),
    }
}

/// Runs the operation of `action` as `actor` against the world's `shop`,
/// `prod`, `shop.apps.test` and `artifact`, after arranging what it needs
/// ([`arrange`]) and clearing the log. Its result without the value.
///
/// The `match` is exhaustive on purpose: a new [`Action`] does not compile
/// until it names the operation it guards.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per action is the table MOD-04 checks"
)]
async fn run(
    world: &World,
    actor: &User,
    action: Action,
    artifact: Option<ArtifactId>,
) -> Result<(), Error> {
    let shop = world.shop.name.clone();
    let prod = world.prod.name.clone();
    let host = world.domain.host.clone();
    let project_scope = ScopeRef::Project(shop.clone());
    let key = SecretKey::new("LOG_LEVEL")?;
    let ports = Arc::clone(&world.ports);
    let settings = Arc::clone(&world.settings);
    let git = GitService::new(Arc::clone(&ports));
    let secrets = SecretsService::new(Arc::clone(&ports));
    let reconciler = Reconciler::new(Arc::clone(&ports), Arc::clone(&settings));
    let deploy = world
        .add_deploy(&world.prod, 1, DeployStatus::Succeeded)
        .await?;
    let blog = RepoRef::new("owner/blog")?;
    world.ports.git.grant(&actor.login, &blog, RepoRole::Admin);
    let code = Host::new("code.example.org")?;
    world
        .ports
        .git
        .accept_token(&code, "token-of-actor", &actor.login);
    let free = arrange(world, action).await?;
    world.log().clear();
    match action {
        Action::ProjectCreate => world
            .projects()
            .create(
                actor,
                NewProject {
                    tenant: None,
                    name: ProjectName::new("blog")?,
                    git_host: Host::new(World::GIT_HOST)?,
                    repo: blog,
                    manifest: None,
                },
            )
            .await
            .map(drop),
        Action::ProjectList => world.projects().list(actor).await.map(drop),
        Action::ProjectGet => world.projects().get(actor, &shop).await.map(drop),
        Action::ProjectSetManifest => world
            .projects()
            .set_manifest(actor, &shop, None)
            .await
            .map(drop),
        Action::ProjectRemove => world.projects().remove(actor, &shop).await,
        Action::ProjectResolve => world
            .projects()
            .resolve(actor, &world.shop.git_host, &world.shop.repo)
            .await
            .map(drop),
        Action::EnvironmentList => world.environments().list(actor, &shop).await.map(drop),
        Action::EnvironmentGet => world
            .environments()
            .get(actor, &shop, &prod)
            .await
            .map(drop),
        Action::EnvironmentUpdate => world
            .environments()
            .update(actor, &shop, &prod, EnvironmentChange::default())
            .await
            .map(drop),
        Action::EnvironmentDeploys => world
            .environments()
            .deploys(actor, &shop, &prod)
            .await
            .map(drop),
        Action::EnvironmentLogs => world
            .environments()
            .logs(actor, &shop, &prod, &LogsRequest::default())
            .await
            .map(drop),
        Action::EnvironmentAdd => world
            .environments()
            .add(
                actor,
                &shop,
                EnvName::new("staging")?,
                Branch::new("staging")?,
                EnvKind::Compose,
            )
            .await
            .map(drop),
        Action::EnvironmentStop => world
            .environments()
            .stop(actor, &shop, &prod)
            .await
            .map(drop),
        Action::EnvironmentStart => world
            .environments()
            .start(actor, &shop, &prod)
            .await
            .map(drop),
        Action::EnvironmentRemove => world.environments().remove(actor, &shop, &prod, true).await,
        Action::EnvironmentShare => world
            .visibility()
            .share(actor, &shop, &prod)
            .await
            .map(drop),
        Action::EnvironmentUnshare => world
            .visibility()
            .unshare(actor, &shop, &prod)
            .await
            .map(drop),
        Action::LinkIssue => world
            .links()
            .issue(actor, &LinkTarget::Environment(world.prod.id))
            .await
            .map(drop),
        Action::TunnelOpen => world.tunnels().open(actor, &shop).await.map(drop),
        Action::DomainList => world.domains().list(actor).await.map(drop),
        Action::DomainAddManaged => world
            .domains()
            .add_managed(actor, &shop, Label::new("blog")?, false)
            .await
            .map(drop),
        Action::DomainAddExternal => world
            .domains()
            .add_external(actor, &shop, Host::new("shop.example.org")?)
            .await
            .map(drop),
        Action::DomainRemove => world.domains().remove(actor, &host).await,
        Action::DomainSetProxied => world
            .domains()
            .set_proxied(actor, &host, true)
            .await
            .map(drop),
        Action::BindingBind => world
            .bindings()
            .bind(
                actor,
                &shop,
                &prod,
                BindRequest {
                    service: ServiceName::new("web")?,
                    port: ContainerPort::new(8080)?,
                    host: free,
                },
            )
            .await
            .map(drop),
        Action::BindingUnbind => world.bindings().unbind(actor, &host).await,
        Action::DeployRedeploy => world
            .deploys()
            .redeploy(actor, &shop, &prod, None)
            .await
            .map(drop),
        Action::DeployRequest => world
            .deploys()
            .request_cli(actor, &shop, &prod, ImageTag::new("v2")?, None)
            .await
            .map(drop),
        Action::DeployGet => world.deploys().get(actor, deploy.id).await.map(drop),
        Action::DeployRotateToken => world.deploys().rotate_token(actor, &shop).await.map(drop),
        Action::DeploySnippet => world.deploys().snippet(actor, &shop).await.map(drop),
        Action::SecretSet => secrets
            .set(
                actor,
                &project_scope,
                key,
                SecretValue::new("debug".to_owned()),
                false,
            )
            .await
            .map(drop),
        Action::SecretGet => secrets.get(actor, &project_scope, &key).await.map(drop),
        Action::SecretList => secrets.list(actor, &project_scope).await.map(drop),
        Action::SecretRemove => secrets.remove(actor, &project_scope, &key).await.map(drop),
        Action::SecretHistory => secrets.history(actor, &project_scope, &key).await.map(drop),
        Action::SecretRollback => secrets
            .rollback(actor, &project_scope, &key, VersionNumber::FIRST)
            .await
            .map(drop),
        Action::SecretSync => world.secret_sync().sync(actor, &shop).await.map(drop),
        Action::SecretResolve => secrets.resolve(actor, &shop, &prod, None).await.map(drop),
        Action::GitList => git.list(actor).await.map(drop),
        Action::GitConnect => git
            .connect(
                actor,
                GitProvider::Forgejo,
                code,
                SecretValue::new("token-of-actor".to_owned()),
            )
            .await
            .map(drop),
        Action::GitDisconnect => git.disconnect(actor, &Host::new(World::GIT_HOST)?).await,
        Action::GitTemplates => TemplateCatalog::new(Arc::clone(&ports), Arc::clone(&settings))
            .list(actor, &[], None)
            .await
            .map(drop),
        Action::AdminToken => world.auth().create_admin_token(actor).await.map(drop),
        Action::AdminUsers => world.auth().users(actor).await.map(drop),
        Action::AdminRevokeSessions => world
            .auth()
            .revoke_sessions(actor, &Login::new("owner")?)
            .await
            .map(drop),
        Action::AdminReconcile => reconciler.reconcile(actor).await,
        Action::ModuleList => world.modules().list(actor).await.map(drop),
        Action::ModuleAvailable => world.modules().available(actor).await.map(drop),
        Action::ModuleInstall => world
            .modules()
            .install(actor, probe_install()?)
            .await
            .map(drop),
        Action::ModuleUpdate => world
            .modules()
            .update(
                actor,
                &ModuleName::new("probe")?,
                ModuleChange {
                    enabled: Some(true),
                    config: None,
                },
            )
            .await
            .map(drop),
        Action::ModuleRemove => {
            world
                .modules()
                .remove(actor, &ModuleName::new("probe")?, false)
                .await
        }
        Action::ArtifactPublish => world
            .artifacts()
            .publish(actor, page("published"))
            .await
            .map(drop),
        Action::ArtifactUpdate => world
            .artifacts()
            .update(actor, arranged(artifact)?, page("v2"))
            .await
            .map(drop),
        Action::ArtifactShare => world
            .artifacts()
            .share(actor, arranged(artifact)?)
            .await
            .map(drop),
        Action::ArtifactUnshare => world
            .artifacts()
            .unshare(actor, arranged(artifact)?)
            .await
            .map(drop),
        Action::ArtifactRollback => world
            .artifacts()
            .rollback(actor, arranged(artifact)?, VersionNumber::FIRST)
            .await
            .map(drop),
        Action::ArtifactRemove => world.artifacts().delete(actor, arranged(artifact)?).await,
        Action::ArtifactRemoveVersion => {
            world
                .artifacts()
                .delete_version(actor, arranged(artifact)?, VersionNumber::FIRST)
                .await
        }
        Action::ArtifactList => world.artifacts().list(actor, false).await.map(drop),
        Action::ArtifactVersions => world
            .artifacts()
            .versions(actor, arranged(artifact)?)
            .await
            .map(drop),
        Action::ArtifactGet => world
            .artifacts()
            .get(actor, arranged(artifact)?)
            .await
            .map(drop),
    }
}

/// The resource the operation of `action` is about in the world of
/// [`perform`] and its `artifact` (the table of `docs/ARCHITECTURE.md`).
fn expected_resource(world: &World, action: Action, artifact: Option<ArtifactId>) -> Resource {
    let project = Resource::Project(world.shop.id);
    let environment = Resource::Environment(world.prod.id);
    let domain = Resource::Domain(world.domain.id);
    let project_secrets = Resource::Secret(SecretScope::Project(world.shop.id));
    match action {
        Action::ProjectCreate
        | Action::ProjectList
        | Action::DomainList
        | Action::GitList
        | Action::GitConnect
        | Action::GitDisconnect
        | Action::GitTemplates
        | Action::AdminToken
        | Action::AdminUsers
        | Action::AdminRevokeSessions
        | Action::AdminReconcile
        | Action::ModuleList
        | Action::ModuleAvailable
        | Action::ModuleInstall
        | Action::ModuleUpdate
        | Action::ModuleRemove
        | Action::ArtifactPublish
        | Action::ArtifactList => Resource::Admin,
        Action::ArtifactUpdate
        | Action::ArtifactShare
        | Action::ArtifactUnshare
        | Action::ArtifactRollback
        | Action::ArtifactRemove
        | Action::ArtifactRemoveVersion
        | Action::ArtifactVersions
        | Action::ArtifactGet => artifact.map_or(Resource::Admin, Resource::Artifact),
        Action::ProjectGet
        | Action::ProjectSetManifest
        | Action::ProjectRemove
        | Action::ProjectResolve
        | Action::EnvironmentList
        | Action::EnvironmentAdd
        | Action::TunnelOpen
        | Action::DomainAddManaged
        | Action::DomainAddExternal
        | Action::DeployRotateToken
        | Action::DeploySnippet => project,
        Action::EnvironmentGet
        | Action::EnvironmentUpdate
        | Action::EnvironmentDeploys
        | Action::EnvironmentLogs
        | Action::EnvironmentStop
        | Action::EnvironmentStart
        | Action::EnvironmentRemove
        | Action::EnvironmentShare
        | Action::EnvironmentUnshare
        | Action::LinkIssue
        | Action::BindingBind
        | Action::DeployRedeploy
        | Action::DeployRequest
        | Action::DeployGet => environment,
        Action::DomainRemove | Action::DomainSetProxied | Action::BindingUnbind => domain,
        Action::SecretSet
        | Action::SecretGet
        | Action::SecretList
        | Action::SecretRemove
        | Action::SecretHistory
        | Action::SecretRollback
        | Action::SecretSync => project_secrets,
        Action::SecretResolve => Resource::Secret(SecretScope::Environment(world.prod.id)),
    }
}

/// The position of `action` in a hand-written list of every variant: the
/// `match` fails to compile when a variant is added without it.
const fn position(action: Action) -> usize {
    match action {
        Action::ProjectCreate => 0,
        Action::ProjectList => 1,
        Action::ProjectGet => 2,
        Action::ProjectSetManifest => 3,
        Action::ProjectRemove => 4,
        Action::ProjectResolve => 5,
        Action::EnvironmentList => 6,
        Action::EnvironmentGet => 7,
        Action::EnvironmentUpdate => 8,
        Action::EnvironmentDeploys => 9,
        Action::EnvironmentLogs => 10,
        Action::EnvironmentAdd => 11,
        Action::EnvironmentStop => 12,
        Action::EnvironmentStart => 13,
        Action::EnvironmentRemove => 14,
        Action::DomainList => 15,
        Action::DomainAddManaged => 16,
        Action::DomainAddExternal => 17,
        Action::DomainRemove => 18,
        Action::DomainSetProxied => 19,
        Action::BindingBind => 20,
        Action::BindingUnbind => 21,
        Action::DeployRedeploy => 22,
        Action::DeployRequest => 23,
        Action::DeployGet => 24,
        Action::DeployRotateToken => 25,
        Action::DeploySnippet => 26,
        Action::SecretSet => 27,
        Action::SecretGet => 28,
        Action::SecretList => 29,
        Action::SecretRemove => 30,
        Action::SecretHistory => 31,
        Action::SecretRollback => 32,
        Action::SecretSync => 33,
        Action::SecretResolve => 34,
        Action::GitList => 35,
        Action::GitConnect => 36,
        Action::GitDisconnect => 37,
        Action::GitTemplates => 38,
        Action::AdminToken => 39,
        Action::AdminUsers => 40,
        Action::AdminRevokeSessions => 41,
        Action::AdminReconcile => 42,
        Action::ModuleList => 43,
        Action::ModuleAvailable => 44,
        Action::ModuleInstall => 45,
        Action::ModuleUpdate => 46,
        Action::ModuleRemove => 47,
        Action::EnvironmentShare => 48,
        Action::EnvironmentUnshare => 49,
        Action::LinkIssue => 50,
        Action::TunnelOpen => 51,
        Action::ArtifactPublish => 52,
        Action::ArtifactUpdate => 53,
        Action::ArtifactShare => 54,
        Action::ArtifactUnshare => 55,
        Action::ArtifactRollback => 56,
        Action::ArtifactRemove => 57,
        Action::ArtifactRemoveVersion => 58,
        Action::ArtifactList => 59,
        Action::ArtifactVersions => 60,
        Action::ArtifactGet => 61,
    }
}

mod action {
    use super::*;

    // TC: MOD-04 (the table of actions is complete: `ALL` holds every
    // variant once)
    #[test]
    fn lists_every_variant_once_when_all_is_read() {
        let mut positions: Vec<usize> = Action::ALL.iter().copied().map(position).collect();
        positions.sort_unstable();
        assert_eq!(positions, (0..62).collect::<Vec<_>>());
    }

    // TC: MOD-04 (modules and the audit log tell actions apart by name)
    #[test]
    fn has_distinct_names_when_all_is_read() {
        let mut names: Vec<&str> = Action::ALL.iter().map(|action| action.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Action::ALL.len());
    }
}

mod operations {
    use super::*;

    // TC: MOD-04
    #[tokio::test]
    async fn ask_authorize_once_and_succeed_when_run_by_admin() {
        let mut wrong = Vec::new();
        for action in Action::ALL {
            let world = World::new().await;
            let (result, artifact) = perform(&world, &world.owner, action).await;
            let asked = world.log().authorized();
            let expected = vec![(
                world.owner.id,
                action,
                expected_resource(&world, action, artifact),
            )];
            // The whole operation ran: a second question later in its flow
            // would show.
            if asked != expected || !ran_to_end(action, &result) {
                wrong.push((action, asked, result));
            }
        }
        assert!(
            wrong.is_empty(),
            "operations that did not ask authorize exactly once as expected: {wrong:#?}"
        );
    }

    // TC: MOD-05 (no module answers: a user who is not the admin may do
    // nothing)
    #[tokio::test]
    async fn are_forbidden_when_actor_is_not_admin_and_no_module_answers() {
        let mut admitted = Vec::new();
        for action in Action::ALL {
            let world = World::new().await;
            let carol = world.add_connected_user("carol").await.unwrap();
            let (result, _artifact) = perform(&world, &carol, action).await;
            // The refusal is the Authorizer's own, not a later rule of the
            // operation (the forge's, the limits'): it asked once and
            // nothing ran after it.
            let expected = format!("carol may not {action}");
            let refused_by_authorizer =
                matches!(&result, Err(Error::Forbidden(reason)) if *reason == expected);
            if !refused_by_authorizer || world.log().authorized().len() != 1 {
                admitted.push((action, result));
            }
        }
        assert!(
            admitted.is_empty(),
            "a non-admin got past authorize: {admitted:#?}"
        );
    }

    // TC: MOD-05 (no module answers: the single admin may do everything)
    #[tokio::test]
    async fn run_to_end_when_actor_is_admin_and_no_module_answers() {
        let mut refused = Vec::new();
        for action in Action::ALL {
            let world = World::new().await;
            let (result, _artifact) = perform(&world, &world.owner, action).await;
            if !ran_to_end(action, &result) {
                refused.push((action, result));
            }
        }
        assert!(refused.is_empty(), "the admin was refused: {refused:#?}");
    }
}

mod check {
    use super::*;

    fn resource(world: &World) -> Resource {
        Resource::Project(world.shop.id)
    }

    // TC: MOD-05
    #[tokio::test]
    async fn allows_admin_when_hook_continues() {
        let world = World::new().await;
        let checked = Authorizer::new(world.ports.as_ref())
            .check(&world.owner, Action::ProjectRemove, &resource(&world))
            .await;
        assert!(checked.is_ok(), "{checked:?}");
    }

    // TC: MOD-05
    #[tokio::test]
    async fn forbids_non_admin_when_hook_continues() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let checked = Authorizer::new(world.ports.as_ref())
            .check(&carol, Action::ProjectRemove, &resource(&world))
            .await;
        assert!(matches!(checked, Err(Error::Forbidden(_))), "{checked:?}");
    }

    // TC: MOD-04 (one question per check, with the action and resource)
    #[tokio::test]
    async fn asks_hook_once_when_checked() {
        let world = World::new().await;
        let _checked = Authorizer::new(world.ports.as_ref())
            .check(&world.owner, Action::ProjectRemove, &resource(&world))
            .await;
        assert_eq!(
            world.log().authorized(),
            vec![(world.owner.id, Action::ProjectRemove, resource(&world))]
        );
    }

    // TC: MOD-05 (a `deny` wins over the admin)
    #[tokio::test]
    async fn forbids_admin_with_reason_when_hook_denies() {
        let world = World::new().await;
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("not a member".to_owned()));
        let checked = Authorizer::new(world.ports.as_ref())
            .check(&world.owner, Action::ProjectRemove, &resource(&world))
            .await;
        assert!(
            matches!(&checked, Err(Error::Forbidden(reason)) if reason.contains("not a member")),
            "{checked:?}"
        );
    }

    // TC: MOD-05 (an `allow` admits a user who is not the admin)
    #[tokio::test]
    async fn allows_non_admin_when_hook_allows() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let checked = Authorizer::new(world.ports.as_ref())
            .check(&carol, Action::ProjectRemove, &resource(&world))
            .await;
        assert!(checked.is_ok(), "{checked:?}");
    }
}

mod visible {
    use super::*;

    // TC: TEN-02 (decision (a): a list a module
    // opened to a non-admin keeps the items `authorize` allows, asking
    // once per item with the list's action)
    #[tokio::test]
    async fn keeps_allowed_items_when_non_admin_lists() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("another tenant".to_owned()));
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let kept = Authorizer::new(world.ports.as_ref())
            .visible(
                &carol,
                Action::EnvironmentList,
                vec![world.prod.clone(), world.qa.clone()],
                |environment| Resource::Environment(environment.id),
            )
            .await;
        assert_eq!(
            (
                kept.iter().map(|kept| kept.id).collect::<Vec<_>>(),
                world.log().authorized()
            ),
            (
                vec![world.qa.id],
                vec![
                    (
                        carol.id,
                        Action::EnvironmentList,
                        Resource::Environment(world.prod.id)
                    ),
                    (
                        carol.id,
                        Action::EnvironmentList,
                        Resource::Environment(world.qa.id)
                    ),
                ]
            )
        );
    }

    // TC: TEN-02 (no opinion about an item keeps it from a non-admin)
    #[tokio::test]
    async fn drops_items_when_hook_has_no_opinion() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let kept = Authorizer::new(world.ports.as_ref())
            .visible(
                &carol,
                Action::ProjectList,
                vec![world.shop.clone()],
                |project| Resource::Project(project.id),
            )
            .await;
        assert!(kept.is_empty(), "{kept:?}");
    }

    // TC: MOD-04, TEN-02 (the admin sees every item, nothing more asked)
    #[tokio::test]
    async fn keeps_every_item_without_asking_when_admin_lists() {
        let world = World::new().await;
        let kept = Authorizer::new(world.ports.as_ref())
            .visible(
                &world.owner,
                Action::ProjectList,
                vec![world.shop.clone()],
                |project| Resource::Project(project.id),
            )
            .await;
        assert_eq!((kept.len(), world.log().authorized()), (1, Vec::new()));
    }
}

mod admin_only {
    use super::*;

    const INSTALLATION: [Action; 7] = [
        Action::AdminToken,
        Action::AdminUsers,
        Action::AdminRevokeSessions,
        Action::AdminReconcile,
        Action::ModuleInstall,
        Action::ModuleUpdate,
        Action::ModuleRemove,
    ];

    // TC: MOD-05 (a module narrows the installation-wide `admin.*`
    // operations, it never opens them to a user who is not the admin)
    #[tokio::test]
    async fn forbids_non_admin_when_hook_allows_admin_operation() {
        let mut admitted = Vec::new();
        for action in INSTALLATION {
            let world = World::new().await;
            let carol = world.add_user("carol", UserRole::User).await.unwrap();
            world
                .ports
                .hooks
                .answer_next("authorize", HookResult::Allow);
            let checked = Authorizer::new(world.ports.as_ref())
                .check(&carol, action, &Resource::Admin)
                .await;
            if !matches!(checked, Err(Error::Forbidden(_))) {
                admitted.push((action, checked));
            }
        }
        assert!(admitted.is_empty(), "{admitted:#?}");
    }

    // TC: MOD-05 (only the `admin.*` operations are kept for the admin:
    // an `allow` still opens the others)
    #[test]
    fn names_only_admin_operations_when_all_is_read() {
        let admin_only: Vec<Action> = Action::ALL
            .into_iter()
            .filter(|action| action.is_admin_only())
            .collect();
        assert_eq!(admin_only, INSTALLATION.to_vec());
    }
}

mod unknown_resource {
    use super::*;

    // TC: MOD-04, SEC-10 (a name that resolves to nothing is asked about as
    // `Admin`, once; a refused caller cannot tell it from a foreign one)
    #[tokio::test]
    async fn forbidden_after_asking_about_admin_when_non_admin_names_unknown_project() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        world.log().clear();
        let refused = world
            .projects()
            .get(&carol, &ProjectName::new("nope").unwrap())
            .await;
        assert_eq!(
            (
                matches!(refused, Err(Error::Forbidden(_))),
                world.log().authorized()
            ),
            (true, vec![(carol.id, Action::ProjectGet, Resource::Admin)]),
            "{refused:?}"
        );
    }

    // TC: MOD-04 (the admin learns that the name is unknown)
    #[tokio::test]
    async fn not_found_when_admin_names_unknown_project() {
        let world = World::new().await;
        let missing = world
            .projects()
            .get(&world.owner, &ProjectName::new("nope").unwrap())
            .await;
        assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
    }
}
