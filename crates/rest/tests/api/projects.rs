//! REST-03: projects (`/projects`, `/projects/{p}`, `/projects/{p}/resolve`).

use http::StatusCode;
use liftoff_core::domain::RepoRef;
use liftoff_core::ports::RepoRole;
use serde_json::json;

use crate::support::{WorldCtx, detail, is_problem, strings};

const PROJECTS: &str = "/api/v1/projects";

/// Makes `owner` admin of `repo` on the world's forge.
fn grant(ctx: &WorldCtx, repo: &str) {
    ctx.world.ports.git.grant(
        &ctx.world.owner.login,
        &RepoRef::new(repo).unwrap(),
        RepoRole::Admin,
    );
}

mod list {
    use super::*;

    // TC: REST-03
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", PROJECTS, None, None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03
    #[tokio::test]
    async fn shop_when_owner() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("GET", PROJECTS, None).await;
        assert_eq!(
            (reply.status, strings(&reply.json(), "name")),
            (StatusCode::OK, vec!["shop".to_owned()]),
            "{}",
            reply.text()
        );
    }
}

mod create {
    use super::*;

    // TC: REST-03
    #[tokio::test]
    async fn created_when_owner_administers_the_repository() {
        let ctx = WorldCtx::new().await;
        grant(&ctx, "owner/blog");
        let reply = ctx
            .owner(
                "POST",
                PROJECTS,
                Some(&json!({"name": "blog", "git_host": "git.example.com", "repo": "owner/blog"})),
            )
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["name"].as_str(),
                body["git_host"].as_str(),
                body["repo"].as_str()
            ),
            (
                StatusCode::CREATED,
                Some("blog"),
                Some("git.example.com"),
                Some("owner/blog")
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03
    #[tokio::test]
    async fn conflict_when_name_is_taken() {
        let ctx = WorldCtx::new().await;
        grant(&ctx, "owner/other");
        let reply = ctx
            .owner(
                "POST",
                PROJECTS,
                Some(
                    &json!({"name": "shop", "git_host": "git.example.com", "repo": "owner/other"}),
                ),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03
    #[tokio::test]
    async fn forbidden_when_owner_does_not_administer_the_repository() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                PROJECTS,
                Some(&json!({"name": "blog", "git_host": "git.example.com", "repo": "owner/blog"})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03 (validate at the boundary: the project name regex)
    #[tokio::test]
    async fn bad_request_naming_the_field_when_name_is_invalid() {
        let ctx = WorldCtx::new().await;
        grant(&ctx, "owner/blog");
        let reply = ctx
            .owner(
                "POST",
                PROJECTS,
                Some(
                    &json!({"name": "Blog!", "git_host": "git.example.com", "repo": "owner/blog"}),
                ),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("name")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03 (`deny_unknown_fields`)
    #[tokio::test]
    async fn bad_request_when_body_has_unknown_field() {
        let ctx = WorldCtx::new().await;
        grant(&ctx, "owner/blog");
        let reply = ctx
            .owner(
                "POST",
                PROJECTS,
                Some(&json!({
                    "name": "blog",
                    "git_host": "git.example.com",
                    "repo": "owner/blog",
                    "template": "acme/web"
                })),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: TEN-04 (naming a tenant without a module that keeps tenants is 424, and nothing is created)
    #[tokio::test]
    async fn failed_dependency_when_tenant_named_without_tenants_module() {
        let ctx = WorldCtx::new().await;
        grant(&ctx, "owner/blog");
        let reply = ctx
            .owner(
                "POST",
                PROJECTS,
                Some(&json!({
                    "name": "blog",
                    "git_host": "git.example.com",
                    "repo": "owner/blog",
                    "tenant": "acme"
                })),
            )
            .await;
        let listed = ctx.owner("GET", PROJECTS, None).await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                strings(&listed.json(), "name")
            ),
            (StatusCode::FAILED_DEPENDENCY, true, vec!["shop".to_owned()]),
            "{}",
            reply.text()
        );
    }
}

mod get {
    use super::*;

    // TC: REST-03
    #[tokio::test]
    async fn shop_with_its_repository_when_known() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("GET", "/api/v1/projects/shop", None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["name"].as_str(),
                body["git_host"].as_str(),
                body["repo"].as_str(),
                body["id"].as_str().map(str::to_owned)
            ),
            (
                StatusCode::OK,
                Some("shop"),
                Some("git.example.com"),
                Some("owner/shop"),
                Some(ctx.world.shop.id.to_string())
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03
    #[tokio::test]
    async fn not_found_naming_the_project_when_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("GET", "/api/v1/projects/nope", None).await;
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

    // TC: REST-03 (SEC-10's owner check: single-user mode, only the admin)
    #[tokio::test]
    async fn forbidden_and_kept_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol(
                "PATCH",
                "/api/v1/projects/shop",
                Some(&json!({"manifest": MANIFEST})),
            )
            .await;
        let again = ctx.owner("GET", "/api/v1/projects/shop", None).await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                again.json()["manifest"].as_str()
            ),
            (StatusCode::FORBIDDEN, true, None),
            "{}",
            reply.text()
        );
    }

    const MANIFEST: &str = "[project]\nname = \"shop\"\n";

    // TC: REST-03
    #[tokio::test]
    async fn manifest_replaced_when_patched() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "PATCH",
                "/api/v1/projects/shop",
                Some(&json!({"manifest": MANIFEST})),
            )
            .await;
        let again = ctx.owner("GET", "/api/v1/projects/shop", None).await;
        assert_eq!(
            (
                reply.status,
                reply.json()["manifest"].as_str(),
                again.json()["manifest"].as_str()
            ),
            (StatusCode::OK, Some(MANIFEST), Some(MANIFEST)),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03
    #[tokio::test]
    async fn not_found_when_project_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "PATCH",
                "/api/v1/projects/nope",
                Some(&json!({"manifest": MANIFEST})),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("nope")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }
}

mod delete {
    use super::*;

    // TC: REST-03 (deleting a project deletes its secrets and deploy tokens)
    #[tokio::test]
    async fn forbidden_and_kept_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        let _blog = ctx.world.add_project("blog", "owner/blog").await.unwrap();
        let reply = ctx.carol("DELETE", "/api/v1/projects/blog", None).await;
        let again = ctx.owner("GET", "/api/v1/projects/blog", None).await;
        assert_eq!(
            (reply.status, is_problem(&reply), again.status),
            (StatusCode::FORBIDDEN, true, StatusCode::OK),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03
    #[tokio::test]
    async fn deleted_and_gone_when_project_has_no_environments() {
        let ctx = WorldCtx::new().await;
        let _blog = ctx.world.add_project("blog", "owner/blog").await.unwrap();
        let reply = ctx.owner("DELETE", "/api/v1/projects/blog", None).await;
        let again = ctx.owner("GET", "/api/v1/projects/blog", None).await;
        assert_eq!(
            (reply.status, reply.json(), again.status),
            (
                StatusCode::OK,
                json!({"deleted": "blog"}),
                StatusCode::NOT_FOUND
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03 (environments go first, like domains with bindings)
    #[tokio::test]
    async fn conflict_and_kept_when_project_has_environments() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("DELETE", "/api/v1/projects/shop", None).await;
        let again = ctx.owner("GET", "/api/v1/projects/shop", None).await;
        assert_eq!(
            (reply.status, is_problem(&reply), again.status),
            (StatusCode::CONFLICT, true, StatusCode::OK),
            "{}",
            reply.text()
        );
    }
}

/// `POST /projects/{p}/resolve`: the CLI's `git remote` to a project; the
/// CLI sends `-` as `{p}` when it knows no name.
mod resolve {
    use super::*;

    const RESOLVE: &str = "/api/v1/projects/-/resolve";

    // TC: REST-03, CLIX-04 (the server half)
    #[tokio::test]
    async fn shop_when_repository_is_owner_shop_on_git_example_com() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                RESOLVE,
                Some(&json!({"git_host": "git.example.com", "repo": "owner/shop"})),
            )
            .await;
        assert_eq!(
            (reply.status, reply.json()["name"].as_str()),
            (StatusCode::OK, Some("shop")),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03
    #[tokio::test]
    async fn not_found_when_repository_has_no_project() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                RESOLVE,
                Some(&json!({"git_host": "git.example.com", "repo": "owner/unknown"})),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("owner/unknown")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-03
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call(
                "POST",
                RESOLVE,
                None,
                Some(&json!({"git_host": "git.example.com", "repo": "owner/shop"})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }
}
