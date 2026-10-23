//! REST-07: deploys (`POST /deploys`, `GET /deploys/{id}[/log]`) and the
//! deploy token (`POST /projects/{p}/token/rotate`, `GET
//! /projects/{p}/snippet`); with the REST halves of DEP-01, DEP-02 and
//! DEP-04 that checked on the service only.

use http::{StatusCode, header};
use liftoff_core::domain::{DeployStatus, RepoRef, SecretKey};
use serde_json::{Value, json};

use crate::support::{Reply, WorldCtx, detail, is_problem, strings};

const DEPLOYS: &str = "/api/v1/deploys";
const COMPOSE: &str = "services:\n  web:\n    image: git.example.com/owner/shop:sha-1\n";

fn ci(repo: &str, git_ref: &str) -> Value {
    json!({"repo": repo, "ref": git_ref, "tag": "sha-1", "compose": COMPOSE})
}

/// `POST /deploys` with `body` as `shop`'s CI.
async fn deploy_as_shop_ci(ctx: &WorldCtx, body: &Value) -> Reply {
    let token = ctx.world.deploy_token(&ctx.world.shop).await.unwrap();
    ctx.call("POST", DEPLOYS, Some(token.expose()), Some(body))
        .await
}

mod ci {
    use super::*;

    // TC: REST-07
    #[tokio::test]
    async fn accepted_with_prod_deploy_one_when_main_pushed() {
        let ctx = WorldCtx::new().await;
        let reply = deploy_as_shop_ci(&ctx, &ci("owner/shop", "main")).await;
        let queued = &reply.json()["deploys"];
        assert_eq!(
            (
                reply.status,
                queued.as_array().map(Vec::len),
                queued[0]["project"].as_str(),
                queued[0]["environment"].as_str(),
                queued[0]["number"].as_u64(),
                queued[0]["deploy_id"].is_string()
            ),
            (
                StatusCode::ACCEPTED,
                Some(1),
                Some("shop"),
                Some("prod"),
                Some(1),
                true
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07 (the CI step of the snippet sends multipart/form-data)
    #[tokio::test]
    async fn accepted_when_ci_posts_multipart_form() {
        let ctx = WorldCtx::new().await;
        let token = ctx.world.deploy_token(&ctx.world.shop).await.unwrap();
        let boundary = "XBOUNDARYX";
        let field = |name: &str, value: &str| {
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n\
                 {value}\r\n"
            )
        };
        let file = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"compose\"; \
             filename=\"compose.yaml\"\r\nContent-Type: application/octet-stream\r\n\r\n\
             {COMPOSE}\r\n"
        );
        let body = format!(
            "{}{}{}{file}--{boundary}--\r\n",
            field("repo", "owner/shop"),
            field("ref", "main"),
            field("tag", "sha-1"),
        );
        let authorization = format!("Bearer {}", token.expose());
        let content_type = format!("multipart/form-data; boundary={boundary}");
        let reply = ctx
            .raw(
                None,
                "POST",
                DEPLOYS,
                &[
                    ("authorization", &authorization),
                    ("content-type", &content_type),
                ],
                body.into_bytes(),
            )
            .await;
        assert_eq!(
            (
                reply.status,
                strings(&reply.json()["deploys"], "environment")
            ),
            (StatusCode::ACCEPTED, vec!["prod".to_owned()]),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07, DEP-02
    #[tokio::test]
    async fn ok_with_no_deploys_when_no_environment_on_ref() {
        let ctx = WorldCtx::new().await;
        let reply = deploy_as_shop_ci(&ctx, &ci("owner/shop", "feature/x")).await;
        assert_eq!(
            (reply.status, reply.json()),
            (StatusCode::OK, json!({"deploys": []})),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07, DEP-01
    #[tokio::test]
    async fn forbidden_when_token_is_of_another_project() {
        let ctx = WorldCtx::new().await;
        let blog = ctx.world.add_project("blog", "owner/blog").await.unwrap();
        let token = ctx.world.deploy_token(&blog).await.unwrap();
        let reply = ctx
            .call(
                "POST",
                DEPLOYS,
                Some(token.expose()),
                Some(&ci("owner/shop", "main")),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07, DEP-04
    #[tokio::test]
    async fn deprecation_header_when_body_carries_env() {
        let ctx = WorldCtx::new().await;
        let mut body = ci("owner/shop", "main");
        body["env"] = json!("GREETING=hi\n");
        let reply = deploy_as_shop_ci(&ctx, &body).await;
        assert_eq!(
            (reply.status, reply.headers.contains_key("deprecation")),
            (StatusCode::ACCEPTED, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07, DEP-04
    #[tokio::test]
    async fn no_deprecation_header_when_body_has_no_env() {
        let ctx = WorldCtx::new().await;
        let reply = deploy_as_shop_ci(&ctx, &ci("owner/shop", "main")).await;
        assert_eq!(
            (reply.status, reply.headers.contains_key("deprecation")),
            (StatusCode::ACCEPTED, false),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn unauthorized_when_no_token() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call("POST", DEPLOYS, None, Some(&ci("owner/shop", "main")))
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07 (`deny_unknown_fields`)
    #[tokio::test]
    async fn bad_request_when_body_has_unknown_field() {
        let ctx = WorldCtx::new().await;
        let mut body = ci("owner/shop", "main");
        body["target"] = json!("acme/qa");
        let reply = deploy_as_shop_ci(&ctx, &body).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }
}

/// `POST /deploys` with a session: `lof deploy --tag T [--compose F]`.
mod session {
    use liftoff_core::domain::EnvState;

    use super::*;

    // TC: REST-07 (a deploy receives every secret of its scope: SEC-10's
    // owner check, single-user mode, the admin)
    #[tokio::test]
    async fn forbidden_and_nothing_queued_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol(
                "POST",
                DEPLOYS,
                Some(&json!({"project": "shop", "environment": "prod", "tag": "sha-2"})),
            )
            .await;
        let deploys = ctx
            .owner(
                "GET",
                "/api/v1/projects/shop/environments/prod/deploys",
                None,
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), deploys.json()),
            (StatusCode::FORBIDDEN, true, json!([])),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07 (the same route with a session)
    #[tokio::test]
    async fn accepted_with_the_next_deploy_when_owner_deploys_a_tag() {
        let ctx = WorldCtx::new().await;
        let _prod = ctx
            .world
            .deployed(&ctx.world.prod, EnvState::Running)
            .await
            .unwrap();
        let reply = ctx
            .owner(
                "POST",
                DEPLOYS,
                Some(&json!({"project": "shop", "environment": "prod", "tag": "sha-2"})),
            )
            .await;
        let queued = &reply.json()["deploys"][0];
        assert_eq!(
            (
                reply.status,
                queued["environment"].as_str(),
                queued["number"].as_u64()
            ),
            (StatusCode::ACCEPTED, Some("prod"), Some(2)),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07 (a session names the environment, not the branch)
    #[tokio::test]
    async fn bad_request_when_a_session_sends_ci_fields() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("POST", DEPLOYS, Some(&ci("owner/shop", "main")))
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

    // TC: REST-07 (a deploy log carries the container output)
    #[tokio::test]
    async fn forbidden_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        let deploy = ctx
            .world
            .add_deploy(&ctx.world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        let reply = ctx
            .carol("GET", &format!("{DEPLOYS}/{}/log", deploy.id), None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn the_deploy_when_known() {
        let ctx = WorldCtx::new().await;
        let deploy = ctx
            .world
            .add_deploy(&ctx.world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        let reply = ctx
            .owner("GET", &format!("{DEPLOYS}/{}", deploy.id), None)
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["id"].as_str().map(str::to_owned),
                body["number"].as_u64(),
                body["project"].as_str(),
                body["environment"].as_str(),
                body["status"].as_str(),
                body["tag"].as_str()
            ),
            (
                StatusCode::OK,
                Some(deploy.id.to_string()),
                Some(1),
                Some("shop"),
                Some("qa"),
                Some("succeeded"),
                Some("sha-1")
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn not_found_when_id_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "GET",
                &format!("{DEPLOYS}/01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).contains("01ARZ3NDEKTSV4RRFFQ69G5FAV")
            ),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07 (validate at the boundary: ids are ULIDs)
    #[tokio::test]
    async fn bad_request_when_id_is_not_a_ulid() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("GET", &format!("{DEPLOYS}/twelve"), None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn plain_text_log_when_log_read() {
        let ctx = WorldCtx::new().await;
        let deploy = ctx
            .world
            .add_deploy(&ctx.world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        let reply = ctx
            .owner("GET", &format!("{DEPLOYS}/{}/log", deploy.id), None)
            .await;
        assert_eq!(
            (
                reply.status,
                reply
                    .headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
            ),
            (StatusCode::OK, Some("text/plain; charset=utf-8")),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let deploy = ctx
            .world
            .add_deploy(&ctx.world.qa, 1, DeployStatus::Succeeded)
            .await
            .unwrap();
        let reply = ctx
            .call("GET", &format!("{DEPLOYS}/{}", deploy.id), None, None)
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }
}

mod token {
    use super::*;

    const ROTATE: &str = "/api/v1/projects/shop/token/rotate";

    // TC: REST-07 (the value is never returned)
    #[tokio::test]
    async fn new_token_id_without_value_when_rotated() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("POST", ROTATE, None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["project"].as_str(),
                body["id"].is_string(),
                reply.text().contains("lf_")
            ),
            (StatusCode::OK, Some("shop"), true, false),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn old_token_refused_when_rotated() {
        let ctx = WorldCtx::new().await;
        let old = ctx.world.deploy_token(&ctx.world.shop).await.unwrap();
        let _rotated = ctx.owner("POST", ROTATE, None).await;
        let reply = ctx
            .call(
                "POST",
                DEPLOYS,
                Some(old.expose()),
                Some(&ci("owner/shop", "main")),
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }

    // TC: REST-07, CRE-08 (the deploy token is the project secret `LIFTOFF_TOKEN` with `ci = true`, write-only to people)
    #[tokio::test]
    async fn stored_as_ci_project_secret_liftoff_token_never_shown_when_rotated() {
        let ctx = WorldCtx::new().await;
        let _rotated = ctx.owner("POST", ROTATE, None).await;
        let shown = ctx
            .owner(
                "GET",
                "/api/v1/secrets/LIFTOFF_TOKEN?scope=project&id=shop",
                None,
            )
            .await;
        let in_ci = ctx
            .world
            .ports
            .git
            .ci_secret(
                &RepoRef::new("owner/shop").unwrap(),
                &SecretKey::new("LIFTOFF_TOKEN").unwrap(),
            )
            .map(|value| value.expose().to_owned())
            .unwrap_or_default();
        let listed = ctx
            .owner("GET", "/api/v1/secrets?scope=project&id=shop", None)
            .await
            .json();
        let ci_flag = listed
            .as_array()
            .and_then(|items| items.iter().find(|s| s["key"] == "LIFTOFF_TOKEN"))
            .and_then(|s| s["ci"].as_bool());
        assert_eq!(
            (
                shown.status,
                in_ci.starts_with("lf_") && !shown.text().contains(&in_ci),
                ci_flag
            ),
            (StatusCode::FORBIDDEN, true, Some(true)),
            "{} {listed}",
            shown.text()
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn unauthorized_when_rotated_without_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("POST", ROTATE, None, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }

    // TC: REST-07
    #[tokio::test]
    async fn not_found_when_project_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("POST", "/api/v1/projects/nope/token/rotate", None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("nope")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-07
    #[tokio::test]
    async fn snippet_posts_to_deploys_with_liftoff_token() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/projects/shop/snippet", None)
            .await;
        let snippet = reply.json()["snippet"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            (
                reply.status,
                snippet.contains("LIFTOFF_TOKEN"),
                snippet.contains("/api/v1/deploys")
            ),
            (StatusCode::OK, true, true),
            "{}",
            reply.text()
        );
    }
}

mod snippet {
    use super::*;

    // TC: SNIP-01 (the file the workflow belongs at travels with it, so
    // `lof create` can commit it)
    #[tokio::test]
    async fn answers_forgejo_workflow_path_when_project_forge_is_forgejo() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/projects/shop/snippet", None)
            .await;
        assert_eq!(
            (reply.status, reply.json()["path"].as_str()),
            (StatusCode::OK, Some(".forgejo/workflows/deploy.yml")),
            "{}",
            reply.text()
        );
    }

    // TC: SNIP-01 (Forgejo Actions: `docker buildx` to the forge's
    // registry, then `POST /deploys` of the API site)
    #[tokio::test]
    async fn answers_forgejo_actions_workflow_when_project_forge_is_forgejo() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/projects/shop/snippet", None)
            .await;
        let snippet = reply.json()["snippet"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            (
                reply.status,
                snippet.contains("runs-on: docker"),
                snippet.contains("echo \"IMAGE=git.example.com/$(echo \"$GITHUB_REPOSITORY\""),
                snippet.contains("tags: ${{ env.IMAGE }}:sha-${{ github.sha }}"),
                snippet.contains("curl -fsS https://liftoff.test/api/v1/deploys"),
                snippet.contains("-F \"compose=@deploy/compose.yaml\""),
            ),
            (StatusCode::OK, true, true, true, true, true),
            "{}",
            reply.text()
        );
    }
}
