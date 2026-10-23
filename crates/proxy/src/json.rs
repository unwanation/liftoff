//! [`render`]: a `ProxyConfig` as Caddy's JSON configuration.

use core::net::SocketAddr;

use liftoff_core::domain::{DomainKind, Host};
use liftoff_core::proxy_config::{ProxyConfig, Route, Site, TLS_ASK_PATH, Tls};
use serde_json::{Value, json};

/// The Caddy JSON document of `cfg`, what `POST /load` receives:
///
/// - `tls = acme`: `apps.http.servers.https` on `:443` (automatic HTTPS,
///   its own redirects off) and `apps.http.servers.http` on `:80` that
///   redirects to HTTPS with 308; `apps.tls.automation` with an on-demand
///   policy for the external hosts, a default policy with the ACME
///   e-mail, and the on-demand `permission` asking the daemon at
///   [`TLS_ASK_PATH`] on `api.upstream` (Caddy's successor of the
///   deprecated `ask` field; it appends `?domain=<host>`);
/// - `tls = edge`: one server `edge` on `127.0.0.1:<edge_port>` with
///   automatic HTTPS disabled and no `tls` app;
/// - routes: the API host's `/internal` paths answered 404 by Caddy
///   itself (they are loopback only, and every request this proxy sends the daemon comes from loopback), then the API site, then
///   the same two for `[artifacts] host` when it is set (to the API's upstream, whose router answers that host with the artifacts alone), then
///   one per site by host; each matched by host, terminal, deleting
///   client `X-Liftoff-*` request headers before `reverse_proxy` (to a compose upstream, or with `Host` as it came to the function or tunnel listener), the 404 or a 503 page
///   («nothing deployed here», or «dev offline» for an offline tunnel);
/// - a gated site ([`Site::gate`], a protected environment): first its paths below `/.liftoff/` go to the gate
///   listener (the callback and logout of the v1 SSO gate), then its
///   route asks the gate `GET /gate/check` with `forward_auth` (a
///   `reverse_proxy` whose `2xx` answer copies `X-Liftoff-User` and
///   `X-Liftoff-Email` into the request and goes on to the upstream; any
///   other answer, the `302` to the login or the `403`, goes back to the
///   client as it is).
#[must_use]
pub fn render(cfg: &ProxyConfig) -> Value {
    let mut routes = vec![
        internal_blocked(&cfg.api.host),
        route(&cfg.api.host, proxy_to(&cfg.api.upstream.to_string())),
    ];
    if let Some(artifacts) = &cfg.artifacts {
        routes.push(internal_blocked(artifacts));
        routes.push(route(artifacts, proxy_to(&cfg.api.upstream.to_string())));
    }
    for site in &cfg.sites {
        if let Some(gate) = site.gate {
            routes.push(gate_paths(&site.host, gate));
        }
        routes.push(site_route(site));
    }
    match &cfg.tls {
        Tls::Acme { email } => {
            let ask = format!("http://{}{TLS_ASK_PATH}", cfg.api.upstream);
            acme(routes, &cfg.sites, email.as_str(), &ask)
        }
        Tls::Edge { listen } => json!({
            "apps": {
                "http": {
                    "servers": {
                        "edge": {
                            "listen": [listen.to_string()],
                            "automatic_https": { "disable": true },
                            "routes": Value::Array(routes),
                        }
                    }
                }
            }
        }),
    }
}

/// The daemon's loopback-only paths, [`TLS_ASK_PATH`] among
/// them.
const INTERNAL_PREFIX: &str = "/internal";

/// The header the gate reads the protected host from .
const GATE_HOST_HEADER: &str = "X-Liftoff-Host";

/// The gate's question about one request.
const GATE_CHECK_PATH: &str = "/gate/check";

/// The gate's own paths on a protected host.
const GATE_PATHS: &str = "/.liftoff/*";

/// The text of liftoff's 503 page (the one v1 served).
const UNAVAILABLE: &str = "liftoff: nothing deployed here";

/// The text of the 503 page of an offline tunnel.
const OFFLINE: &str = "liftoff: dev offline";

/// `tls = acme`: `:443` with the sites, `:80` redirecting, and the ACME
/// automation with on-demand certificates for the external hosts.
fn acme(routes: Vec<Value>, sites: &[Site], email: &str, ask: &str) -> Value {
    let issuers = json!([{ "module": "acme", "email": email }]);
    let external: Vec<&str> = sites
        .iter()
        .filter(|site| site.kind == DomainKind::External)
        .map(|site| site.host.as_str())
        .collect();
    let mut policies = Vec::new();
    if !external.is_empty() {
        policies.push(json!({
            "subjects": external,
            "issuers": issuers,
            "on_demand": true,
        }));
    }
    policies.push(json!({ "issuers": issuers }));
    json!({
        "apps": {
            "http": {
                "servers": {
                    "https": {
                        "listen": [":443"],
                        "automatic_https": { "disable_redirects": true },
                        "routes": Value::Array(routes),
                    },
                    "http": {
                        "listen": [":80"],
                        "routes": [{
                            "handle": [{
                                "handler": "static_response",
                                "status_code": 308,
                                "headers": {
                                    "Location": ["https://{http.request.host}{http.request.uri}"]
                                },
                            }]
                        }],
                    }
                }
            },
            "tls": {
                "automation": {
                    "policies": policies,
                    "on_demand": {
                        "permission": { "module": "http", "endpoint": ask }
                    }
                }
            }
        }
    })
}

/// The route of one domain: its upstream or the 503 page, behind the
/// gate when it has one.
fn site_route(site: &Site) -> Value {
    let handler = match site.route {
        Route::Upstream(port) => proxy_to(&format!("127.0.0.1:{port}")),
        Route::Listener(listener) => proxy_to(&listener.to_string()),
        Route::Unavailable => unavailable(UNAVAILABLE),
        Route::Offline => unavailable(OFFLINE),
    };
    let mut handlers = vec![handler];
    if let Some(gate) = site.gate {
        handlers.insert(0, forward_auth(&gate.to_string()));
    }
    terminal_chain(&json!([{ "host": [site.host.as_str()] }]), handlers)
}

/// The gate's paths below `/.liftoff/` on the gated `host`.
fn gate_paths(host: &Host, gate: SocketAddr) -> Value {
    let handler = json!({
        "handler": "reverse_proxy",
        "upstreams": [{ "dial": gate.to_string() }],
        "headers": { "request": { "set": { GATE_HOST_HEADER: ["{http.request.host}"] } } },
    });
    terminal(
        &json!([{ "host": [host.as_str()], "path": [GATE_PATHS] }]),
        handler,
    )
}

/// Caddy's `forward_auth` to the gate on `dial`, as the Caddyfile
/// directive expands it: `GET /gate/check` with the host and the original
/// method and URI; a `2xx` copies the user headers and goes on.
fn forward_auth(dial: &str) -> Value {
    let copy = |header: &str| {
        json!({
            "match": [{ "not": [{ "vars": { format!("{{http.reverse_proxy.header.{header}}}"): [""] } }] }],
            "handle": [{
                "handler": "headers",
                "request": { "set": { header: [format!("{{http.reverse_proxy.header.{header}}}")] } },
            }],
        })
    };
    json!({
        "handler": "reverse_proxy",
        "upstreams": [{ "dial": dial }],
        "rewrite": { "method": "GET", "uri": GATE_CHECK_PATH },
        "headers": {
            "request": {
                "set": {
                    GATE_HOST_HEADER: ["{http.request.host}"],
                    "X-Forwarded-Method": ["{http.request.method}"],
                    "X-Forwarded-Uri": ["{http.request.uri}"],
                }
            }
        },
        "handle_response": [{
            "match": { "status_code": [2] },
            "routes": [
                { "handle": [{ "handler": "vars" }] },
                copy("X-Liftoff-User"),
                copy("X-Liftoff-Email"),
            ],
        }],
    })
}

/// The terminal route answering `/internal` and everything below it on
/// the API `host` with a bare 404, so a public request never reaches
/// `GET /internal/tls-ask` through the API site's `reverse_proxy`.
fn internal_blocked(host: &Host) -> Value {
    let paths = [INTERNAL_PREFIX.to_owned(), format!("{INTERNAL_PREFIX}/*")];
    terminal(
        &json!([{ "host": [host.as_str()], "path": paths }]),
        json!({ "handler": "static_response", "status_code": 404 }),
    )
}

/// A [`terminal`] route matching `host`.
fn route(host: &Host, handler: Value) -> Value {
    terminal(&json!([{ "host": [host.as_str()] }]), handler)
}

/// A terminal route on `matchers` that strips client `X-Liftoff-*`
/// request headers before `handler`.
fn terminal(matchers: &Value, handler: Value) -> Value {
    terminal_chain(matchers, vec![handler])
}

/// [`terminal`] with several handlers in order.
fn terminal_chain(matchers: &Value, handlers: Vec<Value>) -> Value {
    let mut handle =
        vec![json!({ "handler": "headers", "request": { "delete": ["X-Liftoff-*"] } })];
    handle.extend(handlers);
    json!({
        "match": matchers,
        "handle": Value::Array(handle),
        "terminal": true,
    })
}

/// liftoff's 503 page with `body`.
fn unavailable(body: &str) -> Value {
    json!({
        "handler": "static_response",
        "status_code": 503,
        "headers": { "Content-Type": ["text/plain; charset=utf-8"] },
        "body": body,
    })
}

/// `reverse_proxy` dialling `dial`.
fn proxy_to(dial: &str) -> Value {
    json!({ "handler": "reverse_proxy", "upstreams": [{ "dial": dial }] })
}
