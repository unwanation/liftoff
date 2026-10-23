//! REST-05: bindings (`POST /projects/{p}/environments/{e}/bindings`,
//! `DELETE /bindings/{host}`).

use http::StatusCode;
use serde_json::json;

use liftoff_core::ports::Store as _;

use crate::support::{WorldCtx, detail, is_problem};

const QA_BINDINGS: &str = "/api/v1/projects/shop/environments/qa/bindings";

/// The bindings of the function environment `api` of `shop`.
const API_BINDINGS: &str = "/api/v1/projects/shop/environments/api/bindings";

/// A world with the function environment `api` of `shop` and its free
/// domain `api.apps.test`.
async fn with_function() -> WorldCtx {
    let ctx = WorldCtx::new().await;
    let function = ctx.world.records.function(&ctx.world.shop, "api").unwrap();
    ctx.world
        .ports
        .store
        .insert_environment(&function)
        .await
        .unwrap();
    let _domain = ctx
        .world
        .add_domain(&ctx.world.shop, "api.apps.test")
        .await
        .unwrap();
    ctx
}

// a function environment takes a domain without
// `service:port` (`lof bind <host>`), as `lof create` binds its own.
mod bind_function {
    use super::*;

    // TC: REST-05 (a function environment)
    #[tokio::test]
    async fn created_as_function_binding_when_body_names_only_the_host() {
        let ctx = with_function().await;
        let reply = ctx
            .owner(
                "POST",
                API_BINDINGS,
                Some(&json!({"host": "api.apps.test"})),
            )
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["environment"].as_str(),
                body["service"].as_str(),
                body["container_port"].as_u64()
            ),
            (StatusCode::CREATED, Some("api"), Some("function"), Some(80)),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05 (a compose environment needs `service:port`)
    #[tokio::test]
    async fn bad_request_when_compose_environment_and_no_service() {
        let ctx = with_function().await;
        let reply = ctx
            .owner("POST", QA_BINDINGS, Some(&json!({"host": "api.apps.test"})))
            .await;
        let unbind = ctx
            .owner("DELETE", "/api/v1/bindings/api.apps.test", None)
            .await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).contains("service"),
                unbind.status
            ),
            (StatusCode::BAD_REQUEST, true, StatusCode::NOT_FOUND),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05 (a function has no `service:port`)
    #[tokio::test]
    async fn bad_request_when_function_environment_and_service_given() {
        let ctx = with_function().await;
        let reply = ctx
            .owner(
                "POST",
                API_BINDINGS,
                Some(&json!({"host": "api.apps.test", "service": "web", "port": 3000})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05 (validate at the boundary: a service goes with its port)
    #[tokio::test]
    async fn bad_request_when_service_without_port() {
        let ctx = with_function().await;
        let reply = ctx
            .owner(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "api.apps.test", "service": "web"})),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("port")),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }
}

mod bind {
    use super::*;

    // TC: REST-05, BND-01 (a domain of another project)
    #[tokio::test]
    async fn forbidden_and_unbound_when_domain_belongs_to_another_project() {
        let ctx = WorldCtx::new().await;
        let blog = ctx.world.add_project("blog", "owner/blog").await.unwrap();
        let _domain = ctx.world.add_domain(&blog, "blog.apps.test").await.unwrap();
        let reply = ctx
            .owner(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "blog.apps.test", "service": "web", "port": 3000})),
            )
            .await;
        let unbind = ctx
            .owner("DELETE", "/api/v1/bindings/blog.apps.test", None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), unbind.status),
            (StatusCode::FORBIDDEN, true, StatusCode::NOT_FOUND),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05 (the owner check of SEC-10: single-user mode, the admin)
    #[tokio::test]
    async fn forbidden_and_unbound_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        let _api = ctx
            .world
            .add_domain(&ctx.world.shop, "api.apps.test")
            .await
            .unwrap();
        let reply = ctx
            .carol(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "api.apps.test", "service": "api", "port": 8080})),
            )
            .await;
        let unbind = ctx
            .owner("DELETE", "/api/v1/bindings/api.apps.test", None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), unbind.status),
            (StatusCode::FORBIDDEN, true, StatusCode::NOT_FOUND),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05
    #[tokio::test]
    async fn created_on_qa_when_domain_is_free() {
        let ctx = WorldCtx::new().await;
        let _api = ctx
            .world
            .add_domain(&ctx.world.shop, "api.apps.test")
            .await
            .unwrap();
        let reply = ctx
            .owner(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "api.apps.test", "service": "api", "port": 8080})),
            )
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["host"].as_str(),
                body["project"].as_str(),
                body["environment"].as_str(),
                body["service"].as_str(),
                body["container_port"].as_u64(),
                body["host_port"].as_u64().is_some()
            ),
            (
                StatusCode::CREATED,
                Some("api.apps.test"),
                Some("shop"),
                Some("qa"),
                Some("api"),
                Some(8080),
                true
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05 (a domain routes to one place)
    #[tokio::test]
    async fn conflict_when_domain_is_bound() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "shop.apps.test", "service": "web", "port": 3000})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05
    #[tokio::test]
    async fn not_found_naming_it_when_domain_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "nope.apps.test", "service": "web", "port": 3000})),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("nope.apps.test")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05 (validate at the boundary: container ports are 1-65535)
    #[tokio::test]
    async fn bad_request_when_port_is_zero() {
        let ctx = WorldCtx::new().await;
        let _api = ctx
            .world
            .add_domain(&ctx.world.shop, "api.apps.test")
            .await
            .unwrap();
        let reply = ctx
            .owner(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "api.apps.test", "service": "api", "port": 0})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call(
                "POST",
                QA_BINDINGS,
                None,
                Some(&json!({"host": "shop.apps.test", "service": "web", "port": 3000})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }
}

mod unbind {
    use super::*;

    // TC: REST-05 (the owner check of SEC-10: single-user mode, the admin)
    #[tokio::test]
    async fn forbidden_and_kept_when_actor_does_not_own_the_project() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .carol("DELETE", "/api/v1/bindings/shop.apps.test", None)
            .await;
        let rebound = ctx
            .owner(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "shop.apps.test", "service": "web", "port": 3000})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), rebound.status),
            (StatusCode::FORBIDDEN, true, StatusCode::CONFLICT),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05
    #[tokio::test]
    async fn deleted_and_domain_free_again_when_bound() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .owner("DELETE", "/api/v1/bindings/shop.apps.test", None)
            .await;
        let rebound = ctx
            .owner(
                "POST",
                QA_BINDINGS,
                Some(&json!({"host": "shop.apps.test", "service": "web", "port": 3000})),
            )
            .await;
        assert_eq!(
            (reply.status, reply.json(), rebound.status),
            (
                StatusCode::OK,
                json!({"deleted": "shop.apps.test"}),
                StatusCode::CREATED
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-05
    #[tokio::test]
    async fn not_found_when_domain_is_unbound() {
        let ctx = WorldCtx::new().await;
        let _api = ctx
            .world
            .add_domain(&ctx.world.shop, "api.apps.test")
            .await
            .unwrap();
        let reply = ctx
            .owner("DELETE", "/api/v1/bindings/api.apps.test", None)
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("api.apps.test")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }
}
