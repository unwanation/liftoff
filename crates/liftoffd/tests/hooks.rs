#![cfg(test)]
//! End to end: the built `denier`
//! component (`modules/testing/denier`, a `tenants`-like module) loaded
//! by the module host, the core's `ModuleHooks` over it, and the REST
//! router over the core services: `denier` denies a foreign project
//! through `authorize`, and the client sees `403`; its routes answer
//! below `/api/v1/m/denier/` (HOOK-01, HOOK-03). It lives with the
//! composition root because it joins two adapters (`liftoff-wasm` and
//! `liftoff-rest`), which never depend on each other.
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); the test reads it from `WASM_MODULES_DIR`, which the just recipes
//! set, else from `modules/target/wasm32-wasip2/release/`.

use core::time::Duration;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use liftoff_core::domain::ModuleName;
use liftoff_core::fakes::{InMemoryStore, WithHooks, World};
use liftoff_core::module::ModuleHooks;
use liftoff_rest::{AppState, RestConfig, Services, app_router};
use liftoff_wasm::{Limits, ModuleConfig, ModuleHost, ModuleSet, TokioTimer, TracingLog};
use tower::ServiceExt as _;

/// The reason `denier` gives for its foreign project.
const FOREIGN: &str = "project belongs to another tenant";

fn component(name: &str) -> Vec<u8> {
    let dir = std::env::var_os("WASM_MODULES_DIR").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../modules/target/wasm32-wasip2/release")
        },
        PathBuf::from,
    );
    let path = dir.join(format!("{name}.wasm"));
    std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}; build the modules first with `just modules`",
            path.display()
        )
    })
}

/// The world, and the API over its fakes with `denier` (whose foreign
/// project is `shop`) behind the hooks.
async fn app() -> (World, Router) {
    let world = World::new().await;
    let limits = Limits {
        max_memory_bytes: 64 * 1024 * 1024,
        call_timeout: Duration::from_secs(10),
    };
    let host = ModuleHost::new(InMemoryStore::new(), TokioTimer, TracingLog, limits).unwrap();
    let mut config = ModuleConfig::new();
    let _previous = config.insert("foreign_project".to_owned(), world.shop.id.to_string());
    let set = ModuleSet::new();
    set.insert(
        host.load(
            ModuleName::new("denier").unwrap(),
            &component("denier"),
            config,
        )
        .await
        .unwrap(),
    );
    let hooks = ModuleHooks::new(Arc::clone(host.registry()), set);
    let ports = Arc::new(WithHooks::new(Arc::clone(&world.ports), hooks));
    let services = Services::new(ports, Arc::clone(&world.settings));
    let app = app_router(AppState::new(), services, &RestConfig::default());
    (world, app)
}

/// `GET path` with an optional bearer: the status and the body.
async fn get(app: &Router, path: &str, bearer: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::builder().method("GET").uri(path);
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

mod authorize {
    use super::*;

    // TC: HOOK-01 (the tenants-like module's deny is a
    // 403 in REST)
    #[tokio::test(flavor = "multi_thread")]
    async fn forbidden_when_denier_denies_foreign_project() {
        let (world, app) = app().await;
        let secret = world.login(&world.owner).await.unwrap();
        let (status, body) = get(&app, "/api/v1/projects/shop", Some(secret.expose())).await;
        assert_eq!(
            (status, body.contains(FOREIGN)),
            (StatusCode::FORBIDDEN, true),
            "{body}"
        );
    }
}

mod routes {
    use super::*;

    // TC: HOOK-03
    #[tokio::test(flavor = "multi_thread")]
    async fn unauthorized_when_session_route_has_no_session() {
        let (_world, app) = app().await;
        let (status, body) = get(&app, "/api/v1/m/denier/whoami", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }

    // TC: HOOK-03
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_owner_when_session_route_has_owner_session() {
        let (world, app) = app().await;
        let secret = world.login(&world.owner).await.unwrap();
        assert_eq!(
            get(&app, "/api/v1/m/denier/whoami", Some(secret.expose())).await,
            (StatusCode::OK, world.owner.id.to_string())
        );
    }

    // TC: HOOK-03
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_anonymous_when_public_route_has_no_session() {
        let (_world, app) = app().await;
        assert_eq!(
            get(&app, "/api/v1/m/denier/hello", None).await,
            (StatusCode::OK, "anonymous".to_owned())
        );
    }

    // TC: HOOK-06
    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_when_module_unknown() {
        let (world, app) = app().await;
        let secret = world.login(&world.owner).await.unwrap();
        let (status, body) = get(&app, "/api/v1/m/nosuch/x", Some(secret.expose())).await;
        assert_eq!(
            (status, body.contains("module nosuch not found")),
            (StatusCode::NOT_FOUND, true),
            "{body}"
        );
    }
}
