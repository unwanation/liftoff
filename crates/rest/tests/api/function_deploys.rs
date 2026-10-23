//! `POST /deploys` of a function (`FDEP-01…03`): `multipart/form-data` with the binary field
//! `component`, from a session (`project`, `environment`) or from CI
//! with the deploy token (`repo`, `ref`), as the CI snippet sends it
//! (`curl -F "component=@<file>"`).
//!
//! The world's function runtime is the fake one: a component is any
//! bytes that start with [`COMPONENT_PREAMBLE`].

use http::StatusCode;
use liftoff_core::domain::{ComponentHash, DeployId, Environment};
use liftoff_core::fakes::{COMPONENT_PREAMBLE, FakeFunctions, Records, World};
use liftoff_core::ports::Store as _;

use crate::support::{Reply, WorldCtx, detail, is_problem, strings};

const DEPLOYS: &str = "/api/v1/deploys";
const BOUNDARY: &str = "XFUNCTIONBOUNDARYX";

/// 50 MiB, the limit of the field `component`.
const LIMIT: usize = 50 * 1024 * 1024;

/// A `multipart/form-data` body: the text fields `fields`, then the file
/// field `component` holding `component`.
fn form(fields: &[(&str, &str)], component: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"component\"; \
             filename=\"function.wasm\"\r\nContent-Type: application/wasm\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(component);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// A component the fake runtime accepts, `size` bytes long.
fn component_of_size(size: usize) -> Vec<u8> {
    let mut bytes = COMPONENT_PREAMBLE.to_vec();
    bytes.resize(size, b'x');
    bytes
}

/// The function environment `api` of `shop`, pinned to `main` by `owner`.
async fn function_on_main(world: &World) -> Environment {
    let mut function = world.records.function(&world.shop, "api").unwrap();
    function.pin = Some(Records::pin("main", &world.owner).unwrap());
    world
        .ports
        .store
        .insert_environment(&function)
        .await
        .unwrap();
    function
}

/// `POST /deploys` with `body` as a multipart form and `bearer`.
async fn post_form(ctx: &WorldCtx, bearer: &str, body: Vec<u8>) -> Reply {
    let authorization = format!("Bearer {bearer}");
    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
    ctx.raw(
        None,
        "POST",
        DEPLOYS,
        &[
            ("authorization", &authorization),
            ("content-type", &content_type),
        ],
        body,
    )
    .await
}

/// `owner`'s `lof deploy` of `component` into `shop/api`.
async fn upload(ctx: &WorldCtx, component: &[u8]) -> Reply {
    let session = ctx.world.login(&ctx.world.owner).await.unwrap();
    post_form(
        ctx,
        session.expose(),
        form(&[("project", "shop"), ("environment", "api")], component),
    )
    .await
}

async fn deploy_count(ctx: &WorldCtx, environment: &Environment) -> usize {
    ctx.world
        .ports
        .store
        .deploys_of_environment(environment.id)
        .await
        .unwrap()
        .len()
}

mod limit {
    use super::*;

    // TC: FDEP-01
    #[tokio::test]
    async fn payload_too_large_and_nothing_kept_when_component_is_above_50_mib() {
        let ctx = WorldCtx::new().await;
        let api = function_on_main(&ctx.world).await;
        let reply = upload(&ctx, &component_of_size(LIMIT + 1)).await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                ctx.world.ports.components.hashes(),
                deploy_count(&ctx, &api).await
            ),
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                true,
                Vec::<ComponentHash>::new(),
                0
            ),
            "{}",
            reply.text()
        );
    }

    // TC: FDEP-01 (50 MiB itself is within the limit)
    #[tokio::test]
    async fn accepted_when_component_is_exactly_50_mib() {
        let ctx = WorldCtx::new().await;
        let _api = function_on_main(&ctx.world).await;
        let reply = upload(&ctx, &component_of_size(LIMIT)).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text());
    }
}

mod not_a_component {
    use super::*;

    // TC: FDEP-02
    #[tokio::test]
    async fn bad_request_saying_not_a_wasi_http_component_when_body_is_not_a_component() {
        let ctx = WorldCtx::new().await;
        let _api = function_on_main(&ctx.world).await;
        let reply = upload(&ctx, b"#!/bin/sh\necho not wasm\n").await;
        assert!(
            reply.status == StatusCode::BAD_REQUEST
                && is_problem(&reply)
                && detail(&reply).contains("not a wasi:http component"),
            "{} {}",
            reply.status,
            reply.text()
        );
    }

    // TC: FDEP-02 (refused before anything is written: only the earlier
    // upload's file and deploy exist)
    #[tokio::test]
    async fn keeps_only_earlier_component_when_body_is_not_a_component() {
        let ctx = WorldCtx::new().await;
        let api = function_on_main(&ctx.world).await;
        let earlier = FakeFunctions::component("v1");
        let accepted = upload(&ctx, &earlier).await;
        let _refused = upload(&ctx, b"#!/bin/sh\necho not wasm\n").await;
        assert_eq!(
            (
                accepted.status,
                ctx.world.ports.components.hashes(),
                deploy_count(&ctx, &api).await
            ),
            (StatusCode::ACCEPTED, vec![ComponentHash::of(&earlier)], 1),
            "{}",
            accepted.text()
        );
    }
}

mod session {
    use super::*;

    // TC: FDEP-03 (`lof deploy` of a function: 202 with the queued deploy)
    #[tokio::test]
    async fn accepted_with_api_deploy_one_when_owner_uploads_component() {
        let ctx = WorldCtx::new().await;
        let _api = function_on_main(&ctx.world).await;
        let reply = upload(&ctx, &FakeFunctions::component("v1")).await;
        let queued = &reply.json()["deploys"];
        assert_eq!(
            (
                reply.status,
                queued.as_array().map(Vec::len),
                queued[0]["project"].as_str(),
                queued[0]["environment"].as_str(),
                queued[0]["number"].as_u64()
            ),
            (
                StatusCode::ACCEPTED,
                Some(1),
                Some("shop"),
                Some("api"),
                Some(1)
            ),
            "{}",
            reply.text()
        );
    }

    // TC: FDEP-03 (the queued deploy runs to `succeeded`)
    #[tokio::test]
    async fn deploy_succeeded_when_queued_upload_runs() {
        let ctx = WorldCtx::new().await;
        let _api = function_on_main(&ctx.world).await;
        let reply = upload(&ctx, &FakeFunctions::component("v1")).await;
        let body = reply.json();
        let id = body["deploys"][0]["deploy_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no deploy queued: {} {}", reply.status, reply.text()))
            .to_owned();
        let _done = ctx
            .world
            .deploys()
            .run(DeployId::parse(&id).unwrap())
            .await
            .unwrap();
        let shown = ctx.owner("GET", &format!("{DEPLOYS}/{id}"), None).await;
        assert_eq!(
            shown.json()["status"].as_str(),
            Some("succeeded"),
            "{}",
            shown.text()
        );
    }
}

mod ci {
    use super::*;

    // TC: FDEP-03 (from CI the same with the deploy token; the fields of the function snippet: repo, ref, component)
    #[tokio::test]
    async fn accepted_with_function_environment_of_ref_when_ci_posts_component() {
        let ctx = WorldCtx::new().await;
        let _api = function_on_main(&ctx.world).await;
        let token = ctx.world.deploy_token(&ctx.world.shop).await.unwrap();
        let reply = post_form(
            &ctx,
            token.expose(),
            form(
                &[("repo", "owner/shop"), ("ref", "main")],
                &FakeFunctions::component("v1"),
            ),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                strings(&reply.json()["deploys"], "environment")
            ),
            (StatusCode::ACCEPTED, vec!["api".to_owned()]),
            "{}",
            reply.text()
        );
    }
}

mod text_limit {
    use super::*;

    use serde_json::json;

    /// One byte over `api.max_body_bytes` (1 MiB by default).
    const OVER: usize = 1024 * 1024 + 1;

    // TC: FDEP-01 (JSON keeps the limit of every other body)
    #[tokio::test]
    async fn payload_too_large_when_json_deploy_is_above_max_body_bytes() {
        let ctx = WorldCtx::new().await;
        let compose = "x".repeat(OVER);
        let reply = ctx
            .owner(
                "POST",
                DEPLOYS,
                Some(&json!({
                    "project": "shop",
                    "environment": "production",
                    "tag": "sha-1",
                    "compose": compose
                })),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::PAYLOAD_TOO_LARGE, true),
            "{}",
            reply.text()
        );
    }

    // TC: FDEP-01 (the text fields of a form keep it too)
    #[tokio::test]
    async fn payload_too_large_and_nothing_kept_when_form_text_is_above_max_body_bytes() {
        let ctx = WorldCtx::new().await;
        let api = function_on_main(&ctx.world).await;
        let session = ctx.world.login(&ctx.world.owner).await.unwrap();
        let padding = "x".repeat(OVER);
        let reply = post_form(
            &ctx,
            session.expose(),
            form(
                &[
                    ("project", "shop"),
                    ("environment", "api"),
                    ("compose", &padding),
                ],
                &FakeFunctions::component("v1"),
            ),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                ctx.world.ports.components.hashes(),
                deploy_count(&ctx, &api).await
            ),
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                Vec::<ComponentHash>::new(),
                0
            ),
            "{}",
            reply.text()
        );
    }

    // TC: FDEP-01 (only `POST /deploys` takes more than `max_body_bytes`)
    #[tokio::test]
    async fn payload_too_large_when_other_api_body_is_above_max_body_bytes() {
        let ctx = WorldCtx::new().await;
        let description = "x".repeat(OVER);
        let reply = ctx
            .owner(
                "PATCH",
                "/api/v1/projects/shop/environments/qa",
                Some(&json!({ "description": description })),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{}",
            reply.text()
        );
    }

    // TC: FDEP-01 (the routes outside `/api/v1` keep it too)
    #[tokio::test]
    async fn payload_too_large_when_root_route_body_is_above_max_body_bytes() {
        let ctx = WorldCtx::new().await;
        let length = OVER.to_string();
        let reply = ctx
            .raw(
                None,
                "GET",
                "/gate/start",
                &[("content-length", &length)],
                vec![b'x'; OVER],
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{}",
            reply.text()
        );
    }
}

mod refused_fields {
    use super::*;

    // TC: FDEP-03 (a component replaces tag and compose)
    #[tokio::test]
    async fn bad_request_on_component_when_ci_sends_component_with_tag() {
        let ctx = WorldCtx::new().await;
        let api = function_on_main(&ctx.world).await;
        let token = ctx.world.deploy_token(&ctx.world.shop).await.unwrap();
        let reply = post_form(
            &ctx,
            token.expose(),
            form(
                &[("repo", "owner/shop"), ("ref", "main"), ("tag", "sha-1")],
                &FakeFunctions::component("v1"),
            ),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).starts_with("component"),
                deploy_count(&ctx, &api).await
            ),
            (StatusCode::BAD_REQUEST, true, 0),
            "{}",
            reply.text()
        );
    }

    // TC: FDEP-03 (a session's component goes without compose)
    #[tokio::test]
    async fn bad_request_and_nothing_kept_when_session_sends_component_with_compose() {
        let ctx = WorldCtx::new().await;
        let api = function_on_main(&ctx.world).await;
        let session = ctx.world.login(&ctx.world.owner).await.unwrap();
        let reply = post_form(
            &ctx,
            session.expose(),
            form(
                &[
                    ("project", "shop"),
                    ("environment", "api"),
                    ("compose", "services: {}"),
                ],
                &FakeFunctions::component("v1"),
            ),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).starts_with("component"),
                ctx.world.ports.components.hashes(),
                deploy_count(&ctx, &api).await
            ),
            (
                StatusCode::BAD_REQUEST,
                true,
                Vec::<ComponentHash>::new(),
                0
            ),
            "{}",
            reply.text()
        );
    }

    // TC: FDEP-03 (a deploy token deploys by repo and ref)
    #[tokio::test]
    async fn bad_request_on_project_when_token_sends_component_with_project() {
        let ctx = WorldCtx::new().await;
        let api = function_on_main(&ctx.world).await;
        let token = ctx.world.deploy_token(&ctx.world.shop).await.unwrap();
        let reply = post_form(
            &ctx,
            token.expose(),
            form(
                &[("project", "shop"), ("repo", "owner/shop"), ("ref", "main")],
                &FakeFunctions::component("v1"),
            ),
        )
        .await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).starts_with("project"),
                deploy_count(&ctx, &api).await
            ),
            (StatusCode::BAD_REQUEST, true, 0),
            "{}",
            reply.text()
        );
    }
}

mod unknown_token {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::body::{Body, Bytes};

    /// A deploy token of the right shape that was never issued.
    const FORGED: &str = "lf_0123456789abcdefghijABCDEFGHIJ0123456789";

    // TC: FDEP-01 (an upload of up to 50 MiB is read only for a live
    // token: a forged one is refused before the body is touched)
    #[tokio::test]
    async fn unauthorized_without_reading_body_when_deploy_token_is_not_live() {
        let ctx = WorldCtx::new().await;
        let _api = function_on_main(&ctx.world).await;
        let read = Arc::new(AtomicBool::new(false));
        let body = form(
            &[("repo", "owner/shop"), ("ref", "main")],
            &FakeFunctions::component("v1"),
        );
        let seen = Arc::clone(&read);
        let stream = futures_util::stream::once(async move {
            seen.store(true, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from(body))
        });
        let authorization = format!("Bearer {FORGED}");
        let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
        let reply = ctx
            .with_body(
                "POST",
                DEPLOYS,
                &[
                    ("authorization", &authorization),
                    ("content-type", &content_type),
                ],
                Body::from_stream(stream),
            )
            .await;
        assert_eq!(
            (reply.status, read.load(Ordering::SeqCst)),
            (StatusCode::UNAUTHORIZED, false),
            "{}",
            reply.text()
        );
    }
}
