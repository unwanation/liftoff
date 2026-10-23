//! REST-02: forge connections by token (`/git/connections`); the core knows
//! Forgejo only.

use http::StatusCode;
use liftoff_core::domain::{Host, Login};
use serde_json::json;

use crate::support::{WorldCtx, detail, is_problem, strings};

const CONNECTIONS: &str = "/api/v1/git/connections";

/// `owner`'s token on `code.example.org` that the fake forge accepts.
const TOKEN: &str = "forgejo-token-on-code";

fn code_host() -> Host {
    Host::new("code.example.org").unwrap()
}

mod list {
    use super::*;

    // TC: REST-02
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", CONNECTIONS, None, None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-02
    #[tokio::test]
    async fn owners_forgejo_connection_when_owner() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("GET", CONNECTIONS, None).await;
        assert_eq!(
            (reply.status, reply.json()),
            (
                StatusCode::OK,
                json!([{
                    "provider": "forgejo",
                    "host": "git.example.com",
                    "account_login": "owner",
                    "kind": "token",
                    "expires_at": null
                }])
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-02 (forge tokens never leave the daemon)
    #[tokio::test]
    async fn never_shows_the_credential_when_listed() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("GET", CONNECTIONS, None).await;
        assert!(
            reply.status == StatusCode::OK && !reply.text().contains("forgejo-token-of-owner"),
            "{} {}",
            reply.status,
            reply.text()
        );
    }
}

mod create {
    use super::*;

    // TC: REST-02
    #[tokio::test]
    async fn created_with_account_of_token_when_forge_accepts_it() {
        let ctx = WorldCtx::new().await;
        ctx.world.ports.git.accept_token(
            &code_host(),
            TOKEN,
            &Login::new("owner-on-code").unwrap(),
        );
        let reply = ctx
            .owner(
                "POST",
                CONNECTIONS,
                Some(&json!({"provider": "forgejo", "host": "code.example.org", "token": TOKEN})),
            )
            .await;
        assert_eq!(
            (reply.status, reply.json()),
            (
                StatusCode::CREATED,
                json!({
                    "provider": "forgejo",
                    "host": "code.example.org",
                    "account_login": "owner-on-code",
                    "kind": "token",
                    "expires_at": null
                })
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-02
    #[tokio::test]
    async fn listed_next_to_the_first_when_created() {
        let ctx = WorldCtx::new().await;
        ctx.world
            .ports
            .git
            .accept_token(&code_host(), TOKEN, &Login::new("owner").unwrap());
        let _created = ctx
            .owner(
                "POST",
                CONNECTIONS,
                Some(&json!({"provider": "forgejo", "host": "code.example.org", "token": TOKEN})),
            )
            .await;
        let mut hosts = strings(&ctx.owner("GET", CONNECTIONS, None).await.json(), "host");
        hosts.sort();
        assert_eq!(hosts, ["code.example.org", "git.example.com"]);
    }

    // TC: REST-02
    #[tokio::test]
    async fn forbidden_and_nothing_stored_when_forge_rejects_token() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                CONNECTIONS,
                Some(&json!({"provider": "forgejo", "host": "code.example.org", "token": "nope"})),
            )
            .await;
        let hosts = strings(&ctx.owner("GET", CONNECTIONS, None).await.json(), "host");
        assert_eq!(
            (reply.status, is_problem(&reply), hosts),
            (
                StatusCode::FORBIDDEN,
                true,
                vec!["git.example.com".to_owned()]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: GIT-02, REST-02 (GitHub is served by the `git.github` module through the `Git` port like Forgejo; without the module the port itself answers `ModuleRequired`, GITF-06)
    #[tokio::test]
    async fn created_when_provider_is_github_and_forge_accepts_token() {
        let ctx = WorldCtx::new().await;
        ctx.world.ports.git.accept_token(
            &Host::new("github.com").unwrap(),
            "ghp_token",
            &Login::new("alice").unwrap(),
        );
        let reply = ctx
            .owner(
                "POST",
                CONNECTIONS,
                Some(&json!({"provider": "github", "host": "github.com", "token": "ghp_token"})),
            )
            .await;
        assert_eq!(
            (reply.status, reply.json()),
            (
                StatusCode::CREATED,
                json!({
                    "provider": "github",
                    "host": "github.com",
                    "account_login": "alice",
                    "kind": "token",
                    "expires_at": null
                })
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-02
    #[tokio::test]
    async fn bad_request_when_body_has_unknown_field() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                CONNECTIONS,
                Some(&json!({
                    "provider": "forgejo",
                    "host": "code.example.org",
                    "token": TOKEN,
                    "sudo": "admin"
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
}

mod delete {
    use super::*;

    // TC: REST-02
    #[tokio::test]
    async fn deleted_and_gone_from_list_when_connected() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("DELETE", "/api/v1/git/connections/git.example.com", None)
            .await;
        let listed = ctx.owner("GET", CONNECTIONS, None).await.json();
        assert_eq!(
            (reply.status, reply.json(), listed),
            (
                StatusCode::OK,
                json!({"deleted": "git.example.com"}),
                json!([])
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-02
    #[tokio::test]
    async fn not_found_when_no_connection_on_host() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("DELETE", "/api/v1/git/connections/code.example.org", None)
            .await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                detail(&reply).contains("code.example.org")
            ),
            (StatusCode::NOT_FOUND, true, true),
            "{}",
            reply.text()
        );
    }
}
