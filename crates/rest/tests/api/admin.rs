//! REST-09: administration (`/admin/users`, `/admin/sessions`,
//! `/admin/reconcile`; `/admin/token` is in `auth.rs`).

use http::StatusCode;
use liftoff_core::domain::UserRole;
use serde_json::json;

use crate::support::{WorldCtx, detail, is_problem, strings};

mod users {
    use super::*;

    // TC: REST-09
    #[tokio::test]
    async fn every_user_when_admin() {
        let ctx = WorldCtx::new().await;
        let _carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        let reply = ctx.owner("GET", "/api/v1/admin/users", None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                strings(&body, "login"),
                strings(&body, "role")
            ),
            (
                StatusCode::OK,
                vec!["carol".to_owned(), "owner".to_owned()],
                vec!["user".to_owned(), "admin".to_owned()]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-09
    #[tokio::test]
    async fn forbidden_when_not_admin() {
        let ctx = WorldCtx::new().await;
        let carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        let secret = ctx.world.login(&carol).await.unwrap();
        let reply = ctx
            .call("GET", "/api/v1/admin/users", Some(secret.expose()), None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-09
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", "/api/v1/admin/users", None, None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }
}

mod sessions {
    use super::*;

    // TC: REST-09 (`lof admin sessions revoke <login>`)
    #[tokio::test]
    async fn revoked_and_refused_afterwards_when_login_known() {
        let ctx = WorldCtx::new().await;
        let carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        let secret = ctx.world.login(&carol).await.unwrap();
        let reply = ctx
            .owner("DELETE", "/api/v1/admin/sessions?login=carol", None)
            .await;
        let me = ctx
            .call("GET", "/api/v1/me", Some(secret.expose()), None)
            .await;
        assert_eq!(
            (reply.status, reply.json(), me.status),
            (
                StatusCode::OK,
                json!({"deleted": "carol"}),
                StatusCode::UNAUTHORIZED
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-09
    #[tokio::test]
    async fn not_found_naming_the_login_when_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("DELETE", "/api/v1/admin/sessions?login=mallory", None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("mallory")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-09
    #[tokio::test]
    async fn forbidden_and_owner_kept_when_not_admin() {
        let ctx = WorldCtx::new().await;
        let owner = ctx.world.login(&ctx.world.owner).await.unwrap();
        let carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        let secret = ctx.world.login(&carol).await.unwrap();
        let reply = ctx
            .call(
                "DELETE",
                "/api/v1/admin/sessions?login=owner",
                Some(secret.expose()),
                None,
            )
            .await;
        let me = ctx
            .call("GET", "/api/v1/me", Some(owner.expose()), None)
            .await;
        assert_eq!(
            (reply.status, me.status),
            (StatusCode::FORBIDDEN, StatusCode::OK),
            "{}",
            reply.text()
        );
    }
}

mod reconcile {
    use super::*;

    // TC: REST-09 (the whole Caddy config in one push, no deploys)
    #[tokio::test]
    async fn no_content_and_one_proxy_push_without_deploys_when_admin() {
        let ctx = WorldCtx::new().await;
        let owner = ctx.world.login(&ctx.world.owner).await.unwrap();
        ctx.world.log().clear();
        let reply = ctx
            .call(
                "POST",
                "/api/v1/admin/reconcile",
                Some(owner.expose()),
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                ctx.world.ports.proxy.apply_count(),
                ctx.world.ports.queue.enqueued().len()
            ),
            (StatusCode::NO_CONTENT, 1, 0),
            "{}",
            reply.text()
        );
    }

    // TC: REST-09
    #[tokio::test]
    async fn forbidden_when_not_admin() {
        let ctx = WorldCtx::new().await;
        let carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        let secret = ctx.world.login(&carol).await.unwrap();
        let reply = ctx
            .call(
                "POST",
                "/api/v1/admin/reconcile",
                Some(secret.expose()),
                None,
            )
            .await;
        assert_eq!(
            (reply.status, ctx.world.ports.proxy.apply_count()),
            (StatusCode::FORBIDDEN, 0),
            "{}",
            reply.text()
        );
    }
}
