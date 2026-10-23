//! REST-10: what only the machine itself may ask (`GET
//! /internal/tls-ask?domain=`, Caddy's on-demand TLS permission) and the
//! Caddy half of `GET /readyz`.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use http::StatusCode;
use liftoff_core::System;
use liftoff_core::domain::DomainKind;
use liftoff_core::ports::Store as _;
use liftoff_core::{Error, fakes::World};

use crate::support::{Reply, WorldCtx, detail, is_problem};

const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51000);

/// Stores the external domain `host` of `shop`, verified or not.
async fn external(world: &World, host: &str, verified: bool) {
    let mut domain = world.records.domain(&world.shop, host).unwrap();
    domain.kind = DomainKind::External;
    domain.verified_at = verified.then_some(domain.created_at);
    world.ports.store.insert_domain(&domain).await.unwrap();
}

async fn ask(ctx: &WorldCtx, peer: Option<SocketAddr>, domain: &str) -> Reply {
    ctx.raw(
        peer,
        "GET",
        &format!("/internal/tls-ask?domain={domain}"),
        &[],
        Vec::new(),
    )
    .await
}

mod tls_ask {
    use super::*;

    // TC: REST-10 (a request the API site's Caddy proxied arrives from loopback too; Caddy's own permission call carries none of these headers)
    #[tokio::test]
    async fn forbidden_when_a_loopback_request_was_forwarded() {
        let ctx = WorldCtx::new().await;
        external(&ctx.world, "shop.example.org", true).await;
        let mut statuses = Vec::new();
        for header in ["x-forwarded-for", "forwarded", "via"] {
            let reply = ctx
                .raw(
                    Some(LOOPBACK),
                    "GET",
                    "/internal/tls-ask?domain=shop.example.org",
                    &[(header, "203.0.113.7")],
                    Vec::new(),
                )
                .await;
            statuses.push((header, reply.status, is_problem(&reply)));
        }
        assert_eq!(
            statuses,
            vec![
                ("x-forwarded-for", StatusCode::FORBIDDEN, true),
                ("forwarded", StatusCode::FORBIDDEN, true),
                ("via", StatusCode::FORBIDDEN, true),
            ]
        );
    }

    // TC: REST-10 (200 only for known verified hosts)
    #[tokio::test]
    async fn ok_without_session_when_loopback_asks_for_verified_external_host() {
        let ctx = WorldCtx::new().await;
        external(&ctx.world, "shop.example.org", true).await;
        let reply = ask(&ctx, Some(LOOPBACK), "shop.example.org").await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }

    // TC: REST-10
    #[tokio::test]
    async fn ok_when_ipv6_loopback_asks() {
        let ctx = WorldCtx::new().await;
        external(&ctx.world, "shop.example.org", true).await;
        let peer = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 51000);
        let reply = ask(&ctx, Some(peer), "shop.example.org").await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }

    // TC: REST-10 (answered like an unknown host: the reply does not tell
    // a stored but unverified domain from an unknown one)
    #[tokio::test]
    async fn not_found_like_an_unknown_host_when_external_host_is_unverified() {
        let ctx = WorldCtx::new().await;
        external(&ctx.world, "shop.example.org", false).await;
        let reply = ask(&ctx, Some(LOOPBACK), "shop.example.org").await;
        let unknown = ask(&ctx, Some(LOOPBACK), "evil.example.net").await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                detail(&reply).replace("shop.example.org", "<host>")
            ),
            (
                StatusCode::NOT_FOUND,
                true,
                detail(&unknown).replace("evil.example.net", "<host>")
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REST-10
    #[tokio::test]
    async fn not_found_when_host_unknown() {
        let ctx = WorldCtx::new().await;
        let reply = ask(&ctx, Some(LOOPBACK), "evil.example.net").await;
        assert_eq!(
            (reply.status, detail(&reply).contains("evil.example.net")),
            (StatusCode::NOT_FOUND, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-10 (an unknown query parameter is refused, as on every
    // other input of the API)
    #[tokio::test]
    async fn bad_request_when_query_has_an_unknown_parameter() {
        let ctx = WorldCtx::new().await;
        external(&ctx.world, "shop.example.org", true).await;
        let reply = ask(&ctx, Some(LOOPBACK), "shop.example.org&wildcard=1").await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-10 (`tls-ask` with a non-loopback peer → 403)
    #[tokio::test]
    async fn forbidden_when_peer_is_not_loopback() {
        let ctx = WorldCtx::new().await;
        external(&ctx.world, "shop.example.org", true).await;
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 51000);
        let reply = ask(&ctx, Some(peer), "shop.example.org").await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-10 (no peer address known: refuse rather than trust)
    #[tokio::test]
    async fn forbidden_when_peer_address_is_unknown() {
        let ctx = WorldCtx::new().await;
        external(&ctx.world, "shop.example.org", true).await;
        let reply = ask(&ctx, None, "shop.example.org").await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::FORBIDDEN, true),
            "{}",
            reply.text()
        );
    }

    // TC: REST-10 (only at the root, never under the public `/api/v1`)
    #[tokio::test]
    async fn not_served_under_the_api_prefix() {
        let ctx = WorldCtx::new().await;
        external(&ctx.world, "shop.example.org", true).await;
        let reply = ctx
            .raw(
                Some(LOOPBACK),
                "GET",
                "/api/v1/internal/tls-ask?domain=shop.example.org",
                &[],
                Vec::new(),
            )
            .await;
        assert_ne!(reply.status, StatusCode::OK, "{}", reply.text());
    }
}

/// `tls-ask` through a real listener: `serve_app_on` attaches the peer
/// address of every connection.
mod listener {
    use std::sync::Arc;

    use liftoff_rest::{AppState, RestConfig, Services, serve_app_on};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    // TC: REST-10 (the production path of the peer check)
    #[tokio::test]
    async fn ok_when_a_loopback_connection_asks_through_the_listener() {
        let world = World::new().await;
        external(&world, "shop.example.org", true).await;
        let listener = TcpListener::bind(LOOPBACK.ip().to_string() + ":0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let services = Services::new(Arc::clone(&world.ports), Arc::clone(&world.settings));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_app_on(
            listener,
            AppState::new(),
            services,
            RestConfig::default(),
            async {
                let _stopped = stopped.await;
            },
        ));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"GET /internal/tls-ask?domain=shop.example.org HTTP/1.1\r\n\
                  Host: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut answer = String::new();
        let _read = stream.read_to_string(&mut answer).await.unwrap();
        let _sent = stop.send(());
        server.await.unwrap().unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    }
}

mod readyz {
    use super::*;

    // TC: REST-10 (`readyz` fails when the Caddy admin API is down)
    #[tokio::test]
    async fn unavailable_without_the_cause_when_caddy_admin_unreachable() {
        let ctx = WorldCtx::new().await;
        ctx.world.ports.proxy.fail_always(
            "ready",
            Error::upstream(System::Caddy, "connection refused on 127.0.0.1:2019"),
        );
        let root = ctx.call("GET", "/readyz", None, None).await;
        let api = ctx.call("GET", "/api/v1/readyz", None, None).await;
        assert_eq!(
            (
                root.status,
                api.status,
                root.text().contains("2019") || root.text().contains("caddy")
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::SERVICE_UNAVAILABLE,
                false
            ),
            "{}",
            root.text()
        );
    }

    // TC: REST-10
    #[tokio::test]
    async fn ok_when_caddy_admin_answers() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", "/readyz", None, None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }
}
