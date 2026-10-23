//! Single-user login over HTTP: the bearer layer, `GET /me`,
//! `POST /auth/token`, `DELETE /auth/session`, `POST /admin/token`.

use http::StatusCode;
use liftoff_core::domain::UserRole;
use liftoff_core::ports::AuthResult;
use rstest::rstest;
use serde_json::json;

use crate::support::{UNKNOWN_SESSION, WorldCtx, is_problem};

/// Trades `token` for a session over HTTP; the reply of `POST /auth/token`.
async fn exchange(ctx: &WorldCtx, token: &str) -> crate::support::Reply {
    ctx.call(
        "POST",
        "/api/v1/auth/token",
        None,
        Some(&json!({"token": token, "name": "laptop"})),
    )
    .await
}

mod no_session {
    use super::*;

    // TC: LOGN-04, LOGN-06 (no session → 401)
    #[rstest]
    #[case::me("GET", "/api/v1/me")]
    #[case::server("GET", "/api/v1/server")]
    #[case::logout("DELETE", "/api/v1/auth/session")]
    #[case::admin_token("POST", "/api/v1/admin/token")]
    #[tokio::test]
    async fn unauthorized_when_no_bearer(#[case] method: &str, #[case] path: &str) {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call(method, path, None, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }

    // TC: LOGN-04 (no session → 401)
    #[tokio::test]
    async fn unauthorized_when_session_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call("GET", "/api/v1/me", Some(UNKNOWN_SESSION), None)
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }

    // TC: LOGN-04 (no session → 401)
    #[tokio::test]
    async fn unauthorized_when_bearer_malformed() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", "/api/v1/me", Some("hunter2"), None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }

    // TC: LOGN-04 (the answer never repeats the token)
    #[tokio::test]
    async fn token_not_echoed_when_session_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call("GET", "/api/v1/me", Some(UNKNOWN_SESSION), None)
            .await;
        assert!(
            reply.status == StatusCode::UNAUTHORIZED && !reply.text().contains(UNKNOWN_SESSION),
            "{} {}",
            reply.status,
            reply.text()
        );
    }
}

mod me {
    use super::*;

    // TC: LOGN-06
    #[tokio::test]
    async fn login_role_and_source_when_session_valid() {
        let ctx = WorldCtx::new().await;
        let secret = ctx.world.login(&ctx.world.owner).await.unwrap();
        let reply = ctx
            .call("GET", "/api/v1/me", Some(secret.expose()), None)
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["login"].as_str(),
                body["role"].as_str(),
                body["source"].as_str()
            ),
            (StatusCode::OK, Some("owner"), Some("admin"), Some("local")),
            "{}",
            reply.text()
        );
    }

    // TC: LOGN-05 (the REST layer asks the hook first)
    #[tokio::test]
    async fn hook_actor_when_hook_names_one_without_bearer() {
        let ctx = WorldCtx::new().await;
        ctx.world
            .ports
            .hooks
            .authenticate_next(AuthResult::Actor(ctx.world.owner.id));
        let reply = ctx.call("GET", "/api/v1/me", None, None).await;
        assert_eq!(
            (reply.status, reply.json()["login"].as_str()),
            (StatusCode::OK, Some("owner")),
            "{}",
            reply.text()
        );
    }
}

mod auth_token {
    use super::*;

    // TC: LOGN-01
    #[tokio::test]
    async fn lfs_session_of_owner_when_setup_token_live() {
        let ctx = WorldCtx::new().await;
        let setup = ctx.world.setup_token(&ctx.world.owner).await.unwrap();
        let reply = exchange(&ctx, setup.expose()).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["user"]["login"].as_str(),
                body["token"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("lfs_"))
            ),
            (StatusCode::OK, Some("owner"), true),
            "{}",
            reply.text()
        );
    }

    // TC: LOGN-01
    #[tokio::test]
    async fn granted_session_opens_me_when_exchanged() {
        let ctx = WorldCtx::new().await;
        let setup = ctx.world.setup_token(&ctx.world.owner).await.unwrap();
        let granted = exchange(&ctx, setup.expose()).await.json();
        let token = granted["token"].as_str().unwrap_or_default().to_owned();
        let reply = ctx.call("GET", "/api/v1/me", Some(&token), None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }

    // TC: LOGN-01 (the setup token after the exchange → 401)
    #[tokio::test]
    async fn unauthorized_when_setup_token_exchanged_twice() {
        let ctx = WorldCtx::new().await;
        let setup = ctx.world.setup_token(&ctx.world.owner).await.unwrap();
        let first = exchange(&ctx, setup.expose()).await;
        let second = exchange(&ctx, setup.expose()).await;
        assert_eq!(
            (first.status, second.status),
            (StatusCode::OK, StatusCode::UNAUTHORIZED),
            "{}",
            second.text()
        );
    }
    // TC: LOGN-01 (a token of the wrong shape is refused like an unknown one)
    #[tokio::test]
    async fn unauthorized_when_setup_token_malformed() {
        let ctx = WorldCtx::new().await;
        let _live = ctx.world.setup_token(&ctx.world.owner).await.unwrap();
        let reply = exchange(&ctx, "hunter2").await;
        assert_eq!(
            (reply.status, reply.text().contains("hunter2")),
            (StatusCode::UNAUTHORIZED, false),
            "{}",
            reply.text()
        );
    }
}

mod hook_request {
    use super::*;

    /// The headers the `authenticate` hook saw, over every call.
    fn hook_headers(ctx: &WorldCtx) -> Vec<(String, String)> {
        ctx.world
            .ports
            .hooks
            .authenticate_requests()
            .into_iter()
            .flat_map(|request| request.headers)
            .collect()
    }

    // TC: LOGN-05 (modules are untrusted: they never see liftoff
    // credentials)
    #[tokio::test]
    async fn hook_never_sees_session_or_cookie_when_session_used() {
        let ctx = WorldCtx::new().await;
        let secret = ctx.world.login(&ctx.world.owner).await.unwrap();
        let authorization = format!("Bearer {}", secret.expose());
        let reply = ctx
            .call_with(
                "GET",
                "/api/v1/me",
                &[
                    ("authorization", &authorization),
                    ("cookie", "lf_session=cookie-secret"),
                    ("proxy-authorization", "Basic proxy-secret"),
                    ("accept", "application/json"),
                ],
                None,
            )
            .await;
        let seen = hook_headers(&ctx);
        let leaked: Vec<&(String, String)> = seen
            .iter()
            .filter(|(name, value)| {
                ["authorization", "cookie", "proxy-authorization"].contains(&name.as_str())
                    || value.contains(secret.expose())
                    || value.contains("secret")
            })
            .collect();
        assert_eq!(
            (
                reply.status,
                leaked,
                seen.iter().any(|(name, _)| name == "accept")
            ),
            (StatusCode::OK, Vec::new(), true),
            "{}",
            reply.text()
        );
    }

    // TC: LOGN-05 (anything with a liftoff prefix stays hidden, valid or not)
    #[rstest]
    #[case::malformed_session("lfs_abc")]
    #[case::deploy_token("lf_0123456789abcdefghijABCDEFGHIJ0123456789")]
    #[case::setup_token("lft_0123456789abcdefghijABCDEFGHIJ0123456789")]
    #[tokio::test]
    async fn hook_never_sees_bearer_when_liftoff_prefixed(#[case] token: &str) {
        let ctx = WorldCtx::new().await;
        let _reply = ctx.call("GET", "/api/v1/me", Some(token), None).await;
        let seen = hook_headers(&ctx);
        assert!(
            !seen
                .iter()
                .any(|(name, value)| name == "authorization" || value.contains(token)),
            "{seen:?}"
        );
    }

    // TC: LOGN-05 (a foreign bearer is what an `oidc` module needs)
    #[tokio::test]
    async fn hook_sees_bearer_when_not_liftoff_token() {
        let ctx = WorldCtx::new().await;
        let _reply = ctx
            .call("GET", "/api/v1/me", Some("eyJhbGciOiJSUzI1NiJ9.oidc"), None)
            .await;
        assert!(
            hook_headers(&ctx).contains(&(
                "authorization".to_owned(),
                "Bearer eyJhbGciOiJSUzI1NiJ9.oidc".to_owned()
            )),
            "{:?}",
            hook_headers(&ctx)
        );
    }
}

mod auth_session {
    use super::*;

    // TC: LOGN-04
    #[tokio::test]
    async fn no_content_when_logged_out() {
        let ctx = WorldCtx::new().await;
        let secret = ctx.world.login(&ctx.world.owner).await.unwrap();
        let reply = ctx
            .call(
                "DELETE",
                "/api/v1/auth/session",
                Some(secret.expose()),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    }

    // TC: LOGN-04
    #[tokio::test]
    async fn unauthorized_on_me_when_logged_out() {
        let ctx = WorldCtx::new().await;
        let secret = ctx.world.login(&ctx.world.owner).await.unwrap();
        let _logout = ctx
            .call(
                "DELETE",
                "/api/v1/auth/session",
                Some(secret.expose()),
                None,
            )
            .await;
        let reply = ctx
            .call("GET", "/api/v1/me", Some(secret.expose()), None)
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    }
}

mod admin_token {
    use super::*;

    // TC: LOGN-02
    #[tokio::test]
    async fn created_lft_token_exchangeable_when_admin_session() {
        let ctx = WorldCtx::new().await;
        let secret = ctx.world.login(&ctx.world.owner).await.unwrap();
        let minted = ctx
            .call("POST", "/api/v1/admin/token", Some(secret.expose()), None)
            .await;
        let token = minted.json()["token"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let exchanged = exchange(&ctx, &token).await;
        assert_eq!(
            (
                minted.status,
                token.starts_with("lft_"),
                exchanged.status,
                exchanged.json()["user"]["login"].as_str()
            ),
            (StatusCode::CREATED, true, StatusCode::OK, Some("owner")),
            "{}",
            minted.text()
        );
    }

    // TC: LOGN-02
    #[tokio::test]
    async fn forbidden_when_session_not_admin() {
        let ctx = WorldCtx::new().await;
        let carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        let secret = ctx.world.login(&carol).await.unwrap();
        let reply = ctx
            .call("POST", "/api/v1/admin/token", Some(secret.expose()), None)
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    }
}

/// REST-01: the auth group (`/auth/token`, `/auth/session`, `/me`) answers
/// problem documents and rejects unknown fields.
mod auth_group {
    use super::*;

    // TC: REST-01
    #[tokio::test]
    async fn bad_request_problem_when_token_body_has_unknown_field() {
        let ctx = WorldCtx::new().await;
        let setup = ctx.world.setup_token(&ctx.world.owner).await.unwrap();
        let reply = ctx
            .call(
                "POST",
                "/api/v1/auth/token",
                None,
                Some(&json!({"token": setup.expose(), "machine": "laptop"})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-01
    #[tokio::test]
    async fn unauthorized_problem_with_bearer_challenge_when_me_without_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", "/api/v1/me", None, None).await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                reply
                    .headers
                    .get(http::header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
            ),
            (StatusCode::UNAUTHORIZED, true, Some("Bearer")),
            "{}",
            reply.text()
        );
    }
}
