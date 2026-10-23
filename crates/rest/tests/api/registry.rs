//! `/modules*` (REG-01…07) over
//! the world: its fake registry serves the fake `probe` signed with the
//! key the world pins, and its fake module runtime loads it.

use http::StatusCode;
use liftoff_core::fakes::{Call, FakeRelease, RuntimeCall};
use serde_json::{Value, json};

use crate::support::{WorldCtx, detail, is_problem, strings};

/// The world with `releases` published and the log cleared.
async fn ctx_with(releases: Vec<FakeRelease>) -> WorldCtx {
    let ctx = WorldCtx::new().await;
    for release in releases {
        ctx.world.ports.registry.publish(release).unwrap();
    }
    ctx.world.log().clear();
    ctx
}

/// The world with the fake `probe` published.
async fn ctx() -> WorldCtx {
    ctx_with(vec![FakeRelease::probe()]).await
}

/// `POST /modules` of `probe` with `config`.
fn install(config: &Value) -> Value {
    json!({"name": "probe", "config": config})
}

/// The world with `probe` installed (`greeting = "hi"`, `token =
/// "s3cret"`) and the log cleared.
async fn installed() -> WorldCtx {
    let ctx = ctx().await;
    let reply = ctx
        .owner(
            "POST",
            "/api/v1/modules",
            Some(&install(&json!({"greeting": "hi", "token": "s3cret"}))),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    ctx.world.log().clear();
    ctx
}

/// A release with nothing but a name, a version and `requires`.
fn bare(module: &str, requires: &[&str]) -> FakeRelease {
    FakeRelease::of_manifest(
        &json!({"name": module, "version": "0.1.0", "requires": requires}).to_string(),
    )
    .unwrap()
}

/// The `hello` command of `probe`, as `GET /modules` shows it.
fn hello() -> Value {
    json!({
        "name": "hello",
        "description": "Says hello",
        "args": [{"name": "who", "type": "string", "required": true, "hint": "Who to greet"}]
    })
}

fn purged(ctx: &WorldCtx) -> bool {
    ctx.world
        .log()
        .calls()
        .contains(&Call::Runtime(RuntimeCall::Purge("probe".to_owned())))
}

mod install {
    use super::*;

    // TC: REG-06 (the version names the kept file, so anything but semver
    // is refused at the boundary, before the registry is read)
    #[tokio::test]
    async fn bad_request_naming_version_when_version_is_not_semver() {
        let ctx = ctx().await;
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/modules",
                Some(&json!({"name": "probe", "version": "../../x", "config": {"greeting": "hi"}})),
            )
            .await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).starts_with("version"),
                ctx.world
                    .log()
                    .calls()
                    .iter()
                    .any(|call| matches!(call, Call::Registry(_)))
            ),
            (StatusCode::BAD_REQUEST, true, false),
            "{}",
            reply.text()
        );
    }

    // TC: REG-01
    #[tokio::test]
    async fn bad_gateway_when_signature_is_by_another_key() {
        let ctx = ctx_with(vec![FakeRelease::probe().signed_by_another_key()]).await;
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/modules",
                Some(&install(&json!({"greeting": "hi"}))),
            )
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_GATEWAY, true),
            "{}",
            reply.text()
        );
    }

    // TC: REG-02
    #[tokio::test]
    async fn conflict_naming_both_worlds_when_wit_version_differs() {
        let ctx = ctx_with(vec![FakeRelease::probe().wit_version("0.2.0")]).await;
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/modules",
                Some(&json!({"name": "probe", "version": "0.1.0", "config": {"greeting": "hi"}})),
            )
            .await;
        let detail = detail(&reply);
        assert_eq!(
            (
                reply.status,
                detail.contains("liftoff:module@0.2.0") && detail.contains("liftoff:module@0.1.0")
            ),
            (StatusCode::CONFLICT, true),
            "{}",
            reply.text()
        );
    }

    // TC: REG-03
    #[tokio::test]
    async fn failed_dependency_naming_oidc_when_oidc_is_not_installed() {
        let ctx = ctx_with(vec![bare("oidc", &[]), bare("tenants", &["oidc"])]).await;
        let reply = ctx
            .owner("POST", "/api/v1/modules", Some(&json!({"name": "tenants"})))
            .await;
        assert_eq!(
            (reply.status, detail(&reply)),
            (
                StatusCode::FAILED_DEPENDENCY,
                "install the module `oidc`".to_owned()
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REG-04
    #[tokio::test]
    async fn bad_request_naming_field_when_config_breaks_schema() {
        let ctx = ctx().await;
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/modules",
                Some(&install(&json!({"greeting": 42}))),
            )
            .await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).starts_with("config.greeting: ")
            ),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: REG-05 (a secret is in no answer)
    #[tokio::test]
    async fn answers_without_secret_value_when_installed() {
        let ctx = ctx().await;
        let created = ctx
            .owner(
                "POST",
                "/api/v1/modules",
                Some(&install(&json!({"greeting": "hi", "token": "s3cret"}))),
            )
            .await;
        let listed = ctx.owner("GET", "/api/v1/modules", None).await;
        assert_eq!(
            (
                created.status,
                created.text().contains("s3cret"),
                listed.status,
                listed.text().contains("s3cret")
            ),
            (StatusCode::CREATED, false, StatusCode::OK, false),
            "{} {}",
            created.text(),
            listed.text()
        );
    }

    // TC: REG-06
    #[tokio::test]
    async fn created_with_its_command_when_installed() {
        let ctx = ctx().await;
        let reply = ctx
            .owner(
                "POST",
                "/api/v1/modules",
                Some(&install(&json!({"greeting": "hi"}))),
            )
            .await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                body["name"].clone(),
                body["version"].clone(),
                body["enabled"].clone(),
                body["commands"].clone()
            ),
            (
                StatusCode::CREATED,
                json!("probe"),
                json!("0.1.0"),
                json!(true),
                json!([hello()])
            ),
            "{}",
            reply.text()
        );
    }

    // TC: MOD-05 (installing is the admin's: no module answers)
    #[tokio::test]
    async fn forbidden_when_carol_installs() {
        let ctx = ctx().await;
        let reply = ctx
            .carol(
                "POST",
                "/api/v1/modules",
                Some(&install(&json!({"greeting": "hi"}))),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    }
}

mod list {
    use super::*;

    // TC: GIT-01 (the CLI chooses the device flow when `GET /modules` reports that the forge module has an OAuth application, else it asks for a token)
    #[tokio::test]
    async fn shows_git_provider_of_forge_module_when_listed() {
        let release = FakeRelease::of_manifest(
            &json!({
                "name": "git.github",
                "version": "0.1.0",
                "git_provider": {"hosts": ["github.com"], "oauth": true},
            })
            .to_string(),
        )
        .unwrap();
        let ctx = ctx_with(vec![release]).await;
        let installed = ctx
            .owner(
                "POST",
                "/api/v1/modules",
                Some(&json!({"name": "git.github", "config": {}})),
            )
            .await;
        assert_eq!(
            installed.status,
            StatusCode::CREATED,
            "{}",
            installed.text()
        );
        let reply = ctx.owner("GET", "/api/v1/modules", None).await;
        assert_eq!(
            (reply.status, reply.json()[0]["git_provider"].clone()),
            (
                StatusCode::OK,
                json!({"hosts": ["github.com"], "oauth": true})
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REG-06 (the command is there at once, without a restart)
    #[tokio::test]
    async fn shows_command_of_module_right_after_install() {
        let ctx = installed().await;
        let reply = ctx.owner("GET", "/api/v1/modules", None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                strings(&body, "name"),
                body[0]["commands"].clone(),
                body[0]["routes"].clone()
            ),
            (
                StatusCode::OK,
                vec!["probe".to_owned()],
                json!([hello()]),
                json!([{"method": "POST", "path": "/hello", "public": false}])
            ),
            "{}",
            reply.text()
        );
    }

    // TC: REG-04 (the CLI asks for the configuration
    // from the schema of the release before it installs)
    #[tokio::test]
    async fn lists_release_with_its_schema_when_available_asked() {
        let ctx = ctx().await;
        let reply = ctx.owner("GET", "/api/v1/modules/available", None).await;
        let body = reply.json();
        assert_eq!(
            (
                reply.status,
                strings(&body, "name"),
                body[0]["releases"][0]["version"].clone(),
                body[0]["releases"][0]["config_schema"]["required"].clone()
            ),
            (
                StatusCode::OK,
                vec!["probe".to_owned()],
                json!("0.1.0"),
                json!(["greeting"])
            ),
            "{}",
            reply.text()
        );
    }
}

mod patch {
    use super::*;

    // TC: REG-04 (`lof module config` is checked like the install)
    #[tokio::test]
    async fn bad_request_naming_field_when_new_config_breaks_schema() {
        let ctx = installed().await;
        let reply = ctx
            .owner(
                "PATCH",
                "/api/v1/modules/probe",
                Some(&json!({"config": {"greeting": 42}})),
            )
            .await;
        assert_eq!(
            (
                reply.status,
                detail(&reply).starts_with("config.greeting: ")
            ),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: MOD-09 (`lof module enable` after a fix)
    #[tokio::test]
    async fn enables_module_when_patched_enabled() {
        let ctx = installed().await;
        let _disabled = ctx.world.ports.modules.shared_registry().disable(
            &liftoff_core::domain::ModuleName::new("probe").unwrap(),
            "trapped",
        );
        let reply = ctx
            .owner(
                "PATCH",
                "/api/v1/modules/probe",
                Some(&json!({"enabled": true})),
            )
            .await;
        assert_eq!(
            (
                reply.status,
                reply.json()["enabled"].clone(),
                reply.json()["error"].clone()
            ),
            (StatusCode::OK, json!(true), Value::Null),
            "{}",
            reply.text()
        );
    }
}

mod delete {
    use super::*;

    // TC: REG-07
    #[tokio::test]
    async fn keeps_tables_when_deleted_without_purge() {
        let ctx = installed().await;
        let reply = ctx.owner("DELETE", "/api/v1/modules/probe", None).await;
        let listed = ctx.owner("GET", "/api/v1/modules", None).await;
        assert_eq!(
            (reply.status, purged(&ctx), listed.json()),
            (StatusCode::OK, false, json!([])),
            "{}",
            reply.text()
        );
    }

    // TC: REG-07
    #[tokio::test]
    async fn drops_tables_when_deleted_with_purge() {
        let ctx = installed().await;
        let reply = ctx
            .owner("DELETE", "/api/v1/modules/probe?purge=true", None)
            .await;
        assert_eq!(
            (reply.status, purged(&ctx)),
            (StatusCode::OK, true),
            "{}",
            reply.text()
        );
    }
}

mod openapi {
    use super::*;

    // TC: REG-06 (design: the routes of `/modules` are documented
    // with their types)
    #[tokio::test]
    async fn documents_module_routes_when_generated() {
        let ctx = ctx().await;
        let doc = ctx.owner("GET", "/api/v1/openapi.json", None).await.json();
        let paths = &doc["paths"];
        assert!(
            paths["/api/v1/modules"]["get"].is_object()
                && paths["/api/v1/modules"]["post"].is_object()
                && paths["/api/v1/modules/available"]["get"].is_object()
                && paths["/api/v1/modules/{name}"]["patch"].is_object()
                && paths["/api/v1/modules/{name}"]["delete"].is_object()
                && doc["components"]["schemas"]["ModuleView"].is_object(),
            "{paths}"
        );
    }
}
