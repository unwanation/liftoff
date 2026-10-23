//! Shared fixture: the application router, driven in memory.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use core::net::SocketAddr;

use axum::extract::ConnectInfo;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use liftoff_core::domain::{Login, TemplateSource, UserRole};
use liftoff_core::fakes::{WithHooks, World};
use liftoff_core::ports::{Hooks, Store as _};
use liftoff_core::service::Settings;
use liftoff_rest::{AppState, RestConfig, Services, app_router};
use serde_json::Value;
use tower::ServiceExt as _;

/// Everything a response says that the tests look at.
#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Vec<u8>,
}

impl Reply {
    /// The body as JSON; `Null` when it is empty or not JSON.
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The body as text.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The application router over a fresh [`World`], without a readiness
/// check, for requests without credentials.
pub(crate) struct Ctx {
    app: Router,
}

impl Ctx {
    pub(crate) async fn new() -> Self {
        Self {
            app: app(&World::new().await, AppState::new()),
        }
    }

    /// Sends `method path` without credentials or body.
    pub(crate) async fn send(&self, method: &str, path: &str) -> Reply {
        let request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path)
            .body(Body::empty())
            .unwrap();
        send_request(&self.app, request).await
    }
}

/// The whole API over the shared [`World`], driven in memory.
pub(crate) struct WorldCtx {
    pub(crate) world: World,
    app: Router,
}

impl WorldCtx {
    pub(crate) async fn new() -> Self {
        let world = World::new().await;
        let app = app(&world, AppState::new());
        Self { world, app }
    }

    /// The whole API over the fakes of a fresh [`World`] with the hooks
    /// `hooks` builds in place of its `FakeHooks` (the module dispatcher).
    pub(crate) async fn with_hooks<H: Hooks + 'static>(hooks: impl FnOnce(&World) -> H) -> Self {
        let world = World::new().await;
        let ports = Arc::new(WithHooks::new(Arc::clone(&world.ports), hooks(&world)));
        let services = Services::new(ports, Arc::clone(&world.settings));
        let app = app_router(AppState::new(), services, &RestConfig::default());
        Self { world, app }
    }

    /// The whole API over a fresh [`World`] whose installation lists
    /// `templates` as `[templates] repos`.
    pub(crate) async fn with_templates(templates: Vec<TemplateSource>) -> Self {
        let world = World::new().await;
        let settings = Arc::new(Settings {
            templates,
            ..(*world.settings).clone()
        });
        let services = Services::new(Arc::clone(&world.ports), settings);
        let app = app_router(AppState::new(), services, &RestConfig::default());
        Self { world, app }
    }

    /// The whole API over a fresh [`World`] whose installation's settings
    /// are its own as `change` leaves them (`[artifacts] host`).
    pub(crate) async fn with_settings(change: impl FnOnce(&mut Settings)) -> Self {
        let world = World::new().await;
        let mut settings = (*world.settings).clone();
        change(&mut settings);
        let services = Services::new(Arc::clone(&world.ports), Arc::new(settings));
        let app = app_router(AppState::new(), services, &RestConfig::default());
        Self { world, app }
    }

    /// Sends `method path` with an optional bearer and JSON body.
    pub(crate) async fn call(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Reply {
        let authorization = bearer.map(|token| format!("Bearer {token}"));
        let headers: Vec<(&str, &str)> = authorization
            .iter()
            .map(|value| (header::AUTHORIZATION.as_str(), value.as_str()))
            .collect();
        self.call_with(method, path, &headers, body).await
    }

    /// Sends `method path` with these raw headers and an optional JSON body.
    pub(crate) async fn call_with(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Reply {
        let mut all: Vec<(&str, &str)> = headers.to_vec();
        if body.is_some() {
            all.push((header::CONTENT_TYPE.as_str(), "application/json"));
        }
        let bytes = body.map(|json| serde_json::to_vec(json).unwrap());
        self.raw(None, method, path, &all, bytes.unwrap_or_default())
            .await
    }

    /// Sends `method path` as `owner`'s fresh `cli` session with an
    /// optional JSON body.
    pub(crate) async fn owner(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        let secret = self.world.login(&self.world.owner).await.unwrap();
        self.call(method, path, Some(secret.expose()), body).await
    }

    /// Sends `method path` as a fresh `cli` session of `carol`, a stored
    /// user who is not an admin and so owns nothing in single-user mode, with an optional JSON body.
    pub(crate) async fn carol(&self, method: &str, path: &str, body: Option<&Value>) -> Reply {
        let login = Login::new("carol").unwrap();
        let carol = match self.world.ports.store.user_by_login(&login).await.unwrap() {
            Some(carol) => carol,
            None => self.world.add_user("carol", UserRole::User).await.unwrap(),
        };
        let secret = self.world.login(&carol).await.unwrap();
        self.call(method, path, Some(secret.expose()), body).await
    }

    /// Sends `method path` with `body` and these raw headers.
    pub(crate) async fn with_body(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> Reply {
        let mut request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        send_request(&self.app, request.body(body).unwrap()).await
    }

    /// Sends `method path` with `bytes` as the body, these raw headers and,
    /// when `peer` is given, the peer address axum's `ConnectInfo` reports
    /// (what `serve` attaches to every accepted connection).
    pub(crate) async fn raw(
        &self,
        peer: Option<SocketAddr>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        bytes: Vec<u8>,
    ) -> Reply {
        let mut request = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut request = request.body(Body::from(bytes)).unwrap();
        if let Some(peer) = peer {
            let _previous: Option<ConnectInfo<SocketAddr>> =
                request.extensions_mut().insert(ConnectInfo(peer));
        }
        send_request(&self.app, request).await
    }
}

/// The whole API over the fakes of `world`, with `state`'s readiness check.
pub(crate) fn app(world: &World, state: AppState) -> Router {
    let services = Services::new(Arc::clone(&world.ports), Arc::clone(&world.settings));
    app_router(state, services, &RestConfig::default())
}

/// Drives `app` with `request` and collects the answer.
pub(crate) async fn send_request(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: body.to_vec(),
    }
}

/// A well-formed session token that was never stored.
pub(crate) const UNKNOWN_SESSION: &str = "lfs_0123456789abcdefghijABCDEFGHIJ0123456789";

/// Whether `reply` is an RFC 9457 problem document of its own status.
pub(crate) fn is_problem(reply: &Reply) -> bool {
    reply
        .headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value == "application/problem+json")
        && reply.json()["status"].as_u64() == Some(u64::from(reply.status.as_u16()))
}

/// The `field` of every element of the JSON array `body`, as strings.
pub(crate) fn strings(body: &Value, field: &str) -> Vec<String> {
    body.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item[field].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The `detail` of a problem document; empty when there is none.
pub(crate) fn detail(reply: &Reply) -> String {
    reply.json()["detail"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}
