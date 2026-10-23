//! REST-04: environments (`/projects/{p}/environments[/{e}]`, their
//! `start`, `stop`, `redeploy`, `deploys` and `logs`).

use http::{StatusCode, header};
use liftoff_core::domain::{ComposeProject, DeployStatus, EnvState};
use liftoff_core::ports::RepoRole;
use serde_json::json;

use crate::support::{WorldCtx, detail, is_problem, strings};

const SHOP_ENVS: &str = "/api/v1/projects/shop/environments";

mod list {
    use super::*;

    // TC: REST-04
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", SHOP_ENVS, None, None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04
    #[tokio::test]
    async fn prod_on_main_and_qa_on_dev_when_owner() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("GET", SHOP_ENVS, None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                strings(&body, "name"),
                strings(&body, "branch"),
                strings(&body, "state")
            ),
            (
                StatusCode::OK,
                vec!["prod".to_owned(), "qa".to_owned()],
                vec!["main".to_owned(), "dev".to_owned()],
                vec!["empty".to_owned(), "empty".to_owned()]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04
    #[tokio::test]
    async fn not_found_when_project_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/projects/nope/environments", None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("nope")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }
}

mod create {
    use super::*;

    // TC: REST-04 (ENV-02 over HTTP: creating needs repository admin)
    #[tokio::test]
    async fn forbidden_and_not_created_when_owner_only_writes_the_repository() {
        let ctx = WorldCtx::new().await;
        ctx.world.ports.git.grant(
            &ctx.world.owner.login,
            &ctx.world.shop.repo,
            RepoRole::Write,
        );
        let reply = ctx
            .owner(
                "POST",
                SHOP_ENVS,
                Some(&json!({"name": "staging", "branch": "dev"})),
            )
            .await;
        let again = ctx
            .owner("GET", &format!("{SHOP_ENVS}/staging"), None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), again.status),
            (StatusCode::FORBIDDEN, true, StatusCode::NOT_FOUND),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04
    #[tokio::test]
    async fn created_empty_compose_pinned_to_branch_when_owner() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                SHOP_ENVS,
                Some(&json!({"name": "stage", "branch": "release"})),
            )
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["project"].as_str(),
                body["name"].as_str(),
                body["kind"].as_str(),
                body["branch"].as_str(),
                body["state"].as_str(),
                body["pinned_by"].as_str().map(str::to_owned)
            ),
            (
                StatusCode::CREATED,
                Some("shop"),
                Some("stage"),
                Some("compose"),
                Some("release"),
                Some("empty"),
                Some(ctx.world.owner.id.to_string())
            ),
            "{}",
            reply.text()
        );
    }

    // TC: VIS-01 (a tunnel comes from `GET /tunnel`, private and owned;
    // this route would make it shared and ownerless)
    #[tokio::test]
    async fn bad_request_naming_kind_and_not_created_when_kind_is_tunnel() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                SHOP_ENVS,
                Some(&json!({"name": "shop-dev", "branch": "dev", "kind": "tunnel"})),
            )
            .await;
        let listed = ctx
            .owner("GET", &format!("{SHOP_ENVS}/shop-dev"), None)
            .await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                detail(&reply).contains("kind"),
                listed.status
            ),
            (StatusCode::BAD_REQUEST, true, true, StatusCode::NOT_FOUND),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04 (ENV-05 over HTTP)
    #[tokio::test]
    async fn conflict_when_name_is_taken() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                SHOP_ENVS,
                Some(&json!({"name": "qa", "branch": "other"})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04 (`deny_unknown_fields`)
    #[tokio::test]
    async fn bad_request_when_body_has_unknown_field() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                SHOP_ENVS,
                Some(&json!({"name": "stage", "branch": "release", "target": "x"})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }
}

mod get {
    use super::*;

    // TC: REST-04 (the world's compose environments are `shared`: their
    // domains are public sites)
    #[tokio::test]
    async fn qa_with_its_limits_when_known() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/projects/shop/environments/qa", None)
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["name"].as_str(),
                body["id"].as_str().map(str::to_owned),
                body["cpus"].as_f64(),
                body["memory_mb"].as_u64(),
                body["visibility"].as_str()
            ),
            (
                StatusCode::OK,
                Some("qa"),
                Some(ctx.world.qa.id.to_string()),
                Some(1.0),
                Some(1024),
                Some("shared")
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04
    #[tokio::test]
    async fn not_found_naming_it_when_environment_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/projects/shop/environments/nope", None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("nope")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }
}

mod patch {
    use super::*;

    // TC: REST-04 (`lof admin env set`: limits are the admin's)
    #[tokio::test]
    async fn forbidden_and_unchanged_when_a_non_admin_patches_limits() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol(
                "PATCH",
                "/api/v1/projects/shop/environments/qa",
                Some(&json!({"cpus": 2.0})),
            )
            .await;
        let again = ctx
            .owner("GET", "/api/v1/projects/shop/environments/qa", None)
            .await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                again.json()["cpus"].as_f64() == Some(2.0)
            ),
            (StatusCode::FORBIDDEN, true, false),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04
    #[tokio::test]
    async fn repinned_when_branch_patched() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "PATCH",
                "/api/v1/projects/shop/environments/qa",
                Some(&json!({"branch": "staging"})),
            )
            .await;
        let again = ctx
            .owner("GET", "/api/v1/projects/shop/environments/qa", None)
            .await;
        assert_eq!(
            (
                reply.status,
                reply.json()["branch"].as_str(),
                again.json()["branch"].as_str()
            ),
            (StatusCode::OK, Some("staging"), Some("staging")),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04 (`lof admin env set --cpus --memory`)
    #[tokio::test]
    async fn limits_changed_when_cpus_and_memory_patched() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "PATCH",
                "/api/v1/projects/shop/environments/qa",
                Some(&json!({"cpus": 2.0, "memory_mb": 2048})),
            )
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["cpus"].as_f64(),
                body["memory_mb"].as_u64()
            ),
            (StatusCode::OK, Some(2.0), Some(2048)),
            "{}",
            reply.text()
        );
    }
}

mod delete {
    use super::*;

    // TC: REST-04 (deleting an environment deletes its secrets)
    #[tokio::test]
    async fn forbidden_and_kept_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol(
                "DELETE",
                "/api/v1/projects/shop/environments/qa?confirm=true",
                None,
            )
            .await;
        let again = ctx
            .owner("GET", "/api/v1/projects/shop/environments/qa", None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), again.status),
            (StatusCode::FORBIDDEN, true, StatusCode::OK),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04 (ENV-07: `rm` needs `--yes`)
    #[tokio::test]
    async fn bad_request_and_kept_when_not_confirmed() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("DELETE", "/api/v1/projects/shop/environments/qa", None)
            .await;
        let again = ctx
            .owner("GET", "/api/v1/projects/shop/environments/qa", None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), again.status),
            (StatusCode::BAD_REQUEST, true, StatusCode::OK),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04
    #[tokio::test]
    async fn deleted_and_gone_when_confirmed() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "DELETE",
                "/api/v1/projects/shop/environments/qa?confirm=true",
                None,
            )
            .await;
        let again = ctx
            .owner("GET", "/api/v1/projects/shop/environments/qa", None)
            .await;
        assert_eq!(
            (reply.status, reply.json(), again.status),
            (
                StatusCode::OK,
                json!({"deleted": "qa"}),
                StatusCode::NOT_FOUND
            ),
            "{}",
            reply.text()
        );
    }
}

mod lifecycle {
    use super::*;

    // TC: REST-04 (ENV-06 over HTTP)
    #[tokio::test]
    async fn stopped_when_running_environment_stopped() {
        let ctx = WorldCtx::new().await;
        let _prod = ctx
            .world
            .deployed(&ctx.world.prod, EnvState::Running)
            .await
            .unwrap();
        let reply = ctx
            .owner("POST", "/api/v1/projects/shop/environments/prod/stop", None)
            .await;
        assert_eq!(
            (reply.status, reply.json()["state"].as_str()),
            (StatusCode::OK, Some("stopped")),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04
    #[tokio::test]
    async fn running_when_stopped_environment_started() {
        let ctx = WorldCtx::new().await;
        let _prod = ctx
            .world
            .deployed(&ctx.world.prod, EnvState::Stopped)
            .await
            .unwrap();
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/projects/shop/environments/prod/start",
                None,
            )
            .await;
        assert_eq!(
            (reply.status, reply.json()["state"].as_str()),
            (StatusCode::OK, Some("running")),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04 (never deployed: nothing to start)
    #[tokio::test]
    async fn conflict_when_empty_environment_started() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("POST", "/api/v1/projects/shop/environments/qa/start", None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04, DEP-12 (the REST half)
    #[tokio::test]
    async fn accepted_with_deploy_two_when_deployed_environment_redeployed() {
        let ctx = WorldCtx::new().await;
        let _prod = ctx
            .world
            .deployed(&ctx.world.prod, EnvState::Running)
            .await
            .unwrap();
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/projects/shop/environments/prod/redeploy",
                Some(&json!({"to": 1})),
            )
            .await;
        let queued = &reply.json()["deploys"][0];
        assert_eq!(
            (
                reply.status,
                queued["project"].as_str(),
                queued["environment"].as_str(),
                queued["number"].as_u64()
            ),
            (StatusCode::ACCEPTED, Some("shop"), Some("prod"), Some(2)),
            "{}",
            reply.text()
        );
    }

    // TC: REST-04
    #[tokio::test]
    async fn unauthorized_when_stopped_without_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call(
                "POST",
                "/api/v1/projects/shop/environments/prod/stop",
                None,
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }
}

mod deploys {
    use super::*;

    // TC: REST-04
    #[tokio::test]
    async fn numbers_and_statuses_newest_first_when_listed() {
        let ctx = WorldCtx::new().await;
        let _first = ctx
            .world
            .add_deploy(&ctx.world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        let _second = ctx
            .world
            .add_deploy(&ctx.world.qa, 2, DeployStatus::Failed)
            .await
            .unwrap();
        let reply = ctx
            .owner("GET", "/api/v1/projects/shop/environments/qa/deploys", None)
            .await;
        let body = reply.json();
        let numbers: Vec<u64> = body
            .as_array()
            .map(|items| items.iter().filter_map(|d| d["number"].as_u64()).collect())
            .unwrap_or_default();
        assert_eq!(
            (reply.status, numbers, strings(&body, "status")),
            (
                StatusCode::OK,
                vec![2, 1],
                vec!["failed".to_owned(), "succeeded".to_owned()]
            ),
            "{}",
            reply.text()
        );
    }
}

mod logs {
    use super::*;

    // TC: REST-04 (`lof logs`: one SSE event per line)
    #[tokio::test]
    async fn one_event_per_line_when_logs_read() {
        let ctx = WorldCtx::new().await;
        ctx.world.ports.docker.set_logs(
            &ComposeProject::new(&ctx.world.shop.name, &ctx.world.qa.name),
            vec!["web  | started".to_owned(), "web  | ready".to_owned()],
        );
        let reply = ctx
            .owner(
                "GET",
                "/api/v1/projects/shop/environments/qa/logs?follow=false",
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                reply
                    .headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                reply.text().contains("data: web  | started\n\n"),
                reply.text().contains("data: web  | ready\n\n")
            ),
            (StatusCode::OK, Some("text/event-stream"), true, true),
            "{}",
            reply.text()
        );
    }
}
