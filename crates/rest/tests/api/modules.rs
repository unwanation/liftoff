//! `/api/v1/m/{module}/...` and the module dispatcher behind the core's
//! hooks (`HOOK-01…06`), over `ModuleHooks` and a
//! hand-written `tenants`-like module ([`FakeModules`]).

use std::sync::Arc;

use http::StatusCode;
use liftoff_core::domain::{ModuleName, Origin, UserRole};
use liftoff_core::fakes::{FakeModules, ModuleCall};
use liftoff_core::module::{ModuleHooks, ModuleManifest, ModuleRegistry};
use liftoff_core::ports::{AuthResult, ModuleResponse, Resource, WebPanel};

use crate::support::{UNKNOWN_SESSION, WorldCtx, detail, is_problem};

/// The reason the `tenants`-like module gives for a foreign project.
const FOREIGN: &str = "project shop belongs to another tenant";

/// `oidc` (a public `GET /login` and `POST /{flow}/authenticate`, the
/// `authenticate` hook) and `tenants`
/// (`GET /tenants` behind a session, the `authorize` hook), installed in
/// this order.
fn registry() -> Arc<ModuleRegistry> {
    let registry = Arc::new(ModuleRegistry::new());
    registry.register(
        ModuleName::new("oidc").unwrap(),
        ModuleManifest::parse(
            r#"{"name":"oidc","version":"0.1.0",
                "routes":[{"method":"GET","path":"/login","public":true},
                          {"method":"POST","path":"/{flow}/authenticate","public":true}],
                "hooks":["authenticate","render_page"]}"#,
        )
        .unwrap(),
    );
    registry.register(
        ModuleName::new("tenants").unwrap(),
        ModuleManifest::parse(
            r#"{"name":"tenants","version":"0.1.0","requires":["oidc"],
                "routes":[{"method":"GET","path":"/tenants"}],
                "hooks":["authorize","before_deploy"]}"#,
        )
        .unwrap(),
    );
    registry
}

/// The API over the dispatcher of [`registry`] calling `fake`, and the
/// registry.
async fn ctx(fake: &FakeModules) -> (WorldCtx, Arc<ModuleRegistry>) {
    let registry = registry();
    let dispatcher = ModuleHooks::new(Arc::clone(&registry), fake.clone());
    (WorldCtx::with_hooks(|_| dispatcher).await, registry)
}

mod routes {
    use super::*;

    // TC: HOOK-03
    #[tokio::test]
    async fn unauthorized_when_session_route_has_no_session() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx
            .call("GET", "/api/v1/m/tenants/tenants", None, None)
            .await;
        // The `authenticate` hook is asked first (oidc names nobody); the
        // route itself is never entered.
        let routed: Vec<ModuleCall> = fake
            .calls()
            .into_iter()
            .filter(|call| matches!(call, ModuleCall::Route { .. }))
            .collect();
        assert_eq!(
            (reply.status, is_problem(&reply), routed),
            (StatusCode::UNAUTHORIZED, true, Vec::new()),
            "{}",
            reply.text()
        );
    }

    // TC: HOOK-03
    #[tokio::test]
    async fn module_answers_anonymously_when_public_route_has_no_session() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.call("GET", "/api/v1/m/oidc/login", None, None).await;
        assert_eq!(
            (reply.status, reply.text(), fake.calls()),
            (
                StatusCode::OK,
                "anonymous".to_owned(),
                vec![ModuleCall::Route {
                    module: "oidc".to_owned(),
                    actor: None,
                    method: "GET".to_owned(),
                    path: "/login".to_owned()
                }]
            )
        );
    }

    // TC: HOOK-03 (the route matches without the query;
    // the module sees it, as an OIDC callback needs its `code` and `state`)
    #[tokio::test]
    async fn module_sees_query_when_public_route_has_query() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx
            .call("GET", "/api/v1/m/oidc/login?code=abc&state=xyz", None, None)
            .await;
        assert_eq!(
            (reply.status, fake.calls()),
            (
                StatusCode::OK,
                vec![ModuleCall::Route {
                    module: "oidc".to_owned(),
                    actor: None,
                    method: "GET".to_owned(),
                    path: "/login?code=abc&state=xyz".to_owned()
                }]
            )
        );
    }

    // TC: TEN-07 (v1 SSO-05: login CSRF; modules never see a cookie,
    // but a route sees the values of the `__Host-liftoff_login_*` cookies
    // as `x-liftoff-login-state`, never one the client sent itself)
    #[tokio::test]
    async fn module_sees_login_states_of_cookies_when_route_called() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let _reply = ctx
            .call_with(
                "GET",
                "/api/v1/m/oidc/login?code=abc&state=s1",
                &[
                    (
                        "cookie",
                        "__Host-liftoff_login_s1aaaaaa=s1; __Host-liftoff_session=lfs_secret",
                    ),
                    ("cookie", "__Host-liftoff_login_s2bbbbbb=s2"),
                    ("x-liftoff-login-state", "forged"),
                ],
                None,
            )
            .await;
        let seen: Vec<Vec<(String, String)>> = fake
            .route_headers()
            .into_iter()
            .map(|headers| {
                headers
                    .into_iter()
                    .filter(|(name, _)| name == "cookie" || name.starts_with("x-liftoff"))
                    .collect()
            })
            .collect();
        assert_eq!(
            seen,
            vec![vec![(
                "x-liftoff-login-state".to_owned(),
                "s1, s2".to_owned()
            )]]
        );
    }

    /// `module` answers every route with these `Set-Cookie` values.
    fn sets_cookies(fake: &FakeModules, module: &str, cookies: &[&str]) {
        fake.respond(
            module,
            ModuleResponse {
                status: 200,
                headers: cookies
                    .iter()
                    .map(|cookie| ("set-cookie".to_owned(), (*cookie).to_owned()))
                    .collect(),
                body: Vec::new(),
            },
        );
    }

    /// The names of the cookies `reply` sets.
    fn cookie_names(reply: &crate::support::Reply) -> Vec<String> {
        reply
            .headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|cookie| cookie.split_once('='))
            .map(|(name, _)| name.to_owned())
            .collect()
    }

    /// The core's cookies and one of a module's own.
    const COOKIES: &[&str] = &[
        "__Host-liftoff_session=lfs_fixed; Path=/; Secure; HttpOnly",
        "__Host-liftoff_login_s1aaaaaa=s1; Path=/; Secure; HttpOnly",
        "__Host-liftoff_gate=lfg_fixed; Path=/; Secure; HttpOnly",
        "__HOST-LIFTOFF_SESSION=lfs_fixed; Path=/; Secure",
        "theme=dark; Path=/",
    ];

    // TC: TEN-07 (modules are untrusted; a module that is not the browser login cannot fix the browser session the gate trusts, nor plant a login state, nor a host session)
    #[tokio::test]
    async fn drops_core_cookies_when_another_module_sets_them() {
        let fake = FakeModules::new();
        sets_cookies(&fake, "tenants", COOKIES);
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.owner("GET", "/api/v1/m/tenants/tenants", None).await;
        assert_eq!(
            (reply.status, cookie_names(&reply)),
            (StatusCode::OK, vec!["theme".to_owned()])
        );
    }

    // TC: TEN-07 (v1 SSO-05: the browser login, `oidc`, sets the browser
    // session and its login states, never a host session)
    #[tokio::test]
    async fn keeps_session_and_login_cookies_when_login_module_sets_them() {
        let fake = FakeModules::new();
        sets_cookies(&fake, "oidc", COOKIES);
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.call("GET", "/api/v1/m/oidc/login", None, None).await;
        assert_eq!(
            cookie_names(&reply),
            vec![
                "__Host-liftoff_session".to_owned(),
                "__Host-liftoff_login_s1aaaaaa".to_owned(),
                "__HOST-LIFTOFF_SESSION".to_owned(),
                "theme".to_owned(),
            ]
        );
    }

    // TC: WEB-04 (the browser login's session cookie follows the panel:
    // `SameSite=None` while `web` keeps one; its other cookies stay as
    // they are)
    #[tokio::test]
    async fn same_site_none_on_login_session_cookie_when_panel_kept() {
        let fake = FakeModules::new();
        sets_cookies(
            &fake,
            "oidc",
            &[
                "__Host-liftoff_session=lfs_fixed; Path=/; Secure; HttpOnly; SameSite=Lax",
                "__Host-liftoff_login_s1aaaaaa=s1; Path=/; Secure; HttpOnly; SameSite=Lax",
            ],
        );
        fake.keep_panel(
            "web",
            WebPanel {
                origin: Origin::new("https://panel.test").unwrap(),
            },
        );
        let (ctx, registry) = ctx(&fake).await;
        registry.register(
            ModuleName::new("web").unwrap(),
            ModuleManifest::parse(r#"{"name":"web","version":"0.1.0","hooks":["web_panel"]}"#)
                .unwrap(),
        );
        let reply = ctx.call("GET", "/api/v1/m/oidc/login", None, None).await;
        let cookies: Vec<String> = reply
            .headers
            .get_all(http::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok().map(ToOwned::to_owned))
            .collect();
        assert_eq!(
            cookies,
            vec![
                "__Host-liftoff_session=lfs_fixed; Path=/; Secure; HttpOnly; SameSite=None"
                    .to_owned(),
                "__Host-liftoff_login_s1aaaaaa=s1; Path=/; Secure; HttpOnly; SameSite=Lax"
                    .to_owned(),
            ]
        );
    }

    // TC: TEN-07 (v1 SSO-05: only the browser login sees the login states
    // of the browser's cookies)
    #[tokio::test]
    async fn hides_login_states_when_another_module_route_called() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let secret = ctx.world.login(&ctx.world.owner).await.unwrap();
        let authorization = format!("Bearer {}", secret.expose());
        let _reply = ctx
            .call_with(
                "GET",
                "/api/v1/m/tenants/tenants",
                &[
                    ("authorization", authorization.as_str()),
                    ("cookie", "__Host-liftoff_login_s1aaaaaa=s1"),
                ],
                None,
            )
            .await;
        let seen: Vec<Vec<String>> = fake
            .route_headers()
            .into_iter()
            .map(|headers| {
                headers
                    .into_iter()
                    .map(|(name, _)| name)
                    .filter(|name| name.starts_with("x-liftoff"))
                    .collect()
            })
            .collect();
        assert_eq!(seen, vec![Vec::<String>::new()]);
    }

    // TC: HOOK-03 (a route behind a session runs as the bearer's actor)
    #[tokio::test]
    async fn module_answers_as_owner_when_session_route_has_owner_session() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.owner("GET", "/api/v1/m/tenants/tenants", None).await;
        assert_eq!(
            (reply.status, reply.text()),
            (StatusCode::OK, ctx.world.owner.id.to_string())
        );
    }

    // TC: HOOK-03 (the module's status, headers and body reach the client)
    #[tokio::test]
    async fn redirects_when_public_route_answers_redirect() {
        let fake = FakeModules::new();
        fake.respond(
            "oidc",
            ModuleResponse {
                status: 302,
                headers: vec![(
                    "location".to_owned(),
                    "https://id.example.org/authorize".to_owned(),
                )],
                body: Vec::new(),
            },
        );
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.call("GET", "/api/v1/m/oidc/login", None, None).await;
        assert_eq!(
            (
                reply.status,
                reply
                    .headers
                    .get("location")
                    .and_then(|value| value.to_str().ok())
            ),
            (StatusCode::FOUND, Some("https://id.example.org/authorize"))
        );
    }

    // TC: HOOK-03 (an answer of the API is never cached, even when a module's answer asks for it: a login may set a session)
    #[tokio::test]
    async fn no_store_when_module_answers_public_cache_control() {
        let fake = FakeModules::new();
        fake.respond(
            "oidc",
            ModuleResponse {
                status: 200,
                headers: vec![("cache-control".to_owned(), "public, max-age=600".to_owned())],
                body: Vec::new(),
            },
        );
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.call("GET", "/api/v1/m/oidc/login", None, None).await;
        let cache: Vec<&str> = reply
            .headers
            .get_all("cache-control")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!((reply.status, cache), (StatusCode::OK, vec!["no-store"]));
    }

    // TC: HOOK-05
    #[tokio::test]
    async fn module_required_when_module_disabled() {
        let fake = FakeModules::new();
        let (ctx, registry) = ctx(&fake).await;
        assert!(registry.disable(&ModuleName::new("tenants").unwrap(), "trapped three times"));
        let reply = ctx.owner("GET", "/api/v1/m/tenants/tenants", None).await;
        assert_eq!(
            (reply.status, is_problem(&reply), fake.calls()),
            (StatusCode::FAILED_DEPENDENCY, true, Vec::new()),
            "{}",
            reply.text()
        );
    }

    // TC: HOOK-06
    #[tokio::test]
    async fn not_found_when_module_unknown() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.owner("GET", "/api/v1/m/nosuch/x", None).await;
        assert_eq!(
            (reply.status, is_problem(&reply), detail(&reply)),
            (
                StatusCode::NOT_FOUND,
                true,
                "module nosuch not found".to_owned()
            ),
            "{}",
            reply.text()
        );
    }

    // TC: HOOK-06 (the reserved `/.hook/` paths are the
    // host's own calls; a parameter route that would match them does not)
    #[tokio::test]
    async fn not_found_when_path_is_reserved_hook_path() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx
            .call("POST", "/api/v1/m/oidc/.hook/authenticate", None, None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), fake.calls()),
            (StatusCode::NOT_FOUND, true, Vec::new()),
            "{}",
            reply.text()
        );
    }

    // TC: HOOK-06 (a route the module does not declare)
    #[tokio::test]
    async fn not_found_when_route_undeclared() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.owner("DELETE", "/api/v1/m/tenants/tenants", None).await;
        assert_eq!(
            (reply.status, is_problem(&reply), fake.calls()),
            (StatusCode::NOT_FOUND, true, Vec::new()),
            "{}",
            reply.text()
        );
    }
}

mod hooks {
    use super::*;

    // TC: HOOK-01 (a tenants-like module denies a
    // foreign project through `authorize`, visible as 403)
    #[tokio::test]
    async fn forbidden_when_tenants_denies_foreign_project() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        fake.deny_resource("tenants", Resource::Project(ctx.world.shop.id), FOREIGN);
        let reply = ctx.owner("GET", "/api/v1/projects/shop", None).await;
        assert_eq!(
            (reply.status, is_problem(&reply), detail(&reply)),
            (StatusCode::FORBIDDEN, true, FOREIGN.to_owned()),
            "{}",
            reply.text()
        );
    }

    // TC: HOOK-01 (no deny: the core decides, the admin sees the project)
    #[tokio::test]
    async fn shop_when_tenants_has_no_opinion() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.owner("GET", "/api/v1/projects/shop", None).await;
        assert_eq!(
            (reply.status, reply.json()["name"].as_str()),
            (StatusCode::OK, Some("shop")),
            "{}",
            reply.text()
        );
    }

    // TC: HOOK-02 (a module that refuses decides: even the owner's valid
    // session is not checked)
    #[tokio::test]
    async fn unauthorized_when_oidc_denies() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        fake.authenticate_as("oidc", AuthResult::Deny("token revoked".to_owned()));
        let reply = ctx.owner("GET", "/api/v1/me", None).await;
        assert_eq!(
            (reply.status, is_problem(&reply), detail(&reply)),
            (StatusCode::UNAUTHORIZED, true, "token revoked".to_owned()),
            "{}",
            reply.text()
        );
    }

    // TC: HOOK-02 (the module names the actor; the bearer is not checked)
    #[tokio::test]
    async fn me_is_carol_when_oidc_names_her_and_bearer_unknown() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let carol = ctx.world.add_user("carol", UserRole::User).await.unwrap();
        fake.authenticate_as("oidc", AuthResult::Actor(carol.id));
        let reply = ctx
            .call("GET", "/api/v1/me", Some(UNKNOWN_SESSION), None)
            .await;
        assert_eq!(
            (reply.status, reply.json()["login"].as_str()),
            (StatusCode::OK, Some("carol")),
            "{}",
            reply.text()
        );
    }
}

/// `POST /api/v1/modules/{name}/tools/{tool}`: the MCP tools of modules,
/// which `lof mcp` calls through it.
mod tools {
    use liftoff_core::Error;
    use serde_json::json;

    use super::*;

    /// `tenants` with the tool `create`, behind the dispatcher calling
    /// `fake`; and the registry.
    async fn ctx(fake: &FakeModules) -> (WorldCtx, Arc<ModuleRegistry>) {
        ctx_with(fake, r#"{"type":"object"}"#).await
    }

    /// As [`ctx`], the tool `create` taking the arguments `schema`.
    async fn ctx_with(fake: &FakeModules, schema: &str) -> (WorldCtx, Arc<ModuleRegistry>) {
        let registry = Arc::new(ModuleRegistry::new());
        registry.register(
            ModuleName::new("tenants").unwrap(),
            ModuleManifest::parse(&format!(
                r#"{{"name":"tenants","version":"0.1.0",
                    "tools":[{{"name":"create","description":"Create a tenant","schema":{schema}}}]}}"#
            ))
            .unwrap(),
        );
        let dispatcher = ModuleHooks::new(Arc::clone(&registry), fake.clone());
        (WorldCtx::with_hooks(|_| dispatcher).await, registry)
    }

    const CREATE: &str = "/api/v1/modules/tenants/tools/create";

    // TC: MCP-04 (the tool runs as the caller; its JSON answer is the
    // result)
    #[tokio::test]
    async fn result_when_owner_calls_declared_tool() {
        let fake = FakeModules::new();
        fake.tool(
            "tenants",
            "create",
            Ok(r#"{"slug":"acme","id":7}"#.to_owned()),
        );
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx
            .owner(
                "POST",
                CREATE,
                Some(&json!({"arguments": {"slug": "acme"}})),
            )
            .await;
        assert_eq!(
            (reply.status, reply.json(), fake.calls()),
            (
                StatusCode::OK,
                json!({"result": {"slug": "acme", "id": 7}}),
                vec![ModuleCall::Tool {
                    module: "tenants".to_owned(),
                    actor: Some(ctx.world.owner.id),
                    name: "create".to_owned(),
                    args: r#"{"slug":"acme"}"#.to_owned(),
                }]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: MCP-04 (the core checks the arguments against the tool's
    // schema: a module is untrusted and never sees what the schema
    // forbids)
    #[tokio::test]
    async fn bad_request_when_arguments_break_tool_schema() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx_with(
            &fake,
            r#"{"type":"object","properties":{"slug":{"type":"string"}},"required":["slug"]}"#,
        )
        .await;
        let reply = ctx
            .owner("POST", CREATE, Some(&json!({"arguments": {"slug": 7}})))
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), fake.calls()),
            (StatusCode::BAD_REQUEST, true, Vec::new()),
            "{}",
            reply.text()
        );
    }

    // TC: MCP-04 (no session, no call)
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx
            .call("POST", CREATE, None, Some(&json!({"arguments": {}})))
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), fake.calls()),
            (StatusCode::UNAUTHORIZED, true, Vec::new())
        );
    }

    // TC: MCP-04 (only a declared tool is called)
    #[tokio::test]
    async fn not_found_when_tool_undeclared() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/modules/tenants/tools/purge",
                Some(&json!({})),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply), fake.calls()),
            (StatusCode::NOT_FOUND, true, Vec::new()),
            "{}",
            reply.text()
        );
    }

    // TC: HOOK-05 (a disabled module takes no tool calls)
    #[tokio::test]
    async fn module_required_when_module_disabled() {
        let fake = FakeModules::new();
        let (ctx, registry) = ctx(&fake).await;
        assert!(registry.disable(&ModuleName::new("tenants").unwrap(), "trapped three times"));
        let reply = ctx.owner("POST", CREATE, Some(&json!({}))).await;
        assert_eq!(
            (reply.status, fake.calls()),
            (StatusCode::FAILED_DEPENDENCY, Vec::new()),
            "{}",
            reply.text()
        );
    }

    // TC: MCP-03 (the module's own refusal maps like any error of the
    // core)
    #[tokio::test]
    async fn conflict_when_module_refuses_with_conflict() {
        let fake = FakeModules::new();
        fake.tool(
            "tenants",
            "create",
            Err(Error::conflict("tenant acme exists")),
        );
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx
            .owner(
                "POST",
                CREATE,
                Some(&json!({"arguments": {"slug": "acme"}})),
            )
            .await;
        assert_eq!(
            (reply.status, detail(&reply).contains("tenant acme exists")),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: MCP-04 (an answer that is not JSON is the module's failure)
    #[tokio::test]
    async fn internal_error_when_module_answers_no_json() {
        let fake = FakeModules::new();
        fake.tool("tenants", "create", Ok("not json".to_owned()));
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx.owner("POST", CREATE, Some(&json!({}))).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::INTERNAL_SERVER_ERROR, true),
            "{}",
            reply.text()
        );
    }

    // TC: MCP-04 (the body takes `arguments` and nothing else)
    #[tokio::test]
    async fn bad_request_when_body_has_unknown_field() {
        let fake = FakeModules::new();
        let (ctx, _registry) = ctx(&fake).await;
        let reply = ctx
            .owner("POST", CREATE, Some(&json!({"slug": "acme"})))
            .await;
        assert_eq!(
            (reply.status.is_client_error(), fake.calls()),
            (true, Vec::new()),
            "{}",
            reply.text()
        );
    }
}
