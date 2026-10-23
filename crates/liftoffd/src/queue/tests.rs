//! Tests of [`JobQueue`] over the shared `World` (DEP-11 and the
//! v1 queue cases QUE-01..QUE-04 it ports).
//!
//! The runner stands in for `DeployService::run`, reduced to what the
//! queue can observe: the deploy turns `running`, one `compose_up` of its
//! environment happens (held by a gate when a test needs overlap), the
//! deploy turns `succeeded`.

use core::time::Duration;
use std::sync::{Arc, Mutex};

use liftoff_core::domain::{ComposeProject, Deploy, DeployId, DeployStatus, EnvState, Environment};
use liftoff_core::error::Error;
use liftoff_core::fakes::{FakePorts, FixedClock, InMemoryStore, World};
use liftoff_core::ports::Docker as _;

use super::*;

type Order = Arc<Mutex<Vec<DeployId>>>;
type BoxedRunner = Box<dyn Fn(DeployId) -> RunFuture + Send + Sync>;

async fn fake_run(ports: Arc<FakePorts>, order: Order, id: DeployId) -> Result<(), Error> {
    order.lock().unwrap().push(id);
    let mut deploy = ports.store.deploy(id).await?.unwrap();
    deploy.status = DeployStatus::Running;
    ports.store.update_deploy(&deploy).await?;
    let environment = ports
        .store
        .environment(deploy.environment_id)
        .await?
        .unwrap();
    let project = ports.store.project(environment.project_id).await?.unwrap();
    ports
        .docker
        .compose_up(
            &ComposeProject::new(&project.name, &environment.name),
            Duration::from_secs(1),
        )
        .await?;
    deploy.status = DeployStatus::Succeeded;
    ports.store.update_deploy(&deploy).await?;
    Ok(())
}

struct Rig {
    world: World,
    queue: JobQueue<InMemoryStore, BoxedRunner, FixedClock>,
    order: Order,
}

async fn rig(parallel: usize) -> Rig {
    let world = World::new().await;
    let order = Order::default();
    let (ports, seen) = (Arc::clone(&world.ports), Arc::clone(&order));
    let runner: BoxedRunner =
        Box::new(move |id| Box::pin(fake_run(Arc::clone(&ports), Arc::clone(&seen), id)));
    let queue = JobQueue::new(
        Arc::new(world.ports.store.clone()),
        Arc::new(world.ports.clock.clone()),
        runner,
        parallel,
    );
    Rig {
        world,
        queue,
        order,
    }
}

/// Stores the next deploy of `environment` as `queued`.
async fn queued(rig: &Rig, environment: &Environment) -> Deploy {
    let next = rig
        .world
        .ports
        .store
        .next_deploy_number(environment.id)
        .await
        .unwrap();
    rig.world
        .add_deploy(environment, next.get(), DeployStatus::Queued)
        .await
        .unwrap()
}

async fn status(rig: &Rig, deploy: &Deploy) -> DeployStatus {
    rig.world
        .ports
        .store
        .deploy(deploy.id)
        .await
        .unwrap()
        .unwrap()
        .status
}

/// Polls until `condition` holds; fails the test after five seconds.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..5_000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Polls the store until `deploy` has `wanted`.
async fn wait_status(rig: &Rig, deploy: &Deploy, wanted: DeployStatus) {
    for _ in 0..5_000 {
        if status(rig, deploy).await == wanted {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {wanted:?}");
}

/// A new empty environment `name` of `shop`.
async fn environment(rig: &Rig, name: &str) -> Environment {
    let shop = rig.world.shop.clone();
    rig.world.add_environment(&shop, name).await.unwrap()
}

fn up_calls(rig: &Rig) -> usize {
    rig.world.ports.docker.calls().len()
}

mod same_environment {
    use super::*;

    // TC: DEP-11
    #[tokio::test]
    async fn second_deploy_waits_when_first_is_running() {
        let rig = rig(2).await;
        let qa = rig.world.qa.clone();
        let (first, second) = (queued(&rig, &qa).await, queued(&rig, &qa).await);
        let gate = rig.world.ports.docker.hold("compose_up");
        rig.queue.enqueue(first.id).await.unwrap();
        rig.queue.enqueue(second.id).await.unwrap();
        eventually("the first deploy to reach compose_up", || {
            rig.world.ports.docker.in_flight() == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let held = (
            status(&rig, &first).await,
            status(&rig, &second).await,
            up_calls(&rig),
            rig.world.ports.docker.max_in_flight(),
        );
        gate.release();
        assert_eq!(
            held,
            (DeployStatus::Running, DeployStatus::Queued, 1, 1),
            "the second stays queued and does not touch docker"
        );
    }

    // TC: DEP-11
    #[tokio::test]
    async fn runs_in_creation_order_when_several_are_queued() {
        let rig = rig(2).await;
        let qa = rig.world.qa.clone();
        let ids = [
            queued(&rig, &qa).await.id,
            queued(&rig, &qa).await.id,
            queued(&rig, &qa).await.id,
        ];
        let gate = rig.world.ports.docker.hold("compose_up");
        for id in ids {
            rig.queue.enqueue(id).await.unwrap();
        }
        gate.release();
        eventually("all three deploys to finish", || {
            rig.order.lock().unwrap().len() == 3 && rig.world.ports.docker.in_flight() == 0
        })
        .await;
        assert_eq!(
            (
                rig.order.lock().unwrap().clone(),
                rig.world.ports.docker.max_in_flight()
            ),
            (ids.to_vec(), 1),
            "creation order, never two at once"
        );
    }

    // TC: DEP-11 (enqueue calls may arrive out of order)
    #[tokio::test]
    async fn runs_by_number_when_enqueued_out_of_order() {
        let rig = rig(2).await;
        let qa = rig.world.qa.clone();
        let ids = [
            queued(&rig, &qa).await.id,
            queued(&rig, &qa).await.id,
            queued(&rig, &qa).await.id,
        ];
        let gate = rig.world.ports.docker.hold("compose_up");
        rig.queue.enqueue(ids[0]).await.unwrap();
        eventually("the first deploy to reach compose_up", || {
            rig.world.ports.docker.in_flight() == 1
        })
        .await;
        rig.queue.enqueue(ids[2]).await.unwrap();
        rig.queue.enqueue(ids[1]).await.unwrap();
        gate.release();
        eventually("all three deploys to finish", || {
            rig.order.lock().unwrap().len() == 3 && rig.world.ports.docker.in_flight() == 0
        })
        .await;
        assert_eq!(rig.order.lock().unwrap().clone(), ids.to_vec());
    }
}

mod parallel {
    use super::*;

    // TC: DEP-11
    #[tokio::test]
    async fn runs_two_environments_at_once_when_parallel_is_two() {
        let rig = rig(2).await;
        let (prod, qa) = (rig.world.prod.clone(), rig.world.qa.clone());
        let (first, second) = (queued(&rig, &prod).await, queued(&rig, &qa).await);
        let gate = rig.world.ports.docker.hold("compose_up");
        rig.queue.enqueue(first.id).await.unwrap();
        rig.queue.enqueue(second.id).await.unwrap();
        eventually("both deploys in flight", || {
            rig.world.ports.docker.in_flight() == 2
        })
        .await;
        gate.release();
        wait_status(&rig, &second, DeployStatus::Succeeded).await;
        assert_eq!(rig.world.ports.docker.max_in_flight(), 2);
    }

    // TC: DEP-11 (never more than deploy.parallel)
    #[tokio::test]
    async fn third_environment_waits_when_two_slots_are_taken() {
        let rig = rig(2).await;
        let mut deploys = vec![
            queued(&rig, &rig.world.prod.clone()).await,
            queued(&rig, &rig.world.qa.clone()).await,
        ];
        let dev = environment(&rig, "dev").await;
        deploys.push(queued(&rig, &dev).await);
        let gate = rig.world.ports.docker.hold("compose_up");
        for deploy in &deploys {
            rig.queue.enqueue(deploy.id).await.unwrap();
        }
        eventually("two deploys in flight", || {
            rig.world.ports.docker.in_flight() == 2
        })
        .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let held = (
            rig.world.ports.docker.in_flight(),
            status(&rig, &deploys[2]).await,
        );
        gate.release();
        eventually("the third deploy to succeed", || {
            rig.order.lock().unwrap().len() == 3 && rig.world.ports.docker.in_flight() == 0
        })
        .await;
        assert_eq!(
            (
                held,
                rig.world.ports.docker.max_in_flight(),
                status(&rig, &deploys[2]).await
            ),
            ((2, DeployStatus::Queued), 2, DeployStatus::Succeeded),
            "two in parallel, the third queued until a slot frees"
        );
    }

    // TC: DEP-11 (deploy.parallel = 1 serialises different environments)
    #[tokio::test]
    async fn one_at_a_time_when_parallel_is_one() {
        let rig = rig(1).await;
        let (prod, qa) = (rig.world.prod.clone(), rig.world.qa.clone());
        let (first, second) = (queued(&rig, &prod).await, queued(&rig, &qa).await);
        let gate = rig.world.ports.docker.hold("compose_up");
        rig.queue.enqueue(first.id).await.unwrap();
        rig.queue.enqueue(second.id).await.unwrap();
        eventually("one deploy in flight", || {
            rig.world.ports.docker.in_flight() == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let held = (
            rig.world.ports.docker.in_flight(),
            status(&rig, &second).await,
        );
        gate.release();
        wait_status(&rig, &second, DeployStatus::Succeeded).await;
        wait_status(&rig, &first, DeployStatus::Succeeded).await;
        assert_eq!(
            (held, rig.world.ports.docker.max_in_flight()),
            ((1, DeployStatus::Queued), 1),
            "qa waits for the only slot while prod holds it"
        );
    }
}

mod isolation {
    use super::*;

    // TC: v1 QUE-03
    #[tokio::test]
    async fn next_deploy_runs_when_previous_one_panicked() {
        let rig = rig(2).await;
        let qa = rig.world.qa.clone();
        let (first, second) = (queued(&rig, &qa).await, queued(&rig, &qa).await);
        rig.world.ports.docker.panic_next("compose_up");
        rig.queue.enqueue(first.id).await.unwrap();
        rig.queue.enqueue(second.id).await.unwrap();
        wait_status(&rig, &second, DeployStatus::Succeeded).await;
        assert_eq!(
            (status(&rig, &first).await, status(&rig, &second).await),
            (DeployStatus::Failed, DeployStatus::Succeeded),
            "the panicked deploy is failed, the next one finished"
        );
    }

    // TC: v1 QUE-03, ENV-08 (a crashed deploy must not wedge its environment)
    #[tokio::test]
    async fn environment_accepts_stop_when_deploy_panicked() {
        let rig = rig(2).await;
        let qa = rig
            .world
            .deployed(&rig.world.qa, EnvState::Running)
            .await
            .unwrap();
        let crashing = queued(&rig, &qa).await;
        rig.world.ports.docker.panic_next("compose_up");
        rig.queue.enqueue(crashing.id).await.unwrap();
        wait_status(&rig, &crashing, DeployStatus::Failed).await;
        let crashed = rig
            .world
            .ports
            .store
            .deploy(crashing.id)
            .await
            .unwrap()
            .unwrap();
        let owner = rig.world.owner.clone();
        let stopped = rig
            .world
            .environments()
            .stop(&owner, &rig.world.shop.name, &qa.name)
            .await;
        assert_eq!(
            (
                crashed.error,
                crashed.finished_at.is_some(),
                stopped.is_ok()
            ),
            (Some("deploy crashed".to_owned()), true, true),
            "failed with a generic error, and the environment is no longer busy: {stopped:?}"
        );
    }

    // TC: v1 QUE-03
    #[tokio::test]
    async fn next_deploy_runs_when_previous_one_failed() {
        let rig = rig(2).await;
        let qa = rig.world.qa.clone();
        let (first, second) = (queued(&rig, &qa).await, queued(&rig, &qa).await);
        rig.world
            .ports
            .docker
            .fail_next("compose_up", Error::internal_msg("boom"));
        rig.queue.enqueue(first.id).await.unwrap();
        rig.queue.enqueue(second.id).await.unwrap();
        wait_status(&rig, &second, DeployStatus::Succeeded).await;
        assert_eq!(
            status(&rig, &first).await,
            DeployStatus::Failed,
            "the deploy that errored out is failed; the queue went on"
        );
    }

    // TC: v1 QUE-03 (the slot of a panicked deploy is given back)
    #[tokio::test]
    async fn other_environment_runs_when_the_only_slot_was_held_by_a_panic() {
        let rig = rig(1).await;
        let (prod, qa) = (rig.world.prod.clone(), rig.world.qa.clone());
        let (first, second) = (queued(&rig, &qa).await, queued(&rig, &prod).await);
        rig.world.ports.docker.panic_next("compose_up");
        let gate = rig.world.ports.docker.hold("compose_up");
        rig.queue.enqueue(first.id).await.unwrap();
        rig.queue.enqueue(second.id).await.unwrap();
        eventually("the doomed deploy in flight", || {
            rig.world.ports.docker.in_flight() == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let held = (
            rig.world.ports.docker.in_flight(),
            status(&rig, &second).await,
        );
        gate.release();
        wait_status(&rig, &second, DeployStatus::Succeeded).await;
        assert_eq!(
            (held, status(&rig, &first).await),
            ((1, DeployStatus::Queued), DeployStatus::Failed),
            "prod waited for the slot the panicking deploy held, then got it"
        );
    }
}

mod shutdown {
    use super::*;

    // TC: v1 QUE-04
    #[tokio::test]
    async fn finishes_running_deploy_and_starts_no_new_one() {
        let rig = rig(2).await;
        let qa = rig.world.qa.clone();
        let (first, second) = (queued(&rig, &qa).await, queued(&rig, &qa).await);
        let gate = rig.world.ports.docker.hold("compose_up");
        rig.queue.enqueue(first.id).await.unwrap();
        rig.queue.enqueue(second.id).await.unwrap();
        eventually("the first deploy to run", || {
            rig.world.ports.docker.in_flight() == 1
        })
        .await;
        let closing = rig.queue.shutdown(Duration::from_secs(5));
        let release = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            gate.release();
        };
        let (outcome, ()) = tokio::join!(closing, release);
        assert_eq!(
            (
                outcome,
                status(&rig, &first).await,
                status(&rig, &second).await,
                up_calls(&rig)
            ),
            (
                ShutdownOutcome::Drained,
                DeployStatus::Succeeded,
                DeployStatus::Queued,
                1
            ),
            "the running deploy ran to the end, the queued one never started"
        );
    }

    // TC: v1 QUE-04
    #[tokio::test]
    async fn refuses_new_deploys_when_shutting_down() {
        let rig = rig(2).await;
        let qa = rig.world.qa.clone();
        let deploy = queued(&rig, &qa).await;
        let _outcome: ShutdownOutcome = rig.queue.shutdown(Duration::from_secs(1)).await;
        let result = rig.queue.enqueue(deploy.id).await;
        assert!(
            matches!(result, Err(Error::Unavailable(_))),
            "unavailable, got {result:?}"
        );
    }

    // TC: v1 QUE-04 (the grace period runs out)
    #[tokio::test]
    async fn gives_up_when_running_deploy_outlasts_the_grace_period() {
        let rig = rig(2).await;
        let qa = rig.world.qa.clone();
        let deploy = queued(&rig, &qa).await;
        let _gate = rig.world.ports.docker.hold("compose_up");
        rig.queue.enqueue(deploy.id).await.unwrap();
        eventually("the deploy to run", || {
            rig.world.ports.docker.in_flight() == 1
        })
        .await;
        let outcome = rig.queue.shutdown(Duration::from_millis(20)).await;
        eventually("the deploy to be cut off", || {
            rig.world.ports.docker.in_flight() == 0
        })
        .await;
        assert_eq!(outcome, ShutdownOutcome::TimedOut);
    }
}

mod late_runner {
    use super::*;

    #[tokio::test]
    async fn runs_the_bound_body_when_bound() {
        let runner = LateRunner::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        assert!(runner.bind(move |id| {
            sink.lock().unwrap().push(id);
            Box::pin(core::future::ready(Ok(())))
        }));
        let id = DeployId::new(ulid::Ulid::from_parts(1, 1));
        runner.run(id).await.unwrap();
        assert_eq!(seen.lock().unwrap().clone(), vec![id]);
    }

    #[tokio::test]
    async fn unavailable_when_not_bound_yet() {
        let result = LateRunner::default()
            .run(DeployId::new(ulid::Ulid::from_parts(1, 1)))
            .await;
        assert!(
            matches!(result, Err(Error::Unavailable(_))),
            "unavailable, got {result:?}"
        );
    }

    #[test]
    fn keeps_the_first_body_when_bound_twice() {
        let runner = LateRunner::default();
        let body = |_| -> RunFuture { Box::pin(core::future::ready(Ok(()))) };
        assert_eq!((runner.bind(body), runner.bind(body)), (true, false));
    }
}

mod enqueue {
    use super::*;

    #[tokio::test]
    async fn not_found_when_deploy_is_unknown() {
        let rig = rig(2).await;
        let unsaved = rig
            .world
            .records
            .deploy(&rig.world.qa, 1, DeployStatus::Queued, &rig.world.owner)
            .unwrap();
        let result = rig.queue.enqueue(unsaved.id).await;
        assert!(
            matches!(result, Err(Error::NotFound(_))),
            "not found, got {result:?}"
        );
    }
}
