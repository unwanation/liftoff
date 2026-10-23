//! REST-06: domains (`/domains`, `/domains/{host}`).

use http::StatusCode;
use liftoff_core::domain::Host;
use liftoff_core::fakes::World;
use serde_json::json;

use crate::support::{WorldCtx, detail, is_problem, strings};

const DOMAINS: &str = "/api/v1/domains";

mod list {
    use super::*;

    // TC: REST-06
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", DOMAINS, None, None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06
    #[tokio::test]
    async fn shop_apps_test_managed_when_owner() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.owner("GET", DOMAINS, None).await;
        assert_eq!(
            (reply.status, reply.json()),
            (
                StatusCode::OK,
                json!([{
                    "host": "shop.apps.test",
                    "kind": "managed",
                    "project": "shop",
                    "verified_at": null,
                    "record_lost_at": null,
                    "proxied": false
                }])
            ),
            "{}",
            reply.text()
        );
    }
}

mod create {
    use super::*;

    // TC: REST-06 (the owner check of SEC-10: single-user mode, the admin)
    #[tokio::test]
    async fn forbidden_and_not_stored_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol(
                "POST",
                DOMAINS,
                Some(&json!({"project": "shop", "name": "blog"})),
            )
            .await;
        let hosts = strings(&ctx.owner("GET", DOMAINS, None).await.json(), "host");
        assert_eq!(
            (reply.status, is_problem(&reply), hosts),
            (
                StatusCode::FORBIDDEN,
                true,
                vec!["shop.apps.test".to_owned()]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06
    #[tokio::test]
    async fn managed_under_the_base_when_label_given() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                DOMAINS,
                Some(&json!({"project": "shop", "name": "blog"})),
            )
            .await;
        let body = reply.json();
        assert_eq!(
            (reply.status, body["host"].as_str(), body["kind"].as_str()),
            (StatusCode::CREATED, Some("blog.apps.test"), Some("managed")),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06 (DOM-04 over HTTP: an A record to the public IP)
    #[tokio::test]
    async fn external_and_verified_when_a_record_points_at_the_server() {
        let ctx = WorldCtx::new().await;
        ctx.world.ports.resolver.set_a(
            &Host::new("shop.example.org").unwrap(),
            vec![World::PUBLIC_IP],
        );
        let reply = ctx
            .owner(
                "POST",
                DOMAINS,
                Some(&json!({"project": "shop", "name": "shop.example.org", "external": true})),
            )
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["host"].as_str(),
                body["kind"].as_str(),
                body["verified_at"].is_string()
            ),
            (
                StatusCode::CREATED,
                Some("shop.example.org"),
                Some("external"),
                true
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06 (DOM-02 over HTTP)
    #[tokio::test]
    async fn conflict_when_host_is_taken() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                DOMAINS,
                Some(&json!({"project": "shop", "name": "shop"})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06 (`domains.reserved`)
    #[tokio::test]
    async fn bad_request_naming_the_label_when_reserved() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                DOMAINS,
                Some(&json!({"project": "shop", "name": "liftoff"})),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("liftoff")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }
}

mod create_proxied {
    use super::*;

    // TC: REST-06, DNS-01 (`add --proxied` needs a DNS module → 424)
    #[tokio::test]
    async fn module_required_and_not_stored_when_proxied_without_dns_module() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                DOMAINS,
                Some(&json!({"project": "shop", "name": "blog", "proxied": true})),
            )
            .await;
        let hosts = strings(&ctx.owner("GET", DOMAINS, None).await.json(), "host");
        assert_eq!(
            (
                reply.status.as_u16(),
                detail(&reply).contains("dns.cloudflare"),
                hosts
            ),
            (424, true, vec!["shop.apps.test".to_owned()]),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06, DNS-01 (only managed domains are proxied)
    #[tokio::test]
    async fn bad_request_naming_proxied_when_external() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                DOMAINS,
                Some(&json!({"project": "shop", "name": "shop.example.org", "external": true, "proxied": true})),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("proxied")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }
}

mod patch {
    use super::*;

    // TC: REST-06 (the owner check of SEC-10: single-user mode, the admin)
    #[tokio::test]
    async fn forbidden_when_actor_does_not_own_the_domain() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol(
                "PATCH",
                "/api/v1/domains/shop.apps.test",
                Some(&json!({"proxied": true})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06, REST-10 (`--proxied` needs a DNS module → 424)
    #[tokio::test]
    async fn module_required_naming_dns_cloudflare_when_proxied_set() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "PATCH",
                "/api/v1/domains/shop.apps.test",
                Some(&json!({"proxied": true})),
            )
            .await;
        assert_eq!(
            (
                reply.status.as_u16(),
                is_problem(&reply),
                detail(&reply).contains("dns.cloudflare")
            ),
            (424, true, true),
            "{}",
            reply.text()
        );
    }
}

mod delete {
    use super::*;

    // TC: REST-06 (the owner check of SEC-10: single-user mode, the admin)
    #[tokio::test]
    async fn forbidden_and_kept_when_actor_does_not_own_the_domain() {
        let ctx = WorldCtx::new().await;
        let _blog = ctx
            .world
            .add_domain(&ctx.world.shop, "blog.apps.test")
            .await
            .unwrap();
        let reply = ctx
            .carol("DELETE", "/api/v1/domains/blog.apps.test", None)
            .await;
        let hosts = strings(&ctx.owner("GET", DOMAINS, None).await.json(), "host");
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                hosts.contains(&"blog.apps.test".to_owned())
            ),
            (StatusCode::FORBIDDEN, true, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06 (`rm` only without a binding)
    #[tokio::test]
    async fn conflict_and_kept_when_domain_is_bound() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("DELETE", "/api/v1/domains/shop.apps.test", None)
            .await;
        let hosts = strings(&ctx.owner("GET", DOMAINS, None).await.json(), "host");
        assert_eq!(
            (reply.status, is_problem(&reply), hosts),
            (
                StatusCode::CONFLICT,
                true,
                vec!["shop.apps.test".to_owned()]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06
    #[tokio::test]
    async fn deleted_when_domain_is_unbound() {
        let ctx = WorldCtx::new().await;
        let _blog = ctx
            .world
            .add_domain(&ctx.world.shop, "blog.apps.test")
            .await
            .unwrap();
        let reply = ctx
            .owner("DELETE", "/api/v1/domains/blog.apps.test", None)
            .await;
        assert_eq!(
            (reply.status, reply.json()),
            (StatusCode::OK, json!({"deleted": "blog.apps.test"})),
            "{}",
            reply.text()
        );
    }

    // TC: REST-06
    #[tokio::test]
    async fn not_found_naming_it_when_host_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("DELETE", "/api/v1/domains/nope.apps.test", None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("nope.apps.test")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }
}
