//! The CORS layer of `/api/v1` for the panel `liftoff-web` (WEB-01, WEB-02; the `web` row) over the core's
//! `Hooks::web_panel`, which `FakeHooks::keep_panel` answers: the layer
//! reads the panel's origin through that port on every request, so a
//! change of its answer (the module installed, removed or reconfigured)
//! takes effect at once. End to end over the built module:
//! `crates/liftoffd/tests/web.rs`.

use http::StatusCode;
use liftoff_core::Error;
use liftoff_core::domain::Origin;
use liftoff_core::ports::WebPanel;

use crate::support::{Reply, WorldCtx};

/// The origin of the panel.
const PANEL: &str = "https://panel.test";

/// An origin that is not the panel's.
const FOREIGN: &str = "https://evil.test";

/// The world with the panel at [`PANEL`].
async fn with_panel() -> WorldCtx {
    let ctx = WorldCtx::new().await;
    ctx.world.ports.hooks.keep_panel(Ok(Some(WebPanel {
        origin: Origin::new(PANEL).unwrap(),
    })));
    ctx
}

/// Every value of `name` in `reply`.
fn values(reply: &Reply, name: &str) -> Vec<String> {
    reply
        .headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok().map(ToOwned::to_owned))
        .collect()
}

/// The comma-separated items of every `name` header, lower case, sorted.
fn items(reply: &Reply, name: &str) -> Vec<String> {
    let mut items: Vec<String> = values(reply, name)
        .iter()
        .flat_map(|value| value.split(','))
        .map(|item| item.trim().to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect();
    items.sort();
    items
}

/// `(Access-Control-Allow-Origin, Access-Control-Allow-Credentials)`.
fn cors(reply: &Reply) -> (Vec<String>, Vec<String>) {
    (
        values(reply, "access-control-allow-origin"),
        values(reply, "access-control-allow-credentials"),
    )
}

/// What [`cors`] is for the panel.
fn panel_allowed() -> (Vec<String>, Vec<String>) {
    (vec![PANEL.to_owned()], vec!["true".to_owned()])
}

/// What [`cors`] is without CORS.
const fn none() -> (Vec<String>, Vec<String>) {
    (Vec::new(), Vec::new())
}

/// `GET /api/v1/me` as `owner` from `origin`.
async fn me_from(ctx: &WorldCtx, origin: &str) -> Reply {
    let secret = ctx.world.login(&ctx.world.owner).await.unwrap();
    let bearer = format!("Bearer {}", secret.expose());
    ctx.call_with(
        "GET",
        "/api/v1/me",
        &[("origin", origin), ("authorization", &bearer)],
        None,
    )
    .await
}

/// The credentialed preflight of `POST /api/v1/projects` from `origin`.
async fn preflight_from(ctx: &WorldCtx, origin: &str) -> Reply {
    ctx.call_with(
        "OPTIONS",
        "/api/v1/projects",
        &[
            ("origin", origin),
            ("access-control-request-method", "POST"),
            (
                "access-control-request-headers",
                "authorization,content-type",
            ),
        ],
        None,
    )
    .await
}

mod simple_request {
    use super::*;

    // TC: WEB-01
    #[tokio::test]
    async fn allows_panel_origin_with_credentials_when_origin_is_panel() {
        let ctx = with_panel().await;
        let reply = me_from(&ctx, PANEL).await;
        assert_eq!(
            (reply.status, cors(&reply)),
            (StatusCode::OK, panel_allowed()),
            "{:?}",
            reply.headers
        );
    }

    // TC: WEB-01
    #[tokio::test]
    async fn varies_on_origin_when_origin_is_panel() {
        let ctx = with_panel().await;
        let reply = me_from(&ctx, PANEL).await;
        assert!(
            items(&reply, "vary").contains(&"origin".to_owned()),
            "{:?}",
            reply.headers
        );
    }

    // TC: WEB-01 (an answer that may differ by `Origin` varies on it even
    // when it carries no CORS)
    #[tokio::test]
    async fn varies_on_origin_when_origin_is_foreign() {
        let ctx = with_panel().await;
        let reply = me_from(&ctx, FOREIGN).await;
        assert_eq!(
            (
                cors(&reply),
                items(&reply, "vary").contains(&"origin".to_owned())
            ),
            (none(), true),
            "{:?}",
            reply.headers
        );
    }

    // TC: WEB-01 (the origin is compared whole: no prefix or suffix
    // match)
    #[tokio::test]
    async fn no_cors_headers_when_origin_extends_panel() {
        let ctx = with_panel().await;
        let reply = me_from(&ctx, "https://panel.test.evil.test").await;
        assert_eq!(cors(&reply), none(), "{:?}", reply.headers);
    }

    // TC: WEB-01 (`null` is no origin to trust)
    #[tokio::test]
    async fn no_cors_headers_when_origin_is_null() {
        let ctx = with_panel().await;
        let reply = me_from(&ctx, "null").await;
        assert_eq!(cors(&reply), none(), "{:?}", reply.headers);
    }

    // TC: WEB-01 (no panel: no module answers `web_panel`)
    #[tokio::test]
    async fn no_cors_headers_when_no_panel() {
        let ctx = WorldCtx::new().await;
        let reply = me_from(&ctx, PANEL).await;
        assert_eq!(cors(&reply), none(), "{:?}", reply.headers);
    }

    // TC: WEB-01 (fail closed: a module that cannot answer opens nothing,
    // and the request itself is still served)
    #[tokio::test]
    async fn no_cors_headers_when_port_fails() {
        let ctx = WorldCtx::new().await;
        ctx.world
            .ports
            .hooks
            .keep_panel(Err(Error::unavailable("module web failed to answer")));
        let reply = me_from(&ctx, PANEL).await;
        assert_eq!(
            (reply.status, cors(&reply)),
            (StatusCode::OK, none()),
            "{:?}",
            reply.headers
        );
    }

    // TC: WEB-01 (the port is read per request: a new answer takes effect
    // without rebuilding the router)
    #[tokio::test]
    async fn follows_port_when_panel_changes() {
        let ctx = with_panel().await;
        let _first = me_from(&ctx, PANEL).await;
        ctx.world.ports.hooks.keep_panel(Ok(None));
        let reply = me_from(&ctx, PANEL).await;
        assert_eq!(cors(&reply), none(), "{:?}", reply.headers);
    }

    // TC: WEB-01 (an error answer carries CORS too, so the panel can read
    // the problem document)
    #[tokio::test]
    async fn allows_panel_origin_when_request_is_unauthenticated() {
        let ctx = with_panel().await;
        let reply = ctx
            .call_with("GET", "/api/v1/me", &[("origin", PANEL)], None)
            .await;
        assert_eq!(
            (reply.status, cors(&reply)),
            (StatusCode::UNAUTHORIZED, panel_allowed()),
            "{:?}",
            reply.headers
        );
    }

    // TC: WEB-01 (`/api/v1` alone: the root pages carry no CORS)
    #[tokio::test]
    async fn no_cors_headers_on_root_page_when_origin_is_panel() {
        let ctx = with_panel().await;
        let reply = ctx
            .call_with("GET", "/login", &[("origin", PANEL)], None)
            .await;
        assert_eq!(cors(&reply), none(), "{:?}", reply.headers);
    }

    // TC: WEB-01 (never the viewer of artifacts)
    #[tokio::test]
    async fn no_cors_headers_on_artifact_viewer_when_origin_is_panel() {
        let ctx = with_panel().await;
        let reply = ctx
            .call_with("GET", "/a/AbCdEfGhIjKl", &[("origin", PANEL)], None)
            .await;
        assert_eq!(cors(&reply), none(), "{:?}", reply.headers);
    }
}

mod preflight {
    use super::*;

    // TC: WEB-02 (answered by the layer, without a session)
    #[tokio::test]
    async fn allows_panel_origin_with_credentials_when_preflight_from_panel() {
        let ctx = with_panel().await;
        let reply = preflight_from(&ctx, PANEL).await;
        assert_eq!(
            (reply.status.is_success(), cors(&reply)),
            (true, panel_allowed()),
            "{} {:?} {}",
            reply.status,
            reply.headers,
            reply.text()
        );
    }

    // TC: WEB-02
    #[tokio::test]
    async fn allows_requested_method_when_preflight_from_panel() {
        let ctx = with_panel().await;
        let reply = preflight_from(&ctx, PANEL).await;
        assert!(
            items(&reply, "access-control-allow-methods").contains(&"post".to_owned()),
            "{:?}",
            reply.headers
        );
    }

    // TC: WEB-02
    #[tokio::test]
    async fn allows_requested_headers_when_preflight_from_panel() {
        let ctx = with_panel().await;
        let allowed = items(
            &preflight_from(&ctx, PANEL).await,
            "access-control-allow-headers",
        );
        assert_eq!(
            (
                allowed.contains(&"authorization".to_owned()),
                allowed.contains(&"content-type".to_owned())
            ),
            (true, true),
            "{allowed:?}"
        );
    }

    // TC: WEB-02
    #[tokio::test]
    async fn varies_on_origin_when_preflight_from_panel() {
        let ctx = with_panel().await;
        let reply = preflight_from(&ctx, PANEL).await;
        assert!(
            items(&reply, "vary").contains(&"origin".to_owned()),
            "{:?}",
            reply.headers
        );
    }

    // TC: WEB-02 (another origin's preflight allows nothing)
    #[tokio::test]
    async fn no_cors_headers_when_preflight_from_foreign_origin() {
        let ctx = with_panel().await;
        let reply = preflight_from(&ctx, FOREIGN).await;
        assert_eq!(
            (
                cors(&reply),
                values(&reply, "access-control-allow-methods"),
                values(&reply, "access-control-allow-headers")
            ),
            (none(), Vec::<String>::new(), Vec::<String>::new()),
            "{:?}",
            reply.headers
        );
    }

    // TC: WEB-02 (no panel: a preflight allows nothing)
    #[tokio::test]
    async fn no_cors_headers_when_preflight_without_panel() {
        let ctx = WorldCtx::new().await;
        let reply = preflight_from(&ctx, PANEL).await;
        assert_eq!(cors(&reply), none(), "{:?}", reply.headers);
    }
}
