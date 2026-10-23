//! `tools/call`: the call becomes the REST request with the session of
//! the CLI config; an API error becomes a tool error with the problem's
//! `detail` (`MCP-03`).

use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

use crate::support::{Client, Seen, Stub, TOKEN, args, connect, json as answer, problem, tool_of};

/// Calls `name` with `arguments`; the result as JSON (`content`,
/// `isError`, ...).
async fn call(client: &Client, name: &str, arguments: Value) -> Value {
    let result = client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(arguments)))
        .await
        .unwrap();
    serde_json::to_value(result).unwrap()
}

/// The text of the first content block.
fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn project() -> Value {
    json!({
        "id": "01JA0000000000000000000001",
        "name": "shop",
        "git_host": "git.example.com",
        "repo": "alice/shop",
        "manifest": null,
        "created_by": "01JA0000000000000000000000",
        "created_at": "2026-10-20T10:00:00Z"
    })
}

fn only(calls: Vec<Seen>) -> Seen {
    assert_eq!(calls.len(), 1, "{calls:?}");
    calls.into_iter().next().unwrap()
}

mod translation {
    use super::*;

    // TC: MCP-03 (the session of the CLI config as the bearer)
    #[tokio::test]
    async fn sends_session_as_bearer_when_tool_called() {
        let stub = Stub::start()
            .await
            .route("GET", "/projects", answer(200, &json!([project()])));
        let client = connect(&stub).await;
        call(&client, "liftoff_projects_list", json!({})).await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str(), seen.authorization),
            ("GET", "/api/v1/projects", Some(format!("Bearer {TOKEN}")))
        );
    }

    // TC: MCP-03 (the answer of the API is the tool's result)
    #[tokio::test]
    async fn answers_api_json_when_call_succeeds() {
        let stub = Stub::start()
            .await
            .route("GET", "/projects", answer(200, &json!([project()])));
        let client = connect(&stub).await;
        let result = call(&client, "liftoff_projects_list", json!({})).await;
        assert_eq!(
            (
                result["isError"].as_bool().unwrap_or(false),
                serde_json::from_str::<Value>(&text(&result)).ok(),
            ),
            (false, Some(json!([project()])))
        );
    }

    // TC: MCP-03 (path parameters fill the path)
    #[tokio::test]
    async fn fills_path_when_tool_takes_path_parameters() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let name = tool_of("GET", "/api/v1/projects/{p}/environments/{e}");
        call(&client, &name, json!({"p": "shop", "e": "prod"})).await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str()),
            ("GET", "/api/v1/projects/shop/environments/prod")
        );
    }

    // TC: MCP-03 (query fields go to the query string)
    #[tokio::test]
    async fn sends_query_when_tool_takes_a_query() {
        let stub = Stub::start()
            .await
            .route("GET", "/secrets", answer(200, &json!([])));
        let client = connect(&stub).await;
        let name = tool_of("GET", "/api/v1/secrets");
        call(&client, &name, json!({"scope": "project", "id": "shop"})).await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str(), seen.query_pairs()),
            (
                "GET",
                "/api/v1/secrets",
                vec!["id=shop".to_owned(), "scope=project".to_owned()]
            )
        );
    }

    // TC: MCP-03 (body fields go to the JSON body, nothing else does)
    #[tokio::test]
    async fn sends_body_when_tool_takes_a_body() {
        let stub = Stub::start().await.route(
            "PUT",
            "/secrets",
            answer(
                200,
                &json!({
                    "key": "DATABASE_URL",
                    "scope": "project",
                    "version": 1,
                    "ci": false,
                    "updated_by": "01JA0000000000000000000000",
                    "updated_at": "2026-10-20T10:00:00Z"
                }),
            ),
        );
        let client = connect(&stub).await;
        let body = json!({
            "scope": "project",
            "id": "shop",
            "key": "DATABASE_URL",
            "value": "postgres://db/shop"
        });
        call(&client, "liftoff_secrets_set", body.clone()).await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str(), seen.json()),
            ("PUT", "/api/v1/secrets", body)
        );
    }

    // TC: MCP-03 (path parameters and body side by side: the path takes
    // its own, the body the rest)
    #[tokio::test]
    async fn splits_path_and_body_when_tool_takes_both() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let name = tool_of("POST", "/api/v1/projects/{p}/environments");
        call(
            &client,
            &name,
            json!({"p": "shop", "name": "staging", "branch": "develop"}),
        )
        .await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str(), seen.json()),
            (
                "POST",
                "/api/v1/projects/shop/environments",
                json!({"name": "staging", "branch": "develop"})
            )
        );
    }
}

mod arguments {
    use super::*;

    // TC: MCP-03 (an argument the operation has no place for is a tool
    // error, and no request is sent)
    #[tokio::test]
    async fn tool_error_without_request_when_argument_unknown() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let result = call(&client, "liftoff_projects_list", json!({"bogus": 1})).await;
        assert_eq!(
            (result["isError"].as_bool(), text(&result), stub.calls()),
            (
                Some(true),
                "unknown arguments: bogus".to_owned(),
                Vec::new()
            ),
            "{result}"
        );
    }

    // TC: MCP-03 (a missing path parameter is a tool error naming it, and
    // no request is sent)
    #[tokio::test]
    async fn tool_error_without_request_when_path_argument_missing() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let name = tool_of("GET", "/api/v1/projects/{p}");
        let result = call(&client, &name, json!({})).await;
        assert_eq!(
            (result["isError"].as_bool(), text(&result), stub.calls()),
            (Some(true), "missing argument p".to_owned(), Vec::new()),
            "{result}"
        );
    }

    // TC: MCP-03 (a path parameter is one segment: a `/` in it is
    // encoded, never a second segment)
    #[tokio::test]
    async fn slash_encoded_when_path_argument_has_one() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let name = tool_of("GET", "/api/v1/projects/{p}");
        call(&client, &name, json!({"p": "a/b"})).await;
        assert_eq!(only(stub.calls()).path, "/api/v1/projects/a%2Fb");
    }
}

mod api_errors {
    use super::*;

    // TC: MCP-03 (an API error is a tool error carrying `detail`)
    #[tokio::test]
    async fn tool_error_with_detail_when_api_answers_problem() {
        let stub = Stub::start().await.route(
            "GET",
            "/projects/ghost",
            problem(404, "project ghost not found"),
        );
        let client = connect(&stub).await;
        let name = tool_of("GET", "/api/v1/projects/{p}");
        let result = call(&client, &name, json!({"p": "ghost"})).await;
        assert_eq!(
            (
                result["isError"].as_bool(),
                text(&result).contains("project ghost not found"),
            ),
            (Some(true), true),
            "{result}"
        );
    }

    // TC: MCP-03 (the status travels with the detail, so an agent tells
    // "not found" from "forbidden")
    #[tokio::test]
    async fn tool_error_names_status_when_api_answers_problem() {
        let stub = Stub::start().await.route(
            "GET",
            "/projects",
            problem(403, "the session cannot list projects"),
        );
        let client = connect(&stub).await;
        let result = call(&client, "liftoff_projects_list", json!({})).await;
        assert_eq!(
            (result["isError"].as_bool(), text(&result).contains("403")),
            (Some(true), true),
            "{result}"
        );
    }
}

mod answer_size {
    use super::*;

    // TC: MCP-03 (an answer over 4 MiB is a tool error, never buffered
    // whole)
    #[tokio::test]
    async fn tool_error_when_answer_is_too_large() {
        let huge = "x".repeat((4 << 20) + 1);
        let stub = Stub::start()
            .await
            .route("GET", "/deploys/1/log", crate::support::text(&huge));
        let client = connect(&stub).await;
        let name = tool_of("GET", "/api/v1/deploys/{id}/log");
        let result = call(&client, &name, json!({"id": "1"})).await;
        assert_eq!(
            (
                result["isError"].as_bool(),
                text(&result).contains("larger than"),
            ),
            (Some(true), true),
            "{}",
            text(&result).chars().take(200).collect::<String>()
        );
    }
}

mod module_tools {
    use super::*;

    /// `GET /modules` with `tenants` (the tool `tenant_create`) and
    /// `dns.cloudflare` (the tool `zones_list`).
    fn modules() -> Value {
        let module = |name: &str, tool: &str| {
            json!({
                "name": name,
                "version": "0.1.0",
                "enabled": true,
                "error": null,
                "installed_at": "2026-10-20T10:00:00Z",
                "commands": [],
                "routes": [],
                "tools": [{"name": tool, "description": "A tool", "schema": {"type": "object"}}],
                "config_schema": null
            })
        };
        json!([
            module("tenants", "tenant_create"),
            module("dns.cloudflare", "zones_list")
        ])
    }

    // TC: MCP-04 (a module's tool runs through
    // `POST /modules/{name}/tools/{tool}` with the arguments and the
    // session; the module's answer is the result)
    #[tokio::test]
    async fn module_answer_when_module_tool_called() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", answer(200, &modules()))
            .route(
                "POST",
                "/modules/tenants/tools/tenant_create",
                answer(200, &json!({"result": {"slug": "acme", "id": 7}})),
            );
        let client = connect(&stub).await;
        let result = call(
            &client,
            "liftoff_tenants_tenant_create",
            json!({"slug": "acme"}),
        )
        .await;
        let seen = only(stub.calls());
        assert_eq!(
            (
                result["isError"].as_bool().unwrap_or(false),
                serde_json::from_str::<Value>(&text(&result)).ok(),
                seen.method.as_str(),
                seen.path.as_str(),
                seen.authorization.clone(),
                seen.json(),
            ),
            (
                false,
                Some(json!({"slug": "acme", "id": 7})),
                "POST",
                "/api/v1/modules/tenants/tools/tenant_create",
                Some(format!("Bearer {TOKEN}")),
                json!({"arguments": {"slug": "acme"}}),
            ),
            "{result}"
        );
    }

    // TC: MCP-04 (the module of `liftoff_dns_cloudflare_zones_list` is
    // `dns.cloudflare`, as `GET /modules` names it)
    #[tokio::test]
    async fn dotted_module_called_when_its_tool_called() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", answer(200, &modules()));
        let client = connect(&stub).await;
        call(&client, "liftoff_dns_cloudflare_zones_list", json!({})).await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str()),
            ("POST", "/api/v1/modules/dns.cloudflare/tools/zones_list")
        );
    }

    // TC: MCP-03 (the module's refusal is a tool error with its detail)
    #[tokio::test]
    async fn tool_error_with_detail_when_module_refuses() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", answer(200, &modules()))
            .route(
                "POST",
                "/modules/tenants/tools/tenant_create",
                problem(409, "tenant acme exists"),
            );
        let client = connect(&stub).await;
        let result = call(
            &client,
            "liftoff_tenants_tenant_create",
            json!({"slug": "acme"}),
        )
        .await;
        assert_eq!(
            (
                result["isError"].as_bool(),
                text(&result).contains("409") && text(&result).contains("tenant acme exists"),
            ),
            (Some(true), true),
            "{result}"
        );
    }

    // TC: MCP-04 (an API tool keeps its name: a call of it reaches the
    // API even when a module declares a tool mapping to the same name)
    #[tokio::test]
    async fn api_called_when_module_tool_takes_its_name() {
        let stub = Stub::start()
            .await
            .route(
                "GET",
                "/modules",
                answer(
                    200,
                    &json!([modules()[0].clone()].map(|mut module| {
                        module["name"] = json!("projects");
                        module["tools"][0]["name"] = json!("list");
                        module
                    })),
                ),
            )
            .route("GET", "/projects", answer(200, &json!([])));
        let client = connect(&stub).await;
        call(&client, "liftoff_projects_list", json!({})).await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str()),
            ("GET", "/api/v1/projects")
        );
    }

    // TC: MCP-04 (of two modules mapping to one tool name, the call goes
    // to the one that is listed: the first)
    #[tokio::test]
    async fn first_module_called_when_two_modules_map_to_one_name() {
        let mut clashing = modules();
        clashing[0]["name"] = json!("dns");
        clashing[0]["tools"][0]["name"] = json!("cloudflare_zones_list");
        let clashing = json!([clashing[1].clone(), clashing[0].clone()]);
        let stub = Stub::start()
            .await
            .route("GET", "/modules", answer(200, &clashing));
        let client = connect(&stub).await;
        call(&client, "liftoff_dns_cloudflare_zones_list", json!({})).await;
        assert_eq!(
            only(stub.calls()).path,
            "/api/v1/modules/dns.cloudflare/tools/zones_list"
        );
    }

    // TC: MCP-03 (when the modules cannot be read, a module tool's call
    // is a tool error carrying the API's refusal, not "no tool")
    #[tokio::test]
    async fn tool_error_with_status_when_modules_unreadable() {
        let stub =
            Stub::start()
                .await
                .route("GET", "/modules", problem(401, "the session expired"));
        let client = connect(&stub).await;
        let result = call(
            &client,
            "liftoff_tenants_tenant_create",
            json!({"slug": "acme"}),
        )
        .await;
        assert_eq!(
            (
                result["isError"].as_bool(),
                text(&result).contains("401") && text(&result).contains("the session expired"),
                stub.calls(),
            ),
            (Some(true), true, Vec::new()),
            "{result}"
        );
    }

    // TC: MCP-03 (a list of modules that is not JSON is a tool error of
    // its own)
    #[tokio::test]
    async fn tool_error_when_modules_not_json() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", crate::support::text("not json"));
        let client = connect(&stub).await;
        let result = call(&client, "liftoff_tenants_tenant_create", json!({})).await;
        assert_eq!(
            (
                result["isError"].as_bool(),
                text(&result).contains("modules"),
                text(&result).starts_with("no tool"),
            ),
            (Some(true), true, false),
            "{result}"
        );
    }

    // TC: MCP-03 (a name that is no tool is a tool error, not a request)
    #[tokio::test]
    async fn tool_error_when_tool_unknown() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let result = call(&client, "liftoff_ghosts_list", json!({})).await;
        assert_eq!(
            (result["isError"].as_bool(), text(&result), stub.calls()),
            (
                Some(true),
                "no tool liftoff_ghosts_list".to_owned(),
                Vec::new()
            ),
            "{result}"
        );
    }
}
