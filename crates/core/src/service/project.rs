//! `ProjectService` (test cases ENV-02, ENV-03).
//!
//! A project is a repository: unique by `(git_host, repo)` and by name. `resolve` maps a `git remote` to its project for
//! the CLI. Whoever creates one must be admin of the repository through
//! their own forge connection.

use std::sync::Arc;

use super::authorizer::Authorizer;
use super::settings::Settings;
use super::support::{authorize, authorized, authorized_project, ensure_repo_admin, is_exposures};
use crate::domain::{
    Host, NewProject, Project, ProjectId, ProjectName, RepoRef, TemplateRef, TenantId, TenantSlug,
    User,
};
use crate::error::Error;
use crate::expose::ZONE;
use crate::ports::{Action, Clock as _, Hooks as _, Ports, Resource, Rng as _, Store as _};

/// See the [module documentation](self).
#[derive(Debug)]
pub struct ProjectService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for ProjectService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> ProjectService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// Creates the project `new.name` for `new.repo` on `new.git_host`,
    /// created by `actor`, with the JSON of its `liftoff.toml` if one was
    /// read, in the tenant the actor named by slug or, without one, in
    /// the tenant a module chooses ([`Hooks::project_tenant`]: the `tenants` module's, the actor's only tenant). Without such a module the project is outside every tenant.
    ///
    /// `authorize` is asked about the tenant ([`Resource::Tenant`]: the
    /// `tenants` module checks the actor's membership) or, without one,
    /// about the installation ([`Resource::Admin`]). A tenant's
    /// `max_projects` ([`Hooks::tenant_quotas`]) is the core's rule
    /// (TEN-04). The name is unique across tenants: an environment's
    /// compose project and slice are named after it.
    ///
    /// [`Hooks::tenant_quotas`]: crate::ports::Hooks::tenant_quotas
    /// [`Hooks::project_tenant`]: crate::ports::Hooks::project_tenant
    ///
    /// # Errors
    ///
    /// - [`Error::ModuleRequired`] when the actor named a tenant and no
    ///   module keeps tenants;
    /// - what the module refuses the tenant with (an unknown slug is
    ///   [`Error::NotFound`]);
    /// - [`Error::Conflict`] when the repository already has a project,
    ///   the name is taken (ENV-02) or the tenant holds its
    ///   `max_projects` (TEN-04);
    /// - [`Error::Forbidden`] unless the actor's own connection on
    ///   `git_host` is admin of `repo` (without a connection the message says to run `lof git connect`);
    /// - [`Error::Unavailable`] when the module that keeps tenants could
    ///   not answer;
    /// - whatever the forge or the store report.
    pub async fn create(&self, actor: &User, new: NewProject) -> Result<Project, Error> {
        self.create_from_template(actor, new, None).await
    }

    /// [`ProjectService::create`] of a project made from `template` (`lof create`; `POST /projects {template}`), which
    /// the project row records.
    ///
    /// # Errors
    ///
    /// As [`ProjectService::create`].
    pub async fn create_from_template(
        &self,
        actor: &User,
        new: NewProject,
        template: Option<TemplateRef>,
    ) -> Result<Project, Error> {
        let chosen = self.admit(actor, &new).await?;
        let NewProject {
            name,
            git_host,
            repo,
            manifest,
            ..
        } = new;
        let project = Project {
            id: ProjectId::new(self.ports.rng().ulid()),
            tenant_id: chosen,
            name,
            git_host,
            repo,
            manifest,
            template,
            created_by: actor.id,
            created_at: self.ports.clock().now(),
        };
        ensure_repo_admin(self.ports.as_ref(), actor, &project).await?;
        self.ports.store().insert_project(&project).await?;
        Ok(project)
    }

    /// The tenant `new` goes into, once `actor` may create it there and
    /// nothing in the store stands in its way: the checks of
    /// [`ProjectService::create`] before the forge is asked, which a
    /// `lof create` plan runs when it is built (CRE-02, CRE-10).
    ///
    /// # Errors
    ///
    /// As [`ProjectService::create`], except the forge's.
    pub(super) async fn admit(
        &self,
        actor: &User,
        new: &NewProject,
    ) -> Result<Option<TenantId>, Error> {
        let chosen = self.authorize_create(actor, new.tenant.as_ref()).await?;
        self.admit_in(chosen, new).await?;
        Ok(chosen)
    }

    /// The first half of [`ProjectService::admit`]: the tenant a project
    /// asking for `tenant` goes into, once `authorize` let `actor` create
    /// it there. A `lof create` plan asks it before it reads anything from
    /// the forge.
    ///
    /// # Errors
    ///
    /// [`Error::ModuleRequired`] (`tenants`) for a `tenant` no module
    /// chose; [`Error::Forbidden`] when `authorize` refuses.
    pub(super) async fn authorize_create(
        &self,
        actor: &User,
        tenant: Option<&TenantSlug>,
    ) -> Result<Option<TenantId>, Error> {
        let chosen = self.ports.hooks().project_tenant(actor, tenant).await?;
        if tenant.is_some() && chosen.is_none() {
            return Err(Error::module_required("tenants"));
        }
        let resource = chosen.map_or(Resource::Admin, Resource::Tenant);
        authorize(self.ports.as_ref(), actor, Action::ProjectCreate, &resource).await?;
        Ok(chosen)
    }

    /// The second half of [`ProjectService::admit`]: nothing in the store
    /// stands in the way of `new` in the tenant `chosen` (CRE-02, CRE-10,
    /// TEN-04).
    ///
    /// # Errors
    ///
    /// [`Error::Conflict`] when the repository already is a project, the
    /// name is taken or the tenant is full; whatever the store reports.
    pub(super) async fn admit_in(
        &self,
        chosen: Option<TenantId>,
        new: &NewProject,
    ) -> Result<(), Error> {
        if new.name.as_str() == ZONE {
            return Err(Error::invalid(
                "project",
                format!("{ZONE} is reserved: it holds the ports of `lof expose`"),
            ));
        }
        let store = self.ports.store();
        if let Some(taken) = store.project_by_repo(&new.git_host, &new.repo).await? {
            return Err(Error::conflict(format!(
                "{}/{} already is project {}",
                new.git_host, new.repo, taken.name
            )));
        }
        let projects = store.projects().await?;
        if projects.iter().any(|project| project.name == new.name) {
            return Err(Error::conflict(format!(
                "project {} already exists",
                new.name
            )));
        }
        if let Some(tenant) = chosen {
            self.ensure_below_max_projects(tenant, &projects).await?;
        }
        Ok(())
    }

    /// [`Error::Conflict`] when `tenant` already holds as many of
    /// `projects` as its `max_projects` allows (TEN-04).
    async fn ensure_below_max_projects(
        &self,
        tenant: TenantId,
        projects: &[Project],
    ) -> Result<(), Error> {
        let Some(max) = self
            .ports
            .hooks()
            .tenant_quotas(tenant)
            .await?
            .and_then(|quotas| quotas.max_projects)
        else {
            return Ok(());
        };
        let held = projects
            .iter()
            .filter(|project| project.tenant_id == Some(tenant))
            .count();
        if held >= usize::try_from(max).unwrap_or(usize::MAX) {
            return Err(Error::conflict(format!(
                "the tenant holds {held} projects, its quota max_projects is {max}"
            )));
        }
        Ok(())
    }

    /// The projects `actor` may see, by name: every one for the admin,
    /// those `authorize` allows one by one for anyone else (TEN-02).
    ///
    /// # Errors
    ///
    /// Whatever the store reports.
    pub async fn list(&self, actor: &User) -> Result<Vec<Project>, Error> {
        authorize(
            self.ports.as_ref(),
            actor,
            Action::ProjectList,
            &Resource::Admin,
        )
        .await?;
        let mut projects = self.ports.store().projects().await?;
        projects.retain(|project| !is_exposures(project, &self.settings));
        Ok(Authorizer::new(self.ports.as_ref())
            .visible(actor, Action::ProjectList, projects, |project| {
                Resource::Project(project.id)
            })
            .await)
    }

    /// The project `name`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown project.
    pub async fn get(&self, actor: &User, name: &ProjectName) -> Result<Project, Error> {
        authorized_project(self.ports.as_ref(), actor, Action::ProjectGet, name).await
    }

    /// Replaces the stored `liftoff.toml` of the project `name` (`None`
    /// keeps it).
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` owns the project;
    /// [`Error::NotFound`] for an unknown project; whatever the store
    /// reports.
    pub async fn set_manifest(
        &self,
        actor: &User,
        name: &ProjectName,
        manifest: Option<String>,
    ) -> Result<Project, Error> {
        let mut project =
            authorized_project(self.ports.as_ref(), actor, Action::ProjectSetManifest, name)
                .await?;
        if let Some(manifest) = manifest {
            project.manifest = Some(manifest);
            self.ports.store().update_project(&project).await?;
        }
        Ok(project)
    }

    /// Deletes the project `name` with its deploy token and its secrets.
    /// Its environments and domains go first (`lof env rm`, `lof domain
    /// rm`), so nothing that runs or routes is removed by this call.
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the project;
    /// - [`Error::NotFound`] for an unknown project;
    /// - [`Error::Conflict`] naming what is left while it has an
    ///   environment or a domain;
    /// - whatever the store reports.
    pub async fn remove(&self, actor: &User, name: &ProjectName) -> Result<(), Error> {
        let project =
            authorized_project(self.ports.as_ref(), actor, Action::ProjectRemove, name).await?;
        let store = self.ports.store();
        let environments = store.environments_of_project(project.id).await?;
        if let Some(first) = environments.first() {
            return Err(Error::conflict(format!(
                "project {name} still has environment {}; remove its environments first",
                first.name
            )));
        }
        let domains = store.domains_of_project(project.id).await?;
        if let Some(first) = domains.first() {
            return Err(Error::conflict(format!(
                "project {name} still has domain {}; remove its domains first",
                first.host
            )));
        }
        store.delete_project(project.id).await
    }

    /// The project of `repo` on `git_host` (ENV-03).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when no project has that repository.
    pub async fn resolve(
        &self,
        actor: &User,
        git_host: &Host,
        repo: &RepoRef,
    ) -> Result<Project, Error> {
        let found = self
            .ports
            .store()
            .project_by_repo(git_host, repo)
            .await
            .and_then(|project| {
                project.ok_or_else(|| Error::not_found(format!("project of {git_host}/{repo}")))
            });
        authorized(
            self.ports.as_ref(),
            actor,
            Action::ProjectResolve,
            found,
            |project| Resource::Project(project.id),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::World;

    fn name(raw: &str) -> ProjectName {
        ProjectName::new(raw).unwrap()
    }

    fn host(raw: &str) -> Host {
        Host::new(raw).unwrap()
    }

    fn repo(raw: &str) -> RepoRef {
        RepoRef::new(raw).unwrap()
    }

    mod create {
        use super::*;
        use crate::domain::{TenantSlug, UserRole};
        use crate::ports::{HookResult, RepoRole};

        // TC: ENV-02 (happy path: a new repository gets its project)
        #[tokio::test]
        async fn stores_project_by_actor_when_repo_and_name_free() {
            let world = World::new().await;
            world
                .ports
                .git
                .grant(&world.owner.login, &repo("owner/blog"), RepoRole::Admin);
            let created = world
                .projects()
                .create(
                    &world.owner,
                    NewProject {
                        tenant: None,
                        name: name("blog"),
                        git_host: host("git.example.com"),
                        repo: repo("owner/blog"),
                        manifest: Some("{\"name\":\"blog\"}".to_owned()),
                    },
                )
                .await
                .unwrap();
            let stored = world
                .ports
                .store
                .project_by_repo(&host("git.example.com"), &repo("owner/blog"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    stored.id,
                    stored.name.as_str(),
                    stored.created_by,
                    stored.tenant_id,
                    stored.manifest.as_deref(),
                ),
                (
                    created.id,
                    "blog",
                    world.owner.id,
                    None,
                    Some("{\"name\":\"blog\"}")
                )
            );
        }

        /// `owner` creates `blog` (`owner/blog`, admin there) naming the
        /// tenant `tenant`.
        async fn create_blog(world: &World, tenant: Option<&str>) -> Result<Project, Error> {
            world
                .ports
                .git
                .grant(&world.owner.login, &repo("owner/blog"), RepoRole::Admin);
            let slug = tenant.map(|raw| TenantSlug::new(raw).unwrap());
            world
                .projects()
                .create(
                    &world.owner,
                    NewProject {
                        tenant: slug,
                        name: name("blog"),
                        git_host: host("git.example.com"),
                        repo: repo("owner/blog"),
                        manifest: None,
                    },
                )
                .await
        }

        // TC: TEN-04 (without a module that keeps tenants, naming one is refused, and nothing is stored)
        #[tokio::test]
        async fn module_required_when_tenant_named_without_tenants_module() {
            let world = World::new().await;
            let created = create_blog(&world, Some("acme")).await;
            let stored = world
                .ports
                .store
                .project_by_repo(&host("git.example.com"), &repo("owner/blog"))
                .await
                .unwrap();
            assert_eq!(
                (
                    matches!(created, Err(Error::ModuleRequired(_))),
                    stored.is_none()
                ),
                (true, true),
                "{created:?}"
            );
        }

        // TC: TEN-04 (the project goes to the tenant the module names, and `authorize` is asked about that tenant)
        #[tokio::test]
        async fn stores_project_in_tenant_when_module_names_one() {
            let world = World::new().await;
            let tenant = TenantId::new(world.ports.rng.ulid());
            world.ports.hooks.answer_project_tenant(Ok(Some(tenant)));
            let created = create_blog(&world, Some("acme")).await.unwrap();
            let asked = world.ports.hooks.calls().into_iter().any(|call| {
                call == crate::fakes::HookCall::Authorize {
                    actor: world.owner.id,
                    action: Action::ProjectCreate,
                    resource: Resource::Tenant(tenant),
                }
            });
            assert_eq!(
                (
                    created.tenant_id,
                    asked,
                    world.ports.hooks.project_tenant_requests()
                ),
                (
                    Some(tenant),
                    true,
                    vec![Some(TenantSlug::new("acme").unwrap())]
                )
            );
        }

        // TC: TEN-04 (fail closed: when the module that keeps the quota
        // cannot answer, no project is created past it)
        #[tokio::test]
        async fn unavailable_and_nothing_stored_when_tenant_quotas_fail() {
            let world = World::new().await;
            let tenant = TenantId::new(world.ports.rng.ulid());
            world.ports.hooks.answer_project_tenant(Ok(Some(tenant)));
            world
                .ports
                .hooks
                .answer_quotas(Err(Error::unavailable("module tenants failed")));
            let created = create_blog(&world, None).await;
            let stored = world
                .ports
                .store
                .project_by_repo(&host("git.example.com"), &repo("owner/blog"))
                .await
                .unwrap();
            assert_eq!(
                (
                    matches!(created, Err(Error::Unavailable(_))),
                    stored.is_none()
                ),
                (true, true),
                "{created:?}"
            );
        }

        // TC: ENV-02
        #[tokio::test]
        async fn conflicts_when_repo_already_has_project() {
            let world = World::new().await;
            let err = world
                .projects()
                .create(
                    &world.owner,
                    NewProject {
                        tenant: None,
                        name: name("shop-2"),
                        git_host: host("git.example.com"),
                        repo: repo("owner/shop"),
                        manifest: None,
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: ENV-02 (repositories compare lower-cased)
        #[tokio::test]
        async fn conflicts_when_repo_differs_only_in_case() {
            let world = World::new().await;
            let err = world
                .projects()
                .create(
                    &world.owner,
                    NewProject {
                        tenant: None,
                        name: name("shop-2"),
                        git_host: host("git.example.com"),
                        repo: repo("Owner/Shop"),
                        manifest: None,
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: ENV-02 (the name is unique too)
        #[tokio::test]
        async fn conflicts_when_name_taken_by_other_repo() {
            let world = World::new().await;
            world
                .ports
                .git
                .grant(&world.owner.login, &repo("owner/shop-v2"), RepoRole::Admin);
            let err = world
                .projects()
                .create(
                    &world.owner,
                    NewProject {
                        tenant: None,
                        name: name("shop"),
                        git_host: host("git.example.com"),
                        repo: repo("owner/shop-v2"),
                        manifest: None,
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: ENV-02 (whoever creates must be admin of the repository)
        #[tokio::test]
        async fn forbidden_when_actor_role_is_read() {
            let world = World::new().await;
            world
                .ports
                .git
                .grant(&world.owner.login, &repo("owner/blog"), RepoRole::Read);
            let err = world
                .projects()
                .create(
                    &world.owner,
                    NewProject {
                        tenant: None,
                        name: name("blog"),
                        git_host: host("git.example.com"),
                        repo: repo("owner/blog"),
                        manifest: None,
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Forbidden(_)), "{err:?}");
        }

        // TC: ENV-02 (the role is asked as the actor's own connection)
        #[tokio::test]
        async fn forbidden_with_git_connect_hint_when_actor_has_no_connection() {
            let world = World::new().await;
            let alice = world.add_user("alice", UserRole::User).await.unwrap();
            // A module admits the non-admin (MOD-05); the forge rule
            // still applies on top of `authorize`.
            world
                .ports
                .hooks
                .answer_next("authorize", HookResult::Allow);
            let err = world
                .projects()
                .create(
                    &alice,
                    NewProject {
                        tenant: None,
                        name: name("blog"),
                        git_host: host("git.example.com"),
                        repo: repo("owner/blog"),
                        manifest: None,
                    },
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Forbidden(why) if why.contains("lof git connect")),
                "{err:?}"
            );
        }

        // TC: ENV-02 (a forbidden create stores nothing)
        #[tokio::test]
        async fn stores_nothing_when_forbidden() {
            let world = World::new().await;
            let _refused = world
                .projects()
                .create(
                    &world.owner,
                    NewProject {
                        tenant: None,
                        name: name("blog"),
                        git_host: host("git.example.com"),
                        repo: repo("owner/blog"),
                        manifest: None,
                    },
                )
                .await;
            let stored = world
                .ports
                .store
                .project_by_name(None, &name("blog"))
                .await
                .unwrap();
            assert_eq!(stored, None);
        }

        // TC: ENV-02 (a refused create stores nothing)
        #[tokio::test]
        async fn stores_nothing_when_conflict() {
            let world = World::new().await;
            let _refused = world
                .projects()
                .create(
                    &world.owner,
                    NewProject {
                        tenant: None,
                        name: name("shop-2"),
                        git_host: host("git.example.com"),
                        repo: repo("owner/shop"),
                        manifest: None,
                    },
                )
                .await;
            let names: Vec<String> = world
                .ports
                .store
                .projects()
                .await
                .unwrap()
                .into_iter()
                .map(|p| p.name.to_string())
                .collect();
            assert_eq!(names, vec!["shop".to_owned()]);
        }
    }

    mod set_manifest {
        use super::*;

        // TC: REST-03, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world
                .add_user("carol", crate::domain::UserRole::User)
                .await
                .unwrap();
            world.log().clear();
            let error = world
                .projects()
                .set_manifest(&carol, &name("shop"), Some("[project]\n".to_owned()))
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }
    }

    mod resolve {
        use super::*;

        // TC: ENV-03
        #[tokio::test]
        async fn finds_shop_when_repo_is_owner_shop() {
            let world = World::new().await;
            let project = world
                .projects()
                .resolve(&world.owner, &host("git.example.com"), &repo("owner/shop"))
                .await
                .unwrap();
            assert_eq!(project, world.shop);
        }

        // TC: ENV-03 (a git remote may spell the repository in any case)
        #[tokio::test]
        async fn finds_shop_when_repo_spelled_in_upper_case() {
            let world = World::new().await;
            let project = world
                .projects()
                .resolve(&world.owner, &host("git.example.com"), &repo("Owner/Shop"))
                .await
                .unwrap();
            assert_eq!(project.id, world.shop.id);
        }

        // TC: ENV-03
        #[tokio::test]
        async fn not_found_when_repo_unknown() {
            let world = World::new().await;
            let err = world
                .projects()
                .resolve(&world.owner, &host("git.example.com"), &repo("owner/nope"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        }

        // TC: ENV-03 (the forge host is part of the key)
        #[tokio::test]
        async fn not_found_when_same_repo_on_other_host() {
            let world = World::new().await;
            let err = world
                .projects()
                .resolve(&world.owner, &host("github.com"), &repo("owner/shop"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        }
    }

    mod remove {
        use super::*;

        // TC: REST-03, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world
                .add_user("carol", crate::domain::UserRole::User)
                .await
                .unwrap();
            world.log().clear();
            let error = world
                .projects()
                .remove(&carol, &name("shop"))
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: REST-03 (environments and domains go first)
        #[tokio::test]
        async fn conflict_naming_an_environment_when_the_project_has_one() {
            let world = World::new().await;
            let err = world
                .projects()
                .remove(&world.owner, &name("shop"))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Conflict(why) if why.contains("prod")),
                "{err:?}"
            );
        }

        // TC: REST-03
        #[tokio::test]
        async fn conflict_naming_a_domain_when_the_project_has_one() {
            let world = World::new().await;
            let blog = world.add_project("blog", "owner/blog").await.unwrap();
            let _domain = world.add_domain(&blog, "blog.apps.test").await.unwrap();
            let err = world
                .projects()
                .remove(&world.owner, &name("blog"))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Conflict(why) if why.contains("blog.apps.test")),
                "{err:?}"
            );
        }

        // TC: REST-03
        #[tokio::test]
        async fn gone_when_the_project_is_empty() {
            let world = World::new().await;
            let _blog = world.add_project("blog", "owner/blog").await.unwrap();
            world
                .projects()
                .remove(&world.owner, &name("blog"))
                .await
                .unwrap();
            let err = world
                .projects()
                .get(&world.owner, &name("blog"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        }
    }
}
