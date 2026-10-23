//! `Reconciler`: brings the host and the store back
//! in line without touching deploys.
//!
//! [`Reconciler::reconcile`] backs `POST /admin/reconcile` on a live
//! daemon: every compose environment's state is recomputed from `docker
//! compose ps`, the slices of running environments get their stored limits
//! again, and the whole proxy configuration is pushed in one
//! [`Proxy::apply`](crate::ports::Proxy::apply). Deploys are never
//! touched: the queue may still be running them. Running it again changes
//! nothing.
//!
//! [`Reconciler::startup`] is the start-up half of the reconcile,
//! run once before the REST listener accepts work: deploys left `queued`
//! or `running` by a restart become `failed` ("interrupted by restart"),
//! each environment one of them was running in is put back on its
//! current deploy, the current deploy of every function environment is
//! activated again (active components live in the daemon's memory), every tunnel environment is `offline` (no client is connected yet), then [`Reconciler::run`].

use std::sync::Arc;

use super::Authorizer;
use super::deploy::DeployService;
use super::settings::Settings;
use super::support::{apply_proxy, docker_ok, project_of};
use crate::domain::{
    ComposeProject, Deploy, DeployStatus, EnvKind, EnvState, Environment, SliceName, User,
};
use crate::error::Error;
use crate::ports::{Action, Clock as _, Docker as _, Ports, Resource, Store, Systemd as _};

/// The `error` of a deploy a restart cut short.
const INTERRUPTED: &str = "interrupted by restart";

/// See the [module documentation](self).
#[derive(Debug)]
pub struct Reconciler<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for Reconciler<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> Reconciler<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// `lof admin reconcile`: [`Reconciler::run`] for an admin.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] unless `actor` is an admin (nothing runs);
    /// otherwise as [`Reconciler::run`].
    pub async fn reconcile(&self, actor: &User) -> Result<(), Error> {
        Authorizer::new(self.ports.as_ref())
            .check(actor, Action::AdminReconcile, &Resource::Admin)
            .await?;
        tracing::info!(by = %actor.login, "reconciling");
        self.run().await
    }

    /// Start-up reconcile (v1 QUE-05/QUE-07): every `queued` or
    /// `running` deploy becomes `failed` with `error = "interrupted by
    /// restart"`, the environments of the `running` ones go back to their
    /// current deploy (a queued redeploy; `compose down` for one that is
    /// stopped or never succeeded), every function environment's current
    /// component is activated again, every tunnel is `offline`, then
    /// [`Reconciler::run`].
    /// Call it before the queue accepts work, never on a live daemon.
    ///
    /// # Errors
    ///
    /// The store's error from failing the deploys (nothing else runs);
    /// otherwise the first error of the restore or of `run`, which still
    /// does its part when the restore fails.
    pub async fn startup(&self) -> Result<(), Error> {
        let interrupted = self.fail_interrupted_deploys().await?;
        let recovered = self.recover_interrupted(interrupted).await;
        let reactivated = self.reactivate_functions().await;
        let offline = self.tunnels_offline().await;
        let ran = self.run().await;
        recovered.and(reactivated).and(offline).and(ran)
    }

    /// Every tunnel environment is `offline`: no client is connected to a
    /// daemon that just started. `run`
    /// pushes the proxy configuration with their «dev offline» pages.
    async fn tunnels_offline(&self) -> Result<(), Error> {
        let store = self.ports.store();
        for environment in store.environments().await? {
            if environment.kind != EnvKind::Tunnel || environment.state == EnvState::Offline {
                continue;
            }
            let offline = Environment {
                state: EnvState::Offline,
                ..environment
            };
            store.update_environment(&offline).await?;
        }
        Ok(())
    }

    /// Every function environment with a current deploy gets its
    /// component active again (running or stopped: `start` needs it
    /// active); each is tried, the first failure is returned.
    async fn reactivate_functions(&self) -> Result<(), Error> {
        let deploys = DeployService::new(Arc::clone(&self.ports), Arc::clone(&self.settings));
        let mut first_error = None;
        for environment in self.ports.store().environments().await? {
            if environment.kind != EnvKind::Function || environment.current_deploy_id.is_none() {
                continue;
            }
            let outcome = deploys.reactivate(&environment).await;
            if let Err(error) = &outcome {
                tracing::error!(%error, environment = %environment.id, "a function could not be activated at start");
            }
            keep_first(&mut first_error, outcome);
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Marks every `queued` or `running` deploy `failed`. Returns the ones
    /// that were `running`: a `queued` one never touched docker.
    async fn fail_interrupted_deploys(&self) -> Result<Vec<Deploy>, Error> {
        let store = self.ports.store();
        let now = self.ports.clock().now();
        let stale = store
            .deploys_with_status(&[DeployStatus::Queued, DeployStatus::Running])
            .await?;
        let mut touched_docker = Vec::new();
        for mut deploy in stale {
            tracing::warn!(deploy = %deploy.id, "failing a deploy interrupted by a restart");
            let was_running = deploy.status == DeployStatus::Running;
            deploy.status = DeployStatus::Failed;
            deploy.error = Some(INTERRUPTED.to_owned());
            deploy.finished_at = Some(now);
            store.update_deploy(&deploy).await?;
            if was_running {
                touched_docker.push(deploy);
            }
        }
        Ok(touched_docker)
    }

    /// Puts the host back where the store says it is for every environment
    /// one of `interrupted` was running in. A hard kill leaves the
    /// containers of a cut-short `compose up` next to a store that still
    /// names the previous deploy:
    ///
    /// - a stopped environment stays stopped: `compose down` when
    ///   containers are up;
    /// - an environment with a current deploy gets it back as a queued
    ///   redeploy (`rolled_back_to` = its number), and the interrupted
    ///   deploy's log says so;
    /// - an environment that never succeeded gets `compose down`, so
    ///   nothing half-deployed runs.
    /// - a function environment asks no docker and queues nothing: the
    ///   start activates its current component again, and the
    ///   interrupted deploy's log says so.
    ///
    /// A docker that does not answer queues nothing: a redeploy would fail
    /// too. Every environment is tried; the first failure is returned.
    async fn recover_interrupted(&self, mut interrupted: Vec<Deploy>) -> Result<(), Error> {
        // The newest interrupted deploy speaks for its environment.
        interrupted
            .sort_by_key(|deploy| (deploy.environment_id, core::cmp::Reverse(deploy.number)));
        interrupted.dedup_by_key(|deploy| deploy.environment_id);
        let mut first_error = None;
        for deploy in interrupted {
            let outcome = self.recover_environment(&deploy).await;
            keep_first(&mut first_error, outcome);
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn recover_environment(&self, interrupted: &Deploy) -> Result<(), Error> {
        let store = self.ports.store();
        let Some(environment) = store.environment(interrupted.environment_id).await? else {
            return Ok(());
        };
        if environment.kind != EnvKind::Compose {
            // A function has no containers and nothing to redeploy: its
            // current component is activated again with every other
            // function's (`reactivate_functions`). A tunnel never deploys.
            note_restore(
                store,
                interrupted,
                &format!(
                    "deploy #{} interrupted by a restart, the current component is activated again",
                    interrupted.number
                ),
            )
            .await;
            return Ok(());
        }
        let project = project_of(self.ports.as_ref(), &environment).await?;
        let compose = ComposeProject::new(&project.name, &environment.name);
        let current = match environment.current_deploy_id {
            Some(id) if environment.state != EnvState::Stopped => store.deploy(id).await?,
            _ => None,
        };
        let running = self.ports.docker().compose_ps(&compose).await?;
        let note = if let Some(current) = current {
            let number = DeployService::new(Arc::clone(&self.ports), Arc::clone(&self.settings))
                .restore_current(&current, interrupted)
                .await?;
            format!(
                "deploy #{} interrupted by a restart, restoring #{} (deploy #{number})",
                interrupted.number, current.number
            )
        } else {
            if !running.is_empty() {
                docker_ok(&self.ports.docker().compose_down(&compose, false).await?)?;
            }
            format!(
                "deploy #{} interrupted by a restart, containers taken down",
                interrupted.number
            )
        };
        note_restore(store, interrupted, &note).await;
        Ok(())
    }

    /// Brings the host and the store back in line, in this order:
    ///
    /// 1. each compose environment's state from `compose_ps`: containers
    ///    mean `running` (a `failed` one stays `failed`, its containers are
    ///    kept for debugging), none mean `stopped` after a succeeded
    ///    deploy and `empty` otherwise;
    /// 2. each running environment's slice gets its stored limits;
    /// 3. the whole proxy configuration, in one push.
    ///
    /// A failing step is logged and does not stop the others; the first
    /// failure is returned at the end.
    ///
    /// # Errors
    ///
    /// The first error of any step; the store's errors from reading the
    /// environments abort the run instead.
    pub async fn run(&self) -> Result<(), Error> {
        let mut first_error = None;
        for environment in self.ports.store().environments().await? {
            let outcome = self.reconcile_environment(environment).await;
            keep_first(&mut first_error, outcome);
        }
        keep_first(
            &mut first_error,
            apply_proxy(self.ports.as_ref(), &self.settings).await,
        );
        first_error.map_or(Ok(()), Err)
    }

    /// State and slice of one environment.
    async fn reconcile_environment(&self, mut environment: Environment) -> Result<(), Error> {
        // A function or a tunnel has no compose project and no slice: docker says nothing about it, its state is its listener's.
        let EnvKind::Compose = environment.kind else {
            return Ok(());
        };
        let store = self.ports.store();
        let project = project_of(self.ports.as_ref(), &environment).await?;
        let compose = ComposeProject::new(&project.name, &environment.name);
        let has_containers = !self.ports.docker().compose_ps(&compose).await?.is_empty();
        let state = if has_containers {
            if environment.state == EnvState::Failed {
                EnvState::Failed
            } else {
                EnvState::Running
            }
        } else if store
            .deploys_of_environment(environment.id)
            .await?
            .iter()
            .any(|deploy| deploy.status == DeployStatus::Succeeded)
        {
            EnvState::Stopped
        } else {
            EnvState::Empty
        };
        if state != environment.state {
            tracing::warn!(
                environment = %compose,
                from = %environment.state,
                to = %state,
                "environment state corrected from docker"
            );
            environment.state = state;
            store.update_environment(&environment).await?;
        }
        if environment.state == EnvState::Running {
            self.ports
                .systemd()
                .set_slice_limits(
                    &SliceName::new(&project.name, &environment.name),
                    environment.limits,
                )
                .await?;
        }
        Ok(())
    }
}

/// Logs a failure and remembers it unless an earlier one is kept.
/// Adds `note` to the log of the `interrupted` deploy; a store that
/// refuses is only logged.
async fn note_restore<S: Store>(store: &S, interrupted: &Deploy, note: &str) {
    let mut noted = interrupted.clone();
    noted.log.push_str(note);
    noted.log.push('\n');
    if let Err(error) = store.update_deploy(&noted).await {
        tracing::warn!(%error, deploy = %interrupted.id, "could not note the restore in the deploy log");
    }
}

fn keep_first(slot: &mut Option<Error>, result: Result<(), Error>) {
    if let Err(error) = result {
        tracing::error!(%error, "reconcile step failed");
        slot.get_or_insert(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UserRole;
    use crate::fakes::{DockerCall, FakePorts, World};
    use crate::ports::ServiceStatus;

    fn reconciler(world: &World) -> Reconciler<FakePorts> {
        Reconciler::new(Arc::clone(&world.ports), Arc::clone(&world.settings))
    }

    fn web_running() -> Vec<ServiceStatus> {
        vec![ServiceStatus {
            service: "web".to_owned(),
            state: "running".to_owned(),
            health: None,
        }]
    }

    mod reconcile {
        use super::*;

        // TC: REST-09 (the whole proxy config in one push, no deploys)
        #[tokio::test]
        async fn one_proxy_push_without_deploys_when_nothing_changed() {
            let world = World::new().await;
            reconciler(&world).reconcile(&world.owner).await.unwrap();
            assert_eq!(
                (
                    world.ports.proxy.apply_count(),
                    world.ports.queue.enqueued().len()
                ),
                (1, 0)
            );
        }

        // TC: REST-09
        #[tokio::test]
        async fn forbidden_and_nothing_pushed_when_not_admin() {
            let world = World::new().await;
            let carol = world.add_user("carol", UserRole::User).await.unwrap();
            let error = reconciler(&world).reconcile(&carol).await.unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.ports.proxy.apply_count() == 0,
                "{error:?}"
            );
        }

        // TC: REST-09 (the state comes from docker)
        #[tokio::test]
        async fn running_with_its_limits_when_containers_found() {
            let world = World::new().await;
            world.ports.docker.set_ps(
                &ComposeProject::new(&world.shop.name, &world.qa.name),
                web_running(),
            );
            reconciler(&world).run().await.unwrap();
            let qa = world
                .ports
                .store
                .environment(world.qa.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    qa.state,
                    world
                        .ports
                        .systemd
                        .limits(&SliceName::new(&world.shop.name, &world.qa.name))
                ),
                (EnvState::Running, Some(qa.limits))
            );
        }

        // TC: REST-09
        #[tokio::test]
        async fn stopped_when_deployed_but_no_containers() {
            let world = World::new().await;
            let _prod = world
                .deployed(&world.prod, EnvState::Running)
                .await
                .unwrap();
            reconciler(&world).run().await.unwrap();
            let prod = world
                .ports
                .store
                .environment(world.prod.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(prod.state, EnvState::Stopped);
        }

        // TC: VIS-01 (a tunnel has no compose project or slice: docker
        // and systemd are not asked and its state stays)
        #[tokio::test]
        async fn leaves_tunnel_alone_when_reconciled() {
            let world = World::new().await;
            let mut tunnel = world
                .add_tunnel(&world.shop, "shop-dev", &world.owner)
                .await
                .unwrap();
            tunnel.state = EnvState::Offline;
            world.ports.store.update_environment(&tunnel).await.unwrap();
            reconciler(&world).run().await.unwrap();
            let stored = world
                .ports
                .store
                .environment(tunnel.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (
                    stored.state,
                    world
                        .ports
                        .docker
                        .calls()
                        .contains(&DockerCall::Ps(ComposeProject::new(
                            &world.shop.name,
                            &tunnel.name
                        )))
                ),
                (EnvState::Offline, false)
            );
        }
    }

    mod startup {
        use super::*;
        use crate::domain::{Deploy, DeployNumber, DeploySource};
        use crate::fakes::{Call, DockerCall};

        fn qa_project(world: &World) -> ComposeProject {
            ComposeProject::new(&world.shop.name, &world.qa.name)
        }

        async fn deploy_of(world: &World, deploy: &Deploy) -> Deploy {
            world.ports.store.deploy(deploy.id).await.unwrap().unwrap()
        }

        /// `qa` with a succeeded #1 (current, in `state`) and a `running`
        /// #2 the kill cut short, its containers up.
        async fn killed_mid_deploy(world: &World, state: EnvState) -> Deploy {
            let _qa = world.deployed(&world.qa, state).await.unwrap();
            let cut = world
                .add_deploy(&world.qa, 2, DeployStatus::Running)
                .await
                .unwrap();
            world.ports.docker.set_ps(&qa_project(world), web_running());
            world.log().clear();
            cut
        }

        fn downs(world: &World) -> usize {
            world
                .log()
                .filter(|call| matches!(call, Call::Docker(DockerCall::Down { .. })))
                .len()
        }

        /// The function `api` of `shop` in `state` whose current deploy
        /// #1 is of the kept component `label`; returns it and the
        /// component.
        async fn function_deployed(
            world: &World,
            state: EnvState,
            label: &str,
        ) -> (Environment, Vec<u8>) {
            let component = crate::fakes::FakeFunctions::component(label);
            let hash = world.ports.components.put(&component);
            let api = world.records.function(&world.shop, "api").unwrap();
            world.ports.store.insert_environment(&api).await.unwrap();
            let deploy = Deploy {
                component_hash: Some(hash),
                ..world
                    .records
                    .deploy(&api, 1, DeployStatus::Succeeded, &world.owner)
                    .unwrap()
            };
            world.ports.store.insert_deploy(&deploy).await.unwrap();
            let api = Environment {
                state,
                current_deploy_id: Some(deploy.id),
                ..api
            };
            world.ports.store.update_environment(&api).await.unwrap();
            world.log().clear();
            (api, component)
        }

        // active components live in
        // the daemon's memory, so its start activates them again.
        #[tokio::test]
        async fn current_component_active_when_running_function_restarted() {
            let world = World::new().await;
            let (api, component) = function_deployed(&world, EnvState::Running, "v1").await;
            reconciler(&world).startup().await.unwrap();
            assert_eq!(
                world.ports.functions.active(api.id),
                Some(crate::domain::ComponentHash::of(&component))
            );
        }

        // a stopped function is activated too, so
        // `start` finds its component.
        #[tokio::test]
        async fn current_component_active_when_stopped_function_restarted() {
            let world = World::new().await;
            let (api, component) = function_deployed(&world, EnvState::Stopped, "v1").await;
            reconciler(&world).startup().await.unwrap();
            assert_eq!(
                world.ports.functions.active(api.id),
                Some(crate::domain::ComponentHash::of(&component))
            );
        }

        // a component that does not come back is
        // the start's error and changes nothing stored.
        #[tokio::test]
        async fn error_and_state_kept_when_function_warmup_fails_at_start() {
            let world = World::new().await;
            let (api, _component) = function_deployed(&world, EnvState::Running, "v1").await;
            world
                .ports
                .functions
                .fail_next("activate", Error::invalid("component", "trap"));
            let result = reconciler(&world).startup().await;
            let stored = world
                .ports
                .store
                .environment(api.id)
                .await
                .unwrap()
                .unwrap();
            assert!(
                result.is_err()
                    && stored.state == EnvState::Running
                    && world.ports.functions.active(api.id).is_none(),
                "{result:?} {stored:?}"
            );
        }

        // a function deploy the kill
        // cut short asks no docker and queues no restore; its current
        // component comes back by activation alone.
        #[tokio::test]
        async fn activates_current_without_docker_or_queue_when_function_deploy_interrupted() {
            let world = World::new().await;
            let (api, component) = function_deployed(&world, EnvState::Running, "v1").await;
            let cut = world
                .add_deploy(&api, 2, DeployStatus::Running)
                .await
                .unwrap();
            world.log().clear();
            reconciler(&world).startup().await.unwrap();
            let api_project = ComposeProject::new(&world.shop.name, &api.name);
            let touched = world
                .log()
                .filter(|call| {
                    matches!(call, Call::Enqueue(_))
                        || matches!(call, Call::Docker(DockerCall::Ps(project)) if *project == api_project)
                })
                .len();
            assert_eq!(
                (
                    touched,
                    world.ports.functions.active(api.id),
                    deploy_of(&world, &cut).await.status
                ),
                (
                    0,
                    Some(crate::domain::ComponentHash::of(&component)),
                    DeployStatus::Failed
                )
            );
        }

        // TC: REST-09 (v1 QUE-05: interrupted deploys fail)
        #[tokio::test]
        async fn fails_running_and_queued_deploys_when_daemon_restarted() {
            let world = World::new().await;
            let cut = killed_mid_deploy(&world, EnvState::Running).await;
            let waiting = world
                .add_deploy(&world.qa, 3, DeployStatus::Queued)
                .await
                .unwrap();
            reconciler(&world).startup().await.unwrap();
            let interrupted = Some("interrupted by restart".to_owned());
            let after = [
                deploy_of(&world, &cut).await,
                deploy_of(&world, &waiting).await,
            ];
            assert_eq!(
                after.map(|d| (d.status, d.error, d.finished_at.is_some())),
                [
                    (DeployStatus::Failed, interrupted.clone(), true),
                    (DeployStatus::Failed, interrupted, true),
                ]
            );
        }

        // TC: REST-09 (the admin reconcile leaves deploys alone)
        #[tokio::test]
        async fn keeps_running_deploy_when_daemon_is_live() {
            let world = World::new().await;
            let cut = killed_mid_deploy(&world, EnvState::Running).await;
            reconciler(&world).run().await.unwrap();
            assert_eq!(
                (
                    deploy_of(&world, &cut).await.status,
                    world.ports.queue.enqueued().len()
                ),
                (DeployStatus::Running, 0)
            );
        }

        // TC: REST-09 (v1 QUE-07: rolled back to the current one)
        #[tokio::test]
        async fn queues_redeploy_of_current_when_deploy_was_interrupted() {
            let world = World::new().await;
            let cut = killed_mid_deploy(&world, EnvState::Running).await;
            reconciler(&world).startup().await.unwrap();
            let queued = world.ports.queue.enqueued();
            let restore = world
                .ports
                .store
                .deploys_of_environment(world.qa.id)
                .await
                .unwrap()
                .into_iter()
                .find(|deploy| queued.contains(&deploy.id))
                .expect("a queued redeploy");
            assert_eq!(
                (
                    queued.len(),
                    restore.source,
                    restore.status,
                    restore.rolled_back_to.map(DeployNumber::get),
                    restore.number.get(),
                    deploy_of(&world, &cut).await.log.contains("restoring #1"),
                    downs(&world),
                ),
                (
                    1,
                    DeploySource::Redeploy,
                    DeployStatus::Queued,
                    Some(1),
                    3,
                    true,
                    0
                )
            );
        }

        // TC: REST-09 (v1 QUE-07)
        #[tokio::test]
        async fn takes_project_down_when_no_deploy_ever_succeeded() {
            let world = World::new().await;
            let _cut = world
                .add_deploy(&world.qa, 1, DeployStatus::Running)
                .await
                .unwrap();
            world
                .ports
                .docker
                .set_ps(&qa_project(&world), web_running());
            world.log().clear();
            reconciler(&world).startup().await.unwrap();
            assert_eq!((downs(&world), world.ports.queue.enqueued().len()), (1, 0));
        }

        // TC: REST-09 (v1 QUE-07)
        #[tokio::test]
        async fn keeps_environment_stopped_when_user_had_stopped_it() {
            let world = World::new().await;
            let _cut = killed_mid_deploy(&world, EnvState::Stopped).await;
            reconciler(&world).startup().await.unwrap();
            assert_eq!((downs(&world), world.ports.queue.enqueued().len()), (1, 0));
        }

        // TC: REST-09 (v1 QUE-07: a queued deploy never touched docker)
        #[tokio::test]
        async fn restores_nothing_when_only_a_queued_deploy_was_interrupted() {
            let world = World::new().await;
            let _qa = world.deployed(&world.qa, EnvState::Running).await.unwrap();
            let _waiting = world
                .add_deploy(&world.qa, 2, DeployStatus::Queued)
                .await
                .unwrap();
            world
                .ports
                .docker
                .set_ps(&qa_project(&world), web_running());
            world.log().clear();
            reconciler(&world).startup().await.unwrap();
            assert_eq!((downs(&world), world.ports.queue.enqueued().len()), (0, 0));
        }

        // TC: REST-09 (v1 QUE-07: no redeploy that is bound to fail)
        #[tokio::test]
        async fn queues_nothing_and_reports_when_docker_does_not_answer() {
            let world = World::new().await;
            let cut = killed_mid_deploy(&world, EnvState::Running).await;
            world
                .ports
                .docker
                .fail_next("compose_ps", Error::unavailable("docker"));
            let result = reconciler(&world).startup().await;
            assert_eq!(
                (
                    result.is_err(),
                    world.ports.queue.enqueued().len(),
                    deploy_of(&world, &cut).await.status
                ),
                (true, 0, DeployStatus::Failed)
            );
        }
    }
}
