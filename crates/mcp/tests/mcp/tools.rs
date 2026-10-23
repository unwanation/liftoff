//! `tools/list`: one tool per operation of `liftoff-api` (`MCP-01`), its
//! input schema from the same types as `/openapi.json` (`MCP-02`), and
//! the tools of the installed modules (`MCP-04`).

use std::collections::BTreeSet;

use liftoff_api::OPERATIONS;
use serde_json::{Value, json};

use crate::support::{Client, Stub, connect, json as answer};

/// Every tool `tools/list` returns, as JSON (`name`, `description`,
/// `inputSchema`, ...).
async fn listed(client: &Client) -> Vec<Value> {
    client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| serde_json::to_value(tool).unwrap())
        .collect()
}

fn names(tools: &[Value]) -> BTreeSet<String> {
    tools
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

fn tool<'t>(tools: &'t [Value], name: &str) -> &'t Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("no tool {name} in {:?}", names(tools)))
}

/// The property names of a JSON Schema object.
fn properties(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

/// The `required` names of a JSON Schema object.
fn required(schema: &Value) -> BTreeSet<String> {
    schema["required"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

mod api_tools {
    use super::*;

    // TC: MCP-01 (every `liftoff_*` tool is an operation's; the aliases
    // of the standalone artifacts server, MCPA-01, are named otherwise)
    #[tokio::test]
    async fn every_operation_listed_when_no_module_installed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let expected: BTreeSet<String> = OPERATIONS
            .iter()
            .map(|operation| format!("liftoff_{}", operation.name))
            .collect();
        let prefixed: BTreeSet<String> = names(&listed(&client).await)
            .into_iter()
            .filter(|name| name.starts_with("liftoff_"))
            .collect();
        assert_eq!((expected.is_empty(), prefixed), (false, expected));
    }

    // TC: MCP-01 (the documented names)
    #[tokio::test]
    async fn spec_names_listed_when_started() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let names = names(&listed(&client).await);
        let missing: Vec<&str> = [
            "liftoff_projects_list",
            "liftoff_deploys_create",
            "liftoff_secrets_set",
        ]
        .into_iter()
        .filter(|name| !names.contains(*name))
        .collect();
        assert_eq!(missing, Vec::<&str>::new());
    }

    // TC: MCP-01 (the summary of the operation describes its tool)
    #[tokio::test]
    async fn described_by_summary_when_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        assert_eq!(
            tool(&tools, "liftoff_projects_list")["description"],
            json!("Every project")
        );
    }
}

mod input_schema {
    use super::*;

    /// What the input schema of `operation`'s tool must hold: its path
    /// parameters (required strings), its query fields and its body
    /// fields, each required as its own schema says.
    fn expected(operation: &liftoff_api::Operation) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut props: BTreeSet<String> =
            operation.path_parameters().map(ToOwned::to_owned).collect();
        let mut needed = props.clone();
        let parts = operation
            .query
            .map(|query| (query.json_schema)())
            .into_iter()
            .chain(operation.request.map(|request| (request.json_schema)()));
        for schema in parts {
            props.extend(properties(&schema));
            needed.extend(required(&schema));
        }
        (props, needed)
    }

    // TC: MCP-02 (the arguments are the path parameters, the query and the
    // body of the request, from the `schemars` derives `/openapi.json`'s
    // `ToSchema` derives sit beside)
    #[tokio::test]
    async fn matches_request_when_operation_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let wrong: Vec<String> = OPERATIONS
            .iter()
            .filter_map(|operation| {
                let name = format!("liftoff_{}", operation.name);
                let schema = &tool(&tools, &name)["inputSchema"];
                let got = (properties(schema), required(schema));
                let want = expected(operation);
                (got != want).then(|| format!("{name}: {got:?}, expected {want:?}"))
            })
            .collect();
        assert_eq!(
            (OPERATIONS.is_empty(), wrong),
            (false, Vec::<String>::new())
        );
    }

    // TC: MCP-02 (each argument has the very schema of its field in the
    // query or the body, types and enums included, not just its name)
    #[tokio::test]
    async fn field_schemas_match_request_when_operation_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let mut wrong = Vec::new();
        for operation in OPERATIONS {
            let name = format!("liftoff_{}", operation.name);
            let schema = &tool(&tools, &name)["inputSchema"];
            let parts = operation
                .query
                .map(|query| (query.json_schema)())
                .into_iter()
                .chain(operation.request.map(|request| (request.json_schema)()));
            for part in parts {
                for (field, want) in part["properties"].as_object().into_iter().flatten() {
                    let got = &schema["properties"][field];
                    if got != want {
                        wrong.push(format!("{name}.{field}: {got}, expected {want}"));
                    }
                }
            }
        }
        assert_eq!(wrong, Vec::<String>::new());
    }

    // TC: MCP-02 (the arguments are one flat object: no name of a path
    // parameter, a query field and a body field may stand for two of
    // them, or one would overwrite the other)
    #[test]
    fn argument_names_disjoint_when_operation_has_parts() {
        let clashing: Vec<String> = OPERATIONS
            .iter()
            .filter_map(|operation| {
                let path: BTreeSet<String> =
                    operation.path_parameters().map(ToOwned::to_owned).collect();
                let query = operation
                    .query
                    .map(|query| properties(&(query.json_schema)()))
                    .unwrap_or_default();
                let body = operation
                    .request
                    .map(|request| properties(&(request.json_schema)()))
                    .unwrap_or_default();
                let shared: Vec<&String> = path
                    .intersection(&query)
                    .chain(path.intersection(&body))
                    .chain(query.intersection(&body))
                    .collect();
                (!shared.is_empty()).then(|| format!("{}: {shared:?}", operation.name))
            })
            .collect();
        assert_eq!(clashing, Vec::<String>::new());
    }

    // TC: MCP-02 (an argument the request has no place for is refused by
    // the schema already)
    #[tokio::test]
    async fn closed_when_operation_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let open: Vec<String> = OPERATIONS
            .iter()
            .map(|operation| format!("liftoff_{}", operation.name))
            .filter(|name| {
                tool(&tools, name)["inputSchema"]["additionalProperties"] != json!(false)
            })
            .collect();
        assert_eq!(open, Vec::<String>::new());
    }

    /// Every `$ref` anywhere in `value`.
    fn refs(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    match (key.as_str(), inner.as_str()) {
                        ("$ref", Some(target)) => found.push(target.to_owned()),
                        _ => refs(inner, found),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|inner| refs(inner, found)),
            _ => {}
        }
    }

    // TC: MCP-02 (an input schema stands on its own: every `$ref` of the
    // merged query and body schemas resolves inside it, as REST-10 asks
    // of the document; no request type of today nests another, so the
    // merge of `$defs` is pinned by the unit test
    // `input_schema::refs_resolve_when_body_nests_a_type` too)
    #[tokio::test]
    async fn every_ref_resolves_when_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let mut dangling = Vec::new();
        for tool in &tools {
            let schema = &tool["inputSchema"];
            let mut found = Vec::new();
            refs(schema, &mut found);
            for target in found {
                let resolved = target
                    .strip_prefix('#')
                    .and_then(|pointer| schema.pointer(pointer))
                    .is_some_and(Value::is_object);
                if !resolved {
                    dangling.push(format!("{}: {target}", tool["name"]));
                }
            }
        }
        assert_eq!((tools.len() > 40, dangling), (true, Vec::<String>::new()));
    }

    // TC: MCP-02 (`PUT /secrets` takes `SecretPut`: scope, id, key and
    // value required, ci optional)
    #[tokio::test]
    async fn secret_put_fields_when_secrets_set_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let schema = &tool(&tools, "liftoff_secrets_set")["inputSchema"];
        assert_eq!(
            (schema["type"].clone(), properties(schema), required(schema)),
            (
                json!("object"),
                ["ci", "id", "key", "scope", "value"]
                    .map(ToOwned::to_owned)
                    .into(),
                ["id", "key", "scope", "value"]
                    .map(ToOwned::to_owned)
                    .into(),
            )
        );
    }

    // TC: MCP-02 (path parameters are required string arguments)
    #[tokio::test]
    async fn path_parameters_required_when_path_has_them() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let name = crate::support::tool_of("GET", "/api/v1/projects/{p}/environments/{e}");
        let schema = &tool(&tools, &name)["inputSchema"];
        assert_eq!(
            (
                schema["properties"]["p"]["type"].clone(),
                schema["properties"]["e"]["type"].clone(),
                required(schema),
            ),
            (
                json!("string"),
                json!("string"),
                ["e", "p"].map(ToOwned::to_owned).into(),
            )
        );
    }
}

mod module_tools {
    use super::*;

    /// `GET /modules` with `tenants` (one tool) and `dns.cloudflare`.
    fn modules() -> Value {
        json!([
            {
                "name": "tenants",
                "version": "0.1.0",
                "enabled": true,
                "error": null,
                "installed_at": "2026-10-20T10:00:00Z",
                "commands": [],
                "routes": [],
                "tools": [{
                    "name": "tenant_create",
                    "description": "Create a tenant",
                    "schema": {
                        "type": "object",
                        "properties": {"slug": {"type": "string"}},
                        "required": ["slug"]
                    }
                }],
                "config_schema": null
            },
            {
                "name": "dns.cloudflare",
                "version": "0.1.0",
                "enabled": true,
                "error": null,
                "installed_at": "2026-10-20T10:00:00Z",
                "commands": [],
                "routes": [],
                "tools": [{
                    "name": "zones_list",
                    "description": "The zones of the account",
                    "schema": {"type": "object", "properties": {}}
                }],
                "config_schema": null
            }
        ])
    }

    // TC: MCP-04
    #[tokio::test]
    async fn listed_with_module_prefix_when_module_has_tools() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", answer(200, &modules()));
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let created = tool(&tools, "liftoff_tenants_tenant_create");
        assert_eq!(
            (
                created["description"].clone(),
                created["inputSchema"].clone()
            ),
            (
                json!("Create a tenant"),
                json!({
                    "type": "object",
                    "properties": {"slug": {"type": "string"}},
                    "required": ["slug"]
                })
            )
        );
    }

    // TC: MCP-04 (a dot in a module name is not a valid tool-name
    // character for every client: it becomes `_`)
    #[tokio::test]
    async fn dot_of_module_name_underscored_when_listed() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", answer(200, &modules()));
        let client = connect(&stub).await;
        let names = names(&listed(&client).await);
        assert!(
            names.contains("liftoff_dns_cloudflare_zones_list"),
            "{names:?}"
        );
    }

    // TC: MCP-04 (the API tools stay; the module tools come on top)
    #[tokio::test]
    async fn api_tools_kept_when_modules_add_tools() {
        let stub = Stub::start()
            .await
            .route("GET", "/modules", answer(200, &modules()));
        let client = connect(&stub).await;
        let prefixed = names(&listed(&client).await)
            .into_iter()
            .filter(|name| name.starts_with("liftoff_"))
            .count();
        assert_eq!(
            (OPERATIONS.is_empty(), prefixed),
            (false, OPERATIONS.len() + 2)
        );
    }

    /// A module `name` (enabled) with the tools `tools`.
    fn module(name: &str, tools: &[&str]) -> Value {
        json!({
            "name": name,
            "version": "0.1.0",
            "enabled": true,
            "error": null,
            "installed_at": "2026-10-20T10:00:00Z",
            "commands": [],
            "routes": [],
            "tools": tools
                .iter()
                .map(|tool| json!({"name": tool, "description": format!("{name} {tool}"), "schema": {"type": "object"}}))
                .collect::<Vec<Value>>(),
            "config_schema": null
        })
    }

    // TC: MCP-04 (a tool name is at most 64 characters for MCP clients
    // such as Claude Code: a module tool whose name would be longer is
    // not offered, the others are)
    #[tokio::test]
    async fn not_listed_when_tool_name_longer_than_64() {
        let long = format!("t{}", "x".repeat(63));
        let stub = Stub::start().await.route(
            "GET",
            "/modules",
            answer(
                200,
                &json!([module("tenants", &[long.as_str(), "tenant_create"])]),
            ),
        );
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let too_long: Vec<String> = names(&tools)
            .into_iter()
            .filter(|name| name.len() > 64)
            .collect();
        assert_eq!(
            (
                too_long,
                names(&tools).contains("liftoff_tenants_tenant_create")
            ),
            (Vec::<String>::new(), true)
        );
    }

    // TC: MCP-04 (tool names are unique in a server: when two modules map
    // to the same name, the first module installed keeps it)
    #[tokio::test]
    async fn listed_once_when_two_modules_map_to_one_name() {
        let stub = Stub::start().await.route(
            "GET",
            "/modules",
            answer(
                200,
                &json!([
                    module("dns.cloudflare", &["zones_list"]),
                    module("dns", &["cloudflare_zones_list"])
                ]),
            ),
        );
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let clashing: Vec<&Value> = tools
            .iter()
            .filter(|tool| tool["name"] == "liftoff_dns_cloudflare_zones_list")
            .collect();
        assert_eq!(
            clashing
                .iter()
                .map(|tool| tool["description"].clone())
                .collect::<Vec<Value>>(),
            vec![json!("dns.cloudflare zones_list")]
        );
    }

    // TC: MCP-04 (an API tool keeps its name: a module tool mapping to it
    // is not offered)
    #[tokio::test]
    async fn api_tool_listed_once_when_module_tool_takes_its_name() {
        let stub = Stub::start().await.route(
            "GET",
            "/modules",
            answer(200, &json!([module("projects", &["list"])])),
        );
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let same: Vec<Value> = tools
            .iter()
            .filter(|tool| tool["name"] == "liftoff_projects_list")
            .map(|tool| tool["description"].clone())
            .collect();
        assert_eq!(same, vec![json!("Every project")]);
    }

    // TC: MCP-04 (a disabled module takes no calls: its tools are not
    // offered)
    #[tokio::test]
    async fn not_listed_when_module_disabled() {
        let mut disabled = modules();
        disabled[0]["enabled"] = json!(false);
        disabled[0]["error"] = json!("migration 2 failed");
        let stub = Stub::start()
            .await
            .route("GET", "/modules", answer(200, &disabled));
        let client = connect(&stub).await;
        let names = names(&listed(&client).await);
        assert!(
            !names.contains("liftoff_tenants_tenant_create")
                && names.contains("liftoff_dns_cloudflare_zones_list"),
            "{names:?}"
        );
    }
}

mod templates {
    use super::*;

    // TC: TPL-03 (`GET /git/templates` is an operation, so its tool takes the same query)
    #[tokio::test]
    async fn tool_takes_group_and_repos_when_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let schema = &tool(&tools, "liftoff_git_templates_list")["inputSchema"];
        assert_eq!(
            (properties(schema), required(schema)),
            (
                BTreeSet::from(["group".to_owned(), "repos".to_owned()]),
                BTreeSet::new()
            )
        );
    }
}
