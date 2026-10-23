#![cfg(test)]
//! PRX-01..PRX-04: the Caddy JSON of four situations, and of a protected
//! site (TEN-06), as `insta` snapshots in `tests/snapshots/prx0*.snap`
//! (hand-written and accepted by `caddy validate`, PRX-06), plus the header rule checked on
//! every route, and (live) a throwaway `caddy run` of the acme rendering
//! showing that on-demand TLS asks the daemon and a 403 blocks the
//! certificate.

use liftoff_core::domain::{Binding, Domain, DomainKind, EnvState, Environment, Timestamp};
use liftoff_core::fakes::World;
use liftoff_core::proxy_config::ProxyConfig;
use liftoff_core::service::{Settings, TlsMode};
use liftoff_proxy::render;
use serde_json::{Value, json};

fn in_state(environment: &Environment, state: EnvState) -> Environment {
    let mut changed = environment.clone();
    changed.state = state;
    changed
}

fn config(
    domains: &[Domain],
    bindings: &[Binding],
    environments: &[Environment],
    settings: &Settings,
) -> ProxyConfig {
    ProxyConfig::from(domains, bindings, environments, settings)
}

/// `shop.apps.test` → `web:3000` on running `prod` at 20000, external
/// `shop.example.org` → `web:3000` on running `qa` at 20001.
fn bound_and_running(world: &World, settings: &Settings) -> ProxyConfig {
    let mut external = world
        .records
        .domain(&world.shop, "shop.example.org")
        .unwrap();
    external.kind = DomainKind::External;
    external.verified_at = Some(Timestamp::UNIX_EPOCH);
    let external_binding = world
        .records
        .binding(&world.qa, &external, "web", 3000, 20_001)
        .unwrap();
    config(
        &[world.domain.clone(), external],
        &[world.binding.clone(), external_binding],
        &[
            in_state(&world.prod, EnvState::Running),
            in_state(&world.qa, EnvState::Running),
        ],
        settings,
    )
}

/// Unbound `docs.apps.test`, and `shop.apps.test` bound to stopped `prod`.
fn unbound_and_stopped(world: &World) -> ProxyConfig {
    let docs = world.records.domain(&world.shop, "docs.apps.test").unwrap();
    config(
        &[world.domain.clone(), docs],
        core::slice::from_ref(&world.binding),
        &[in_state(&world.prod, EnvState::Stopped), world.qa.clone()],
        &world.settings,
    )
}

/// `shop.apps.test` → running `prod` at 20000, `tls = edge` on 7002.
fn edge(world: &World) -> ProxyConfig {
    let mut settings = (*world.settings).clone();
    settings.proxy.tls = TlsMode::Edge { port: 7002 };
    config(
        core::slice::from_ref(&world.domain),
        core::slice::from_ref(&world.binding),
        &[in_state(&world.prod, EnvState::Running)],
        &settings,
    )
}

/// No domain at all: only the API site.
fn api_only(world: &World) -> ProxyConfig {
    config(
        &[],
        &[],
        core::slice::from_ref(&world.prod),
        &world.settings,
    )
}

/// Compares the pretty JSON of `cfg` with `tests/snapshots/<name>.snap`.
fn assert_caddy_json(name: &str, cfg: &ProxyConfig) {
    let pretty = serde_json::to_string_pretty(&render(cfg)).unwrap();
    insta::with_settings!({ prepend_module_to_snapshot => false, omit_expression => true }, {
        insta::assert_snapshot!(name, pretty);
    });
}

/// Every route of every server that serves sites (the `:80` redirect
/// server serves none).
fn site_routes(caddy: &Value) -> Vec<Value> {
    caddy["apps"]["http"]["servers"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(name, _)| name.as_str() != "http")
        .flat_map(|(_, server)| server["routes"].as_array().unwrap().clone())
        .collect()
}

mod render_snapshot {
    use super::*;

    // TC: PRX-01
    #[tokio::test]
    async fn matches_snapshot_when_acme_and_domains_bound_to_running_environments() {
        let world = World::new().await;
        assert_caddy_json(
            "prx01_acme_bound_running",
            &bound_and_running(&world, &world.settings),
        );
    }

    // TC: PRX-02
    #[tokio::test]
    async fn matches_snapshot_when_domain_unbound_and_environment_stopped() {
        let world = World::new().await;
        assert_caddy_json(
            "prx02_acme_unbound_and_stopped",
            &unbound_and_stopped(&world),
        );
    }

    // TC: PRX-03
    #[tokio::test]
    async fn matches_snapshot_when_tls_is_edge() {
        let world = World::new().await;
        assert_caddy_json("prx03_edge_bound_running", &edge(&world));
    }

    // TC: PRX-04
    #[tokio::test]
    async fn matches_snapshot_when_only_api_site() {
        let world = World::new().await;
        assert_caddy_json("prx04_acme_api_only", &api_only(&world));
    }
}

mod render_rules {
    use super::*;

    fn strip() -> Value {
        json!({ "handler": "headers", "request": { "delete": ["X-Liftoff-*"] } })
    }

    // TC: PRX-04 (X-Liftoff-* stripped first on every site, proxied ones)
    #[tokio::test]
    async fn strips_liftoff_headers_first_when_site_is_proxied() {
        let world = World::new().await;
        let caddy = render(&bound_and_running(&world, &world.settings));
        let first: Vec<Value> = site_routes(&caddy)
            .iter()
            .map(|route| route["handle"][0].clone())
            .collect();
        assert_eq!(first, vec![strip(), strip(), strip(), strip()]);
    }

    // TC: PRX-04 (X-Liftoff-* stripped first on every site, 503 ones)
    #[tokio::test]
    async fn strips_liftoff_headers_first_when_site_is_unavailable() {
        let world = World::new().await;
        let caddy = render(&unbound_and_stopped(&world));
        let first: Vec<Value> = site_routes(&caddy)
            .iter()
            .map(|route| route["handle"][0].clone())
            .collect();
        assert_eq!(first, vec![strip(), strip(), strip(), strip()]);
    }

    // TC: PRX-04 (X-Liftoff-* stripped first on every site behind the edge)
    #[tokio::test]
    async fn strips_liftoff_headers_first_when_tls_is_edge() {
        let world = World::new().await;
        let caddy = render(&edge(&world));
        let first: Vec<Value> = site_routes(&caddy)
            .iter()
            .map(|route| route["handle"][0].clone())
            .collect();
        assert_eq!(first, vec![strip(), strip(), strip()]);
    }

    /// The route that answers the API host's `/internal` paths with 404.
    fn internal_blocked() -> Value {
        json!({
            "match": [{ "host": ["liftoff.test"], "path": ["/internal", "/internal/*"] }],
            "handle": [
                strip(),
                { "handler": "static_response", "status_code": 404 }
            ],
            "terminal": true
        })
    }

    // TC: PRX-04 (`/internal/tls-ask` is loopback only, and the API site's proxy reaches the daemon from loopback: Caddy answers the API host's `/internal` paths itself, before proxying)
    #[tokio::test]
    async fn internal_paths_of_api_host_answered_404_before_proxying_when_acme() {
        let world = World::new().await;
        let caddy = render(&api_only(&world));
        assert_eq!(
            caddy["apps"]["http"]["servers"]["https"]["routes"][0],
            internal_blocked()
        );
    }

    // TC: PRX-04 (the same behind the edge)
    #[tokio::test]
    async fn internal_paths_of_api_host_answered_404_before_proxying_when_edge() {
        let world = World::new().await;
        let caddy = render(&edge(&world));
        assert_eq!(
            caddy["apps"]["http"]["servers"]["edge"]["routes"][0],
            internal_blocked()
        );
    }

    // TC: PRX-04 (the API site goes to `api.bind`)
    #[tokio::test]
    async fn api_route_proxies_to_api_bind_when_rendered() {
        let world = World::new().await;
        let caddy = render(&api_only(&world));
        assert_eq!(
            caddy["apps"]["http"]["servers"]["https"]["routes"][1],
            json!({
                "match": [{ "host": ["liftoff.test"] }],
                "handle": [
                    strip(),
                    { "handler": "reverse_proxy", "upstreams": [{ "dial": "127.0.0.1:7000" }] }
                ],
                "terminal": true
            })
        );
    }
}

/// PRX-07 (first bullet; 5.8 step 4):
/// `api.apps.test` bound to the environment `api` of `shop`, behind the
/// edge on 7002 (the shape `unserver` runs). A running function goes to
/// `[functions] bind` (`127.0.0.1:7003`), a running tunnel to
/// `[tunnel] bind` (`127.0.0.1:7004`), both with `Host` unchanged
/// (Caddy's `reverse_proxy` passes it through as it came: the listener
/// routes by it); an offline tunnel gets the 503 page «dev offline».
mod render_listeners {
    use liftoff_core::domain::EnvKind;

    use super::*;

    /// `api.apps.test` bound to `api` of `kind` in `state`, `tls = edge`.
    fn one_address(world: &World, kind: EnvKind, state: EnvState) -> ProxyConfig {
        let environment = if kind == EnvKind::Tunnel {
            world.records.tunnel(&world.shop, "api", &world.owner)
        } else {
            world.records.function(&world.shop, "api")
        }
        .unwrap();
        let domain = world.records.domain(&world.shop, "api.apps.test").unwrap();
        let binding = world
            .records
            .binding(&environment, &domain, "web", 80, 20_100)
            .unwrap();
        let mut settings = (*world.settings).clone();
        settings.proxy.tls = TlsMode::Edge { port: 7002 };
        config(
            &[domain],
            &[binding],
            &[in_state(&environment, state)],
            &settings,
        )
    }

    // TC: PRX-07 (function domain → `functions.bind`)
    #[tokio::test]
    async fn matches_snapshot_when_function_environment_runs() {
        let world = World::new().await;
        assert_caddy_json(
            "prx07_edge_function_running",
            &one_address(&world, EnvKind::Function, EnvState::Running),
        );
    }

    // TC: PRX-07 (tunnel domain `running` → `tunnel.bind`)
    #[tokio::test]
    async fn matches_snapshot_when_tunnel_environment_runs() {
        let world = World::new().await;
        assert_caddy_json(
            "prx07_edge_tunnel_running",
            &one_address(&world, EnvKind::Tunnel, EnvState::Running),
        );
    }

    // TC: PRX-07 (tunnel domain `offline` → 503 «dev offline»)
    #[tokio::test]
    async fn matches_snapshot_when_tunnel_environment_is_offline() {
        let world = World::new().await;
        assert_caddy_json(
            "prx07_edge_tunnel_offline",
            &one_address(&world, EnvKind::Tunnel, EnvState::Offline),
        );
    }
}

/// The gate (TEN-06, TEN-09; v1 SSO-03 ported):
/// a domain bound to a running, protected environment asks the gate
/// listener (`[gate] bind`, `127.0.0.1:7001`) with `forward_auth` before
/// its upstream, after the client's `X-Liftoff-*` are stripped, and the
/// gate's own paths below `/.liftoff/` on that host go to the gate.
mod render_gate {
    use super::*;

    /// [`bound_and_running`] with `prod` (behind `shop.apps.test`)
    /// protected.
    fn gated(world: &World) -> ProxyConfig {
        let mut prod = in_state(&world.prod, EnvState::Running);
        prod.protected = true;
        config(
            core::slice::from_ref(&world.domain),
            core::slice::from_ref(&world.binding),
            &[prod],
            &world.settings,
        )
    }

    // TC: TEN-06 (the whole rendering of a protected site, which PRX-06
    // runs through `caddy validate`)
    #[tokio::test]
    async fn matches_snapshot_when_bound_environment_is_protected() {
        let world = World::new().await;
        assert_caddy_json("prx05_gated_bound_running", &gated(&world));
    }

    /// The routes that match `shop.apps.test` and nothing else.
    fn shop_routes(caddy: &Value) -> Vec<Value> {
        site_routes(caddy)
            .into_iter()
            .filter(|route| route["match"][0]["host"] == json!(["shop.apps.test"]))
            .collect()
    }

    /// The route of `shop.apps.test` without a path matcher.
    fn shop_route(caddy: &Value) -> Value {
        shop_routes(caddy)
            .into_iter()
            .find(|route| route["match"][0].get("path").is_none())
            .unwrap()
    }

    /// The handler of the site route that asks the gate.
    fn gate_handler(caddy: &Value) -> Value {
        shop_route(caddy)["handle"]
            .as_array()
            .unwrap()
            .iter()
            .find(|handler| handler["upstreams"][0]["dial"] == json!("127.0.0.1:7001"))
            .cloned()
            .unwrap_or(Value::Null)
    }

    // TC: TEN-06 (strip, then `forward_auth` to `/gate/check` on the gate
    // listener, then the upstream)
    #[tokio::test]
    async fn asks_gate_check_before_upstream_when_environment_protected() {
        let world = World::new().await;
        let caddy = render(&gated(&world));
        let handlers: Vec<(Value, Value, Value)> = shop_route(&caddy)["handle"]
            .as_array()
            .unwrap()
            .iter()
            .map(|handler| {
                (
                    handler["handler"].clone(),
                    handler["upstreams"][0]["dial"].clone(),
                    handler["rewrite"]["uri"].clone(),
                )
            })
            .collect();
        assert_eq!(
            handlers,
            vec![
                (json!("headers"), Value::Null, Value::Null),
                (
                    json!("reverse_proxy"),
                    json!("127.0.0.1:7001"),
                    json!("/gate/check")
                ),
                (
                    json!("reverse_proxy"),
                    json!("127.0.0.1:20000"),
                    Value::Null
                ),
            ]
        );
    }

    // TC: TEN-06 (the v1 SSO gate: the gate takes the host from
    // `X-Liftoff-Host` and the original path from `X-Forwarded-Uri`)
    #[tokio::test]
    async fn passes_host_and_uri_to_gate_when_environment_protected() {
        let world = World::new().await;
        let set = &gate_handler(&render(&gated(&world)))["headers"]["request"]["set"];
        assert_eq!(
            (
                set["X-Liftoff-Host"].clone(),
                set["X-Forwarded-Uri"].clone()
            ),
            (
                json!(["{http.request.host}"]),
                json!(["{http.request.uri}"])
            )
        );
    }

    // TC: TEN-07 (the user headers the gate answers reach the upstream)
    #[tokio::test]
    async fn copies_user_headers_from_gate_when_environment_protected() {
        let world = World::new().await;
        let text = gate_handler(&render(&gated(&world))).to_string();
        assert!(
            text.contains("{http.reverse_proxy.header.X-Liftoff-User}")
                && text.contains("{http.reverse_proxy.header.X-Liftoff-Email}"),
            "{text}"
        );
    }

    // TC: TEN-06 (v1 SSO-03: `/.liftoff/*` of a protected host is the
    // gate's: its callback and logout)
    #[tokio::test]
    async fn routes_liftoff_paths_to_gate_when_environment_protected() {
        let world = World::new().await;
        let caddy = render(&gated(&world));
        let liftoff_paths: Vec<(Value, Value)> = shop_routes(&caddy)
            .iter()
            .filter(|route| route["match"][0]["path"] == json!(["/.liftoff/*"]))
            .map(|route| {
                let handle = route["handle"].as_array().unwrap();
                (
                    handle.first().cloned().unwrap_or(Value::Null),
                    handle
                        .last()
                        .map_or(Value::Null, |last| last["upstreams"][0]["dial"].clone()),
                )
            })
            .collect();
        assert_eq!(
            liftoff_paths,
            vec![(
                json!({ "handler": "headers", "request": { "delete": ["X-Liftoff-*"] } }),
                json!("127.0.0.1:7001")
            )]
        );
    }

    // TC: TEN-06 (v1 SSO-03: the gate's `/.liftoff/*` route passes
    // `X-Liftoff-Host`, without which the callback answers 400)
    #[tokio::test]
    async fn passes_host_to_gate_on_liftoff_paths_when_environment_protected() {
        let world = World::new().await;
        let caddy = render(&gated(&world));
        let hosts: Vec<Value> = shop_routes(&caddy)
            .iter()
            .filter(|route| route["match"][0]["path"] == json!(["/.liftoff/*"]))
            .map(|route| {
                route["handle"]
                    .as_array()
                    .and_then(|handle| handle.last())
                    .map_or(Value::Null, |last| {
                        last["headers"]["request"]["set"]["X-Liftoff-Host"].clone()
                    })
            })
            .collect();
        assert_eq!(hosts, vec![json!(["{http.request.host}"])]);
    }
}

/// VIEW-08: `[artifacts] host =
/// "artifacts.test"` with no domain, `tls = acme`. The artifacts' host is
/// served like the API host, to `[api] bind` (whose router answers it
/// with the artifacts alone), with its `/internal` paths answered 404 by
/// Caddy, since every request this proxy sends the daemon comes from
/// loopback.
mod render_artifacts_host {
    use liftoff_core::domain::Host;

    use super::*;

    fn artifacts_host(world: &World) -> ProxyConfig {
        let mut settings = (*world.settings).clone();
        settings.artifacts.host = Some(Host::new("artifacts.test").unwrap());
        config(&[], &[], core::slice::from_ref(&world.prod), &settings)
    }

    fn strip() -> Value {
        json!({ "handler": "headers", "request": { "delete": ["X-Liftoff-*"] } })
    }

    // TC: VIEW-08
    #[tokio::test]
    async fn matches_snapshot_when_artifacts_host_set() {
        let world = World::new().await;
        assert_caddy_json("prx08_acme_artifacts_host", &artifacts_host(&world));
    }

    // TC: VIEW-08 (the artifacts' host goes to `api.bind`, `Host` as it
    // came: the router tells the two hosts apart by it)
    #[tokio::test]
    async fn artifacts_route_proxies_to_api_bind_when_artifacts_host_set() {
        let world = World::new().await;
        let caddy = render(&artifacts_host(&world));
        let routes = site_routes(&caddy);
        assert!(
            routes.contains(&json!({
                "match": [{ "host": ["artifacts.test"] }],
                "handle": [
                    strip(),
                    { "handler": "reverse_proxy", "upstreams": [{ "dial": "127.0.0.1:7000" }] }
                ],
                "terminal": true
            })),
            "{routes:#?}"
        );
    }

    // TC: VIEW-08 (`/internal/tls-ask` stays loopback only on the
    // artifacts' host too)
    #[tokio::test]
    async fn internal_paths_of_artifacts_host_answered_404_when_artifacts_host_set() {
        let world = World::new().await;
        let caddy = render(&artifacts_host(&world));
        let routes = site_routes(&caddy);
        let blocked = json!({
            "match": [{ "host": ["artifacts.test"], "path": ["/internal", "/internal/*"] }],
            "handle": [
                strip(),
                { "handler": "static_response", "status_code": 404 }
            ],
            "terminal": true
        });
        let proxied = routes
            .iter()
            .position(|route| route["match"] == json!([{ "host": ["artifacts.test"] }]));
        assert!(
            routes
                .iter()
                .position(|route| *route == blocked)
                .is_some_and(|at| proxied.is_some_and(|proxied| at < proxied)),
            "{routes:#?}"
        );
    }
}

mod caddy_validate {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// The JSON body of an `insta` snapshot file (after its `---` header).
    fn snapshot_body(path: &Path) -> String {
        let text = std::fs::read_to_string(path).unwrap();
        let mut parts = text.splitn(3, "---\n");
        let (_, _, body) = (parts.next(), parts.next(), parts.next());
        body.unwrap().to_owned()
    }

    fn snapshots() -> Vec<PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "snap")
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("prx"))
            })
            .collect();
        found.sort();
        found
    }

    // TC: PRX-06, PRX-07
    #[test]
    #[ignore = "live: runs the caddy binary (`caddy validate`) on the snapshots"]
    fn every_snapshot_passes_caddy_validate_when_loaded() {
        let scratch = tempfile::tempdir().unwrap();
        let mut refused = Vec::new();
        let found = snapshots();
        let names: Vec<&str> = found
            .iter()
            .filter_map(|path| path.file_stem()?.to_str())
            .collect();
        // A new snapshot is added here on purpose: caddy validates each.
        assert_eq!(
            names,
            [
                "prx01_acme_bound_running",
                "prx02_acme_unbound_and_stopped",
                "prx03_edge_bound_running",
                "prx04_acme_api_only",
                "prx05_gated_bound_running",
                "prx07_edge_function_running",
                "prx07_edge_tunnel_offline",
                "prx07_edge_tunnel_running",
                "prx08_acme_artifacts_host",
            ]
        );
        for snapshot in found {
            let config = scratch.path().join("caddy.json");
            std::fs::write(&config, snapshot_body(&snapshot)).unwrap();
            let output = Command::new("caddy")
                .arg("validate")
                .arg("--config")
                .arg(&config)
                .output()
                .unwrap();
            if !output.status.success() {
                refused.push((
                    snapshot.display().to_string(),
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ));
            }
        }
        assert_eq!(refused, Vec::<(String, String)>::new());
    }
}

mod caddy_on_demand {
    use core::net::SocketAddr;
    use core::time::Duration;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::routing::get;
    use liftoff_core::domain::Host;
    use liftoff_core::proxy_config::{Route, Site, TLS_ASK_PATH};

    use super::*;

    /// The external host the stand-in daemon refuses (403).
    const DENIED: &str = "shop.example.org";
    /// The external host the stand-in daemon allows (200).
    const ALLOWED: &str = "allowed.example.org";

    /// A throwaway `caddy run`, killed when dropped.
    struct CaddyRun(Child);

    impl Drop for CaddyRun {
        fn drop(&mut self) {
            drop(self.0.kill());
            drop(self.0.wait());
        }
    }

    async fn free_port() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    }

    #[derive(serde::Deserialize)]
    struct Ask {
        domain: String,
    }

    async fn answer_ask(
        State(asked): State<Arc<Mutex<Vec<String>>>>,
        Query(ask): Query<Ask>,
    ) -> StatusCode {
        let status = if ask.domain == ALLOWED {
            StatusCode::OK
        } else {
            StatusCode::FORBIDDEN
        };
        asked.lock().unwrap().push(ask.domain);
        status
    }

    /// The daemon's `ask` endpoint: 200 for [`ALLOWED`], 403 otherwise;
    /// records every `?domain=`.
    async fn stand_in_daemon(asked: Arc<Mutex<Vec<String>>>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route(TLS_ASK_PATH, get(answer_ask))
            .with_state(asked);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    /// The rendered acme configuration, made runnable without privileges
    /// or public ACME: loopback ephemeral ports, Caddy's internal issuer,
    /// storage in `scratch`, no trust-store install.
    fn runnable(
        mut caddy: Value,
        scratch: &std::path::Path,
        admin: SocketAddr,
        http: SocketAddr,
        https: SocketAddr,
    ) -> Value {
        let app = &mut caddy["apps"]["http"];
        app["http_port"] = json!(http.port());
        app["https_port"] = json!(https.port());
        app["servers"]["https"]["listen"] = json!([https.to_string()]);
        app["servers"]["http"]["listen"] = json!([http.to_string()]);
        for policy in caddy["apps"]["tls"]["automation"]["policies"]
            .as_array_mut()
            .unwrap()
        {
            policy["issuers"] = json!([{ "module": "internal" }]);
        }
        caddy["apps"]["pki"] =
            json!({ "certificate_authorities": { "local": { "install_trust": false } } });
        caddy["admin"] = json!({ "listen": admin.to_string() });
        caddy["storage"] = json!({ "module": "file_system", "root": scratch.join("data") });
        caddy
    }

    async fn wait_until_ready(admin: SocketAddr) {
        let client = reqwest::Client::new();
        for _ in 0..100 {
            let probe = client.get(format!("http://{admin}/config/")).send().await;
            if probe.is_ok_and(|answer| answer.status().is_success()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("caddy admin api on {admin} never answered");
    }

    // TC: PRX-06 (on-demand TLS asks the daemon before issuing, and a refusal blocks the certificate)
    #[tokio::test]
    #[ignore = "live: runs a throwaway `caddy run` on loopback ephemeral ports"]
    async fn certificate_only_when_daemon_allows_on_demand_host() {
        let world = World::new().await;
        let asked = Arc::new(Mutex::new(Vec::new()));
        let daemon = stand_in_daemon(Arc::clone(&asked)).await;
        let mut settings = (*world.settings).clone();
        settings.proxy.api_bind = daemon;
        let mut cfg = bound_and_running(&world, &settings);
        cfg.sites.push(Site {
            host: Host::new(ALLOWED).unwrap(),
            kind: DomainKind::External,
            route: Route::Unavailable,
            gate: None,
        });

        let scratch = tempfile::tempdir().unwrap();
        let (admin, http, https) = (free_port().await, free_port().await, free_port().await);
        let file = scratch.path().join("caddy.json");
        let caddy = runnable(render(&cfg), scratch.path(), admin, http, https);
        std::fs::write(&file, serde_json::to_vec(&caddy).unwrap()).unwrap();
        let _run = CaddyRun(
            Command::new("caddy")
                .arg("run")
                .arg("--config")
                .arg(&file)
                .env("HOME", scratch.path())
                .env("XDG_DATA_HOME", scratch.path().join("data"))
                .env("XDG_CONFIG_HOME", scratch.path().join("config"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_until_ready(admin).await;

        let client = reqwest::Client::builder()
            .tls_danger_accept_invalid_certs(true)
            .resolve(DENIED, https)
            .resolve(ALLOWED, https)
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        let denied = client
            .get(format!("https://{DENIED}:{}/", https.port()))
            .send()
            .await;
        let allowed = client
            .get(format!("https://{ALLOWED}:{}/", https.port()))
            .send()
            .await
            .map(|answer| answer.status().as_u16());
        let mut seen = asked.lock().unwrap().clone();
        seen.dedup();
        assert_eq!(
            (denied.is_err(), allowed.ok(), seen),
            (true, Some(503), vec![DENIED.to_owned(), ALLOWED.to_owned()])
        );
    }
}
