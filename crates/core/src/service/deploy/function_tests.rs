//! Tests of the function branch of [`DeployService`] (`FDEP-02…06`) over the shared [`World`], with the fake
//! function runtime ([`FakeFunctions`]: a component is any bytes that
//! start with the component preamble; `fail_next("activate", …)` is a
//! warm-up that traps) and the fake kept files ([`FakeComponentFiles`]).
//!
//! Besides the world's `prod` and `qa`, [`function_on_main`] adds the
//! function environment `api` of `shop`, pinned to `main` by `owner`.

use super::*;
use crate::domain::{ComponentHash, DeploySource, DeployStatus, EnvState, Environment};
use crate::fakes::{Call, FakeFunctions, FunctionCall, HookCall, Records, World};

fn project(raw: &str) -> ProjectName {
    ProjectName::new(raw).unwrap()
}

fn env(raw: &str) -> EnvName {
    EnvName::new(raw).unwrap()
}

fn number(raw: u32) -> DeployNumber {
    DeployNumber::new(raw).unwrap()
}

/// What a non-component `[function] component` holds.
const NOT_A_COMPONENT: &[u8] = b"#!/bin/sh\necho this is not wasm\n";

/// The function environment `api` of `shop`, pinned to `main` by
/// `owner`; the log is cleared.
async fn function_on_main(world: &World) -> Environment {
    let mut function = world.records.function(&world.shop, "api").unwrap();
    function.pin = Some(Records::pin("main", &world.owner).unwrap());
    world
        .ports
        .store
        .insert_environment(&function)
        .await
        .unwrap();
    world.log().clear();
    function
}

async fn stored_env(world: &World, environment: &Environment) -> Environment {
    world
        .ports
        .store
        .environment(environment.id)
        .await
        .unwrap()
        .unwrap()
}

async fn deploys_of(world: &World, environment: &Environment) -> Vec<Deploy> {
    world
        .ports
        .store
        .deploys_of_environment(environment.id)
        .await
        .unwrap()
}

/// `owner`'s `lof deploy` of `component` into `shop/api`.
async fn request(world: &World, component: &[u8]) -> Result<QueuedDeploy, Error> {
    world
        .deploys()
        .request_function(
            &world.owner,
            &project("shop"),
            &env("api"),
            component.to_vec(),
        )
        .await
}

/// Requests `component` into `shop/api` as `owner` and runs the deploy.
async fn deploy(world: &World, component: &[u8]) -> Deploy {
    let queued = request(world, component).await.unwrap();
    world.deploys().run(queued.deploy_id).await.unwrap()
}

/// What an earlier deploy left: succeeded deploy `raw` of `environment`
/// of the component `label`, whose file is kept. Returns the deploy and
/// the component.
async fn deployed_before(
    world: &World,
    environment: &Environment,
    raw: u32,
    label: &str,
) -> (Deploy, Vec<u8>) {
    let component = FakeFunctions::component(label);
    let hash = world.ports.components.put(&component);
    let deploy = Deploy {
        component_hash: Some(hash),
        ..world
            .records
            .deploy(environment, raw, DeployStatus::Succeeded, &world.owner)
            .unwrap()
    };
    world.ports.store.insert_deploy(&deploy).await.unwrap();
    (deploy, component)
}

/// Makes `deploy` of `component` the running, active one of
/// `environment`; the log is cleared.
async fn make_current(world: &World, environment: &Environment, deploy: &Deploy, component: &[u8]) {
    let mut current = stored_env(world, environment).await;
    current.current_deploy_id = Some(deploy.id);
    current.state = EnvState::Running;
    world
        .ports
        .store
        .update_environment(&current)
        .await
        .unwrap();
    world.ports.functions.set_active(environment.id, component);
    world.log().clear();
}

/// The function calls (runtime and kept files) logged so far, in order.
fn function_calls(world: &World) -> Vec<FunctionCall> {
    world
        .log()
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Function(call) => Some(call),
            _ => None,
        })
        .collect()
}

/// The warm-up of the next activation traps.
fn warm_up_traps(world: &World) {
    world.ports.functions.fail_next(
        "activate",
        Error::invalid(
            "component",
            "the function's warm-up failed: the function trapped: wasm trap: wasm `unreachable` instruction executed",
        ),
    );
}

mod request_function {
    use super::*;

    // TC: FDEP-02
    #[tokio::test]
    async fn validation_naming_component_when_bytes_are_not_a_component() {
        let world = World::new().await;
        let _api = function_on_main(&world).await;
        let result = request(&world, NOT_A_COMPONENT).await;
        assert!(
            matches!(&result, Err(Error::Validation(e)) if e.field == "component"),
            "{result:?}"
        );
    }

    // TC: FDEP-02 (refused before anything is written: what an earlier
    // deploy kept and queued is all there is)
    #[tokio::test]
    async fn keeps_no_file_and_queues_nothing_when_bytes_are_not_a_component() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let earlier = FakeFunctions::component("v1");
        let _queued = request(&world, &earlier).await.unwrap();
        let _refused = request(&world, NOT_A_COMPONENT).await;
        assert_eq!(
            (
                world.ports.components.hashes(),
                deploys_of(&world, &api).await.len(),
                world.ports.queue.enqueued().len()
            ),
            (vec![ComponentHash::of(&earlier)], 1, 1)
        );
    }

    // TC: FDEP-02 (the check comes before the file is kept)
    #[tokio::test]
    async fn checks_component_before_keeping_it_when_requested() {
        let world = World::new().await;
        let _api = function_on_main(&world).await;
        let component = FakeFunctions::component("v1");
        let hash = ComponentHash::of(&component);
        let _queued = request(&world, &component).await.unwrap();
        assert_eq!(
            function_calls(&world),
            vec![FunctionCall::Check(hash.clone()), FunctionCall::Save(hash)]
        );
    }

    // TC: FDEP-03 (the file by its hash)
    #[tokio::test]
    async fn keeps_component_under_its_hash_when_requested() {
        let world = World::new().await;
        let _api = function_on_main(&world).await;
        let component = FakeFunctions::component("v1");
        let _queued = request(&world, &component).await.unwrap();
        assert_eq!(
            world.ports.components.file(&ComponentHash::of(&component)),
            Some(component)
        );
    }

    // TC: FDEP-03 (`deploys.component_hash`, `source = cli`, queued)
    #[tokio::test]
    async fn queues_cli_deploy_with_component_hash_when_requested() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let component = FakeFunctions::component("v1");
        let queued = request(&world, &component).await.unwrap();
        let stored = world
            .ports
            .store
            .deploy(queued.deploy_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                queued.environment.to_string(),
                stored.environment_id,
                stored.source,
                stored.status,
                stored.component_hash,
                world.ports.queue.enqueued()
            ),
            (
                "api".to_owned(),
                api.id,
                DeploySource::Cli,
                DeployStatus::Queued,
                Some(ComponentHash::of(&component)),
                vec![queued.deploy_id]
            )
        );
    }

    // TC: FDEP-03 (a component goes to a function environment only)
    #[tokio::test]
    async fn validation_naming_kind_and_nothing_kept_when_environment_is_compose() {
        let world = World::new().await;
        let result = world
            .deploys()
            .request_function(
                &world.owner,
                &project("shop"),
                &env("prod"),
                FakeFunctions::component("v1"),
            )
            .await;
        assert!(
            matches!(&result, Err(Error::Validation(e)) if e.field == "kind")
                && world.ports.components.hashes().is_empty()
                && world.ports.queue.enqueued().is_empty(),
            "{result:?}"
        );
    }
}

mod request_ci_function {
    use super::*;

    /// What the function workflow of `owner/shop` sends after a push of
    /// `main`.
    fn ci(component: &[u8]) -> CiFunctionRequest {
        CiFunctionRequest {
            repo: RepoRef::new("owner/shop").unwrap(),
            git_ref: Branch::new("main").unwrap(),
            component: component.to_vec(),
        }
    }

    // TC: FDEP-03 (from CI the same through the deploy token; `prod`, a compose environment of `main`, gets no component)
    #[tokio::test]
    async fn queues_function_environment_of_pushed_branch_only_when_ci_sends_component() {
        let world = World::new().await;
        let token = world.deploy_token(&world.shop).await.unwrap();
        let _api = function_on_main(&world).await;
        let outcome = world
            .deploys()
            .request_ci_function(&token, ci(&FakeFunctions::component("v1")))
            .await
            .unwrap();
        let environments: Vec<String> = outcome
            .deploys
            .iter()
            .map(|queued| queued.environment.to_string())
            .collect();
        assert_eq!(
            (environments, deploys_of(&world, &world.prod).await.len()),
            (vec!["api".to_owned()], 0)
        );
    }

    // TC: FDEP-02 (from CI as well)
    #[tokio::test]
    async fn validation_and_nothing_kept_when_ci_sends_non_component() {
        let world = World::new().await;
        let token = world.deploy_token(&world.shop).await.unwrap();
        let api = function_on_main(&world).await;
        let result = world
            .deploys()
            .request_ci_function(&token, ci(NOT_A_COMPONENT))
            .await;
        assert!(
            matches!(&result, Err(Error::Validation(e)) if e.field == "component")
                && world.ports.components.hashes().is_empty()
                && deploys_of(&world, &api).await.is_empty(),
            "{result:?}"
        );
    }
}

mod run_function {
    use super::*;

    // TC: FDEP-03
    #[tokio::test]
    async fn succeeded_when_warm_up_passes() {
        let world = World::new().await;
        let _api = function_on_main(&world).await;
        let done = deploy(&world, &FakeFunctions::component("v1")).await;
        assert_eq!(
            (done.status, done.error),
            (DeployStatus::Succeeded, None),
            "{}",
            done.log
        );
    }

    // TC: FDEP-03 (the route switches to the new component)
    #[tokio::test]
    async fn activates_new_component_when_run() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let (before, old) = deployed_before(&world, &api, 1, "v1").await;
        make_current(&world, &api, &before, &old).await;
        let component = FakeFunctions::component("v2");
        let _done = deploy(&world, &component).await;
        assert_eq!(
            world.ports.functions.active(api.id),
            Some(ComponentHash::of(&component))
        );
    }

    // TC: FDEP-03 (the environment runs the new deploy)
    #[tokio::test]
    async fn environment_runs_new_deploy_when_run() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let done = deploy(&world, &FakeFunctions::component("v1")).await;
        let stored = stored_env(&world, &api).await;
        assert_eq!(
            (stored.state, stored.current_deploy_id),
            (EnvState::Running, Some(done.id))
        );
    }

    // TC: FDEP-03 (kept, compiled ahead, warmed up, switched)
    #[tokio::test]
    async fn precompiles_kept_component_before_activating_it_when_run() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let component = FakeFunctions::component("v1");
        let hash = ComponentHash::of(&component);
        let queued = request(&world, &component).await.unwrap();
        world.log().clear();
        let _done = world.deploys().run(queued.deploy_id).await.unwrap();
        assert_eq!(
            function_calls(&world)
                .into_iter()
                .filter(|call| !matches!(call, FunctionCall::Remove(_)))
                .collect::<Vec<_>>(),
            vec![
                FunctionCall::Precompile(hash.clone()),
                FunctionCall::Activate {
                    environment: api.id,
                    component: hash
                },
            ]
        );
    }

    // TC: FDEP-03 (the warm-up runs with the secrets in `wasi:config`)
    #[tokio::test]
    async fn activates_with_secrets_of_environment_as_config_when_run() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let _done = deploy(&world, &FakeFunctions::component("v1")).await;
        let config = world.ports.functions.config(api.id).unwrap_or_default();
        let database = config
            .vars()
            .iter()
            .find(|(key, _)| key.as_str() == "DATABASE_URL")
            .map(|(_, value)| value.expose().to_owned());
        assert_eq!(database.as_deref(), Some("postgres://db/shop"));
    }

    // TC: FDEP-03 (no compose project, no slice)
    #[tokio::test]
    async fn touches_neither_docker_nor_slice_nor_compose_files_when_run() {
        let world = World::new().await;
        let _api = function_on_main(&world).await;
        let _done = deploy(&world, &FakeFunctions::component("v1")).await;
        let host = world
            .log()
            .filter(|call| matches!(call, Call::Docker(_) | Call::Systemd(_) | Call::Files(_)));
        assert_eq!(host, Vec::<Call>::new());
    }

    // TC: FDEP-03 (the queue runs a function deploy like a compose one: hook `before_deploy` may refuse it)
    #[tokio::test]
    async fn failed_and_nothing_activated_when_before_deploy_denies() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        world
            .ports
            .hooks
            .answer_next("before_deploy", HookResult::Deny("frozen".to_owned()));
        let done = deploy(&world, &FakeFunctions::component("v1")).await;
        assert_eq!(
            (done.status, world.ports.functions.active(api.id)),
            (DeployStatus::Failed, None)
        );
    }

    // TC: FDEP-03 (hook `after_deploy` of the succeeded deploy)
    #[tokio::test]
    async fn after_deploy_hook_of_deploy_when_succeeded() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let done = deploy(&world, &FakeFunctions::component("v1")).await;
        assert!(
            world
                .log()
                .calls()
                .contains(&Call::Hook(HookCall::AfterDeploy {
                    environment: api.id,
                    deploy: done.id
                })),
            "{:?}",
            world.log().calls()
        );
    }

    // TC: FDEP-04
    #[tokio::test]
    async fn failed_with_warm_up_reason_when_warm_up_traps() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let (before, old) = deployed_before(&world, &api, 1, "v1").await;
        make_current(&world, &api, &before, &old).await;
        warm_up_traps(&world);
        let done = deploy(&world, &FakeFunctions::component("v2")).await;
        assert!(
            done.status == DeployStatus::Failed
                && done
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("warm-up"),
            "{done:?}"
        );
    }

    // TC: FDEP-04 (the route stays on the previous component)
    #[tokio::test]
    async fn previous_component_stays_active_when_warm_up_traps() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let (before, old) = deployed_before(&world, &api, 1, "v1").await;
        make_current(&world, &api, &before, &old).await;
        warm_up_traps(&world);
        let _done = deploy(&world, &FakeFunctions::component("v2")).await;
        assert_eq!(
            world.ports.functions.active(api.id),
            Some(ComponentHash::of(&old))
        );
    }

    // TC: FDEP-04 (the environment keeps running the previous deploy)
    #[tokio::test]
    async fn environment_keeps_previous_deploy_when_warm_up_traps() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let (before, old) = deployed_before(&world, &api, 1, "v1").await;
        make_current(&world, &api, &before, &old).await;
        warm_up_traps(&world);
        let _done = deploy(&world, &FakeFunctions::component("v2")).await;
        let stored = stored_env(&world, &api).await;
        assert_eq!(
            (stored.state, stored.current_deploy_id),
            (EnvState::Running, Some(before.id))
        );
    }

    // TC: FDEP-04 (nothing changed, so nothing is rolled back: no
    // rollback deploy as after a failed compose `up`)
    #[tokio::test]
    async fn no_rollback_deploy_when_warm_up_traps() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let (before, old) = deployed_before(&world, &api, 1, "v1").await;
        make_current(&world, &api, &before, &old).await;
        warm_up_traps(&world);
        let done = deploy(&world, &FakeFunctions::component("v2")).await;
        let numbers: Vec<u32> = deploys_of(&world, &api)
            .await
            .iter()
            .map(|deploy| deploy.number.get())
            .collect();
        assert_eq!(
            (done.number.get(), done.rolled_back_to, numbers),
            (2, None, vec![2, 1])
        );
    }
}

mod redeploy_function {
    use super::*;

    /// `api` with deploys #1 (`v1`) and #2 (`v2`, current) succeeded;
    /// returns the component of #1.
    async fn two_deploys(world: &World, api: &Environment) -> Vec<u8> {
        let (_first, v1) = deployed_before(world, api, 1, "v1").await;
        let (second, v2) = deployed_before(world, api, 2, "v2").await;
        make_current(world, api, &second, &v2).await;
        v1
    }

    // TC: FDEP-05
    #[tokio::test]
    async fn activates_component_of_deploy_n_when_redeployed_to_n() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let v1 = two_deploys(&world, &api).await;
        let queued = world
            .deploys()
            .redeploy(&world.owner, &project("shop"), &env("api"), Some(number(1)))
            .await
            .unwrap();
        let done = world.deploys().run(queued.deploy_id).await.unwrap();
        assert_eq!(
            (done.status, world.ports.functions.active(api.id)),
            (DeployStatus::Succeeded, Some(ComponentHash::of(&v1)))
        );
    }

    // TC: FDEP-05 (the copy carries the component_hash of #N)
    #[tokio::test]
    async fn queues_redeploy_with_component_hash_of_n_when_redeployed_to_n() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let v1 = two_deploys(&world, &api).await;
        let queued = world
            .deploys()
            .redeploy(&world.owner, &project("shop"), &env("api"), Some(number(1)))
            .await
            .unwrap();
        let stored = world
            .ports
            .store
            .deploy(queued.deploy_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (stored.number.get(), stored.source, stored.component_hash),
            (3, DeploySource::Redeploy, Some(ComponentHash::of(&v1)))
        );
    }
}

mod component_cleanup {
    use super::*;

    /// `api` with the world's 20 deploys of history filled: #1…#20 of the
    /// components `v1`…`v20`, #20 current. Returns the components by
    /// number (index 0 is #1).
    async fn full_history(world: &World, api: &Environment) -> Vec<Vec<u8>> {
        let mut components = Vec::new();
        let mut last = None;
        for raw in 1..=20 {
            let (deployed, component) = deployed_before(world, api, raw, &format!("v{raw}")).await;
            components.push(component);
            last = Some(deployed);
        }
        let last = last.unwrap();
        let current = components.last().unwrap().clone();
        make_current(world, api, &last, &current).await;
        components
    }

    // TC: FDEP-06 (#1 leaves the last 20 when #21 succeeds)
    #[tokio::test]
    async fn removes_file_of_deploy_out_of_history_when_run() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let components = full_history(&world, &api).await;
        let _done = deploy(&world, &FakeFunctions::component("v21")).await;
        assert_eq!(
            world
                .ports
                .components
                .file(&ComponentHash::of(&components[0])),
            None
        );
    }

    // TC: FDEP-06 (the files of #2…#21 stay)
    #[tokio::test]
    async fn keeps_files_of_last_twenty_deploys_when_run() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let components = full_history(&world, &api).await;
        let newest = FakeFunctions::component("v21");
        let _done = deploy(&world, &newest).await;
        let mut expected: Vec<ComponentHash> = components
            .iter()
            .skip(1)
            .chain(core::iter::once(&newest))
            .map(|component| ComponentHash::of(component))
            .collect();
        expected.sort();
        assert_eq!(world.ports.components.hashes(), expected);
    }

    // TC: FDEP-06 (a file another environment's deploy refers to stays)
    #[tokio::test]
    async fn keeps_file_another_environment_refers_to_when_run() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let components = full_history(&world, &api).await;
        let staging = world.records.function(&world.shop, "staging").unwrap();
        world
            .ports
            .store
            .insert_environment(&staging)
            .await
            .unwrap();
        let shared = Deploy {
            component_hash: Some(ComponentHash::of(&components[0])),
            ..world
                .records
                .deploy(&staging, 1, DeployStatus::Succeeded, &world.owner)
                .unwrap()
        };
        world.ports.store.insert_deploy(&shared).await.unwrap();
        let _done = deploy(&world, &FakeFunctions::component("v21")).await;
        assert_eq!(
            world
                .ports
                .components
                .file(&ComponentHash::of(&components[0])),
            Some(components[0].clone())
        );
    }

    // TC: FDEP-06 (a failed deploy trims the history and its files too:
    // failed uploads cannot pile up on disk)
    #[tokio::test]
    async fn removes_file_of_deploy_out_of_history_when_deploy_fails() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let components = full_history(&world, &api).await;
        warm_up_traps(&world);
        let done = deploy(&world, &FakeFunctions::component("v21")).await;
        assert_eq!(
            (
                done.status,
                world
                    .ports
                    .components
                    .file(&ComponentHash::of(&components[0])),
                deploys_of(&world, &api).await.len()
            ),
            (DeployStatus::Failed, None, 20)
        );
    }

    // TC: FDEP-06 (a kept file no deploy refers to at all goes too)
    #[tokio::test]
    async fn removes_file_no_deploy_refers_to_when_run() {
        let world = World::new().await;
        let _api = function_on_main(&world).await;
        let stray = world
            .ports
            .components
            .put(&FakeFunctions::component("left over"));
        let _done = deploy(&world, &FakeFunctions::component("v1")).await;
        assert_eq!(world.ports.components.file(&stray), None);
    }
}

mod keeping_files {
    use super::*;

    // TC: FDEP-06 (a component that cannot be kept fails its deploy and
    // queues nothing)
    #[tokio::test]
    async fn failed_and_not_queued_when_component_cannot_be_kept() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        world
            .ports
            .components
            .fail_next("save", Error::internal_msg("disk full"));
        let result = request(&world, &FakeFunctions::component("v1")).await;
        let stored: Vec<DeployStatus> = deploys_of(&world, &api)
            .await
            .iter()
            .map(|deploy| deploy.status)
            .collect();
        assert_eq!(
            (result.is_err(), stored, world.ports.queue.enqueued().len()),
            (true, vec![DeployStatus::Failed], 0)
        );
    }

    // TC: FDEP-06 (the file of a deploy still in the queue stays when
    // another deploy finishes)
    #[tokio::test]
    async fn keeps_file_of_queued_deploy_when_another_deploy_succeeds() {
        let world = World::new().await;
        let _api = function_on_main(&world).await;
        let waiting = FakeFunctions::component("queued");
        let _queued = request(&world, &waiting).await.unwrap();
        let _done = deploy(&world, &FakeFunctions::component("v2")).await;
        assert_eq!(
            world.ports.components.file(&ComponentHash::of(&waiting)),
            Some(waiting)
        );
    }
}

/// The kept files are written and cleaned under one lock
/// ([`ComponentsLock`](crate::ports::ComponentsLock)): a request keeps
/// its rows and file, a redeploy picks and copies its deploy, and a
/// cleanup lists, reads and removes, each without the others in
/// between. A test holds the lock, polls the operation once and sees
/// it wait (the fakes answer at once, so a pending poll is the lock).
mod components_lock {
    use super::*;

    use core::pin::pin;
    use futures_util::FutureExt as _;

    // TC: FDEP-06 (a request stores nothing while a cleanup runs)
    #[tokio::test]
    async fn request_stores_nothing_while_lock_is_held() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let held = world.ports.components.lock().hold().await;
        let component = FakeFunctions::component("v1");
        let mut requested = pin!(request(&world, &component));
        let polled = (&mut requested).now_or_never();
        let while_held = (polled.is_none(), deploys_of(&world, &api).await.len());
        drop(held);
        let queued = requested.await;
        assert_eq!(
            (
                while_held,
                queued.is_ok(),
                deploys_of(&world, &api).await.len()
            ),
            ((true, 0), true, 1)
        );
    }

    // TC: FDEP-06 (a cleanup removes nothing while a request keeps its
    // file)
    #[tokio::test]
    async fn cleanup_removes_nothing_while_lock_is_held() {
        let world = World::new().await;
        let _api = function_on_main(&world).await;
        let stray = world
            .ports
            .components
            .put(&FakeFunctions::component("left over"));
        let queued = request(&world, &FakeFunctions::component("v1"))
            .await
            .unwrap();
        let held = world.ports.components.lock().hold().await;
        let deploys = world.deploys();
        let mut running = pin!(deploys.run(queued.deploy_id));
        let polled = (&mut running).now_or_never();
        let while_held = (
            polled.is_none(),
            world.ports.components.file(&stray).is_some(),
        );
        drop(held);
        let _done = running.await.unwrap();
        assert_eq!(
            (while_held, world.ports.components.file(&stray)),
            ((true, true), None)
        );
    }

    // TC: FDEP-05, FDEP-06 (a redeploy copies a component's hash only
    // between cleanups)
    #[tokio::test]
    async fn redeploy_stores_nothing_while_lock_is_held() {
        let world = World::new().await;
        let api = function_on_main(&world).await;
        let (_first, _v1) = deployed_before(&world, &api, 1, "v1").await;
        let (second, v2) = deployed_before(&world, &api, 2, "v2").await;
        make_current(&world, &api, &second, &v2).await;
        let held = world.ports.components.lock().hold().await;
        let deploys = world.deploys();
        let owner = world.owner.clone();
        let (shop, name) = (project("shop"), env("api"));
        let mut redeployed = pin!(deploys.redeploy(&owner, &shop, &name, Some(number(1))));
        let polled = (&mut redeployed).now_or_never();
        let while_held = (polled.is_none(), deploys_of(&world, &api).await.len());
        drop(held);
        let queued = redeployed.await;
        assert_eq!(
            (
                while_held,
                queued.is_ok(),
                deploys_of(&world, &api).await.len()
            ),
            ((true, 2), true, 3)
        );
    }
}
