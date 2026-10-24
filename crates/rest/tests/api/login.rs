//! The browser's way in without OIDC (11): `GET /api/v1/auth/links/{token}` (`OTL-06`),
//! the session handover to the target's host by the gate's one-time code
//! (`OTL-07`, the API's half: the listener's half is in
//! `crates/liftoffd/tests/login.rs`), the login page without the `oidc`
//! module (`OTL-08`) and the module's page with it (`OTL-09`).
//!
//! The target is the private function `api` of `shop` at
//! `api.apps.test`; the world's API host is `liftoff.test`.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use http::{StatusCode, header};
use liftoff_core::domain::{
    EnvState, Environment, LinkTarget, ModuleName, SessionKind, SessionSecret, UserRole, Visibility,
};
use liftoff_core::fakes::{Call, FakeModules, World};
use liftoff_core::module::{HookName, ModuleHooks, ModuleManifest, ModuleRegistry};
use liftoff_core::ports::{HookResult, PageKind, Store as _};
use liftoff_rest::pages;

use crate::support::{Reply, WorldCtx};

/// The address of the function `api`.
const HOST: &str = "api.apps.test";

/// The cookie of the `browser` session on the API host.
const API_COOKIE: &str = "__Host-liftoff_session";

/// Where every link of the world points (its API host is `liftoff.test`).
const LINK_ORIGIN: &str = "https://liftoff.test";

/// Where a link sends the browser: the gate's callback on the target's
/// host with a one-time code (`lfc_…`).
const CALLBACK_PREFIX: &str = "https://api.apps.test/.liftoff/callback?code=lfc_";

/// Stores the running, private function `api` of `shop` at [`HOST`].
async fn private_api(world: &World) -> Environment {
    let api = Environment {
        visibility: Visibility::Private,
        state: EnvState::Running,
        ..world.records.function(&world.shop, "api").unwrap()
    };
    world.ports.store.insert_environment(&api).await.unwrap();
    world.add_address(&world.shop, &api, HOST).await.unwrap();
    api
}

/// A fresh one-time link of `owner` to `environment`: its full URL.
async fn link_to(world: &World, environment: &Environment) -> String {
    world
        .links()
        .issue(&world.owner, &LinkTarget::Environment(environment.id))
        .await
        .unwrap()
        .url
}

/// The path of `url` on the API host.
fn api_path(url: &str) -> String {
    url.strip_prefix(LINK_ORIGIN).unwrap_or(url).to_owned()
}

/// Percent-encodes everything but the unreserved characters.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// The values of `name` in `reply`.
fn header_values(reply: &Reply, name: &header::HeaderName) -> Vec<String> {
    reply
        .headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok().map(ToOwned::to_owned))
        .collect()
}

/// The `Location` of `reply`; empty without one.
fn location(reply: &Reply) -> String {
    header_values(reply, &header::LOCATION)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The `Set-Cookie` of `reply` that sets `name`: its value and its
/// attributes, sorted.
fn set_cookie(reply: &Reply, name: &str) -> Option<(String, Vec<String>)> {
    header_values(reply, &header::SET_COOKIE)
        .into_iter()
        .find_map(|line| {
            let mut parts = line.split(';').map(str::trim);
            let value = parts.next()?.strip_prefix(name)?.strip_prefix('=')?;
            let mut attributes: Vec<String> = parts.map(ToOwned::to_owned).collect();
            attributes.sort();
            Some((value.to_owned(), attributes))
        })
}

/// Whether `reply` carries the v1 headers of a page: `nosniff`,
/// a CSP that allows nothing by default, `no-store`.
fn has_page_headers(reply: &Reply) -> bool {
    let csp = header_values(reply, &header::CONTENT_SECURITY_POLICY);
    header_values(reply, &header::X_CONTENT_TYPE_OPTIONS) == ["nosniff"]
        && csp.len() == 1
        && csp.iter().all(|csp| csp.contains("default-src 'none'"))
        && header_values(reply, &header::CACHE_CONTROL) == ["no-store"]
}

/// Whether `reply` is a page of the daemon's own: the v1 headers with
/// the pages' CSP, which allows their one stylesheet ([`pages::CSP`]).
fn has_daemon_page_headers(reply: &Reply) -> bool {
    has_page_headers(reply)
        && header_values(reply, &header::CONTENT_SECURITY_POLICY) == [pages::CSP]
}

/// Whether `reply` is an HTML page.
fn is_html(reply: &Reply) -> bool {
    header_values(reply, &header::CONTENT_TYPE)
        .first()
        .is_some_and(|value| value.starts_with("text/html"))
}

mod link {
    use super::*;

    // TC: OTL-06 (the link redirects to its target, by the target's
    // host: the API's cookie is not seen there)
    #[tokio::test]
    async fn redirects_to_callback_on_target_host_when_link_is_fresh() {
        let ctx = WorldCtx::new().await;
        let api = private_api(&ctx.world).await;
        let url = link_to(&ctx.world, &api).await;
        let reply = ctx.call("GET", &api_path(&url), None, None).await;
        assert_eq!(
            (reply.status, location(&reply).starts_with(CALLBACK_PREFIX)),
            (StatusCode::FOUND, true),
            "{:?} {}",
            reply.headers,
            reply.text()
        );
    }

    // TC: OTL-06 (`HttpOnly; Secure; SameSite=Lax` on the API host; a `__Host-` cookie has `Path=/` and no `Domain`; it lives as long as the `browser` session, 24 hours)
    #[tokio::test]
    async fn sets_api_cookie_httponly_secure_lax_when_link_is_fresh() {
        let ctx = WorldCtx::new().await;
        let api = private_api(&ctx.world).await;
        let url = link_to(&ctx.world, &api).await;
        let reply = ctx.call("GET", &api_path(&url), None, None).await;
        let attributes = set_cookie(&reply, API_COOKIE).map(|(_value, attributes)| attributes);
        assert_eq!(
            attributes,
            Some(vec![
                "HttpOnly".to_owned(),
                "Max-Age=86400".to_owned(),
                "Path=/".to_owned(),
                "SameSite=Lax".to_owned(),
                "Secure".to_owned(),
            ]),
            "{:?}",
            reply.headers
        );
    }

    // TC: OTL-06 (the cookie is the `browser` session of the link's user)
    #[tokio::test]
    async fn cookie_is_browser_session_of_link_user_when_link_is_fresh() {
        let ctx = WorldCtx::new().await;
        let api = private_api(&ctx.world).await;
        let url = link_to(&ctx.world, &api).await;
        let reply = ctx.call("GET", &api_path(&url), None, None).await;
        let secret = set_cookie(&reply, API_COOKIE)
            .and_then(|(value, _attributes)| SessionSecret::parse(&value).ok());
        let stored = match secret {
            Some(secret) => ctx
                .world
                .ports
                .store
                .session_by_hash(&secret.hash())
                .await
                .unwrap()
                .map(|session| (session.user_id, session.kind)),
            None => None,
        };
        assert_eq!(
            stored,
            Some((ctx.world.owner.id, SessionKind::Browser)),
            "{:?}",
            reply.headers
        );
    }

    // TC: OTL-06 (OTL-02 at the route: the second use is a page, `410`,
    // and sets nothing)
    #[tokio::test]
    async fn gone_page_without_cookie_when_link_used_twice() {
        let ctx = WorldCtx::new().await;
        let api = private_api(&ctx.world).await;
        let url = link_to(&ctx.world, &api).await;
        let _first = ctx.call("GET", &api_path(&url), None, None).await;
        let reply = ctx.call("GET", &api_path(&url), None, None).await;
        assert_eq!(
            (
                reply.status,
                is_html(&reply),
                header_values(&reply, &header::SET_COOKIE)
            ),
            (StatusCode::GONE, true, Vec::<String>::new()),
            "{}",
            reply.text()
        );
    }

    // TC: OTL-06 (a token that is no link is as gone as a used one)
    #[tokio::test]
    async fn gone_when_token_is_malformed() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call("GET", "/api/v1/auth/links/not-a-link", None, None)
            .await;
        assert_eq!(reply.status, StatusCode::GONE, "{}", reply.text());
    }

    /// What a `fmt` subscriber wrote, shared with the test.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned()
        }
    }

    impl io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The route of a link as the request span logs it.
    const LINK_ROUTE: &str = "/api/v1/auth/links/{token}";

    // TC: OTL-06 (the link's token never reaches the log; the request's span names the route, not the path that carries it)
    #[tokio::test]
    async fn token_absent_from_log_when_link_opened() {
        let ctx = WorldCtx::new().await;
        let api = private_api(&ctx.world).await;
        let url = link_to(&ctx.world, &api).await;
        let token = url.rsplit('/').next().unwrap_or_default().to_owned();
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let status = {
            let _default = tracing::subscriber::set_default(subscriber);
            ctx.call("GET", &api_path(&url), None, None).await.status
        };
        let log = captured.text();
        assert_eq!(
            (
                status,
                token.starts_with("lfl_"),
                log.contains(&token),
                log.contains(LINK_ROUTE)
            ),
            (StatusCode::FOUND, true, false, true),
            "{log}"
        );
    }

    // TC: OTL-06 (the page of a link is served with the v1 headers)
    #[tokio::test]
    async fn page_has_v1_headers_when_link_is_gone() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call("GET", "/api/v1/auth/links/not-a-link", None, None)
            .await;
        assert!(has_daemon_page_headers(&reply), "{:?}", reply.headers);
    }
}

mod handover {
    use super::*;

    // TC: OTL-07 (`/gate/start` hands the browser session over to a
    // private function's address, protected or not: the listener's login
    // redirect goes there)
    #[tokio::test]
    async fn redirects_to_callback_with_code_when_private_address_and_browser_session() {
        let ctx = WorldCtx::new().await;
        let _api = private_api(&ctx.world).await;
        let session = ctx.world.browser_login(&ctx.world.owner).await.unwrap();
        let cookie = format!("{API_COOKIE}={}", session.expose());
        let reply = ctx
            .call_with(
                "GET",
                "/gate/start?host=api.apps.test&rd=%2Fitems",
                &[("cookie", &cookie)],
                None,
            )
            .await;
        assert_eq!(
            (reply.status, location(&reply).starts_with(CALLBACK_PREFIX)),
            (StatusCode::FOUND, true),
            "{:?} {}",
            reply.headers,
            reply.text()
        );
    }

    // TC: OTL-07, VIS-04 (a browser session of someone who may not open
    // the private address gets no code for it)
    #[tokio::test]
    async fn refused_when_private_address_and_strangers_browser_session() {
        let ctx = WorldCtx::new().await;
        let _api = private_api(&ctx.world).await;
        let bob = ctx.world.add_user("bob", UserRole::User).await.unwrap();
        let session = ctx.world.browser_login(&bob).await.unwrap();
        let cookie = format!("{API_COOKIE}={}", session.expose());
        ctx.world.log().clear();
        let reply = ctx
            .call_with(
                "GET",
                "/gate/start?host=api.apps.test&rd=%2Fitems",
                &[("cookie", &cookie)],
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                location(&reply),
                ctx.world
                    .log()
                    .calls()
                    .contains(&Call::Store("insert_gate_code"))
            ),
            (StatusCode::FORBIDDEN, String::new(), false),
            "{}",
            reply.text()
        );
    }

    // TC: OTL-07, VIS-05 (the owner of a tunnel opens its dev address
    // without being the admin)
    #[tokio::test]
    async fn redirects_to_callback_when_tunnel_owner_is_not_admin() {
        let ctx = WorldCtx::new().await;
        let world = &ctx.world;
        let bob = world.add_user("bob", UserRole::User).await.unwrap();
        let dev = Environment {
            state: EnvState::Running,
            ..world.records.tunnel(&world.shop, "shop-dev", &bob).unwrap()
        };
        world.ports.store.insert_environment(&dev).await.unwrap();
        world
            .add_address(&world.shop, &dev, "shop-dev.apps.test")
            .await
            .unwrap();
        let session = world.browser_login(&bob).await.unwrap();
        let cookie = format!("{API_COOKIE}={}", session.expose());
        let reply = ctx
            .call_with(
                "GET",
                "/gate/start?host=shop-dev.apps.test&rd=%2F",
                &[("cookie", &cookie)],
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                location(&reply)
                    .starts_with("https://shop-dev.apps.test/.liftoff/callback?code=lfc_")
            ),
            (StatusCode::FOUND, true),
            "{:?} {}",
            reply.headers,
            reply.text()
        );
    }

    // TC: OTL-07, VIS-04 (a shared address needs no session: `/gate/start`
    // knows no host of it, v1 SSO-04)
    #[tokio::test]
    async fn not_found_when_address_is_shared() {
        let ctx = WorldCtx::new().await;
        let world = &ctx.world;
        let api = Environment {
            visibility: Visibility::Shared,
            state: EnvState::Running,
            ..world.records.function(&world.shop, "api").unwrap()
        };
        world.ports.store.insert_environment(&api).await.unwrap();
        world.add_address(&world.shop, &api, HOST).await.unwrap();
        let session = world.browser_login(&world.owner).await.unwrap();
        let cookie = format!("{API_COOKIE}={}", session.expose());
        let reply = ctx
            .call_with(
                "GET",
                "/gate/start?host=api.apps.test&rd=%2F",
                &[("cookie", &cookie)],
                None,
            )
            .await;
        assert_eq!(
            (reply.status, location(&reply)),
            (StatusCode::NOT_FOUND, String::new()),
            "{}",
            reply.text()
        );
    }

    // TC: TEN-07 (on a protected host the module decides first: a `gate`
    // hook that denies the admin issues no code, as `/gate/check` refuses)
    #[tokio::test]
    async fn refused_without_code_when_gate_hook_denies_admin_on_protected_host() {
        let fake = FakeModules::new();
        let registry = Arc::new(ModuleRegistry::new());
        registry.register(
            ModuleName::new("tenants").unwrap(),
            ModuleManifest::parse(r#"{"name":"tenants","version":"0.1.0","hooks":["gate"]}"#)
                .unwrap(),
        );
        fake.answer(
            "tenants",
            HookName::Gate,
            HookResult::Deny("not a member".to_owned()),
        );
        let dispatcher = ModuleHooks::new(registry, fake.clone());
        let ctx = WorldCtx::with_hooks(|_| dispatcher).await;
        let world = &ctx.world;
        world
            .ports
            .store
            .set_environment_protected(world.prod.id, true)
            .await
            .unwrap();
        let session = world.browser_login(&world.owner).await.unwrap();
        let cookie = format!("{API_COOKIE}={}", session.expose());
        world.log().clear();
        let reply = ctx
            .call_with(
                "GET",
                "/gate/start?host=shop.apps.test&rd=%2F",
                &[("cookie", &cookie)],
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                location(&reply),
                world
                    .log()
                    .calls()
                    .contains(&Call::Store("insert_gate_code"))
            ),
            (StatusCode::FORBIDDEN, String::new(), false),
            "{}",
            reply.text()
        );
    }

    // TC: OTL-07 (the code travels in the query, the API's session never
    // does)
    #[tokio::test]
    async fn location_carries_no_api_session_when_link_redirects() {
        let ctx = WorldCtx::new().await;
        let api = private_api(&ctx.world).await;
        let url = link_to(&ctx.world, &api).await;
        let reply = ctx.call("GET", &api_path(&url), None, None).await;
        let session = set_cookie(&reply, API_COOKIE)
            .map_or_else(|| "lfs_".to_owned(), |(value, _attributes)| value);
        assert_eq!(
            (
                reply.status,
                location(&reply).contains(&session),
                location(&reply).contains("lfs_")
            ),
            (StatusCode::FOUND, false, false),
            "{:?}",
            reply.headers
        );
    }
}

mod page {
    use super::*;

    /// `GET /login` for the address `api.apps.test`, from `/items` there.
    const LOGIN: &str = "/login?host=api.apps.test&rd=%2Fitems";

    // TC: OTL-08 (a field to paste the link, and what to run for one)
    #[tokio::test]
    async fn offers_link_field_and_cli_hint_when_no_login_module() {
        let ctx = WorldCtx::new().await;
        let _api = private_api(&ctx.world).await;
        let reply = ctx.call("GET", LOGIN, None, None).await;
        let body = reply.text();
        assert_eq!(
            (
                reply.status,
                is_html(&reply),
                body.contains("name=\"link\""),
                body.contains("lof expose"),
                body.contains("lof artifact open"),
            ),
            (StatusCode::OK, true, true, true, true),
            "{body}"
        );
    }

    // TC: OTL-08 (embedded HTML without JavaScript)
    #[tokio::test]
    async fn has_no_script_when_rendered() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", LOGIN, None, None).await;
        let body = reply.text().to_ascii_lowercase();
        assert_eq!(
            (
                reply.status,
                body.contains("<form"),
                body.contains("<script"),
                ["onload=", "onclick=", "onsubmit="]
                    .iter()
                    .any(|handler| body.contains(handler)),
            ),
            (StatusCode::OK, true, false, false),
            "{body}"
        );
    }

    // TC: OTL-08 (served with the v1 headers: nosniff, CSP, no-store)
    #[tokio::test]
    async fn has_v1_headers_when_rendered() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", LOGIN, None, None).await;
        assert_eq!(
            (reply.status, has_daemon_page_headers(&reply)),
            (StatusCode::OK, true),
            "{:?}",
            reply.headers
        );
    }

    // TC: OTL-08 (the form keeps where the browser came from)
    #[tokio::test]
    async fn form_carries_host_and_return_path_when_asked_for_address() {
        let ctx = WorldCtx::new().await;
        let reply = ctx.call("GET", LOGIN, None, None).await;
        let body = reply.text();
        assert_eq!(
            (
                body.contains("value=\"api.apps.test\""),
                body.contains("value=\"/items\"")
            ),
            (true, true),
            "{body}"
        );
    }

    // TC: OTL-08 (what the page repeats of the query is escaped)
    #[tokio::test]
    async fn escapes_return_path_when_it_holds_markup() {
        let ctx = WorldCtx::new().await;
        let reply = ctx
            .call(
                "GET",
                "/login?host=api.apps.test&rd=%2F%22%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E",
                None,
                None,
            )
            .await;
        let body = reply.text();
        assert_eq!(
            (reply.status, body.contains("<script>alert(1)")),
            (StatusCode::OK, false),
            "{body}"
        );
    }

    // TC: OTL-08 (a browser without a session on a private address goes
    // to the login page, not to a login module that is not there)
    #[tokio::test]
    async fn gate_start_sends_browser_to_login_page_when_no_session_and_no_login_module() {
        let ctx = WorldCtx::new().await;
        let _api = private_api(&ctx.world).await;
        let reply = ctx
            .call(
                "GET",
                "/gate/start?host=api.apps.test&rd=%2Fitems",
                None,
                None,
            )
            .await;
        assert_eq!(
            (reply.status, location(&reply)),
            (
                StatusCode::FOUND,
                "https://liftoff.test/login?host=api.apps.test&rd=%2Fitems".to_owned()
            ),
            "{}",
            reply.text()
        );
    }

    // TC: OTL-08 (the pasted link opens the session and hands it over, as
    // the link itself does)
    #[tokio::test]
    async fn redirects_to_callback_with_api_cookie_when_pasted_link_is_for_address() {
        let ctx = WorldCtx::new().await;
        let api = private_api(&ctx.world).await;
        let url = link_to(&ctx.world, &api).await;
        let reply = ctx
            .call(
                "GET",
                &format!(
                    "/login?link={}&host=api.apps.test&rd=%2Fitems",
                    encode(&url)
                ),
                None,
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                location(&reply).starts_with(CALLBACK_PREFIX),
                set_cookie(&reply, API_COOKIE).is_some()
            ),
            (StatusCode::FOUND, true, true),
            "{:?} {}",
            reply.headers,
            reply.text()
        );
    }

    // TC: OTL-08 (OTL-03 on the page: a link of another target, pasted on
    // the login page of this address, is refused and stays unused)
    #[tokio::test]
    async fn forbidden_without_cookie_when_pasted_link_is_for_other_target() {
        let ctx = WorldCtx::new().await;
        let _api = private_api(&ctx.world).await;
        let tunnel = ctx
            .world
            .add_tunnel(&ctx.world.shop, "shop-dev", &ctx.world.owner)
            .await
            .unwrap();
        let url = link_to(&ctx.world, &tunnel).await;
        let reply = ctx
            .call(
                "GET",
                &format!("/login?link={}&host=api.apps.test&rd=%2F", encode(&url)),
                None,
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                is_html(&reply),
                header_values(&reply, &header::SET_COOKIE)
            ),
            (StatusCode::FORBIDDEN, true, Vec::<String>::new()),
            "{}",
            reply.text()
        );
    }
}

mod module_page {
    use super::*;

    /// What the `oidc`-like module renders as the login page.
    const MODULE_LOGIN: &str = "<!doctype html><title>Sign in</title><p>with the provider</p>";

    /// The API over the dispatcher of an installed `oidc`-like module
    /// that renders the login page, calling `fake`.
    async fn with_login_module(fake: &FakeModules) -> WorldCtx {
        let registry = Arc::new(ModuleRegistry::new());
        registry.register(
            ModuleName::new("oidc").unwrap(),
            ModuleManifest::parse(
                r#"{"name":"oidc","version":"0.1.0",
                    "routes":[{"method":"GET","path":"/login","public":true}],
                    "hooks":["authenticate","render_page"],
                    "replaces":["login"]}"#,
            )
            .unwrap(),
        );
        fake.page("oidc", PageKind::Login, MODULE_LOGIN);
        let dispatcher = ModuleHooks::new(registry, fake.clone());
        WorldCtx::with_hooks(|_| dispatcher).await
    }

    // TC: OTL-09 (with the module, the login page is the module's
    // `render_page(login)`)
    #[tokio::test]
    async fn serves_modules_page_when_module_renders_login() {
        let fake = FakeModules::new();
        let ctx = with_login_module(&fake).await;
        let reply = ctx
            .call("GET", "/login?host=api.apps.test&rd=%2F", None, None)
            .await;
        assert_eq!(
            (reply.status, is_html(&reply), reply.text()),
            (StatusCode::OK, true, MODULE_LOGIN.to_owned())
        );
    }

    // TC: OTL-09 (the module's page keeps the v1 headers)
    #[tokio::test]
    async fn has_v1_headers_when_module_renders_login() {
        let fake = FakeModules::new();
        let ctx = with_login_module(&fake).await;
        let reply = ctx.call("GET", "/login", None, None).await;
        assert_eq!(
            (reply.status, has_page_headers(&reply)),
            (StatusCode::OK, true),
            "{:?}",
            reply.headers
        );
    }
}
