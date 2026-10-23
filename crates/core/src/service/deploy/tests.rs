//! Tests of [`DeployService`] over the shared [`World`].
//!
//! The world: project `shop` (`owner/shop` on `git.example.com`) with `prod`
//! pinned to `main` and `qa` pinned to `dev`, both by `owner`, both empty;
//! `shop.apps.test` bound to `web:3000` on `prod` at host port 20000;
//! secrets `DATABASE_URL = postgres://db/shop`, `LOG_LEVEL = info` on the
//! project and `DATABASE_URL = postgres://db/shop_qa` on `qa`.
//! [`arranged`] adds a deploy token of `shop` and lets `owner` read the
//! image `git.example.com/owner/shop:sha-7`.

use super::*;
use crate::domain::{
    ComposeProject, DeploySource, DeployStatus, EnvState, Environment, HostPort, ScopeRef,
    SecretKey, SecretScope, SecretValue, Trigger, UpdatedBy, UserRole,
};
use crate::fakes::{Call, DockerCall, GitCall, HookCall, Records, World};
use crate::ports::{Resource, Secrets as _};
use crate::proxy_config::Route;

/// The compose file CI sends: one `web` service from the forge registry,
/// tagged through `${IMAGE_TAG}`.
const COMPOSE: &str = "services:\n  web:\n    image: git.example.com/owner/shop:${IMAGE_TAG}\n";

fn project(raw: &str) -> ProjectName {
    ProjectName::new(raw).unwrap()
}

fn env(raw: &str) -> EnvName {
    EnvName::new(raw).unwrap()
}

fn tag(raw: &str) -> ImageTag {
    ImageTag::new(raw).unwrap()
}

fn number(raw: u32) -> DeployNumber {
    DeployNumber::new(raw).unwrap()
}

/// The image `git.example.com/owner/shop:<tag>` as the `Git` port sees it.
fn image(raw_tag: &str) -> PackageRef {
    PackageRef {
        owner: "owner".to_owned(),
        name: "shop".to_owned(),
        tag: tag(raw_tag),
    }
}

/// What the workflow of `owner/shop` sends after a push of `branch`.
fn ci(branch: &str, raw_tag: &str) -> CiDeployRequest {
    CiDeployRequest {
        repo: RepoRef::new("owner/shop").unwrap(),
        git_ref: Branch::new(branch).unwrap(),
        tag: tag(raw_tag),
        compose: COMPOSE.to_owned(),
        env: None,
    }
}

/// The world with a live deploy token of `shop` and the image `sha-7`
/// readable by `owner`; the log is cleared.
async fn arranged() -> (World, DeployTokenSecret) {
    let world = World::new().await;
    let token = world.deploy_token(&world.shop).await.unwrap();
    world
        .ports
        .git
        .allow_image(&world.owner.login, &image("sha-7"));
    world.log().clear();
    (world, token)
}

/// Requests `request` with `token` and runs the single deploy it queued.
async fn deploy(world: &World, token: &DeployTokenSecret, request: CiDeployRequest) -> Deploy {
    let outcome = world.deploys().request_ci(token, request).await.unwrap();
    assert_eq!(outcome.deploys.len(), 1, "one environment is pinned there");
    world
        .deploys()
        .run(outcome.deploys[0].deploy_id)
        .await
        .unwrap()
}

fn compose_project(environment: &str) -> ComposeProject {
    ComposeProject::new(&project("shop"), &env(environment))
}

/// The `.env` written for `environment`, as its lines.
fn env_lines(world: &World, environment: &str) -> Vec<String> {
    world
        .ports
        .files
        .file(&compose_project(environment), ProjectFile::Env)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Whether `line` is one of the `.env` lines of `environment`.
fn has_line(world: &World, environment: &str, line: &str) -> bool {
    env_lines(world, environment).iter().any(|l| l == line)
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

async fn stored_deploy(world: &World, environment: &Environment, raw: u32) -> Option<Deploy> {
    world
        .ports
        .store
        .deploy_by_number(environment.id, number(raw))
        .await
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

/// Re-pins `prod` to `main` by `by`.
async fn pin_prod_by(world: &World, by: &User) {
    let mut prod = stored_env(world, &world.prod).await;
    prod.pin = Some(Records::pin("main", by).unwrap());
    world.ports.store.update_environment(&prod).await.unwrap();
}

/// Whether docker was called at all.
fn docker_touched(world: &World) -> bool {
    !world.ports.docker.calls().is_empty()
}

/// The error of a finished deploy, or the empty string.
fn error_of(deploy: &Deploy) -> &str {
    deploy.error.as_deref().unwrap_or_default()
}

/// Stores a succeeded deploy `raw` of `environment` with `compose` and tag
/// `sha-<raw>`.
async fn succeeded_with(
    world: &World,
    environment: &Environment,
    raw: u32,
    compose: &str,
) -> Deploy {
    let deploy = Deploy {
        compose: compose.to_owned(),
        ..world
            .records
            .deploy(environment, raw, DeployStatus::Succeeded, &world.owner)
            .unwrap()
    };
    world.ports.store.insert_deploy(&deploy).await.unwrap();
    deploy
}

/// Makes `deploy` the current one of `environment`, running.
async fn make_current(world: &World, environment: &Environment, deploy: &Deploy) {
    let mut current = stored_env(world, environment).await;
    current.current_deploy_id = Some(deploy.id);
    current.state = EnvState::Running;
    world
        .ports
        .store
        .update_environment(&current)
        .await
        .unwrap();
}

/// The function environment `api` of `shop` pinned to `main` by `owner`
/// beside `prod` (its deploys are components).
async fn function_on_main(world: &World) -> Environment {
    let mut function = world.records.function(&world.shop, "api").unwrap();
    function.pin = Some(Records::pin("main", &world.owner).unwrap());
    world
        .ports
        .store
        .insert_environment(&function)
        .await
        .unwrap();
    function
}

mod check_deploy_token {
    use super::*;

    // TC: FDEP-01 (asked before an upload is read)
    #[tokio::test]
    async fn unauthenticated_when_token_is_not_live() {
        let (world, _shop_token) = arranged().await;
        let (_unsaved, secret) = world.records.deploy_token(&world.shop, &world.owner);
        let result = world.deploys().check_deploy_token(&secret).await;
        assert!(
            matches!(result, Err(Error::Unauthenticated(_))),
            "{result:?}"
        );
    }

    // TC: FDEP-01
    #[tokio::test]
    async fn ok_when_token_is_live() {
        let (world, token) = arranged().await;
        let result = world.deploys().check_deploy_token(&token).await;
        assert!(result.is_ok(), "{result:?}");
    }
}

mod request_ci {
    use super::*;

    // TC: DEP-01
    #[tokio::test]
    async fn forbidden_when_token_belongs_to_another_project() {
        let (world, _shop_token) = arranged().await;
        let blog = world.add_project("blog", "owner/blog").await.unwrap();
        let blog_token = world.deploy_token(&blog).await.unwrap();
        let result = world
            .deploys()
            .request_ci(&blog_token, ci("main", "sha-7"))
            .await;
        assert!(
            matches!(result, Err(Error::Forbidden(_))),
            "a token deploys only into its own project, got {result:?}"
        );
    }

    // TC: DEP-01 (nothing is created for a foreign repository)
    #[tokio::test]
    async fn creates_nothing_when_token_belongs_to_another_project() {
        let (world, _shop_token) = arranged().await;
        let blog = world.add_project("blog", "owner/blog").await.unwrap();
        let blog_token = world.deploy_token(&blog).await.unwrap();
        let _refused = world
            .deploys()
            .request_ci(&blog_token, ci("main", "sha-7"))
            .await;
        assert_eq!(
            (
                deploys_of(&world, &world.prod).await.len(),
                world.ports.queue.enqueued().len()
            ),
            (0, 0)
        );
    }

    // TC: DEP-01 (a token that is not stored is no credential)
    #[tokio::test]
    async fn unauthenticated_when_token_is_not_live() {
        let (world, _shop_token) = arranged().await;
        let (_unsaved, secret) = world.records.deploy_token(&world.shop, &world.owner);
        let result = world
            .deploys()
            .request_ci(&secret, ci("main", "sha-7"))
            .await;
        assert!(
            matches!(result, Err(Error::Unauthenticated(_))),
            "unauthenticated, got {result:?}"
        );
    }

    // TC: DEP-01 (the token's own project is deployed to)
    #[tokio::test]
    async fn queues_prod_deploy_one_when_token_pushes_main() {
        let (world, token) = arranged().await;
        let outcome = world
            .deploys()
            .request_ci(&token, ci("main", "sha-7"))
            .await
            .unwrap();
        let summary: Vec<_> = outcome
            .deploys
            .iter()
            .map(|q| {
                (
                    q.project.to_string(),
                    q.environment.to_string(),
                    q.number.get(),
                )
            })
            .collect();
        assert_eq!(summary, vec![("shop".to_owned(), "prod".to_owned(), 1)]);
    }

    // TC: VIS-01 (a compose deploy from CI reaches only the compose
    // environments of the branch; a function's come in)
    #[tokio::test]
    async fn queues_prod_only_when_function_is_pinned_to_pushed_branch() {
        let (world, token) = arranged().await;
        let function = function_on_main(&world).await;
        let outcome = world
            .deploys()
            .request_ci(&token, ci("main", "sha-7"))
            .await
            .unwrap();
        let environments: Vec<String> = outcome
            .deploys
            .iter()
            .map(|queued| queued.environment.to_string())
            .collect();
        assert_eq!(
            (environments, deploys_of(&world, &function).await.len()),
            (vec!["prod".to_owned()], 0)
        );
    }

    // TC: DEP-01 (source ci, queued, handed to the queue)
    #[tokio::test]
    async fn records_ci_origin_and_enqueues_when_token_pushes_main() {
        let (world, token) = arranged().await;
        let outcome = world
            .deploys()
            .request_ci(&token, ci("main", "sha-7"))
            .await
            .unwrap();
        let id = outcome.deploys[0].deploy_id;
        let stored = world.ports.store.deploy(id).await.unwrap().unwrap();
        let token_id = world
            .ports
            .store
            .live_deploy_token(world.shop.id)
            .await
            .unwrap()
            .unwrap()
            .id;
        assert_eq!(
            (
                stored.environment_id,
                stored.source,
                stored.status,
                stored.git_ref,
                stored.tag,
                stored.compose,
                stored.triggered_by,
                world.ports.queue.enqueued(),
            ),
            (
                world.prod.id,
                DeploySource::Ci,
                DeployStatus::Queued,
                Some(Branch::new("main").unwrap()),
                tag("sha-7"),
                COMPOSE.to_owned(),
                Trigger::Token(token_id),
                vec![id],
            )
        );
    }

    // TC: DEP-02
    #[tokio::test]
    async fn empty_when_no_environment_is_pinned_to_ref() {
        let (world, token) = arranged().await;
        let outcome = world
            .deploys()
            .request_ci(&token, ci("feature/x", "sha-7"))
            .await
            .unwrap();
        assert_eq!(
            outcome,
            CiDeployOutcome {
                deploys: Vec::new(),
                deprecated_env: false,
            }
        );
    }

    // TC: DEP-02 (nothing stored, nothing queued)
    #[tokio::test]
    async fn creates_nothing_when_no_environment_is_pinned_to_ref() {
        let (world, token) = arranged().await;
        let _outcome = world
            .deploys()
            .request_ci(&token, ci("feature/x", "sha-7"))
            .await
            .unwrap();
        assert_eq!(
            (
                deploys_of(&world, &world.prod).await.len(),
                deploys_of(&world, &world.qa).await.len(),
                world.ports.queue.enqueued().len()
            ),
            (0, 0, 0)
        );
    }

    // TC: DEP-04
    #[tokio::test]
    async fn flags_deprecation_when_body_has_env() {
        let (world, token) = arranged().await;
        let request = CiDeployRequest {
            env: Some("FEATURE_FLAG=on\n".to_owned()),
            ..ci("main", "sha-7")
        };
        let outcome = world.deploys().request_ci(&token, request).await.unwrap();
        assert_eq!(
            (outcome.deprecated_env, outcome.deploys.len()),
            (true, 1),
            "accepted, with the deprecation flagged for the header"
        );
    }

    // TC: DEP-04 (no `env`, no deprecation)
    #[tokio::test]
    async fn does_not_flag_deprecation_when_body_has_no_env() {
        let (world, token) = arranged().await;
        let outcome = world
            .deploys()
            .request_ci(&token, ci("main", "sha-7"))
            .await
            .unwrap();
        assert!(!outcome.deprecated_env, "no env, no Deprecation header");
    }

    // TC: DEP-04 (the values become environment secrets written by ci)
    #[tokio::test]
    async fn stores_env_lines_as_environment_secrets_by_ci_when_body_has_env() {
        let (world, token) = arranged().await;
        let request = CiDeployRequest {
            env: Some("FEATURE_FLAG=on\n".to_owned()),
            ..ci("main", "sha-7")
        };
        let _outcome = world.deploys().request_ci(&token, request).await.unwrap();
        let stored = world
            .ports
            .store
            .secret(
                &SecretScope::Environment(world.prod.id),
                &SecretKey::new("FEATURE_FLAG").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                world.ports.secrets.decrypt(&stored.value).unwrap(),
                stored.updated_by
            ),
            ("on".to_owned(), UpdatedBy::Ci)
        );
    }

    // TC: DEP-04 (a line that is not a secret is refused before anything)
    #[tokio::test]
    async fn invalid_env_and_nothing_stored_when_env_key_is_not_a_secret_key() {
        let (world, token) = arranged().await;
        let request = CiDeployRequest {
            env: Some("feature_flag=on\n".to_owned()),
            ..ci("main", "sha-7")
        };
        let result = world.deploys().request_ci(&token, request).await;
        let secrets = world
            .ports
            .store
            .secrets_of_scope(&SecretScope::Environment(world.prod.id))
            .await
            .unwrap();
        assert!(
            matches!(&result, Err(Error::Validation(e)) if e.field == "env")
                && secrets.is_empty()
                && deploys_of(&world, &world.prod).await.is_empty(),
            "validation on env and nothing stored, got {result:?}"
        );
    }
}

mod run {
    use super::*;

    // TC: DEP-03
    #[tokio::test]
    async fn writes_env_from_project_secrets_and_system_vars_when_body_has_no_env() {
        let (world, token) = arranged().await;
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        assert_eq!(done.status, DeployStatus::Succeeded, "{:?}", done.error);
        let expected = [
            "DATABASE_URL=postgres://db/shop",
            "LOG_LEVEL=info",
            "LIFTOFF_PROJECT=shop",
            "LIFTOFF_ENV=prod",
            "LIFTOFF_DOMAIN=shop.apps.test",
            "IMAGE_TAG=sha-7",
        ];
        let missing: Vec<_> = expected
            .iter()
            .filter(|line| !has_line(&world, "prod", line))
            .collect();
        assert!(
            missing.is_empty(),
            "missing {missing:?} in {:?}",
            env_lines(&world, "prod")
        );
    }

    // TC: DEP-03 (the environment secret overrides the project one)
    #[tokio::test]
    async fn writes_environment_value_when_both_scopes_set_a_key() {
        let (world, token) = arranged().await;
        let done = deploy(&world, &token, ci("dev", "sha-7")).await;
        assert_eq!(done.status, DeployStatus::Succeeded, "{:?}", done.error);
        assert_eq!(
            (
                has_line(&world, "qa", "DATABASE_URL=postgres://db/shop_qa"),
                has_line(&world, "qa", "DATABASE_URL=postgres://db/shop"),
                has_line(&world, "qa", "LOG_LEVEL=info"),
                has_line(&world, "qa", "LIFTOFF_ENV=qa"),
            ),
            (true, false, true, true),
            "{:?}",
            env_lines(&world, "qa")
        );
    }

    // TC: DEP-03 (the env is assembled when the deploy runs, not stored)
    #[tokio::test]
    async fn writes_current_secret_value_when_secret_changed_after_request() {
        let (world, token) = arranged().await;
        let outcome = world
            .deploys()
            .request_ci(&token, ci("main", "sha-7"))
            .await
            .unwrap();
        world
            .secrets()
            .set(
                &world.owner,
                &ScopeRef::Project(project("shop")),
                SecretKey::new("LOG_LEVEL").unwrap(),
                SecretValue::new("debug".to_owned()),
                false,
            )
            .await
            .unwrap();
        let done = world
            .deploys()
            .run(outcome.deploys[0].deploy_id)
            .await
            .unwrap();
        assert_eq!(
            (
                done.status,
                has_line(&world, "prod", "LOG_LEVEL=debug"),
                has_line(&world, "prod", "LOG_LEVEL=info"),
            ),
            (DeployStatus::Succeeded, true, false)
        );
    }

    // TC: DEP-04 (values sent in the body reach the compose project)
    #[tokio::test]
    async fn writes_body_env_value_when_body_had_env() {
        let (world, token) = arranged().await;
        let request = CiDeployRequest {
            env: Some("FEATURE_FLAG=on\n".to_owned()),
            ..ci("main", "sha-7")
        };
        let done = deploy(&world, &token, request).await;
        assert_eq!(
            (done.status, has_line(&world, "prod", "FEATURE_FLAG=on")),
            (DeployStatus::Succeeded, true)
        );
    }

    // TC: DEP-05
    #[tokio::test]
    async fn fails_listing_every_error_when_compose_is_invalid() {
        let (world, token) = arranged().await;
        let request = CiDeployRequest {
            compose: "services:\n  web:\n    image: nginx:alpine\n    privileged: true\n  worker:\n    build: .\n"
                .to_owned(),
            ..ci("main", "sha-7")
        };
        let done = deploy(&world, &token, request).await;
        let error = error_of(&done);
        assert!(
            done.status == DeployStatus::Failed
                && error.contains("services.web.privileged: forbidden")
                && error.contains("services.worker.build: forbidden")
                && error.contains("services.worker.image: required"),
            "failed with all three errors, got {:?} {error:?}",
            done.status
        );
    }

    // TC: DEP-05 (before docker)
    #[tokio::test]
    async fn leaves_docker_untouched_when_compose_is_invalid() {
        let (world, token) = arranged().await;
        let request = CiDeployRequest {
            compose: "services:\n  web:\n    image: nginx:alpine\n    privileged: true\n"
                .to_owned(),
            ..ci("main", "sha-7")
        };
        let _done = deploy(&world, &token, request).await;
        assert!(!docker_touched(&world), "{:?}", world.ports.docker.calls());
    }

    // TC: DEP-06
    #[tokio::test]
    async fn fails_naming_binding_when_bound_service_is_missing_from_compose() {
        let (world, token) = arranged().await;
        let request = CiDeployRequest {
            compose: "services:\n  api:\n    image: nginx:alpine\n".to_owned(),
            ..ci("main", "sha-7")
        };
        let done = deploy(&world, &token, request).await;
        let error = error_of(&done);
        assert!(
            done.status == DeployStatus::Failed
                && error.contains("shop.apps.test")
                && error.contains("web")
                && !docker_touched(&world),
            "failed before docker naming the binding, got {:?} {error:?}",
            done.status
        );
    }

    // TC: DEP-07
    #[tokio::test]
    async fn checks_image_as_pinned_by_connection_when_ci_deploys() {
        let (world, token) = arranged().await;
        let _done = deploy(&world, &token, ci("main", "sha-7")).await;
        let checks: Vec<_> = world
            .ports
            .git
            .calls()
            .into_iter()
            .filter(|call| matches!(call, GitCall::ImageExists { .. }))
            .collect();
        assert_eq!(
            checks,
            vec![GitCall::ImageExists {
                login: world.owner.login.clone(),
                image: image("sha-7"),
            }]
        );
    }

    // TC: DEP-07 (pinned_by, not whoever issued the token)
    #[tokio::test]
    async fn checks_image_as_carol_when_carol_pinned_environment() {
        let (world, token) = arranged().await;
        let carol = world.add_connected_user("carol").await.unwrap();
        world.ports.git.allow_image(&carol.login, &image("sha-7"));
        pin_prod_by(&world, &carol).await;
        world.log().clear();
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        let logins: Vec<_> = world
            .ports
            .git
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                GitCall::ImageExists { login, .. } => Some(login.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            (done.status, logins),
            (DeployStatus::Succeeded, vec!["carol".to_owned()])
        );
    }

    // TC: DEP-07 (no connection)
    #[tokio::test]
    async fn fails_asking_to_connect_forgejo_when_pinned_by_has_no_connection() {
        let (world, token) = arranged().await;
        let dave = world.add_user("dave", UserRole::User).await.unwrap();
        pin_prod_by(&world, &dave).await;
        world.log().clear();
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        let error = error_of(&done);
        assert!(
            done.status == DeployStatus::Failed
                && error.contains("connect Forgejo")
                && !docker_touched(&world),
            "failed before docker asking to connect Forgejo, got {:?} {error:?}",
            done.status
        );
    }

    // TC: DEP-07 (the image is not readable by the connection)
    #[tokio::test]
    async fn fails_naming_image_when_connection_cannot_read_it() {
        let (world, token) = arranged().await;
        let done = deploy(&world, &token, ci("main", "sha-8")).await;
        let error = error_of(&done);
        assert!(
            done.status == DeployStatus::Failed
                && error.contains("git.example.com/owner/shop:sha-8")
                && !docker_touched(&world),
            "failed before docker naming the image, got {:?} {error:?}",
            done.status
        );
    }

    // TC: DEP-07 (a secret interpolated into `image:` stays out of the error and the journal)
    #[tokio::test]
    async fn keeps_secret_out_of_error_when_image_uses_it_and_is_not_readable() {
        let (world, token) = arranged().await;
        world
            .secrets()
            .set(
                &world.owner,
                &ScopeRef::Project(project("shop")),
                SecretKey::new("SECRET_TAG").unwrap(),
                SecretValue::new("hunter2".to_owned()),
                false,
            )
            .await
            .unwrap();
        let request = CiDeployRequest {
            compose: "services:\n  web:\n    image: git.example.com/owner/shop:${SECRET_TAG}\n"
                .to_owned(),
            ..ci("main", "sha-7")
        };
        let done = deploy(&world, &token, request).await;
        let error = error_of(&done);
        assert!(
            done.status == DeployStatus::Failed
                && error.contains("git.example.com/owner/shop:${SECRET_TAG}")
                && !error.contains("hunter2")
                && !done.log.contains("hunter2"),
            "failed naming the image as written, without the secret, got {:?} {error:?} {:?}",
            done.status,
            done.log
        );
    }

    // TC: DEP-08
    #[tokio::test]
    async fn fails_with_reason_when_before_deploy_denies() {
        let (world, token) = arranged().await;
        world.ports.hooks.answer_next(
            "before_deploy",
            HookResult::Deny("deploys are frozen".to_owned()),
        );
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        assert!(
            done.status == DeployStatus::Failed && error_of(&done).contains("deploys are frozen"),
            "failed with the hook's reason, got {:?} {:?}",
            done.status,
            done.error
        );
    }

    // TC: DEP-08 (docker and the slice are not touched)
    #[tokio::test]
    async fn leaves_docker_untouched_when_before_deploy_denies() {
        let (world, token) = arranged().await;
        world.ports.hooks.answer_next(
            "before_deploy",
            HookResult::Deny("deploys are frozen".to_owned()),
        );
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        let asked = world.ports.hooks.calls().contains(&HookCall::BeforeDeploy {
            environment: world.prod.id,
            deploy: done.id,
        });
        assert_eq!(
            (
                asked,
                docker_touched(&world),
                world.ports.systemd.calls().is_empty()
            ),
            (true, false, true)
        );
    }

    // TC: DEP-09
    #[tokio::test]
    async fn succeeds_and_environment_runs_it_when_everything_passes() {
        let (world, token) = arranged().await;
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        let prod = stored_env(&world, &world.prod).await;
        assert_eq!(
            (done.status, prod.current_deploy_id, prod.state),
            (DeployStatus::Succeeded, Some(done.id), EnvState::Running),
            "{:?}",
            done.error
        );
    }

    // TC: DEP-09 (the proxy routes the bound domain to the running deploy)
    #[tokio::test]
    async fn routes_bound_domain_to_host_port_when_deploy_succeeded() {
        let (world, token) = arranged().await;
        let _done = deploy(&world, &token, ci("main", "sha-7")).await;
        let route = world.ports.proxy.applied().and_then(|config| {
            config
                .sites
                .into_iter()
                .find(|site| site.host.to_string() == "shop.apps.test")
                .map(|site| site.route)
        });
        assert_eq!(route, Some(Route::Upstream(HostPort::new(20_000).unwrap())));
    }

    // TC: DEP-09 (order: before_deploy, pull, up, proxy, after_deploy)
    #[tokio::test]
    async fn calls_after_deploy_after_proxy_when_deploy_succeeded() {
        let (world, token) = arranged().await;
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        let steps: Vec<_> = world
            .log()
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Hook(HookCall::BeforeDeploy { .. }) => Some("before_deploy"),
                Call::Docker(DockerCall::Pull(_)) => Some("pull"),
                Call::Docker(DockerCall::Up { .. }) => Some("up"),
                Call::Proxy(_) => Some("proxy"),
                Call::Hook(HookCall::AfterDeploy { .. }) => Some("after_deploy"),
                _ => None,
            })
            .collect();
        let after = world.ports.hooks.calls().contains(&HookCall::AfterDeploy {
            environment: world.prod.id,
            deploy: done.id,
        });
        assert_eq!(
            (steps, after),
            (
                vec!["before_deploy", "pull", "up", "proxy", "after_deploy"],
                true
            )
        );
    }

    // TC: DEP-09 (history trimmed to 20)
    #[tokio::test]
    async fn keeps_newest_twenty_deploys_when_twenty_first_succeeds() {
        let (world, token) = arranged().await;
        for raw in 1..=20 {
            let _old = world
                .add_deploy(&world.prod, raw, DeployStatus::Succeeded)
                .await
                .unwrap();
        }
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        let kept = deploys_of(&world, &world.prod).await;
        assert_eq!(
            (
                done.number.get(),
                kept.len(),
                kept.first().map(|d| d.number.get()),
                kept.last().map(|d| d.number.get())
            ),
            (21, 20, Some(21), Some(2))
        );
    }

    // TC: DEP-03 (a value compose would interpolate is written single-quoted
    // and the deploy goes through)
    #[tokio::test]
    async fn writes_single_quoted_secret_when_value_has_dollar() {
        let (world, token) = arranged().await;
        world
            .secrets()
            .set(
                &world.owner,
                &ScopeRef::Project(project("shop")),
                SecretKey::new("PASSWORD").unwrap(),
                SecretValue::new("a$b".to_owned()),
                false,
            )
            .await
            .unwrap();
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        assert_eq!(
            (done.status, has_line(&world, "prod", "PASSWORD='a$b'")),
            (DeployStatus::Succeeded, true),
            "{:?} {:?}",
            done.error,
            env_lines(&world, "prod")
        );
    }

    // TC: DEP-09 (trimming the history is bookkeeping: the deploy stands)
    #[tokio::test]
    async fn succeeds_when_history_cannot_be_trimmed() {
        let (world, token) = arranged().await;
        world
            .ports
            .store
            .fail_next("prune_deploys", Error::internal_msg("disk full"));
        let outcome = world
            .deploys()
            .request_ci(&token, ci("main", "sha-7"))
            .await
            .unwrap();
        let ran = world.deploys().run(outcome.deploys[0].deploy_id).await;
        let prod = stored_env(&world, &world.prod).await;
        assert_eq!(
            (ran.as_ref().map(|done| done.status).ok(), prod.state),
            (Some(DeployStatus::Succeeded), EnvState::Running),
            "{ran:?}"
        );
    }

    // TC: DEP-10 (a rollback that came up is not undone by a failed trim)
    #[tokio::test]
    async fn environment_runs_rollback_when_history_cannot_be_trimmed_after_it() {
        let (world, token) = arranged().await;
        let _prod = world
            .deployed(&world.prod, EnvState::Running)
            .await
            .unwrap();
        world
            .ports
            .docker
            .fail_next_up(["container web is unhealthy"]);
        world
            .ports
            .store
            .fail_next("prune_deploys", Error::internal_msg("disk full"));
        let outcome = world
            .deploys()
            .request_ci(&token, ci("main", "sha-7"))
            .await
            .unwrap();
        let ran = world.deploys().run(outcome.deploys[0].deploy_id).await;
        let rollback = stored_deploy(&world, &world.prod, 3).await.map(|d| d.id);
        let prod = stored_env(&world, &world.prod).await;
        assert_eq!(
            (
                ran.is_ok(),
                rollback.is_some(),
                prod.current_deploy_id,
                prod.state
            ),
            (true, true, rollback, EnvState::Running),
            "{ran:?}"
        );
    }

    // TC: DEP-10
    #[tokio::test]
    async fn fails_with_last_stderr_line_when_up_fails() {
        let (world, token) = arranged().await;
        let _prod = world
            .deployed(&world.prod, EnvState::Running)
            .await
            .unwrap();
        world
            .ports
            .docker
            .fail_next_up(["recreating web", "container web is unhealthy"]);
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        assert_eq!(
            (done.number.get(), done.status, done.error.as_deref()),
            (2, DeployStatus::Failed, Some("container web is unhealthy"))
        );
    }

    // TC: DEP-10 (the rollback is a new deploy with rolled_back_to)
    #[tokio::test]
    async fn redeploys_previous_success_with_rolled_back_to_when_up_fails() {
        let (world, token) = arranged().await;
        let _prod = world
            .deployed(&world.prod, EnvState::Running)
            .await
            .unwrap();
        let first = stored_deploy(&world, &world.prod, 1).await.unwrap();
        world
            .ports
            .docker
            .fail_next_up(["container web is unhealthy"]);
        let _done = deploy(&world, &token, ci("main", "sha-7")).await;
        let rollback = stored_deploy(&world, &world.prod, 3).await.unwrap();
        assert_eq!(
            (
                rollback.source,
                rollback.status,
                rollback.rolled_back_to,
                rollback.tag,
                rollback.compose
            ),
            (
                DeploySource::Redeploy,
                DeployStatus::Succeeded,
                Some(number(1)),
                tag("sha-1"),
                first.compose
            )
        );
    }

    // TC: DEP-10 (the environment runs the rollback)
    #[tokio::test]
    async fn environment_runs_rollback_when_up_fails() {
        let (world, token) = arranged().await;
        let _prod = world
            .deployed(&world.prod, EnvState::Running)
            .await
            .unwrap();
        world
            .ports
            .docker
            .fail_next_up(["container web is unhealthy"]);
        let _done = deploy(&world, &token, ci("main", "sha-7")).await;
        let rollback = stored_deploy(&world, &world.prod, 3).await.unwrap();
        let prod = stored_env(&world, &world.prod).await;
        assert_eq!(
            (prod.current_deploy_id, prod.state),
            (Some(rollback.id), EnvState::Running)
        );
    }

    // TC: DEP-10 (nothing to roll back to)
    #[tokio::test]
    async fn environment_failed_without_rollback_when_up_fails_on_first_deploy() {
        let (world, token) = arranged().await;
        world
            .ports
            .docker
            .fail_next_up(["container web is unhealthy"]);
        let done = deploy(&world, &token, ci("main", "sha-7")).await;
        let prod = stored_env(&world, &world.prod).await;
        assert_eq!(
            (
                done.status,
                prod.state,
                deploys_of(&world, &world.prod).await.len()
            ),
            (DeployStatus::Failed, EnvState::Failed, 1)
        );
    }
}

mod redeploy {
    use super::*;

    // TC: VIS-01 (a tunnel has nothing to redeploy: its code runs on the
    // developer's machine)
    #[tokio::test]
    async fn validation_naming_kind_and_nothing_queued_when_environment_is_tunnel() {
        let world = World::new().await;
        let tunnel = world
            .add_tunnel(&world.shop, "shop-dev", &world.owner)
            .await
            .unwrap();
        let tunnel = world.deployed(&tunnel, EnvState::Running).await.unwrap();
        let result = world
            .deploys()
            .redeploy(&world.owner, &project("shop"), &tunnel.name, None)
            .await;
        assert!(
            matches!(&result, Err(Error::Validation(e)) if e.field == "kind")
                && world.ports.queue.enqueued().is_empty(),
            "{result:?}"
        );
    }

    // TC: REST-04, SEC-10 (the owner check: single-user mode, the admin)
    #[tokio::test]
    async fn forbidden_and_nothing_queued_when_actor_is_not_admin() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        world.log().clear();
        let error = world
            .deploys()
            .redeploy(&carol, &project("shop"), &env("prod"), None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Forbidden(_))
                && world.log().only_authorized()
                && world.ports.queue.enqueued().is_empty(),
            "refused before any change, got {error:?} {:?}",
            world.log().calls()
        );
    }

    const COMPOSE_ONE: &str = "services:\n  web:\n    image: nginx:1.27\n";
    const COMPOSE_TWO: &str = "services:\n  web:\n    image: nginx:1.28\n";

    /// `prod` with succeeded deploys #1 (`COMPOSE_ONE`, `sha-1`) and #2
    /// (`COMPOSE_TWO`, `sha-2`), #2 current; the log is cleared.
    async fn deployed_twice() -> World {
        let world = World::new().await;
        let _first = succeeded_with(&world, &world.prod, 1, COMPOSE_ONE).await;
        let second = succeeded_with(&world, &world.prod, 2, COMPOSE_TWO).await;
        make_current(&world, &world.prod, &second).await;
        world.log().clear();
        world
    }

    // TC: DEP-12
    #[tokio::test]
    async fn copies_compose_and_tag_of_n_when_to_is_n() {
        let world = deployed_twice().await;
        let queued = world
            .deploys()
            .redeploy(
                &world.owner,
                &project("shop"),
                &env("prod"),
                Some(number(1)),
            )
            .await
            .unwrap();
        let copy = world
            .ports
            .store
            .deploy(queued.deploy_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                copy.number.get(),
                copy.tag,
                copy.compose,
                copy.source,
                copy.status,
                copy.rolled_back_to
            ),
            (
                3,
                tag("sha-1"),
                COMPOSE_ONE.to_owned(),
                DeploySource::Redeploy,
                DeployStatus::Queued,
                None
            )
        );
    }

    // TC: DEP-12 (handed to the queue)
    #[tokio::test]
    async fn enqueues_copy_when_redeployed() {
        let world = deployed_twice().await;
        let queued = world
            .deploys()
            .redeploy(
                &world.owner,
                &project("shop"),
                &env("prod"),
                Some(number(1)),
            )
            .await
            .unwrap();
        assert_eq!(world.ports.queue.enqueued(), vec![queued.deploy_id]);
    }

    // TC: DEP-12 (without --to: the current deploy)
    #[tokio::test]
    async fn copies_current_deploy_when_to_is_absent() {
        let world = deployed_twice().await;
        let queued = world
            .deploys()
            .redeploy(&world.owner, &project("shop"), &env("prod"), None)
            .await
            .unwrap();
        let copy = world
            .ports
            .store
            .deploy(queued.deploy_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (copy.tag, copy.compose),
            (tag("sha-2"), COMPOSE_TWO.to_owned())
        );
    }

    // TC: DEP-12 (only a succeeded deploy can be redeployed)
    #[tokio::test]
    async fn invalid_to_when_n_is_a_failed_deploy() {
        let world = deployed_twice().await;
        let _failed = world
            .add_deploy(&world.prod, 3, DeployStatus::Failed)
            .await
            .unwrap();
        let result = world
            .deploys()
            .redeploy(
                &world.owner,
                &project("shop"),
                &env("prod"),
                Some(number(3)),
            )
            .await;
        assert!(
            matches!(&result, Err(Error::Validation(e)) if e.field == "to"),
            "validation on to, got {result:?}"
        );
    }

    // TC: DEP-12 (nothing was ever deployed)
    #[tokio::test]
    async fn invalid_to_when_environment_was_never_deployed() {
        let world = World::new().await;
        let result = world
            .deploys()
            .redeploy(&world.owner, &project("shop"), &env("qa"), None)
            .await;
        assert!(
            matches!(&result, Err(Error::Validation(e)) if e.field == "to"),
            "validation on to, got {result:?}"
        );
    }

    // TC: DEP-12
    #[tokio::test]
    async fn not_found_when_environment_is_unknown() {
        let world = World::new().await;
        let result = world
            .deploys()
            .redeploy(&world.owner, &project("shop"), &env("staging"), None)
            .await;
        assert!(
            matches!(result, Err(Error::NotFound(_))),
            "not found, got {result:?}"
        );
    }
}

mod dotenv_line {
    use super::*;

    fn line(value: &str) -> Result<String, String> {
        let key = SecretKey::new("PASSWORD").map_err(|error| error.to_string())?;
        dotenv_line(&key, &SecretValue::new(value.to_owned()))
    }

    // TC: DEP-03 (a secret reaches compose as written)
    #[test]
    fn bare_when_value_is_plain() {
        assert_eq!(
            line("postgres://db/shop"),
            Ok("PASSWORD=postgres://db/shop".to_owned())
        );
    }

    // TC: DEP-03 (single quotes: compose does not interpolate)
    #[test]
    fn single_quoted_when_value_has_dollar_or_space() {
        assert_eq!(line("a $b\\c"), Ok("PASSWORD='a $b\\c'".to_owned()));
    }

    // TC: DEP-03
    #[test]
    fn double_quoted_when_value_has_only_a_single_quote() {
        assert_eq!(line("it's"), Ok("PASSWORD=\"it's\"".to_owned()));
    }

    // TC: DEP-03 (a value compose would read differently is refused)
    #[test]
    fn refused_when_value_has_a_newline() {
        assert!(line("a\nb").is_err(), "a newline cannot be written");
    }
}

mod rotate_token {
    use super::*;
    use crate::domain::RepoRef;

    // TC: REST-07 (the value lives in `LIFTOFF_TOKEN` and the repository's CI; the old token is revoked)
    #[tokio::test]
    async fn new_token_in_secret_and_ci_and_old_one_revoked_when_rotated() {
        let world = World::new().await;
        let old = world.deploy_token(&world.shop).await.unwrap();
        let token = world
            .deploys()
            .rotate_token(&world.owner, &project("shop"))
            .await
            .unwrap();
        // `LIFTOFF_TOKEN` is write-only through the secrets service: read
        // what the store holds.
        let sealed = world
            .ports
            .store
            .secret(
                &SecretScope::Project(world.shop.id),
                &SecretKey::new("LIFTOFF_TOKEN").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        let stored = SecretValue::new(world.ports.secrets.decrypt(&sealed.value).unwrap());
        let in_ci = world.ports.git.ci_secret(
            &RepoRef::new("owner/shop").unwrap(),
            &SecretKey::new("LIFTOFF_TOKEN").unwrap(),
        );
        let store = &world.ports.store;
        assert_eq!(
            (
                store
                    .live_deploy_token(world.shop.id)
                    .await
                    .unwrap()
                    .map(|t| t.id),
                store.live_deploy_token_by_hash(&old.hash()).await.unwrap(),
                DeployTokenSecret::parse(stored.expose())
                    .map(|s| s.hash())
                    .ok(),
                in_ci.map(|v| v.expose().to_owned()),
            ),
            (
                Some(token.id),
                None,
                Some(token.token_hash),
                Some(stored.expose().to_owned())
            )
        );
    }

    // TC: CRE-08 (the actor's forge connection is used only on its behalf; a copy in the project scope would be readable by the project's other members and by every workflow of the repository)
    #[tokio::test]
    async fn stores_no_connection_token_in_project_secrets_when_rotated() {
        let world = World::new().await;
        let _token = world
            .deploys()
            .rotate_token(&world.owner, &project("shop"))
            .await
            .unwrap();
        let leaked: Vec<String> = world
            .ports
            .store
            .secrets_of_scope(&SecretScope::Project(world.shop.id))
            .await
            .unwrap()
            .iter()
            .filter(|secret| {
                world.ports.secrets.decrypt(&secret.value).unwrap() == World::OWNER_TOKEN
            })
            .map(|secret| secret.key.to_string())
            .collect();
        assert_eq!(leaked, Vec::<String>::new());
    }

    // TC: CRE-08 (the registry login is a credential of its own, asked by
    // `lof create`; rotating the deploy token leaves it alone)
    #[tokio::test]
    async fn stores_no_registry_login_when_rotated() {
        let world = World::new().await;
        let _token = world
            .deploys()
            .rotate_token(&world.owner, &project("shop"))
            .await
            .unwrap();
        let mut found = Vec::new();
        for name in ["REGISTRY_USER", "REGISTRY_TOKEN"] {
            found.push(
                world
                    .ports
                    .store
                    .secret(
                        &SecretScope::Project(world.shop.id),
                        &SecretKey::new(name).unwrap(),
                    )
                    .await
                    .unwrap()
                    .is_some(),
            );
        }
        assert_eq!(found, vec![false, false]);
    }

    // TC: REST-07 (a refused forge keeps the project deployable)
    #[tokio::test]
    async fn old_token_kept_when_forge_refuses_the_secret() {
        let world = World::new().await;
        let old = world.deploy_token(&world.shop).await.unwrap();
        world.ports.git.fail_next(
            "put_ci_secret",
            Error::forbidden("owner may not manage the CI secrets"),
        );
        let error = world
            .deploys()
            .rotate_token(&world.owner, &project("shop"))
            .await
            .unwrap_err();
        let live = world
            .ports
            .store
            .live_deploy_token_by_hash(&old.hash())
            .await
            .unwrap();
        assert!(
            matches!(error, Error::Forbidden(_)) && live.is_some(),
            "{error:?}"
        );
    }

    // TC: REST-07 (nothing is written before the forge accepted; a later `secrets sync` must not push a token that was never issued)
    #[tokio::test]
    async fn stored_secret_unchanged_when_forge_refuses_the_secret() {
        let world = World::new().await;
        let first = world
            .deploys()
            .rotate_token(&world.owner, &project("shop"))
            .await
            .unwrap();
        world.ports.git.fail_next(
            "put_ci_secret",
            Error::forbidden("owner may not manage the CI secrets"),
        );
        let _refused = world
            .deploys()
            .rotate_token(&world.owner, &project("shop"))
            .await
            .unwrap_err();
        let sealed = world
            .ports
            .store
            .secret(
                &SecretScope::Project(world.shop.id),
                &SecretKey::new("LIFTOFF_TOKEN").unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        let (stored, version) = (
            SecretValue::new(world.ports.secrets.decrypt(&sealed.value).unwrap()),
            sealed.version,
        );
        assert_eq!(
            (
                version.get(),
                DeployTokenSecret::parse(stored.expose())
                    .map(|s| s.hash())
                    .ok()
            ),
            (1, Some(first.token_hash))
        );
    }
}

mod request_cli {
    use super::*;

    // TC: REST-07, SEC-10 (the owner check: single-user mode, the admin)
    #[tokio::test]
    async fn forbidden_and_nothing_queued_when_actor_is_not_admin() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        world.log().clear();
        let error = world
            .deploys()
            .request_cli(&carol, &project("shop"), &env("prod"), tag("sha-9"), None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::Forbidden(_))
                && world.log().only_authorized()
                && world.ports.queue.enqueued().is_empty(),
            "refused before any change, got {error:?} {:?}",
            world.log().calls()
        );
    }

    // TC: REST-07 (`lof deploy --tag T` with a session)
    #[tokio::test]
    async fn queued_by_the_user_with_the_current_compose_when_none_sent() {
        let world = World::new().await;
        let prod = world
            .deployed(&world.prod, EnvState::Running)
            .await
            .unwrap();
        let queued = world
            .deploys()
            .request_cli(
                &world.owner,
                &project("shop"),
                &env("prod"),
                tag("sha-9"),
                None,
            )
            .await
            .unwrap();
        let deploy = world
            .ports
            .store
            .deploy(queued.deploy_id)
            .await
            .unwrap()
            .unwrap();
        let current = world
            .ports
            .store
            .deploy(prod.current_deploy_id.unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                deploy.source,
                deploy.triggered_by,
                deploy.tag.as_str(),
                deploy.compose == current.compose,
                world.ports.queue.enqueued(),
            ),
            (
                DeploySource::Cli,
                Trigger::User(world.owner.id),
                "sha-9",
                true,
                vec![queued.deploy_id]
            )
        );
    }

    // TC: VIS-01 (`lof deploy --tag` sends compose: not for a function)
    #[tokio::test]
    async fn validation_naming_kind_and_nothing_queued_when_environment_is_function() {
        let world = World::new().await;
        let function = function_on_main(&world).await;
        let result = world
            .deploys()
            .request_cli(
                &world.owner,
                &project("shop"),
                &function.name,
                tag("sha-9"),
                Some(COMPOSE.to_owned()),
            )
            .await;
        assert!(
            matches!(&result, Err(Error::Validation(e)) if e.field == "kind")
                && world.ports.queue.enqueued().is_empty()
                && deploys_of(&world, &function).await.is_empty(),
            "{result:?}"
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn invalid_naming_compose_when_nothing_deployed_and_none_sent() {
        let world = World::new().await;
        let error = world
            .deploys()
            .request_cli(
                &world.owner,
                &project("shop"),
                &env("qa"),
                tag("sha-9"),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::Validation(e) if e.field == "compose"),
            "{error:?}"
        );
    }
}

mod get {
    use super::*;

    // TC: REST-07, SEC-10 (the owner check: single-user mode, the admin;
    // a deploy record leads to its log)
    #[tokio::test]
    async fn forbidden_before_any_change_when_actor_is_not_admin() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let deploy = world
            .add_deploy(&world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        world.log().clear();
        let error = world.deploys().get(&carol, deploy.id).await.unwrap_err();
        assert!(
            matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
            "refused before any change, got {error:?} {:?}",
            world.log().calls()
        );
    }

    // TC: SEC-10, MOD-04 (the `authorize` hook is the one point every owner check asks; a deploy is guarded as its environment)
    #[tokio::test]
    async fn asks_authorize_with_action_and_resource_when_called() {
        let world = World::new().await;
        let deploy = world
            .add_deploy(&world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        world.log().clear();
        let _record = world.deploys().get(&world.owner, deploy.id).await.unwrap();
        assert_eq!(
            world.log().calls().first(),
            Some(&Call::Hook(HookCall::Authorize {
                actor: world.owner.id,
                action: Action::DeployGet,
                resource: Resource::Environment(world.qa.id),
            }))
        );
    }

    // TC: SEC-10 (a `deny` wins over the admin)
    #[tokio::test]
    async fn forbidden_with_reason_when_authorize_denies_admin() {
        let world = World::new().await;
        let deploy = world
            .add_deploy(&world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Deny("not a member".to_owned()));
        let error = world
            .deploys()
            .get(&world.owner, deploy.id)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::Forbidden(reason) if reason.contains("not a member")),
            "{error:?}"
        );
    }

    // TC: SEC-10 (an `allow` lets a non-admin in)
    #[tokio::test]
    async fn succeeds_for_non_admin_when_authorize_allows() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let deploy = world
            .add_deploy(&world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let record = world.deploys().get(&carol, deploy.id).await.unwrap();
        assert_eq!(record.deploy.id, deploy.id);
    }
}

mod snippet {
    use super::*;
    use crate::domain::GitProvider;
    use crate::manifest::{Deployable, RepoPath};
    use crate::snippet::{CiSnippet, SnippetTarget};

    /// The snippet of `shop` on `git.example.com` from `provider`, calling the
    /// world's API site `liftoff.test`.
    fn shop_target(provider: GitProvider) -> SnippetTarget {
        SnippetTarget {
            provider,
            forge_host: Host::new(World::GIT_HOST).unwrap(),
            api_host: Host::new("liftoff.test").unwrap(),
        }
    }

    /// A compose project deploying `compose`.
    fn compose(path: &str) -> Deployable {
        Deployable::Service {
            compose: RepoPath::new(path).unwrap(),
            expose: None,
        }
    }

    /// Makes the world's forge host `git.example.com` one that `provider`'s
    /// module serves.
    fn forge_is(world: &World, provider: GitProvider) {
        world
            .ports
            .git
            .serve(&Host::new(World::GIT_HOST).unwrap(), provider);
    }

    /// Stores `manifest` as the `liftoff.toml` of `shop`.
    async fn set_shop_manifest(world: &World, manifest: &str) {
        let mut shop = world.shop.clone();
        shop.manifest = Some(manifest.to_owned());
        world.ports.store.update_project(&shop).await.unwrap();
    }

    async fn shop_snippet(world: &World) -> Result<CiSnippet, Error> {
        world
            .deploys()
            .snippet(&world.owner, &project("shop"))
            .await
    }

    // TC: SNIP-01 (a project without a manifest is a compose project with
    // `deploy/compose.yaml`)
    #[tokio::test]
    async fn forgejo_compose_workflow_when_forge_is_forgejo_and_no_manifest() {
        let world = World::new().await;
        let snippet = shop_snippet(&world).await.unwrap();
        let expected = CiSnippet::render(
            &shop_target(GitProvider::Forgejo),
            &compose("deploy/compose.yaml"),
        );
        assert_eq!(
            (snippet.path.as_str(), snippet.content),
            (".forgejo/workflows/deploy.yml", expected.content)
        );
    }

    // TC: SNIP-02
    #[tokio::test]
    async fn github_workflow_when_project_forge_is_github() {
        let world = World::new().await;
        forge_is(&world, GitProvider::Github);
        let snippet = shop_snippet(&world).await.unwrap();
        let expected = CiSnippet::render(
            &shop_target(GitProvider::Github),
            &compose("deploy/compose.yaml"),
        );
        assert_eq!(
            (snippet.path.as_str(), snippet.content),
            (".github/workflows/deploy.yml", expected.content)
        );
    }

    // TC: SNIP-03
    #[tokio::test]
    async fn gitlab_ci_file_when_project_forge_is_gitlab() {
        let world = World::new().await;
        forge_is(&world, GitProvider::Gitlab);
        let snippet = shop_snippet(&world).await.unwrap();
        let expected = CiSnippet::render(
            &shop_target(GitProvider::Gitlab),
            &compose("deploy/compose.yaml"),
        );
        assert_eq!(
            (snippet.path.as_str(), snippet.content),
            (".gitlab-ci.yml", expected.content)
        );
    }

    // TC: SNIP-01 (the compose file is the manifest's `[service] compose`)
    #[tokio::test]
    async fn sends_manifest_compose_file_when_manifest_has_service() {
        let world = World::new().await;
        set_shop_manifest(&world, "[service]\ncompose = \"compose.yaml\"\n").await;
        let snippet = shop_snippet(&world).await.unwrap();
        let expected =
            CiSnippet::render(&shop_target(GitProvider::Forgejo), &compose("compose.yaml"));
        assert_eq!(snippet, expected);
    }

    // TC: SNIP-01 (a function builds and uploads its component)
    #[tokio::test]
    async fn function_workflow_when_manifest_has_function() {
        let world = World::new().await;
        set_shop_manifest(
            &world,
            "[function]\nbuild = \"cargo build --release --target wasm32-wasip2\"\ncomponent = \"target/wasm32-wasip2/release/shop.wasm\"\n",
        )
        .await;
        let snippet = shop_snippet(&world).await.unwrap();
        let expected = CiSnippet::render(
            &shop_target(GitProvider::Forgejo),
            &Deployable::Function {
                build: "cargo build --release --target wasm32-wasip2".to_owned(),
                component: RepoPath::new("target/wasm32-wasip2/release/shop.wasm").unwrap(),
            },
        );
        assert_eq!(snippet, expected);
    }

    // TC: SNIP-01 (the provider is a fact about the project's forge host,
    // not about the caller: an actor without a connection there gets the
    // same workflow)
    #[tokio::test]
    async fn renders_for_forge_provider_when_actor_has_no_connection_on_forge() {
        let world = World::new().await;
        forge_is(&world, GitProvider::Gitlab);
        world
            .ports
            .store
            .delete_git_connection(world.owner.id, &world.connection.host)
            .await
            .unwrap();
        let snippet = shop_snippet(&world).await.unwrap();
        assert_eq!(snippet.path.as_str(), ".gitlab-ci.yml");
    }

    // TC: SNIP-01, GITF-06 (no git module serves the project's forge host)
    #[tokio::test]
    async fn validation_on_host_when_no_module_serves_forge() {
        let world = World::new().await;
        let other = world.add_project("blog", "owner/blog").await.unwrap();
        let mut moved = other.clone();
        moved.git_host = Host::new("git.elsewhere.test").unwrap();
        world.ports.store.update_project(&moved).await.unwrap();
        let error = world
            .deploys()
            .snippet(&world.owner, &project("blog"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::Validation(invalid) if invalid.field == "host"),
            "{error:?}"
        );
    }

    // TC: SNIP-01 (a stored text that is not a v2 manifest, such as a v1
    // project's, is the default compose project)
    #[rstest::rstest]
    #[case::not_toml(r#"{"service":"web","port":3000}"#)]
    #[case::v1_project_table("[project]\nname = \"shop\"\n")]
    #[case::empty("")]
    #[tokio::test]
    async fn compose_fallback_when_stored_manifest_is_not_v2(#[case] stored: &str) {
        let world = World::new().await;
        set_shop_manifest(&world, stored).await;
        let snippet = shop_snippet(&world).await.unwrap();
        let expected = CiSnippet::render(
            &shop_target(GitProvider::Forgejo),
            &compose("deploy/compose.yaml"),
        );
        assert_eq!(snippet, expected);
    }

    // TC: SNIP-01 (a v2 manifest with a mistake is reported, never
    // replaced by the compose default)
    #[rstest::rstest]
    #[case::typo_in_function("[function]\nbuild = \"make\"\ncomponnet = \"fn.wasm\"\n")]
    #[case::bad_expose("[service]\ncompose = \"compose.yaml\"\nexpose = \"web\"\n")]
    #[tokio::test]
    async fn validation_on_manifest_when_stored_v2_manifest_is_malformed(#[case] stored: &str) {
        let world = World::new().await;
        set_shop_manifest(&world, stored).await;
        let error = shop_snippet(&world).await.unwrap_err();
        assert!(
            matches!(&error, Error::Validation(invalid) if invalid.field == "manifest"),
            "{error:?}"
        );
    }
}
