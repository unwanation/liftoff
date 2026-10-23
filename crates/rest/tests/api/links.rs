//! `POST /auth/links`: a one-time
//! link that opens a `browser` session for a target.

use http::StatusCode;
use liftoff_core::domain::{LinkSecret, UserRole};
use liftoff_core::ports::{HookResult, Store as _};
use serde_json::json;

use crate::support::{WorldCtx, detail, is_problem};

const LINKS: &str = "/api/v1/auth/links";

/// The prefix of every link of the world (its API host is
/// `liftoff.test`).
const LINK_PREFIX: &str = "https://liftoff.test/api/v1/auth/links/";

/// `{"target": "environment:<id of prod>"}`.
fn prod_target(ctx: &WorldCtx) -> serde_json::Value {
    json!({"target": format!("environment:{}", ctx.world.prod.id)})
}

mod create {
    use super::*;

    // TC: OTL-01 (the link lives ten minutes: the world's clock stands at
    // 2026-10-13T00:00:00Z)
    #[tokio::test]
    async fn created_with_expiry_ten_minutes_ahead_when_owner_asks() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("POST", LINKS, Some(&prod_target(&ctx))).await;
        assert_eq!(
            (reply.status, reply.json()["expires_at"].as_str()),
            (StatusCode::CREATED, Some("2026-10-13T00:10:00Z")),
            "{}",
            reply.text()
        );
    }

    // TC: OTL-01 (the link is `GET /auth/links/{token}` on the API host)
    #[tokio::test]
    async fn url_on_api_host_when_created() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("POST", LINKS, Some(&prod_target(&ctx))).await;
        let url = reply.json()["url"].as_str().unwrap_or_default().to_owned();
        let token = url.strip_prefix(LINK_PREFIX).unwrap_or_default();
        assert_eq!(
            (reply.status, LinkSecret::parse(token).is_ok()),
            (StatusCode::CREATED, true),
            "{}",
            reply.text()
        );
    }

    // TC: OTL-05 (the store holds the hash of the token in the URL)
    #[tokio::test]
    async fn stored_by_hash_of_token_when_created() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("POST", LINKS, Some(&prod_target(&ctx))).await;
        let url = reply.json()["url"].as_str().unwrap_or_default().to_owned();
        let token = LinkSecret::parse(url.strip_prefix(LINK_PREFIX).unwrap_or_default());
        let stored = match token {
            Ok(token) => ctx
                .world
                .ports
                .store
                .one_time_link(&token.hash())
                .await
                .unwrap()
                .map(|link| link.user_id),
            Err(_) => None,
        };
        assert_eq!(stored, Some(ctx.world.owner.id), "{}", reply.text());
    }

    // TC: OTL-03 (a link only for a target the caller may open)
    #[tokio::test]
    async fn forbidden_when_caller_may_not_open_target() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.carol("POST", LINKS, Some(&prod_target(&ctx))).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: OTL-03 (a target that is no environment is refused)
    #[tokio::test]
    async fn bad_request_when_target_malformed() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("POST", LINKS, Some(&json!({"target": "project:shop"})))
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("target")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: OTL-03
    #[tokio::test]
    async fn not_found_when_target_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                LINKS,
                Some(&json!({"target": "environment:01ARZ3NDEKTSV4RRFFQ69G5FAV"})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.text());
    }

    // TC: OTL-04 (only a session may ask for a link)
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call("POST", LINKS, None, Some(&prod_target(&ctx)))
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }

    // TC: VIS-06 (a module lets carol open the project, but a link to
    // another user's tunnel is only the owner's or the admin's to hand out)
    #[tokio::test]
    async fn forbidden_when_target_is_another_users_tunnel_and_authorize_allows() {
        let ctx = WorldCtx::new().await;
        let alice = ctx.world.add_user("alice", UserRole::User).await.unwrap();
        let tunnel = ctx
            .world
            .add_tunnel(&ctx.world.shop, "shop-alice", &alice)
            .await
            .unwrap();
        ctx.world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let reply = ctx
            .carol(
                "POST",
                LINKS,
                Some(&json!({"target": format!("environment:{}", tunnel.id)})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    }
}
