//! `POST /projects/{p}/environments/{e}/share|unshare`.

use http::StatusCode;
use liftoff_core::domain::{UserRole, Visibility};
use liftoff_core::ports::Store as _;

use crate::support::{WorldCtx, is_problem};

/// The visibility of the environment `id` as stored now.
async fn stored(ctx: &WorldCtx, id: liftoff_core::domain::EnvironmentId) -> Visibility {
    ctx.world
        .ports
        .store
        .environment(id)
        .await
        .unwrap()
        .unwrap()
        .visibility
}

mod share {
    use super::*;

    // TC: VIS-01
    #[tokio::test]
    async fn shared_when_owner_shares_private_tunnel() {
        let ctx = WorldCtx::new().await;
        let tunnel = ctx
            .world
            .add_tunnel(&ctx.world.shop, "shop-dev", &ctx.world.owner)
            .await
            .unwrap();
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/projects/shop/environments/shop-dev/share",
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                reply.json()["visibility"].as_str(),
                stored(&ctx, tunnel.id).await
            ),
            (StatusCode::OK, Some("shared"), Visibility::Shared),
            "{}",
            reply.text()
        );
    }

    // TC: VIS-06
    #[tokio::test]
    async fn forbidden_when_environment_is_not_callers() {
        let ctx = WorldCtx::new().await;
        let tunnel = ctx
            .world
            .add_tunnel(&ctx.world.shop, "shop-dev", &ctx.world.owner)
            .await
            .unwrap();
        let reply = ctx
            .carol(
                "POST",
                "/api/v1/projects/shop/environments/shop-dev/share",
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                stored(&ctx, tunnel.id).await
            ),
            (StatusCode::FORBIDDEN, true, Visibility::Private),
            "{}",
            reply.text()
        );
    }

    // TC: VIS-06
    #[tokio::test]
    async fn not_found_when_environment_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/projects/shop/environments/nope/share",
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    }

    // TC: VIS-06
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call(
                "POST",
                "/api/v1/projects/shop/environments/prod/share",
                None,
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }
}

mod unshare {
    use super::*;

    // TC: VIS-01 (`prod` of the world is shared)
    #[tokio::test]
    async fn private_when_owner_unshares_shared_environment() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/projects/shop/environments/prod/unshare",
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                reply.json()["visibility"].as_str(),
                stored(&ctx, ctx.world.prod.id).await
            ),
            (StatusCode::OK, Some("private"), Visibility::Private),
            "{}",
            reply.text()
        );
    }

    // TC: VIS-06
    #[tokio::test]
    async fn forbidden_when_tunnel_is_another_users() {
        let ctx = WorldCtx::new().await;
        let alice = ctx.world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = ctx
            .world
            .add_tunnel(&ctx.world.shop, "shop-alice", &alice)
            .await
            .unwrap();
        let shared = liftoff_core::domain::Environment {
            visibility: Visibility::Shared,
            ..tunnel.clone()
        };
        ctx.world
            .ports
            .store
            .update_environment(&shared)
            .await
            .unwrap();
        let reply = ctx
            .carol(
                "POST",
                "/api/v1/projects/shop/environments/shop-alice/unshare",
                None,
            )
            .await;
        assert_eq!(
            (reply.status, stored(&ctx, tunnel.id).await),
            (StatusCode::FORBIDDEN, Visibility::Shared),
            "{}",
            reply.text()
        );
    }
}

mod owner {
    use super::*;

    // TC: VIS-01 (amendment of: `lof dev --share` reads whose tunnel the environment is before it shares it)
    #[tokio::test]
    async fn names_owner_when_environment_is_tunnel() {
        let ctx = WorldCtx::new().await;
        ctx.world
            .add_tunnel(&ctx.world.shop, "shop-dev", &ctx.world.owner)
            .await
            .unwrap();
        let reply = ctx
            .owner("GET", "/api/v1/projects/shop/environments/shop-dev", None)
            .await;
        let expected = ctx.world.owner.id.to_string();
        assert_eq!(
            (reply.status, reply.json()["owner"].as_str()),
            (StatusCode::OK, Some(expected.as_str())),
            "{}",
            reply.text()
        );
    }

    // TC: VIS-01
    #[tokio::test]
    async fn names_no_owner_when_environment_is_compose() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("GET", "/api/v1/projects/shop/environments/prod", None)
            .await;
        assert_eq!(
            (reply.status, reply.json()["owner"].clone()),
            (StatusCode::OK, serde_json::Value::Null),
            "{}",
            reply.text()
        );
    }
}
