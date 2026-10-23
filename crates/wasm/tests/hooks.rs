#![cfg(test)]
//! The hook dispatcher over real components (HOOK-01…06): the core's `ModuleHooks` calls the built contract
//! components `allimports` (subscribed to `authorize` and
//! `before_deploy`, no opinion on either) and `denier` (a `tenants`-like
//! module, `modules/testing/denier`) through the host's `ModuleSet`.
//!
//! The components are built by `just modules` (part of `just test` and
//! `just ci`); `liftoff_wasm::testing::component` reads them from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.

use std::sync::Arc;

use liftoff_core::Error;
use liftoff_core::domain::{DeployStatus, ModuleName};
use liftoff_core::fakes::{InMemoryStore, World};
use liftoff_core::module::{ModuleCalls as _, ModuleHooks, RouteSpec};
use liftoff_core::ports::{
    Action, AuthResult, HookResult, Hooks as _, ModulePage, ModuleRequest, PageKind, PageRequest,
    Resource,
};
use liftoff_wasm::testing::{Host, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TokioTimer, TracingLog};

/// The reason `denier` gives for its foreign project.
const FOREIGN: &str = "project belongs to another tenant";

type Dispatcher = ModuleHooks<ModuleSet<InMemoryStore, TracingLog>>;

fn name(raw: &str) -> ModuleName {
    ModuleName::new(raw).unwrap()
}

/// `allimports` and then `denier` with `config` loaded into one host and
/// the dispatcher over them.
async fn dispatcher(config: &[(&str, &str)]) -> Dispatcher {
    let host: Host<InMemoryStore> =
        Host::new(InMemoryStore::new(), TokioTimer, TracingLog, limits()).unwrap();
    let set = ModuleSet::new();
    set.insert(
        host.load(
            name("allimports"),
            &component("allimports").unwrap(),
            ModuleConfig::new(),
        )
        .await
        .unwrap(),
    );
    let config: ModuleConfig = config
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    set.insert(
        host.load(name("denier"), &component("denier").unwrap(), config)
            .await
            .unwrap(),
    );
    ModuleHooks::new(Arc::clone(host.registry()), set)
}

fn get(path: &str) -> ModuleRequest {
    ModuleRequest {
        method: "GET".to_owned(),
        path: path.to_owned(),
        ..ModuleRequest::default()
    }
}

mod authorize {
    use super::*;

    // TC: HOOK-01 (`allimports` continues, the
    // tenants-like `denier` denies the foreign project)
    #[tokio::test(flavor = "multi_thread")]
    async fn denies_foreign_project_when_denier_follows_allimports() {
        let world = World::new().await;
        let shop = world.shop.id.to_string();
        let hooks = dispatcher(&[("foreign_project", &shop)]).await;
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
    #[tokio::test(flavor = "multi_thread")]
    async fn continues_when_resource_is_not_foreign() {
        let world = World::new().await;
        let shop = world.shop.id.to_string();
        let hooks = dispatcher(&[("foreign_project", &shop)]).await;
        assert_eq!(
            hooks
                .authorize(
                    &world.owner,
                    Action::EnvironmentGet,
                    &Resource::Environment(world.prod.id)
                )
                .await,
            HookResult::Continue
        );
    }
}

mod before_deploy {
    use super::*;

    // TC: HOOK-01 (the catalogue's case: `Continue` + `Deny` → `Deny`)
    #[tokio::test(flavor = "multi_thread")]
    async fn denies_when_denier_denies_after_allimports_continues() {
        let world = World::new().await;
        let deploy = world
            .records
            .deploy(&world.prod, 1, DeployStatus::Queued, &world.owner)
            .unwrap();
        let hooks = dispatcher(&[("deny_deploys", "tenant quota of 2 CPUs is used up")]).await;
        assert_eq!(
            hooks.before_deploy(&world.prod, &deploy).await,
            HookResult::Deny("tenant quota of 2 CPUs is used up".to_owned())
        );
    }

    // TC: HOOK-01
    #[tokio::test(flavor = "multi_thread")]
    async fn continues_when_no_module_denies() {
        let world = World::new().await;
        let deploy = world
            .records
            .deploy(&world.prod, 1, DeployStatus::Queued, &world.owner)
            .unwrap();
        let hooks = dispatcher(&[]).await;
        assert_eq!(
            hooks.before_deploy(&world.prod, &deploy).await,
            HookResult::Continue
        );
    }
}

mod authenticate {
    use super::*;
    use liftoff_core::ports::HookRequest;

    fn request() -> HookRequest {
        HookRequest {
            method: "GET".to_owned(),
            path: "/api/v1/me".to_owned(),
            headers: vec![("authorization".to_owned(), "Bearer oidc-token".to_owned())],
        }
    }

    // TC: HOOK-02
    #[tokio::test(flavor = "multi_thread")]
    async fn names_actor_when_denier_names_owner() {
        let world = World::new().await;
        let owner = world.owner.id.to_string();
        let hooks = dispatcher(&[("actor", &owner)]).await;
        assert_eq!(
            hooks.authenticate(&request()).await,
            AuthResult::Actor(world.owner.id)
        );
    }

    // TC: HOOK-02 (a `403` from `/.hook/authenticate` is a refusal with
    // its reason)
    #[tokio::test(flavor = "multi_thread")]
    async fn denies_when_denier_denies_authenticate() {
        let world = World::new().await;
        let owner = world.owner.id.to_string();
        let hooks = dispatcher(&[("actor", &owner), ("deny_auth", "token revoked")]).await;
        assert_eq!(
            hooks.authenticate(&request()).await,
            AuthResult::Deny("token revoked".to_owned())
        );
    }

    // TC: HOOK-02 (a `200` that names no user ID is the module's failure)
    #[tokio::test(flavor = "multi_thread")]
    async fn fails_when_denier_names_no_user_id() {
        let hooks = dispatcher(&[("actor", "carol")]).await;
        let failed = hooks
            .calls()
            .authenticate(&name("denier"), &request())
            .await;
        assert!(matches!(&failed, Err(Error::Internal(_))), "{failed:?}");
    }

    // TC: HOOK-02
    #[tokio::test(flavor = "multi_thread")]
    async fn continues_when_denier_names_nobody() {
        let hooks = dispatcher(&[]).await;
        assert_eq!(hooks.authenticate(&request()).await, AuthResult::Continue);
    }
}

mod render_page {
    use super::*;

    // TC: HOOK-04
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_denier_html_when_it_renders_login() {
        let hooks = dispatcher(&[("login_page", "<h1>Sign in to your tenant</h1>")]).await;
        assert_eq!(
            hooks.render_page(&PageRequest::of(PageKind::Login)).await,
            Some(ModulePage::Html(
                "<h1>Sign in to your tenant</h1>".to_owned()
            ))
        );
    }

    // TC: HOOK-04 (no module page: the core keeps its own)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_core_page_when_denier_has_no_page() {
        let hooks = dispatcher(&[("login_page", "<h1>Sign in to your tenant</h1>")]).await;
        assert_eq!(
            hooks
                .render_page(&PageRequest::of(PageKind::Forbidden))
                .await,
            None
        );
    }
}

mod disabled {
    use super::*;

    // TC: HOOK-05
    #[tokio::test(flavor = "multi_thread")]
    async fn continues_when_denier_disabled() {
        let world = World::new().await;
        let shop = world.shop.id.to_string();
        let hooks = dispatcher(&[("foreign_project", &shop)]).await;
        assert!(
            hooks
                .registry()
                .disable(&name("denier"), "trapped three times")
        );
        assert_eq!(
            hooks
                .authorize(
                    &world.owner,
                    Action::ProjectGet,
                    &Resource::Project(world.shop.id)
                )
                .await,
            HookResult::Continue
        );
    }

    // TC: HOOK-05
    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_route_with_module_required_when_denier_disabled() {
        let hooks = dispatcher(&[]).await;
        assert!(
            hooks
                .registry()
                .disable(&name("denier"), "trapped three times")
        );
        let refused = hooks.module_route(&name("denier"), "GET", "/whoami").await;
        assert!(
            matches!(&refused, Err(Error::ModuleRequired(module)) if module == "denier"),
            "{refused:?}"
        );
    }
}

mod routes {
    use super::*;

    // TC: HOOK-03
    #[tokio::test(flavor = "multi_thread")]
    async fn returns_public_route_when_denier_declares_it_public() {
        let hooks = dispatcher(&[]).await;
        assert_eq!(
            hooks
                .module_route(&name("denier"), "GET", "/hello")
                .await
                .unwrap(),
            RouteSpec {
                method: "GET".to_owned(),
                path: "/hello".to_owned(),
                public: true
            }
        );
    }

    // TC: HOOK-03
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_anonymous_when_public_route_runs_without_actor() {
        let hooks = dispatcher(&[]).await;
        let response = hooks
            .handle_route(&name("denier"), None, get("/hello"))
            .await
            .unwrap();
        assert_eq!(
            (response.status, response.body),
            (200, b"anonymous".to_vec())
        );
    }

    // TC: HOOK-03
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_actor_when_session_route_runs_as_owner() {
        let world = World::new().await;
        let hooks = dispatcher(&[]).await;
        let response = hooks
            .handle_route(&name("denier"), Some(&world.owner), get("/whoami"))
            .await
            .unwrap();
        assert_eq!(
            (response.status, response.body),
            (200, world.owner.id.to_string().into_bytes())
        );
    }

    // TC: HOOK-06
    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_when_module_unknown() {
        let hooks = dispatcher(&[]).await;
        let refused = hooks.module_route(&name("nosuch"), "GET", "/x").await;
        assert!(
            matches!(&refused, Err(Error::NotFound(what)) if what.contains("nosuch")),
            "{refused:?}"
        );
    }
}
