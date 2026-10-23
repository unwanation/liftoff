//! `GET /git/templates`:
//! the catalogue of `lof create`, `TPL-03`…`TPL-06` through the router.

use http::StatusCode;
use liftoff_core::domain::{Host, RepoRef, TemplateSource};
use liftoff_core::fakes::World;
use liftoff_core::ports::RepoRole;
use liftoff_core::service::MAX_REQUESTED_TEMPLATES;
use serde_json::{Value, json};

use crate::support::{WorldCtx, is_problem};

const TEMPLATES: &str = "/api/v1/git/templates";

/// A template's `liftoff.toml` with `group` and `name`, `[service]`.
fn manifest(group: &str, name: &str) -> String {
    format!(
        "[template]\ngroup = \"{group}\"\nname = \"{name}\"\ndescription = \"{name} template\"\n\n[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n"
    )
}

/// A function template's `liftoff.toml`.
fn function_manifest(name: &str) -> String {
    format!(
        "[template]\ngroup = \"function\"\nname = \"{name}\"\ndescription = \"{name} template\"\n\n[function]\nbuild = \"make\"\ncomponent = \"dist/fn.wasm\"\n"
    )
}

fn repo(raw: &str) -> RepoRef {
    RepoRef::new(raw).unwrap()
}

/// `owner/name` on `git.example.com`.
fn on_forge(raw: &str) -> TemplateSource {
    TemplateSource {
        host: Some(Host::new(World::GIT_HOST).unwrap()),
        repo: repo(raw),
    }
}

/// Gives `raw` the `liftoff.toml` `toml`, readable by `owner`.
fn publish(ctx: &WorldCtx, raw: &str, toml: &str) {
    ctx.world.ports.git.template(&repo(raw), toml);
    ctx.world
        .ports
        .git
        .grant(&ctx.world.owner.login, &repo(raw), RepoRole::Read);
}

/// An entry of the answer, as `TemplateView` spells it.
fn entry(repo: &str, name: &str, kind: &str) -> Value {
    json!({
        "host": "git.example.com",
        "repo": repo,
        "name": name,
        "description": format!("{name} template"),
        "kind": kind
    })
}

/// The server of the plan's criterion: three templates on `git.example.com`.
async fn three() -> WorldCtx {
    let ctx = WorldCtx::with_templates(vec![
        on_forge("acme/app"),
        on_forge("acme/hono-api"),
        on_forge("acme/astro-site"),
    ])
    .await;
    publish(&ctx, "acme/app", &manifest("web", "acme-app"));
    publish(&ctx, "acme/hono-api", &manifest("api", "hono-api"));
    publish(&ctx, "acme/astro-site", &manifest("web", "astro-site"));
    ctx
}

mod list {
    use super::*;

    // TC: TPL-03
    #[tokio::test]
    async fn unauthorized_when_no_session() {
        let ctx = three().await;
        let reply = ctx.call("GET", TEMPLATES, None, None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-03
    #[tokio::test]
    async fn grouped_and_sorted_catalogue_when_owner_lists() {
        let ctx = three().await;
        let reply = ctx.owner("GET", TEMPLATES, None).await;
        assert_eq!(
            (reply.status, reply.json()),
            (
                StatusCode::OK,
                json!({
                    "groups": [
                        {
                            "group": "web",
                            "templates": [
                                entry("acme/app", "acme-app", "service"),
                                entry("acme/astro-site", "astro-site", "service")
                            ]
                        },
                        {
                            "group": "api",
                            "templates": [entry("acme/hono-api", "hono-api", "service")]
                        }
                    ],
                    "warnings": []
                })
            ),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-03 (`?group=`)
    #[tokio::test]
    async fn only_that_group_when_group_asked() {
        let ctx = three().await;
        let reply = ctx
            .owner("GET", &format!("{TEMPLATES}?group=api"), None)
            .await;
        assert_eq!(
            (reply.status, reply.json()["groups"].clone()),
            (
                StatusCode::OK,
                json!([{
                    "group": "api",
                    "templates": [entry("acme/hono-api", "hono-api", "service")]
                }])
            ),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-03 (the group is validated at the boundary)
    #[tokio::test]
    async fn bad_request_when_group_unknown() {
        let ctx = three().await;
        let reply = ctx
            .owner("GET", &format!("{TEMPLATES}?group=cli"), None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-03 (`deny_unknown_fields`)
    #[tokio::test]
    async fn bad_request_when_query_has_unknown_field() {
        let ctx = three().await;
        let reply = ctx.owner("GET", &format!("{TEMPLATES}?sudo=1"), None).await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-04
    #[tokio::test]
    async fn warning_listed_and_rest_returned_when_a_repo_has_no_manifest() {
        let ctx =
            WorldCtx::with_templates(vec![on_forge("acme/app"), on_forge("acme/plain")]).await;
        publish(&ctx, "acme/app", &manifest("web", "acme-app"));
        ctx.world
            .ports
            .git
            .grant(&ctx.world.owner.login, &repo("acme/plain"), RepoRole::Read);
        let reply = ctx.owner("GET", TEMPLATES, None).await;
        let body = reply.json();
        let warned: Vec<Value> = body["warnings"]
            .as_array()
            .map(|items| items.iter().map(|w| w["repo"].clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            (reply.status, body["groups"].clone(), warned),
            (
                StatusCode::OK,
                json!([{
                    "group": "web",
                    "templates": [entry("acme/app", "acme-app", "service")]
                }]),
                vec![json!("git.example.com/acme/plain")]
            ),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-06 (`?repos=`: the CLI's `[local] templates` next to the
    // server's)
    #[tokio::test]
    async fn request_repos_added_when_named() {
        let ctx = WorldCtx::with_templates(vec![on_forge("acme/app")]).await;
        publish(&ctx, "acme/app", &manifest("web", "acme-app"));
        publish(&ctx, "me/my-api", &manifest("api", "my-api"));
        publish(&ctx, "me/my-fn", &function_manifest("my-fn"));
        let reply = ctx
            .owner(
                "GET",
                &format!("{TEMPLATES}?repos=me/my-api,git.example.com/me/my-fn"),
                None,
            )
            .await;
        assert_eq!(
            (reply.status, reply.json()["groups"].clone()),
            (
                StatusCode::OK,
                json!([
                    {"group": "web", "templates": [entry("acme/app", "acme-app", "service")]},
                    {"group": "api", "templates": [entry("me/my-api", "my-api", "service")]},
                    {"group": "function", "templates": [entry("me/my-fn", "my-fn", "function")]}
                ])
            ),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-06 (each requested repo is validated at the boundary)
    #[tokio::test]
    async fn bad_request_when_a_requested_repo_is_invalid() {
        let ctx = three().await;
        let reply = ctx
            .owner("GET", &format!("{TEMPLATES}?repos=me/my-api,oops"), None)
            .await;
        assert_eq!(
            (reply.status, is_problem(&reply)),
            (StatusCode::BAD_REQUEST, true),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-06 (`?repos=` names at most `MAX_REQUESTED_TEMPLATES`
    // repositories)
    #[tokio::test]
    async fn bad_request_on_repos_when_too_many_requested() {
        let ctx = three().await;
        let many: Vec<String> = (0..=MAX_REQUESTED_TEMPLATES)
            .map(|index| format!("me/tpl-{index}"))
            .collect();
        let reply = ctx
            .owner(
                "GET",
                &format!("{TEMPLATES}?repos={}", many.join(",")),
                None,
            )
            .await;
        assert_eq!(
            (
                reply.status,
                is_problem(&reply),
                reply.text().contains("repos")
            ),
            (StatusCode::BAD_REQUEST, true, true),
            "{}",
            reply.text()
        );
    }

    // TC: TPL-05 (the second call within ten minutes does not ask the
    // forge)
    #[tokio::test]
    async fn forge_asked_once_when_listed_twice() {
        let ctx = three().await;
        let _first = ctx.owner("GET", TEMPLATES, None).await;
        let second = ctx.owner("GET", TEMPLATES, None).await;
        let asked = ctx
            .world
            .ports
            .git
            .calls()
            .into_iter()
            .filter(|call| matches!(call, liftoff_core::fakes::GitCall::ListTemplates { .. }))
            .count();
        assert_eq!(
            (second.status, asked),
            (StatusCode::OK, 1),
            "{}",
            second.text()
        );
    }
}

mod document {
    use super::*;

    // TC: TPL-03 (the route is in `/openapi.json` with its query, so MCP gets the same operation)
    #[tokio::test]
    async fn documented_with_group_and_repos_when_generated() {
        let ctx = WorldCtx::new().await;
        let doc = ctx.call("GET", "/openapi.json", None, None).await.json();
        let mut names: Vec<String> = doc["paths"][TEMPLATES]["get"]["parameters"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item["in"] == "query")
                    .filter_map(|item| item["name"].as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        assert_eq!(
            (
                names,
                doc["components"]["schemas"]["TemplateCatalogView"].is_object()
            ),
            (vec!["group".to_owned(), "repos".to_owned()], true)
        );
    }
}
