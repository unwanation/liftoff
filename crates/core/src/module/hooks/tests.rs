use std::sync::Arc;

use super::*;
use crate::domain::{Bearer, DeployStatus, UserRole};
use crate::fakes::{FakeModules, ModuleCall, WithHooks, World};
use crate::module::ModuleManifest;
use crate::ports::PageKind;
use crate::service::{AuthService, ProjectService};

/// The reason the `tenants`-like module gives for a foreign project.
const FOREIGN: &str = "project shop belongs to another tenant";

fn name(raw: &str) -> ModuleName {
    ModuleName::new(raw).unwrap()
}

/// The manifest of `module` subscribed to every hook, with a route
/// `GET /tenants` behind a session and a public `GET /login`.
fn manifest(module: &str) -> ModuleManifest {
    ModuleManifest::parse(&format!(
        r#"{{"name":"{module}","version":"0.1.0",
            "routes":[{{"method":"GET","path":"/tenants"}},
                      {{"method":"GET","path":"/login","public":true}}],
            "hooks":["authenticate","authorize","before_deploy","after_deploy",
                     "domain_created","domain_removed","gate","render_page"]}}"#
    ))
    .unwrap()
}

/// The manifest of `module` subscribed to no hook.
fn quiet(module: &str) -> ModuleManifest {
    ModuleManifest::parse(&format!(r#"{{"name":"{module}","version":"0.1.0"}}"#)).unwrap()
}

/// The dispatcher over `modules` registered in this order, every one
/// subscribed to every hook, and the fake behind it.
fn dispatcher(modules: &[&str]) -> (ModuleHooks<FakeModules>, FakeModules) {
    let registry = Arc::new(ModuleRegistry::new());
    for module in modules {
        registry.register(name(module), manifest(module));
    }
    let fake = FakeModules::new();
    (ModuleHooks::new(registry, fake.clone()), fake)
}

fn request(path: &str) -> HookRequest {
    HookRequest {
        method: "GET".to_owned(),
        path: path.to_owned(),
        headers: vec![("authorization".to_owned(), "Bearer oidc-carol".to_owned())],
    }
}

mod authorize {
    use super::*;

    // TC: HOOK-01
    #[tokio::test]
    async fn denies_when_second_module_denies_after_continue() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["oidc", "tenants"]);
        fake.deny_resource("tenants", Resource::Project(world.shop.id), FOREIGN);
        assert_eq!(
            hooks
                .authorize(
                    &world.owner,
                    Action::ProjectGet,
                    &Resource::Project(world.shop.id)
                )
                .await,
            HookResult::Deny(FOREIGN.to_owned())
        );
    }

    // TC: HOOK-01 (the first deny wins over an earlier allow)
    #[tokio::test]
    async fn denies_when_later_module_denies_after_allow() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["oidc", "tenants"]);
        fake.answer("oidc", HookName::Authorize, HookResult::Allow);
        fake.deny_resource("tenants", Resource::Project(world.shop.id), FOREIGN);
        assert_eq!(
            hooks
                .authorize(
                    &world.owner,
                    Action::ProjectGet,
                    &Resource::Project(world.shop.id)
                )
                .await,
            HookResult::Deny(FOREIGN.to_owned())
        );
    }

    // TC: HOOK-01
    #[tokio::test]
    async fn asks_no_later_module_when_first_denies() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["tenants", "oidc"]);
        fake.deny_resource("tenants", Resource::Project(world.shop.id), FOREIGN);
        let _denied = hooks
            .authorize(
                &world.owner,
                Action::ProjectGet,
                &Resource::Project(world.shop.id),
            )
            .await;
        assert_eq!(fake.asked(HookName::Authorize), vec!["tenants".to_owned()]);
    }

    // TC: HOOK-01 (installation order)
    #[tokio::test]
    async fn asks_modules_in_install_order_when_none_decides() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["oidc", "tenants", "web"]);
        let answer = hooks
            .authorize(&world.owner, Action::ProjectList, &Resource::Admin)
            .await;
        assert_eq!(
            (answer, fake.asked(HookName::Authorize)),
            (
                HookResult::Continue,
                vec!["oidc".to_owned(), "tenants".to_owned(), "web".to_owned()]
            )
        );
    }

    // TC: HOOK-01
    #[tokio::test]
    async fn allows_when_one_module_allows_and_none_denies() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["oidc", "tenants"]);
        fake.answer("tenants", HookName::Authorize, HookResult::Allow);
        assert_eq!(
            hooks
                .authorize(&world.owner, Action::ProjectList, &Resource::Admin)
                .await,
            HookResult::Allow
        );
    }

    // TC: HOOK-01 (a module asks only about the hooks it subscribed to)
    #[tokio::test]
    async fn asks_no_module_when_none_subscribed() {
        let world = World::new().await;
        let registry = Arc::new(ModuleRegistry::new());
        registry.register(name("dns"), quiet("dns"));
        let fake = FakeModules::new();
        let hooks = ModuleHooks::new(registry, fake.clone());
        let answer = hooks
            .authorize(&world.owner, Action::ProjectList, &Resource::Admin)
            .await;
        assert_eq!((answer, fake.calls()), (HookResult::Continue, Vec::new()));
    }

    // TC: HOOK-01 (a tenants-like module denies a
    // foreign project through `authorize`, and the core service refuses)
    #[tokio::test]
    async fn forbids_project_get_when_tenants_denies_project() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["tenants"]);
        fake.deny_resource("tenants", Resource::Project(world.shop.id), FOREIGN);
        let ports = Arc::new(WithHooks::new(Arc::clone(&world.ports), hooks));
        let refused = ProjectService::new(ports, Arc::clone(&world.settings))
            .get(&world.owner, &world.shop.name)
            .await;
        assert!(
            matches!(&refused, Err(Error::Forbidden(why)) if why == FOREIGN),
            "{refused:?}"
        );
    }
}

mod before_deploy {
    use super::*;

    // TC: HOOK-01 (the catalogue's case: `Continue` + `Deny` → `Deny`)
    #[tokio::test]
    async fn denies_when_second_module_denies_after_continue() {
        let world = World::new().await;
        let deploy = world
            .records
            .deploy(&world.prod, 1, DeployStatus::Queued, &world.owner)
            .unwrap();
        let (hooks, fake) = dispatcher(&["oidc", "tenants"]);
        fake.answer(
            "tenants",
            HookName::BeforeDeploy,
            HookResult::Deny("tenant quota of 2 CPUs is used up".to_owned()),
        );
        assert_eq!(
            (
                hooks.before_deploy(&world.prod, &deploy).await,
                fake.asked(HookName::BeforeDeploy)
            ),
            (
                HookResult::Deny("tenant quota of 2 CPUs is used up".to_owned()),
                vec!["oidc".to_owned(), "tenants".to_owned()]
            )
        );
    }

    // TC: HOOK-01
    #[tokio::test]
    async fn continues_when_every_module_continues() {
        let world = World::new().await;
        let deploy = world
            .records
            .deploy(&world.prod, 1, DeployStatus::Queued, &world.owner)
            .unwrap();
        let (hooks, _fake) = dispatcher(&["oidc", "tenants"]);
        assert_eq!(
            hooks.before_deploy(&world.prod, &deploy).await,
            HookResult::Continue
        );
    }
}

mod authenticate {
    use super::*;

    // TC: HOOK-02
    #[tokio::test]
    async fn names_actor_when_module_names_it() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["oidc"]);
        fake.authenticate_as("oidc", AuthResult::Actor(world.owner.id));
        assert_eq!(
            hooks.authenticate(&request("/api/v1/me")).await,
            AuthResult::Actor(world.owner.id)
        );
    }

    // TC: HOOK-02
    #[tokio::test]
    async fn names_actor_when_second_module_names_it_after_continue() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["web", "oidc"]);
        fake.authenticate_as("oidc", AuthResult::Actor(world.owner.id));
        assert_eq!(
            hooks.authenticate(&request("/api/v1/me")).await,
            AuthResult::Actor(world.owner.id)
        );
    }

    // TC: HOOK-02
    #[tokio::test]
    async fn continues_when_no_module_names_actor() {
        let (hooks, fake) = dispatcher(&["web", "oidc"]);
        let answer = hooks.authenticate(&request("/api/v1/me")).await;
        assert_eq!(
            (answer, fake.calls()),
            (
                AuthResult::Continue,
                vec![
                    ModuleCall::Authenticate {
                        module: "web".to_owned(),
                        path: "/api/v1/me".to_owned()
                    },
                    ModuleCall::Authenticate {
                        module: "oidc".to_owned(),
                        path: "/api/v1/me".to_owned()
                    }
                ]
            )
        );
    }

    // TC: HOOK-02 (a module that refuses decides: no later module is
    // asked)
    #[tokio::test]
    async fn denies_when_module_denies() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["web", "oidc"]);
        fake.authenticate_as("web", AuthResult::Deny("token revoked".to_owned()));
        fake.authenticate_as("oidc", AuthResult::Actor(world.owner.id));
        let answer = hooks.authenticate(&request("/api/v1/me")).await;
        assert_eq!(
            (answer, fake.calls()),
            (
                AuthResult::Deny("token revoked".to_owned()),
                vec![ModuleCall::Authenticate {
                    module: "web".to_owned(),
                    path: "/api/v1/me".to_owned()
                }]
            )
        );
    }

    // TC: HOOK-02 (the bearer is not checked: an unknown session passes)
    #[tokio::test]
    async fn authenticates_module_actor_when_bearer_is_unknown() {
        let world = World::new().await;
        let carol = world.add_user("carol", UserRole::User).await.unwrap();
        let (hooks, fake) = dispatcher(&["oidc"]);
        fake.authenticate_as("oidc", AuthResult::Actor(carol.id));
        let ports = Arc::new(WithHooks::new(Arc::clone(&world.ports), hooks));
        let unknown = Bearer::parse("lfs_0123456789abcdefghijABCDEFGHIJ0123456789").unwrap();
        let principal = AuthService::new(ports)
            .authenticate(&request("/api/v1/me"), Some(&unknown))
            .await
            .unwrap();
        assert_eq!(
            (principal.user().login.to_string(), principal.session()),
            ("carol".to_owned(), None)
        );
    }
}

mod render_page {
    use super::*;

    // TC: HOOK-04
    #[tokio::test]
    async fn returns_module_html_when_module_renders_login() {
        let (hooks, fake) = dispatcher(&["oidc"]);
        fake.page(
            "oidc",
            PageKind::Login,
            r#"<meta http-equiv="refresh" content="0; url=https://id.example.org/authorize">"#,
        );
        assert_eq!(
            hooks.render_page(&PageRequest::of(PageKind::Login)).await,
            Some(ModulePage::Html(
                r#"<meta http-equiv="refresh" content="0; url=https://id.example.org/authorize">"#
                    .to_owned()
            ))
        );
    }

    // TC: HOOK-04
    #[tokio::test]
    async fn returns_first_html_when_two_modules_render_forbidden() {
        let (hooks, fake) = dispatcher(&["web", "tenants"]);
        fake.page("web", PageKind::Forbidden, "<h1>Not yours</h1>");
        fake.page("tenants", PageKind::Forbidden, "<h1>Another tenant</h1>");
        assert_eq!(
            hooks
                .render_page(&PageRequest::of(PageKind::Forbidden))
                .await,
            Some(ModulePage::Html("<h1>Not yours</h1>".to_owned()))
        );
    }

    // TC: HOOK-04 (no module page: the core keeps its own)
    #[tokio::test]
    async fn keeps_core_page_when_no_module_renders_it() {
        let (hooks, fake) = dispatcher(&["oidc"]);
        fake.page("oidc", PageKind::Login, "<h1>Sign in</h1>");
        assert_eq!(
            hooks
                .render_page(&PageRequest::of(PageKind::Unavailable))
                .await,
            None
        );
    }
}

mod tenant_data {
    use super::*;
    use crate::domain::{TenantId, TenantSlug};
    use crate::ports::Rng as _;

    /// The dispatcher over `tenants` subscribed to the data hooks of
    /// tenants, failing every call with `error`.
    fn failing(error: Error) -> ModuleHooks<FakeModules> {
        let registry = Arc::new(ModuleRegistry::new());
        registry.register(
            name("tenants"),
            ModuleManifest::parse(
                r#"{"name":"tenants","version":"0.1.0",
                    "hooks":["tenant_quotas","project_tenant"]}"#,
            )
            .unwrap(),
        );
        let fake = FakeModules::new();
        fake.fail("tenants", error);
        ModuleHooks::new(registry, fake)
    }

    // TC: TEN-04 (fail closed: a module that keeps quotas and fails makes
    // the quota unavailable, never absent)
    #[tokio::test]
    async fn unavailable_when_quotas_module_fails() {
        let world = World::new().await;
        let hooks = failing(Error::internal_msg("trapped"));
        let quotas = hooks
            .tenant_quotas(TenantId::new(world.ports.rng.ulid()))
            .await;
        assert!(matches!(quotas, Err(Error::Unavailable(_))), "{quotas:?}");
    }

    // TC: TEN-04 (fail closed: no project goes outside every tenant
    // because the module failed)
    #[tokio::test]
    async fn unavailable_when_project_tenant_module_fails() {
        let world = World::new().await;
        let hooks = failing(Error::internal_msg("trapped"));
        let chosen = hooks.project_tenant(&world.owner, None).await;
        assert!(matches!(chosen, Err(Error::Unavailable(_))), "{chosen:?}");
    }

    // TC: TEN-04 (the module's refusal of a slug reaches the caller)
    #[tokio::test]
    async fn not_found_when_module_does_not_know_the_slug() {
        let world = World::new().await;
        let hooks = failing(Error::not_found("tenant nope"));
        let slug = TenantSlug::new("nope").unwrap();
        let chosen = hooks.project_tenant(&world.owner, Some(&slug)).await;
        assert!(matches!(chosen, Err(Error::NotFound(_))), "{chosen:?}");
    }
}

mod disabled {
    use super::*;

    // TC: HOOK-05
    #[tokio::test]
    async fn skips_disabled_module_when_authorize_called() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["oidc", "tenants"]);
        fake.deny_resource("tenants", Resource::Project(world.shop.id), FOREIGN);
        assert!(
            hooks
                .registry()
                .disable(&name("tenants"), "trapped three times")
        );
        let answer = hooks
            .authorize(
                &world.owner,
                Action::ProjectGet,
                &Resource::Project(world.shop.id),
            )
            .await;
        assert_eq!(
            (answer, fake.asked(HookName::Authorize)),
            (HookResult::Continue, vec!["oidc".to_owned()])
        );
    }

    // TC: HOOK-05
    #[tokio::test]
    async fn skips_disabled_module_when_page_rendered() {
        let (hooks, fake) = dispatcher(&["oidc"]);
        fake.page("oidc", PageKind::Login, "<h1>Sign in</h1>");
        assert!(
            hooks
                .registry()
                .disable(&name("oidc"), "trapped three times")
        );
        let page = hooks.render_page(&PageRequest::of(PageKind::Login)).await;
        assert_eq!((page, fake.calls()), (None, Vec::new()));
    }

    // TC: HOOK-05
    #[tokio::test]
    async fn refuses_route_with_module_required_when_module_disabled() {
        let (hooks, _fake) = dispatcher(&["tenants"]);
        assert!(
            hooks
                .registry()
                .disable(&name("tenants"), "trapped three times")
        );
        let refused = hooks
            .module_route(&name("tenants"), "GET", "/tenants")
            .await;
        assert!(
            matches!(&refused, Err(Error::ModuleRequired(module)) if module == "tenants"),
            "{refused:?}"
        );
    }
}

mod module_route {
    use super::*;

    // TC: HOOK-03 (a route behind a session)
    #[tokio::test]
    async fn returns_session_route_when_declared_without_public() {
        let (hooks, _fake) = dispatcher(&["tenants"]);
        assert_eq!(
            hooks
                .module_route(&name("tenants"), "GET", "/tenants")
                .await
                .unwrap(),
            RouteSpec {
                method: "GET".to_owned(),
                path: "/tenants".to_owned(),
                public: false
            }
        );
    }

    // TC: HOOK-03
    #[tokio::test]
    async fn returns_public_route_when_declared_public() {
        let (hooks, _fake) = dispatcher(&["oidc"]);
        assert_eq!(
            hooks
                .module_route(&name("oidc"), "GET", "/login")
                .await
                .unwrap(),
            RouteSpec {
                method: "GET".to_owned(),
                path: "/login".to_owned(),
                public: true
            }
        );
    }

    // TC: HOOK-06
    #[tokio::test]
    async fn not_found_when_module_unknown() {
        let (hooks, _fake) = dispatcher(&["tenants"]);
        let refused = hooks.module_route(&name("nosuch"), "GET", "/x").await;
        assert!(
            matches!(&refused, Err(Error::NotFound(what)) if what.contains("nosuch")),
            "{refused:?}"
        );
    }

    // TC: HOOK-06 (a route the module does not declare)
    #[tokio::test]
    async fn not_found_when_route_undeclared() {
        let (hooks, _fake) = dispatcher(&["tenants"]);
        let refused = hooks
            .module_route(&name("tenants"), "DELETE", "/tenants")
            .await;
        assert!(matches!(&refused, Err(Error::NotFound(_))), "{refused:?}");
    }
}

mod handle_route {
    use super::*;

    // TC: HOOK-03 (the route runs as the actor)
    #[tokio::test]
    async fn hands_request_to_module_as_actor_when_called() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["tenants"]);
        let response = hooks
            .handle_route(
                &name("tenants"),
                Some(&world.owner),
                ModuleRequest {
                    method: "GET".to_owned(),
                    path: "/tenants".to_owned(),
                    headers: Vec::new(),
                    body: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (response.status, response.body, fake.calls()),
            (
                200,
                world.owner.id.to_string().into_bytes(),
                vec![ModuleCall::Route {
                    module: "tenants".to_owned(),
                    actor: Some(world.owner.id),
                    method: "GET".to_owned(),
                    path: "/tenants".to_owned()
                }]
            )
        );
    }

    // TC: HOOK-05 (a disabled module's route is not entered)
    #[tokio::test]
    async fn refuses_with_module_required_when_module_disabled() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["tenants"]);
        assert!(
            hooks
                .registry()
                .disable(&name("tenants"), "trapped three times")
        );
        let refused = hooks
            .handle_route(
                &name("tenants"),
                Some(&world.owner),
                ModuleRequest {
                    method: "GET".to_owned(),
                    path: "/tenants".to_owned(),
                    ..ModuleRequest::default()
                },
            )
            .await;
        assert!(
            matches!(&refused, Err(Error::ModuleRequired(_))) && fake.calls().is_empty(),
            "{refused:?}"
        );
    }
}

mod handle_tool {
    use super::*;

    /// The dispatcher over `tenants`, which declares the MCP tool
    /// `create`, and the fake behind it.
    fn with_tool() -> (ModuleHooks<FakeModules>, FakeModules) {
        let registry = Arc::new(ModuleRegistry::new());
        registry.register(
            name("tenants"),
            ModuleManifest::parse(
                r#"{"name":"tenants","version":"0.1.0",
                    "tools":[{"name":"create","description":"Create a tenant","schema":{"type":"object"}}]}"#,
            )
            .unwrap(),
        );
        let fake = FakeModules::new();
        (ModuleHooks::new(registry, fake.clone()), fake)
    }

    // TC: MCP-04 (a module's tool runs as the actor that calls it)
    #[tokio::test]
    async fn runs_tool_as_actor_when_module_declares_it() {
        let world = World::new().await;
        let (hooks, fake) = with_tool();
        fake.tool("tenants", "create", Ok(r#"{"slug":"acme"}"#.to_owned()));
        let answer = hooks
            .handle_tool(
                &name("tenants"),
                &world.owner,
                "create",
                r#"{"slug":"acme"}"#,
            )
            .await;
        assert_eq!(
            (answer.ok(), fake.calls()),
            (
                Some(r#"{"slug":"acme"}"#.to_owned()),
                vec![ModuleCall::Tool {
                    module: "tenants".to_owned(),
                    actor: Some(world.owner.id),
                    name: "create".to_owned(),
                    args: r#"{"slug":"acme"}"#.to_owned(),
                }]
            )
        );
    }

    // TC: MCP-04 (only the tools the manifest declares are called)
    #[tokio::test]
    async fn not_found_when_tool_undeclared() {
        let world = World::new().await;
        let (hooks, fake) = with_tool();
        let refused = hooks
            .handle_tool(&name("tenants"), &world.owner, "purge", "{}")
            .await;
        assert!(
            matches!(&refused, Err(Error::NotFound(what)) if what.contains("purge"))
                && fake.calls().is_empty(),
            "{refused:?}"
        );
    }

    /// The dispatcher over `tenants`, whose tool `create` takes a
    /// required string `slug` and nothing else.
    fn with_typed_tool() -> (ModuleHooks<FakeModules>, FakeModules) {
        let registry = Arc::new(ModuleRegistry::new());
        registry.register(
            name("tenants"),
            ModuleManifest::parse(
                r#"{"name":"tenants","version":"0.1.0",
                    "tools":[{"name":"create","description":"Create a tenant",
                              "schema":{"type":"object",
                                        "properties":{"slug":{"type":"string"}},
                                        "required":["slug"],
                                        "additionalProperties":false}}]}"#,
            )
            .unwrap(),
        );
        let fake = FakeModules::new();
        (ModuleHooks::new(registry, fake.clone()), fake)
    }

    // TC: MCP-04 (modules are untrusted: the core checks the
    // arguments against the tool's declared schema before the module
    // sees them)
    #[tokio::test]
    async fn validation_when_arguments_break_tool_schema() {
        let world = World::new().await;
        let (hooks, fake) = with_typed_tool();
        let mut refused = Vec::new();
        for args in [
            r#"{"slug":7}"#,
            "{}",
            r#"{"slug":"acme","admin":true}"#,
            "[]",
        ] {
            refused.push(
                hooks
                    .handle_tool(&name("tenants"), &world.owner, "create", args)
                    .await,
            );
        }
        assert!(
            refused
                .iter()
                .all(|refused| matches!(refused, Err(Error::Validation(_))))
                && fake.calls().is_empty(),
            "{refused:?}"
        );
    }

    // TC: MCP-04 (the refusal names the argument, as a config refusal
    // names its field)
    #[tokio::test]
    async fn names_argument_when_required_argument_missing() {
        let world = World::new().await;
        let (hooks, _fake) = with_typed_tool();
        let refused = hooks
            .handle_tool(&name("tenants"), &world.owner, "create", "{}")
            .await;
        assert!(
            matches!(&refused, Err(Error::Validation(error)) if error.to_string().contains("arguments.slug")),
            "{refused:?}"
        );
    }

    // TC: MCP-04 (conforming arguments pass)
    #[tokio::test]
    async fn calls_module_when_arguments_match_tool_schema() {
        let world = World::new().await;
        let (hooks, fake) = with_typed_tool();
        fake.tool("tenants", "create", Ok("{}".to_owned()));
        let answer = hooks
            .handle_tool(
                &name("tenants"),
                &world.owner,
                "create",
                r#"{"slug":"acme"}"#,
            )
            .await;
        assert_eq!(
            (answer.ok(), fake.calls().len()),
            (Some("{}".to_owned()), 1)
        );
    }

    // TC: HOOK-06 (a module that is not installed)
    #[tokio::test]
    async fn not_found_when_module_unknown() {
        let world = World::new().await;
        let (hooks, fake) = with_tool();
        let refused = hooks
            .handle_tool(&name("nosuch"), &world.owner, "create", "{}")
            .await;
        assert!(
            matches!(&refused, Err(Error::NotFound(what)) if what.contains("nosuch"))
                && fake.calls().is_empty(),
            "{refused:?}"
        );
    }

    // TC: HOOK-05 (a disabled module's tools are not entered)
    #[tokio::test]
    async fn refuses_with_module_required_when_module_disabled() {
        let world = World::new().await;
        let (hooks, fake) = with_tool();
        assert!(
            hooks
                .registry()
                .disable(&name("tenants"), "trapped three times")
        );
        let refused = hooks
            .handle_tool(&name("tenants"), &world.owner, "create", "{}")
            .await;
        assert!(
            matches!(&refused, Err(Error::ModuleRequired(_))) && fake.calls().is_empty(),
            "{refused:?}"
        );
    }
}

mod failing {
    use super::*;

    fn trapped() -> Error {
        Error::internal_msg("module tenants trapped: wasm trap: unreachable")
    }

    // TC: HOOK-01 (a deciding hook fails closed, so a
    // broken tenants module never opens a foreign project)
    #[tokio::test]
    async fn denies_when_module_fails_authorize() {
        let world = World::new().await;
        let (hooks, fake) = dispatcher(&["oidc", "tenants"]);
        fake.fail("tenants", trapped());
        assert_eq!(
            hooks
                .authorize(
                    &world.owner,
                    Action::ProjectGet,
                    &Resource::Project(world.shop.id)
                )
                .await,
            HookResult::Deny(failed_to_answer(&name("tenants")))
        );
    }

    // TC: HOOK-01 (a failing module does not refuse a
    // notification)
    #[tokio::test]
    async fn continues_when_module_fails_after_deploy() {
        let world = World::new().await;
        let deploy = world
            .records
            .deploy(&world.prod, 1, DeployStatus::Succeeded, &world.owner)
            .unwrap();
        let (hooks, fake) = dispatcher(&["tenants", "oidc"]);
        fake.fail("tenants", trapped());
        assert_eq!(
            (
                hooks.after_deploy(&world.prod, &deploy).await,
                fake.asked(HookName::AfterDeploy)
            ),
            (
                HookResult::Continue,
                vec!["tenants".to_owned(), "oidc".to_owned()]
            )
        );
    }

    // TC: HOOK-02 (a failing authenticate is skipped and
    // leaves the bearer to the core)
    #[tokio::test]
    async fn continues_when_module_fails_authenticate() {
        let (hooks, fake) = dispatcher(&["tenants"]);
        fake.fail("tenants", trapped());
        assert_eq!(
            hooks.authenticate(&request("/api/v1/me")).await,
            AuthResult::Continue
        );
    }

    // TC: HOOK-04 (a failing module's page is skipped
    // for the next one)
    #[tokio::test]
    async fn returns_next_html_when_first_module_fails_render_page() {
        let (hooks, fake) = dispatcher(&["tenants", "oidc"]);
        fake.fail("tenants", trapped());
        fake.page("oidc", PageKind::Login, "<h1>Sign in</h1>");
        assert_eq!(
            hooks.render_page(&PageRequest::of(PageKind::Login)).await,
            Some(ModulePage::Html("<h1>Sign in</h1>".to_owned()))
        );
    }
}

mod module_available {
    use super::*;

    // TC: HOOK-06 (`domain add --proxied` without the module)
    #[tokio::test]
    async fn not_found_when_module_unknown() {
        let (hooks, _fake) = dispatcher(&["tenants"]);
        let refused = hooks.module_available(&name("dns.cloudflare")).await;
        assert!(matches!(&refused, Err(Error::NotFound(_))), "{refused:?}");
    }

    // TC: HOOK-05
    #[tokio::test]
    async fn module_required_when_module_disabled() {
        let (hooks, _fake) = dispatcher(&["dns.cloudflare"]);
        assert!(
            hooks
                .registry()
                .disable(&name("dns.cloudflare"), "trapped three times")
        );
        let refused = hooks.module_available(&name("dns.cloudflare")).await;
        assert!(
            matches!(&refused, Err(Error::ModuleRequired(module)) if module == "dns.cloudflare"),
            "{refused:?}"
        );
    }

    // TC: HOOK-03
    #[tokio::test]
    async fn available_when_module_enabled() {
        let (hooks, fake) = dispatcher(&["dns.cloudflare"]);
        let answer = hooks.module_available(&name("dns.cloudflare")).await;
        assert!(
            answer.is_ok() && fake.calls().is_empty(),
            "{answer:?} {:?}",
            fake.calls()
        );
    }
}

mod mcp {
    use super::*;
    use crate::domain::Login;
    use crate::ports::Clock as _;

    /// The dispatcher over `modules` (name and manifest JSON), in order.
    fn over(modules: &[(&str, &str)]) -> (ModuleHooks<FakeModules>, FakeModules) {
        let registry = Arc::new(ModuleRegistry::new());
        for (module, manifest) in modules {
            registry.register(name(module), ModuleManifest::parse(manifest).unwrap());
        }
        let fake = FakeModules::new();
        (ModuleHooks::new(registry, fake.clone()), fake)
    }

    fn resource(url: &str) -> McpResource {
        McpResource {
            resource: url.to_owned(),
            audiences: vec!["liftoff-mcp".to_owned()],
        }
    }

    fn identity(world: &World) -> OAuthIdentity {
        OAuthIdentity {
            issuer: "https://id.example".to_owned(),
            subject: "z1".to_owned(),
            login: Login::new("bob").unwrap(),
            email: String::new(),
            role: UserRole::User,
            expires_at: world.ports.clock.now(),
        }
    }

    const MCP: &str = r#"{"name":"mcp","version":"0.1.0","hooks":["mcp_resource"]}"#;
    const OTHER: &str =
        r#"{"name":"web","version":"0.1.0","hooks":["mcp_resource","verify_token"]}"#;
    const OIDC: &str =
        r#"{"name":"oidc","version":"0.1.0","hooks":["verify_token"],"replaces":["login"]}"#;

    // TC: MCPH-02 (the resource is the `mcp` module's alone)
    #[tokio::test]
    async fn answers_mcp_module_resource_when_another_module_subscribes() {
        let (hooks, fake) = over(&[("web", OTHER), ("mcp", MCP)]);
        fake.keep_resource("web", resource("https://evil.example/mcp"));
        fake.keep_resource("mcp", resource("https://liftoff.test/mcp"));
        assert_eq!(
            hooks.mcp_resource().await.unwrap(),
            Some(resource("https://liftoff.test/mcp"))
        );
    }

    // TC: MCPH-04 (only the module that replaces login vouches for a token)
    #[tokio::test]
    async fn no_opinion_when_token_verifier_does_not_replace_login() {
        let world = World::new().await;
        let (hooks, fake) = over(&[("web", OTHER)]);
        fake.verify_as("web", Ok(Some(identity(&world))));
        assert_eq!(
            hooks
                .verify_token("at", &["liftoff-mcp".to_owned()])
                .await
                .unwrap(),
            None
        );
    }

    // TC: MCPH-04 (the login module's identity is the answer)
    #[tokio::test]
    async fn answers_identity_when_login_module_verifies() {
        let world = World::new().await;
        let (hooks, fake) = over(&[("web", OTHER), ("oidc", OIDC)]);
        fake.verify_as("oidc", Ok(Some(identity(&world))));
        assert_eq!(
            hooks
                .verify_token("at", &["liftoff-mcp".to_owned()])
                .await
                .unwrap(),
            Some(identity(&world))
        );
    }

    // TC: MCPH-03 (a refused token stays a refusal: 401, not 503)
    #[tokio::test]
    async fn unauthenticated_when_login_module_refuses() {
        let (hooks, fake) = over(&[("oidc", OIDC)]);
        fake.verify_as(
            "oidc",
            Err(Error::unauthenticated("the audience is not allowed")),
        );
        let verified = hooks.verify_token("at", &["liftoff-mcp".to_owned()]).await;
        assert!(
            matches!(verified, Err(Error::Unauthenticated(_))),
            "{verified:?}"
        );
    }

    // TC: MCPH-03 (a module that fails makes the check unavailable)
    #[tokio::test]
    async fn unavailable_when_login_module_fails() {
        let (hooks, fake) = over(&[("oidc", OIDC)]);
        fake.fail("oidc", Error::internal_msg("trapped"));
        let verified = hooks.verify_token("at", &["liftoff-mcp".to_owned()]).await;
        assert!(
            matches!(verified, Err(Error::Unavailable(_))),
            "{verified:?}"
        );
    }
}

mod web_panel {
    use super::*;
    use crate::domain::Origin;
    use crate::ports::WebPanel;

    fn over(modules: &[(&str, &str)]) -> (ModuleHooks<FakeModules>, FakeModules) {
        let registry = Arc::new(ModuleRegistry::new());
        for (module, manifest) in modules {
            registry.register(name(module), ModuleManifest::parse(manifest).unwrap());
        }
        let fake = FakeModules::new();
        (ModuleHooks::new(registry, fake.clone()), fake)
    }

    fn panel(origin: &str) -> WebPanel {
        WebPanel {
            origin: Origin::new(origin).unwrap(),
        }
    }

    const WEB: &str = r#"{"name":"web","version":"0.1.0","hooks":["web_panel"]}"#;
    const OTHER: &str = r#"{"name":"tenants","version":"0.1.0","hooks":["web_panel"]}"#;

    // TC: WEB-01 (the panel is the `web` module's alone: another module
    // opens no CORS)
    #[tokio::test]
    async fn answers_web_module_panel_when_another_module_subscribes() {
        let (hooks, fake) = over(&[("tenants", OTHER), ("web", WEB)]);
        fake.keep_panel("tenants", panel("https://evil.test"));
        fake.keep_panel("web", panel("https://panel.test"));
        assert_eq!(
            hooks.web_panel().await.unwrap(),
            Some(panel("https://panel.test"))
        );
    }

    // TC: WEB-01
    #[tokio::test]
    async fn no_panel_when_only_another_module_answers() {
        let (hooks, fake) = over(&[("tenants", OTHER)]);
        fake.keep_panel("tenants", panel("https://evil.test"));
        assert_eq!(hooks.web_panel().await.unwrap(), None);
    }

    // TC: WEB-01 (a module that fails makes the question unavailable)
    #[tokio::test]
    async fn unavailable_when_web_module_fails() {
        let (hooks, fake) = over(&[("web", WEB)]);
        fake.fail("web", Error::internal_msg("trapped"));
        let answer = hooks.web_panel().await;
        assert!(matches!(answer, Err(Error::Unavailable(_))), "{answer:?}");
    }
}

mod panel_pages {
    use super::*;
    use crate::domain::{Origin, WebUrl};
    use crate::ports::WebPanel;

    const WEB: &str = r#"{"name":"web","version":"0.1.0","hooks":["web_panel","render_page"]}"#;
    const OIDC: &str = r#"{"name":"oidc","version":"0.1.0","hooks":["render_page"]}"#;
    const OTHER: &str = r#"{"name":"tenants","version":"0.1.0","hooks":["render_page"]}"#;

    fn over(modules: &[(&str, &str)]) -> (ModuleHooks<FakeModules>, FakeModules) {
        let registry = Arc::new(ModuleRegistry::new());
        for (module, manifest) in modules {
            registry.register(name(module), ModuleManifest::parse(manifest).unwrap());
        }
        let fake = FakeModules::new();
        fake.keep_panel(
            "web",
            WebPanel {
                origin: Origin::new("https://panel.test").unwrap(),
            },
        );
        (ModuleHooks::new(registry, fake.clone()), fake)
    }

    fn url(raw: &str) -> WebUrl {
        WebUrl::new(raw).unwrap()
    }

    // TC: WEB-03 (with `take_pages` the panel's login wins over `oidc`'s
    // page, whichever module was installed first)
    #[tokio::test]
    async fn web_redirect_wins_when_oidc_installed_first() {
        let (hooks, fake) = over(&[("oidc", OIDC), ("web", WEB)]);
        fake.page("oidc", PageKind::Login, "<h1>Sign in</h1>");
        fake.page_redirect("web", PageKind::Login, url("https://panel.test/login"));
        assert_eq!(
            hooks.render_page(&PageRequest::of(PageKind::Login)).await,
            Some(ModulePage::Redirect(url("https://panel.test/login")))
        );
    }

    // TC: WEB-03 (without `take_pages`, `oidc`'s page as before)
    #[tokio::test]
    async fn oidc_page_when_web_has_no_opinion() {
        let (hooks, fake) = over(&[("web", WEB), ("oidc", OIDC)]);
        fake.page("oidc", PageKind::Login, "<h1>Sign in</h1>");
        assert_eq!(
            hooks.render_page(&PageRequest::of(PageKind::Login)).await,
            Some(ModulePage::Html("<h1>Sign in</h1>".to_owned()))
        );
    }

    // TC: WEB-03 (only `web` may send the browser away from the daemon)
    #[tokio::test]
    async fn ignores_redirect_when_module_is_not_web() {
        let (hooks, fake) = over(&[("tenants", OTHER), ("web", WEB)]);
        fake.page_redirect(
            "tenants",
            PageKind::Forbidden,
            url("https://panel.test/forbidden"),
        );
        assert_eq!(
            hooks
                .render_page(&PageRequest::of(PageKind::Forbidden))
                .await,
            None
        );
    }

    // TC: WEB-03 (`web`'s redirect goes to the panel it keeps, nowhere
    // else)
    #[tokio::test]
    async fn ignores_redirect_when_origin_is_not_panel() {
        let (hooks, fake) = over(&[("web", WEB)]);
        fake.page_redirect("web", PageKind::Login, url("https://evil.test/login"));
        assert_eq!(
            hooks.render_page(&PageRequest::of(PageKind::Login)).await,
            None
        );
    }

    // TC: WEB-03 (an ignored redirect leaves the next module's page)
    #[tokio::test]
    async fn next_module_page_when_redirect_ignored() {
        let (hooks, fake) = over(&[("web", WEB), ("oidc", OIDC)]);
        fake.page_redirect("web", PageKind::Login, url("https://evil.test/login"));
        fake.page("oidc", PageKind::Login, "<h1>Sign in</h1>");
        assert_eq!(
            hooks.render_page(&PageRequest::of(PageKind::Login)).await,
            Some(ModulePage::Html("<h1>Sign in</h1>".to_owned()))
        );
    }
}
