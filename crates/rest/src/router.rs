use axum::Router;
use axum::extract::{DefaultBodyLimit, Request};
use axum::middleware::from_fn_with_state;
use axum::routing::{any, delete, get, patch, post};
use liftoff_core::domain::Host;
use liftoff_core::ports::Ports;
use tower::ServiceExt as _;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use crate::config::RestConfig;
use crate::cors;
use crate::extract::{ArtifactLimits, DeployLimits};
use crate::routes::{
    admin, artifacts, artifacts_view, auth, bindings, deploys, domains, environments, gate, git,
    internal, login, mcp, modules, projects, registry, secrets, tunnel,
};
use crate::services::Services;
use crate::state::AppState;
use crate::{middleware, openapi, probes};

/// Builds the whole application over `services` with the production
/// middleware stack.
///
/// The routes live under `/api/v1`, next to the
/// probes and `/openapi.json`; those two are also served at the root,
/// with `GET /internal/tls-ask`.
///
/// `GET /gate/start` (the gate's login) is served
/// at the root of the API host, next to the probes; it reads the browser
/// session's cookie itself, and so is `GET /login`, the browser's login
/// page. `POST /auth/token` and the page a
/// one-time link opens, `GET /auth/links/{token}`, are open; `/m/{module}/...` (the routes of modules)
/// authenticates in the core, since a module's `public` route needs no
/// session; `POST /deploys` takes a deploy token or a
/// session; every other `/api/v1` route except the probes and the
/// `OpenAPI` document asks
/// [`AuthService::authenticate`](liftoff_core::service::AuthService::authenticate)
/// first (the `authenticate` hook, then the bearer session) and answers
/// `401` without a principal. `/readyz` asks the installed
/// [`Readiness`](crate::Readiness) check and then the proxy's admin API.
///
/// Routes that wait on docker, systemd, Caddy or the forge get
/// `long_request_timeout`, and so does `POST /deploys`, whose function
/// component may take a while to upload; `.../logs` streams without a
/// timeout; the rest get `request_timeout`. Every body is at most
/// `max_body_bytes` except the component of `POST /deploys`, at most
/// 50 MiB on top.
///
/// The artifact routes take a version of up to `[artifacts]
/// max_size_mb`: their bodies have a limit of
/// their own on that, and `long_request_timeout`.
///
/// The viewer of artifacts is served at
/// `/a/{id}…` of the root; when `[artifacts] host` is set, requests to
/// that host go to the viewer alone at `/{id}…`, with the gate's
/// `/.liftoff/callback` that hands the API host's session over to it
/// (everything else there is `404`), and the API host has no `/a/`
/// (VIEW-08).
///
/// The returned router has no state left to inject and can be served
/// directly or driven in tests with [`tower::ServiceExt::oneshot`]; the
/// loopback check of `tls-ask` needs the peer address
/// ([`axum::extract::ConnectInfo`]), which [`serve_app`](crate::serve_app)
/// attaches.
pub fn app_router<P: Ports>(state: AppState, services: Services<P>, config: &RestConfig) -> Router {
    let streams = Router::new()
        .route(
            "/projects/{p}/environments/{e}/logs",
            get(environments::logs::<P>),
        )
        .route_layer(from_fn_with_state(
            services.clone(),
            auth::authenticate::<P>,
        ));
    // A function's component is up to 50 MiB: the one route
    // whose body may be larger than `max_body_bytes`, and whose upload
    // may take longer than `request_timeout`.
    let limits = DeployLimits {
        text: config.max_body_bytes,
    };
    let deploy = Router::new()
        .route("/deploys", post(deploys::create::<P>))
        .route_layer(from_fn_with_state(
            services.clone(),
            auth::deploy_caller::<P>,
        ))
        .layer(axum::Extension(limits))
        .layer(DefaultBodyLimit::max(limits.body()))
        .layer(RequestBodyLimitLayer::new(limits.body()))
        .layer(long_timeout(config));
    // `GET /auth/links/{token}` is the page a one-time link opens in a
    // browser: the link is the credential.
    let open = Router::new()
        .route("/auth/token", post(auth::exchange::<P>))
        .route("/auth/links/{token}", get(login::open_link::<P>))
        .layer(short_timeout(config));
    // The tunnel of `lof dev`: the route checks
    // its `cli` session itself (a deploy token or another session is
    // `403`, final for the client); the connection outlives the request.
    let tunnel = Router::new()
        .route("/tunnel", get(tunnel::upgrade::<P>))
        .layer(short_timeout(config));
    // Module routes authenticate in the core: a `public` route needs no
    // session.
    let module_routes = Router::new()
        .route("/m/{module}", any(modules::handle::<P>))
        .route("/m/{module}/{*path}", any(modules::handle::<P>))
        .layer(short_timeout(config));
    let api = Router::new()
        .merge(short_routes(&services, config))
        .merge(long_routes(&services, config))
        .merge(streams)
        .merge(open)
        .merge(tunnel)
        .merge(module_routes)
        .merge(probe_routes::<P>().layer(short_timeout(config)))
        .layer(RequestBodyLimitLayer::new(config.max_body_bytes))
        .merge(deploy)
        .merge(artifact_routes(&services, config))
        .with_state(services.clone())
        // CORS for the panel `liftoff-web`:
        // `/api/v1` alone.
        .layer(from_fn_with_state(services.clone(), cors::cors::<P>));
    let root = probe_routes::<P>()
        .route("/internal/tls-ask", get(internal::tls_ask::<P>))
        .route("/gate/start", get(gate::start::<P>))
        .route("/login", get(login::page::<P>))
        .route(
            "/.well-known/oauth-protected-resource",
            get(mcp::protected_resource::<P>),
        );
    let artifacts_host = services.settings.artifacts.host.clone();
    let root = match &artifacts_host {
        None => root.merge(viewer_routes::<P>("/a")),
        Some(_) => root,
    }
    .layer(short_timeout(config))
    .layer(RequestBodyLimitLayer::new(config.max_body_bytes))
    .with_state(services.clone());
    let app = Router::new()
        .nest("/api/v1", api)
        .merge(root)
        .fallback(probes::not_found)
        .layer(axum::Extension(state.ready))
        .layer(axum::Extension(state.tunnels));
    match artifacts_host {
        None => middleware::apply(app),
        Some(host) => {
            let viewer = viewer_routes::<P>("")
                .route("/.liftoff/callback", get(artifacts_view::callback::<P>))
                .layer(short_timeout(config))
                .layer(RequestBodyLimitLayer::new(config.max_body_bytes))
                .with_state(services)
                .fallback(probes::not_found);
            // The stack goes on each router, not around `by_host`: the
            // request's span names the route that matched.
            by_host(host, middleware::apply(viewer), middleware::apply(app))
        }
    }
}

/// The artifact routes behind
/// a session. A version is up to `[artifacts] max_size_mb`, so their
/// bodies get [`ArtifactLimits`] instead of `max_body_bytes`, and
/// `long_request_timeout` for the upload of that much.
fn artifact_routes<P: Ports>(services: &Services<P>, config: &RestConfig) -> Router<Services<P>> {
    let limits = ArtifactLimits {
        version: usize::try_from(services.settings.artifacts.max_size_bytes).unwrap_or(usize::MAX),
    };
    Router::new()
        .route(
            "/artifacts",
            get(artifacts::list::<P>).post(artifacts::publish::<P>),
        )
        .route(
            "/artifacts/{id}",
            get(artifacts::get::<P>)
                .put(artifacts::update::<P>)
                .delete(artifacts::delete::<P>),
        )
        .route("/artifacts/{id}/versions", get(artifacts::versions::<P>))
        .route("/artifacts/{id}/rollback", post(artifacts::rollback::<P>))
        .route(
            "/artifacts/{id}/versions/{n}",
            delete(artifacts::delete_version::<P>),
        )
        .route("/artifacts/{id}/share", post(artifacts::share::<P>))
        .route("/artifacts/{id}/unshare", post(artifacts::unshare::<P>))
        .route_layer(from_fn_with_state(
            services.clone(),
            auth::authenticate::<P>,
        ))
        .layer(axum::Extension(limits))
        .layer(DefaultBodyLimit::max(limits.body()))
        .layer(RequestBodyLimitLayer::new(limits.body()))
        .layer(long_timeout(config))
}

/// The viewer of artifacts under `prefix`:
/// `/a` on the API host, nothing on `[artifacts] host`.
fn viewer_routes<P: Ports>(prefix: &str) -> Router<Services<P>> {
    Router::new()
        .route(
            &format!("{prefix}/{{id}}"),
            get(artifacts_view::wrapper::<P>),
        )
        .route(
            &format!("{prefix}/{{id}}/v/{{n}}"),
            get(artifacts_view::version::<P>),
        )
        .route(
            &format!("{prefix}/{{id}}/v/{{n}}/{{*file}}"),
            get(artifacts_view::file::<P>),
        )
        .route(
            &format!("{prefix}/{{id}}/share"),
            post(artifacts_view::share::<P>),
        )
        .route(
            &format!("{prefix}/{{id}}/unshare"),
            post(artifacts_view::unshare::<P>),
        )
}

/// `artifacts` for requests to `host` (`[artifacts] host`, VIEW-08),
/// `api` for every other.
fn by_host(host: Host, artifacts: Router, api: Router) -> Router {
    Router::new().fallback(move |request: Request| {
        let target = if artifacts_view::host_of(&request).as_ref() == Some(&host) {
            artifacts.clone()
        } else {
            api.clone()
        };
        async move {
            match target.oneshot(request).await {
                Ok(reply) => reply,
                Err(never) => match never {},
            }
        }
    })
}

/// The routes behind a session that answer within `request_timeout`.
fn short_routes<P: Ports>(services: &Services<P>, config: &RestConfig) -> Router<Services<P>> {
    Router::new()
        .route("/auth/session", delete(auth::logout::<P>))
        .route("/auth/links", post(auth::create_link::<P>))
        .route("/me", get(auth::me))
        .route("/server", get(probes::server::<P>))
        .route("/admin/token", post(auth::admin_token::<P>))
        .route("/admin/users", get(admin::users::<P>))
        .route("/admin/sessions", delete(admin::revoke_sessions::<P>))
        .route("/git/connections", get(git::list::<P>))
        .route("/projects", get(projects::list::<P>))
        .route(
            "/projects/{p}",
            get(projects::get::<P>)
                .patch(projects::patch::<P>)
                .delete(projects::delete::<P>),
        )
        .route("/projects/{p}/resolve", post(projects::resolve::<P>))
        .route("/projects/{p}/environments", get(environments::list::<P>))
        .route(
            "/projects/{p}/environments/{e}",
            get(environments::get::<P>),
        )
        .route(
            "/projects/{p}/environments/{e}/redeploy",
            post(environments::redeploy::<P>),
        )
        .route(
            "/projects/{p}/environments/{e}/share",
            post(environments::share::<P>),
        )
        .route(
            "/projects/{p}/environments/{e}/unshare",
            post(environments::unshare::<P>),
        )
        .route(
            "/projects/{p}/environments/{e}/deploys",
            get(environments::deploys::<P>),
        )
        .route("/projects/{p}/snippet", get(deploys::snippet::<P>))
        .route("/domains", get(domains::list::<P>))
        .route("/deploys/{id}", get(deploys::get::<P>))
        .route("/deploys/{id}/log", get(deploys::log::<P>))
        .route("/secrets", get(secrets::list::<P>).put(secrets::put::<P>))
        .route("/secrets/resolve", get(secrets::resolve::<P>))
        .route(
            "/secrets/{key}",
            get(secrets::get::<P>).delete(secrets::delete::<P>),
        )
        .route("/secrets/{key}/versions", get(secrets::versions::<P>))
        .route("/secrets/{key}/rollback", post(secrets::rollback::<P>))
        .route("/modules", get(registry::list::<P>))
        .route(
            "/modules/{name}/tools/{tool}",
            post(registry::call_tool::<P>),
        )
        .route_layer(from_fn_with_state(
            services.clone(),
            auth::authenticate::<P>,
        ))
        .layer(short_timeout(config))
}

/// The routes behind a session that wait on docker, systemd, Caddy or the
/// forge (`long_request_timeout`).
fn long_routes<P: Ports>(services: &Services<P>, config: &RestConfig) -> Router<Services<P>> {
    Router::new()
        .route("/admin/reconcile", post(admin::reconcile::<P>))
        .route("/git/connections", post(git::create::<P>))
        .route("/git/connections/{host}", delete(git::delete::<P>))
        .route(
            "/git/connections/{host}/device",
            post(git::device_start::<P>),
        )
        .route(
            "/git/connections/{host}/device/poll",
            post(git::device_poll::<P>),
        )
        .route("/git/templates", get(git::templates::<P>))
        .route("/git/create-plan", post(git::create_plan::<P>))
        .route("/git/generate", post(git::generate::<P>))
        .route("/projects", post(projects::create::<P>))
        .route(
            "/projects/{p}/environments",
            post(environments::create::<P>),
        )
        .route(
            "/projects/{p}/environments/{e}",
            patch(environments::patch::<P>).delete(environments::delete::<P>),
        )
        .route(
            "/projects/{p}/environments/{e}/start",
            post(environments::start::<P>),
        )
        .route(
            "/projects/{p}/environments/{e}/stop",
            post(environments::stop::<P>),
        )
        .route(
            "/projects/{p}/environments/{e}/bindings",
            post(environments::bind::<P>),
        )
        .route("/bindings/{host}", delete(bindings::unbind::<P>))
        .route("/domains", post(domains::create::<P>))
        .route(
            "/domains/{host}",
            patch(domains::patch::<P>).delete(domains::delete::<P>),
        )
        .route("/projects/{p}/token/rotate", post(deploys::rotate::<P>))
        .route("/secrets/sync", post(secrets::sync::<P>))
        .route("/modules", post(registry::install::<P>))
        .route("/modules/available", get(registry::available::<P>))
        .route(
            "/modules/{name}",
            patch(registry::patch::<P>).delete(registry::delete::<P>),
        )
        .route_layer(from_fn_with_state(
            services.clone(),
            auth::authenticate::<P>,
        ))
        .layer(long_timeout(config))
}

fn short_timeout(config: &RestConfig) -> TimeoutLayer {
    middleware::timeout(config.request_timeout)
}

fn long_timeout(config: &RestConfig) -> TimeoutLayer {
    middleware::timeout(config.long_request_timeout)
}

/// Probes and the `OpenAPI` document, which need no credentials.
fn probe_routes<P: Ports>() -> Router<Services<P>> {
    Router::new()
        .route("/healthz", get(probes::healthz))
        .route("/readyz", get(probes::readyz_with_proxy::<P>))
        .route("/openapi.json", get(openapi::serve))
}
