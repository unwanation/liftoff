//! Wire bodies of the v2 API (shapes of `liftoff-api`) for the project
//! `liftoff` and its environment `prod`, and a stub that answers every
//! route a phase-1 command calls.

use crate::support::{Stub, json, sse, text};

pub(crate) const ME: &str =
    r#"{"id":"u1","login":"owner","email":"owner@example.com","role":"admin","source":"local"}"#;

/// What `POST /projects/-/resolve` answers: the name differs from the repo
/// on purpose, so a test can tell the resolved name from a guess.
pub(crate) const RESOLVED_PROJECT: &str = r#"{"id":"01J000000000000000000000P1","name":"web-app","git_host":"git.example.com","repo":"acme/shop","manifest":null,"created_by":"u1","created_at":"2026-10-18T10:00:00Z"}"#;

pub(crate) const PROJECT: &str = r#"{"id":"01J000000000000000000000P2","name":"liftoff","git_host":"git.example.com","repo":"acme/shop","manifest":null,"created_by":"u1","created_at":"2026-10-18T10:00:00Z"}"#;

pub(crate) const ENV: &str = r#"{"id":"01J000000000000000000000E1","project":"liftoff","name":"prod","kind":"compose","branch":"main","pinned_by":"u1","state":"running","cpus":1.0,"memory_mb":512,"current_deploy_id":"01J000000000000000000000D1","visibility":"public","created_at":"2026-10-18T10:00:00Z"}"#;

pub(crate) const ENV_QA: &str = r#"{"id":"01J000000000000000000000E2","project":"liftoff","name":"qa","kind":"compose","branch":"develop","pinned_by":"u1","state":"stopped","cpus":1.0,"memory_mb":512,"current_deploy_id":null,"visibility":"public","created_at":"2026-10-18T10:00:00Z"}"#;

pub(crate) const DEPLOY: &str = r#"{"id":"01J000000000000000000000D1","number":3,"project":"liftoff","environment":"prod","source":"cli","ref":null,"tag":"v1","status":"succeeded","error":null,"rolled_back_to":null,"triggered_by":"u1","started_at":"2026-10-18T10:00:00Z","finished_at":"2026-10-18T10:01:00Z"}"#;

pub(crate) const QUEUED: &str = r#"{"deploys":[{"project":"liftoff","environment":"prod","deploy_id":"01J000000000000000000000D2","number":4}]}"#;

pub(crate) const DOMAIN: &str = r#"{"host":"shop.apps.example.com","kind":"managed","project":"liftoff","verified_at":"2026-10-18T10:00:00Z","record_lost_at":null,"proxied":false}"#;

pub(crate) const BINDING: &str = r#"{"host":"shop.apps.example.com","project":"liftoff","environment":"prod","service":"web","container_port":3000,"host_port":20001}"#;

pub(crate) const SECRET: &str = r#"{"key":"API_KEY","scope":"environment","version":2,"ci":false,"updated_by":"u1","updated_at":"2026-10-18T10:00:00Z"}"#;

pub(crate) const SECRET_VALUE: &str = r#"{"key":"API_KEY","value":"s3cr3t","version":2}"#;

pub(crate) const REVISIONS: &str = r#"[{"version":1,"updated_by":"u1","updated_at":"2026-10-17T10:00:00Z"},{"version":2,"updated_by":"u1","updated_at":"2026-10-18T10:00:00Z"}]"#;

pub(crate) const TOKEN: &str = r#"{"project":"liftoff","id":"01J000000000000000000000T1","created_at":"2026-10-18T10:00:00Z"}"#;

pub(crate) const SNIPPET: &str = r#"{"path":".forgejo/workflows/deploy.yml","snippet":"curl -fsS -H \"Authorization: Bearer $LIFTOFF_TOKEN\" https://liftoff.example.com/api/v1/deploys"}"#;

// A made-up token of the right shape.
pub(crate) const SETUP_TOKEN: &str = r#"{"token":"lft_0123456789abcdefghijABCDEFGHIJ0123456789"}"#; // gitleaks:allow

// A made-up session token of the right shape.
pub(crate) const SESSION_GRANTED: &str = r#"{"token":"lfs_granted0123456789abcdefghijABCDEFGHIJ01","user":{"id":"u1","login":"owner","email":"owner@example.com","role":"admin","source":"local"}}"#; // gitleaks:allow

pub(crate) const CONNECTION: &str = r#"{"provider":"forgejo","host":"git.example.com","account_login":"owner","kind":"token","expires_at":null}"#;

pub(crate) const RESOLVED_ENV: &str = r#"{"vars":[{"key":"ZETA","value":"last-alphabetically"},{"key":"ALPHA","value":"1"},{"key":"LIFTOFF_PROJECT","value":"liftoff"},{"key":"LIFTOFF_ENV","value":"prod"}]}"#;

/// `{"deleted": what}`.
pub(crate) fn deleted(what: &str) -> String {
    format!(r#"{{"deleted":"{what}"}}"#)
}

/// A stub that answers every route a phase-1 command of `liftoff`/`prod`
/// calls with a well-formed body.
pub(crate) async fn full_stub() -> Stub {
    other_routes(environment_routes(Stub::start().await))
}

/// Sessions, projects and environments.
fn environment_routes(stub: Stub) -> Stub {
    let env = "/projects/liftoff/environments/prod";
    stub.route("GET", "/me", vec![json(200, ME)])
        .route("DELETE", "/auth/session", vec![json(204, "")])
        .route("POST", "/auth/token", vec![json(200, SESSION_GRANTED)])
        .route("GET", "/healthz", vec![json(200, r#"{"status":"ok"}"#)])
        .route("GET", "/readyz", vec![json(200, r#"{"status":"ready"}"#)])
        .route("POST", "/projects", vec![json(201, PROJECT)])
        .route("GET", "/projects/liftoff", vec![json(200, PROJECT)])
        .route(
            "POST",
            "/projects/-/resolve",
            vec![json(200, RESOLVED_PROJECT)],
        )
        .route(
            "GET",
            "/projects/liftoff/environments",
            vec![json(200, &format!("[{ENV},{ENV_QA}]"))],
        )
        .route(
            "POST",
            "/projects/liftoff/environments",
            vec![json(201, ENV_QA)],
        )
        .route(
            "DELETE",
            "/projects/liftoff/environments/qa",
            vec![json(200, &deleted("qa"))],
        )
        .route("GET", env, vec![json(200, ENV)])
        .route("PATCH", env, vec![json(200, ENV)])
        .route("POST", &format!("{env}/stop"), vec![json(200, ENV)])
        .route("POST", &format!("{env}/start"), vec![json(200, ENV)])
        .route("POST", &format!("{env}/redeploy"), vec![json(202, QUEUED)])
        .route(
            "GET",
            &format!("{env}/deploys"),
            vec![json(200, &format!("[{DEPLOY}]"))],
        )
        .route(
            "GET",
            &format!("{env}/logs"),
            vec![sse("data: web | listening on 3000\n\n")],
        )
        .route("POST", &format!("{env}/bindings"), vec![json(201, BINDING)])
        .route(
            "DELETE",
            "/bindings/shop.apps.example.com",
            vec![json(200, &deleted("shop.apps.example.com"))],
        )
}

/// Deploys, domains, secrets, tokens, administration and forges.
fn other_routes(stub: Stub) -> Stub {
    stub.route("POST", "/deploys", vec![json(202, QUEUED)])
        .route(
            "GET",
            "/deploys/01J000000000000000000000D2",
            vec![json(200, DEPLOY)],
        )
        .route(
            "GET",
            "/deploys/01J000000000000000000000D2/log",
            vec![text(200, "pulled\nstarted\n")],
        )
        .route("GET", "/domains", vec![json(200, &format!("[{DOMAIN}]"))])
        .route("POST", "/domains", vec![json(201, DOMAIN)])
        .route(
            "PATCH",
            "/domains/shop.apps.example.com",
            vec![json(200, DOMAIN)],
        )
        .route(
            "DELETE",
            "/domains/shop.apps.example.com",
            vec![json(200, &deleted("shop.apps.example.com"))],
        )
        .route("GET", "/secrets", vec![json(200, &format!("[{SECRET}]"))])
        .route("PUT", "/secrets", vec![json(200, SECRET)])
        .route("GET", "/secrets/API_KEY", vec![json(200, SECRET_VALUE)])
        .route(
            "DELETE",
            "/secrets/API_KEY",
            vec![json(200, &deleted("API_KEY"))],
        )
        .route(
            "GET",
            "/secrets/API_KEY/versions",
            vec![json(200, REVISIONS)],
        )
        .route("POST", "/secrets/API_KEY/rollback", vec![json(200, SECRET)])
        .route("GET", "/secrets/resolve", vec![json(200, RESOLVED_ENV)])
        .route(
            "POST",
            "/projects/liftoff/token/rotate",
            vec![json(201, TOKEN)],
        )
        .route("GET", "/projects/liftoff/snippet", vec![json(200, SNIPPET)])
        .route("POST", "/admin/token", vec![json(201, SETUP_TOKEN)])
        .route("GET", "/admin/users", vec![json(200, &format!("[{ME}]"))])
        .route(
            "DELETE",
            "/admin/sessions",
            vec![json(200, &deleted("bob"))],
        )
        .route("POST", "/admin/reconcile", vec![json(200, "{}")])
        .route(
            "GET",
            "/git/connections",
            vec![json(200, &format!("[{CONNECTION}]"))],
        )
        .route("POST", "/git/connections", vec![json(201, CONNECTION)])
        .route(
            "DELETE",
            "/git/connections/git.example.com",
            vec![json(200, &deleted("git.example.com"))],
        )
}
