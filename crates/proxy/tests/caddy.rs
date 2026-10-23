#![cfg(test)]
//! PRX-05 and the readiness probe: `CaddyAdmin` against an in-process
//! stand-in for Caddy's admin API (`POST /load`, `GET /config/`).

use core::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response};
use liftoff_core::domain::{Host, Label, ProjectName};
use liftoff_core::fakes::{WithProxy, World};
use liftoff_core::ports::{Proxy as _, Store as _};
use liftoff_core::proxy_config::ProxyConfig;
use liftoff_core::service::DomainService;
use liftoff_core::{Error, System};
use liftoff_proxy::{CaddyAdmin, render};

/// What Caddy 2 answers when a listener of the new configuration cannot
/// start (its admin API wraps the text in `{"error": ...}`).
const REFUSAL: &str = r#"{"error":"loading config: loading new config: http app module: start: listening on :443: listen tcp :443: bind: permission denied"}"#;
const REFUSAL_TEXT: &str = "loading config: loading new config: http app module: start: listening on :443: listen tcp :443: bind: permission denied";

/// A request the stand-in saw.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    content_type: Option<String>,
    body: String,
}

/// A stand-in admin API answering every request with one status and body.
struct FakeAdmin {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeAdmin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeAdmin {
    async fn answering(status: u16, body: &'static str) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().fallback({
            let seen = Arc::clone(&seen);
            move |request: Request<Body>| record(Arc::clone(&seen), request, status, body)
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { addr, seen, task }
    }

    fn caddy(&self) -> CaddyAdmin {
        CaddyAdmin::new(self.addr).unwrap()
    }

    /// The only request the stand-in saw.
    fn only(&self) -> Seen {
        let seen = self.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "expected exactly one request: {seen:?}");
        seen.into_iter().next().unwrap()
    }
}

async fn record(
    seen: Arc<Mutex<Vec<Seen>>>,
    request: Request<Body>,
    status: u16,
    body: &'static str,
) -> Response<Body> {
    let (parts, request_body) = request.into_parts();
    let bytes = axum::body::to_bytes(request_body, usize::MAX)
        .await
        .unwrap();
    seen.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        target: parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string),
        content_type: parts
            .headers
            .get("content-type")
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned()),
        body: String::from_utf8_lossy(&bytes).into_owned(),
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// The configuration of the untouched world.
async fn world_config(world: &World) -> ProxyConfig {
    ProxyConfig::from_store(&world.ports.store, &world.settings)
        .await
        .unwrap()
}

/// An address nothing listens on.
async fn closed_port() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

fn caddy_detail(result: Result<(), Error>) -> Option<String> {
    match result {
        Err(Error::Upstream {
            system: System::Caddy,
            detail,
        }) => Some(detail),
        _ => None,
    }
}

mod apply {
    use super::*;

    // TC: PRX-05 (the push itself: one POST /load)
    #[tokio::test]
    async fn posts_to_load_when_applied() {
        let world = World::new().await;
        let admin = FakeAdmin::answering(200, "").await;
        admin
            .caddy()
            .apply(world_config(&world).await)
            .await
            .unwrap();
        let seen = admin.only();
        assert_eq!(
            (seen.method.as_str(), seen.target.as_str()),
            ("POST", "/load")
        );
    }

    // TC: PRX-05 (the push itself: Caddy's JSON adapter)
    #[tokio::test]
    async fn sends_json_content_type_when_applied() {
        let world = World::new().await;
        let admin = FakeAdmin::answering(200, "").await;
        admin
            .caddy()
            .apply(world_config(&world).await)
            .await
            .unwrap();
        assert_eq!(
            admin.only().content_type.as_deref(),
            Some("application/json")
        );
    }

    // TC: PRX-05 (the push itself: the whole rendered configuration)
    #[tokio::test]
    async fn sends_rendered_config_when_applied() {
        let world = World::new().await;
        let config = world_config(&world).await;
        let admin = FakeAdmin::answering(200, "").await;
        admin.caddy().apply(config.clone()).await.unwrap();
        let sent: serde_json::Value = serde_json::from_str(&admin.only().body).unwrap();
        assert_eq!(sent, render(&config));
    }

    // TC: PRX-05
    #[tokio::test]
    async fn upstream_with_caddy_error_text_when_load_answers_400() {
        let world = World::new().await;
        let admin = FakeAdmin::answering(400, REFUSAL).await;
        let result = admin.caddy().apply(world_config(&world).await).await;
        assert_eq!(caddy_detail(result).as_deref(), Some(REFUSAL_TEXT));
    }

    // TC: PRX-05 (an answer that is not Caddy's JSON error object)
    #[tokio::test]
    async fn upstream_with_whole_body_when_400_body_is_not_json() {
        let world = World::new().await;
        let admin = FakeAdmin::answering(400, "bad request").await;
        let result = admin.caddy().apply(world_config(&world).await).await;
        assert_eq!(caddy_detail(result).as_deref(), Some("bad request"));
    }

    // TC: PRX-05 (no admin API at all: the actor gets no internal address,
    // the cause goes to the log)
    #[tokio::test]
    async fn upstream_when_admin_api_is_unreachable() {
        let world = World::new().await;
        let caddy = CaddyAdmin::new(closed_port().await).unwrap();
        let result = caddy.apply(world_config(&world).await).await;
        assert_eq!(
            caddy_detail(result).as_deref(),
            Some("admin api unreachable")
        );
    }
}

mod domain_add_through_caddy {
    use super::*;

    fn domains_over(world: &World, admin: &FakeAdmin) -> DomainService<WithProxy<CaddyAdmin>> {
        DomainService::new(
            Arc::new(WithProxy::new(Arc::clone(&world.ports), admin.caddy())),
            Arc::clone(&world.settings),
        )
    }

    // TC: PRX-05 (the error reaches the actor)
    #[tokio::test]
    async fn actor_gets_caddy_text_when_caddy_refuses_new_domain() {
        let world = World::new().await;
        let admin = FakeAdmin::answering(400, REFUSAL).await;
        let result = domains_over(&world, &admin)
            .add_managed(
                &world.owner,
                &ProjectName::new("shop").unwrap(),
                Label::new("docs").unwrap(),
                false,
            )
            .await;
        assert_eq!(
            caddy_detail(result.map(drop)).as_deref(),
            Some(REFUSAL_TEXT)
        );
    }

    // TC: PRX-05 (the store write is compensated)
    #[tokio::test]
    async fn domain_not_stored_when_caddy_refuses_new_domain() {
        let world = World::new().await;
        let admin = FakeAdmin::answering(400, REFUSAL).await;
        let refused = domains_over(&world, &admin)
            .add_managed(
                &world.owner,
                &ProjectName::new("shop").unwrap(),
                Label::new("docs").unwrap(),
                false,
            )
            .await;
        let stored = world
            .ports
            .store
            .domain_by_host(&Host::new("docs.apps.test").unwrap())
            .await
            .unwrap();
        assert_eq!((refused.is_err(), stored), (true, None));
    }
}

// Plan 2.5: `GET /config/` backs `readyz` (no catalogue ID of its own).
mod ready {
    use super::*;

    #[tokio::test]
    async fn gets_config_when_probed() {
        let admin = FakeAdmin::answering(200, "{}").await;
        admin.caddy().ready().await.unwrap();
        let seen = admin.only();
        assert_eq!(
            (seen.method.as_str(), seen.target.as_str()),
            ("GET", "/config/")
        );
    }

    #[tokio::test]
    async fn ok_when_config_answers_200() {
        let admin = FakeAdmin::answering(200, "{}").await;
        assert!(admin.caddy().ready().await.is_ok());
    }

    #[tokio::test]
    async fn upstream_when_config_answers_500() {
        let admin = FakeAdmin::answering(500, r#"{"error":"boom"}"#).await;
        assert!(caddy_detail(admin.caddy().ready().await).is_some());
    }

    #[tokio::test]
    async fn upstream_when_admin_api_is_unreachable() {
        let caddy = CaddyAdmin::new(closed_port().await).unwrap();
        assert_eq!(
            caddy_detail(caddy.ready().await).as_deref(),
            Some("admin api unreachable")
        );
    }
}
