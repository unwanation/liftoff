//! `GET /openapi.json` and the probes under `/api/v1`.

use http::StatusCode;

use crate::support::{Ctx, WorldCtx, detail};

mod document {
    use super::*;

    #[tokio::test]
    async fn served_without_credentials_when_anonymous() {
        let ctx = Ctx::new().await;
        let reply = ctx.send("GET", "/openapi.json").await;
        let doc = reply.json();
        assert_eq!(
            (
                reply.status,
                doc["openapi"].as_str().unwrap_or_default().starts_with('3'),
                doc["info"]["title"].clone(),
            ),
            (StatusCode::OK, true, serde_json::json!("liftoff")),
            "{}",
            reply.text()
        );
    }

    #[tokio::test]
    async fn describes_routes_and_schemas_when_generated() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        assert!(
            doc["paths"]["/api/v1/healthz"]["get"].is_object()
                && doc["paths"]["/api/v1/readyz"]["get"].is_object()
                && doc["components"]["schemas"]["UserView"].is_object(),
            "{}",
            doc["paths"]
        );
    }

    /// Every documented operation exists in the router of the whole API
    /// ([`liftoff_rest::app_router`]): asked without
    /// credentials it answers anything but "no such route" or "wrong method".
    #[tokio::test]
    async fn every_documented_operation_is_routed_when_document_lists_it() {
        let ctx = WorldCtx::new().await;
        let doc = ctx.call("GET", "/openapi.json", None, None).await.json();
        let mut missing = Vec::new();
        let mut checked = 0_usize;
        for (path, item) in doc["paths"].as_object().unwrap() {
            let uri = path.replace(['{', '}'], "");
            for method in ["get", "put", "post", "delete", "patch"] {
                if item.get(method).is_none() {
                    continue;
                }
                checked += 1;
                let reply = ctx.call(&method.to_uppercase(), &uri, None, None).await;
                if matches!(
                    reply.status,
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                ) {
                    missing.push(format!("{method} {path} -> {}", reply.status));
                }
            }
        }
        assert!(
            missing.is_empty() && checked >= 6,
            "{checked} checked, not routed: {missing:?}"
        );
    }

    /// Every `$ref` anywhere in `value`.
    fn refs(value: &serde_json::Value, found: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, inner) in map {
                    match (key.as_str(), inner.as_str()) {
                        ("$ref", Some(target)) => found.push(target.to_owned()),
                        _ => refs(inner, found),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for inner in items {
                    refs(inner, found);
                }
            }
            _ => {}
        }
    }

    /// The `(in, name)` of every parameter of the operation `method path`.
    fn parameters(doc: &serde_json::Value, method: &str, path: &str) -> Vec<(String, String)> {
        doc["paths"][path][method]["parameters"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        (
                            item["in"].as_str().unwrap_or_default().to_owned(),
                            item["name"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    // TC: REST-10 (the document a client is generated from)
    #[tokio::test]
    async fn every_schema_ref_resolves_when_generated() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let mut found = Vec::new();
        refs(&doc, &mut found);
        let dangling: Vec<&String> = found
            .iter()
            .filter(|target| {
                target
                    .strip_prefix("#/components/schemas/")
                    .is_none_or(|name| !doc["components"]["schemas"][name].is_object())
            })
            .collect();
        assert!(
            !found.is_empty() && dangling.is_empty(),
            "{} refs, dangling: {dangling:?}",
            found.len()
        );
    }

    // TC: REST-10 (every `{name}` of a path is a required path parameter)
    #[tokio::test]
    async fn every_path_parameter_documented_when_generated() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let mut missing = Vec::new();
        for (path, item) in doc["paths"].as_object().unwrap() {
            let names: Vec<&str> = path
                .split('/')
                .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
                .collect();
            for (method, operation) in item.as_object().unwrap() {
                let documented: Vec<(String, String, bool)> = operation["parameters"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| {
                                (
                                    item["in"].as_str().unwrap_or_default().to_owned(),
                                    item["name"].as_str().unwrap_or_default().to_owned(),
                                    item["required"].as_bool().unwrap_or(false),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for name in &names {
                    if !documented.contains(&("path".to_owned(), (*name).to_owned(), true)) {
                        missing.push(format!("{method} {path}: {name}"));
                    }
                }
            }
        }
        assert_eq!(missing, Vec::<String>::new());
    }

    // TC: REST-10 (the queries of the routes, from the `IntoParams`
    // derives of `liftoff-api`)
    #[tokio::test]
    async fn query_parameters_documented_when_route_takes_a_query() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let scope = ["scope", "id"].as_slice();
        let expected: &[(&str, &str, &[&str])] = &[
            ("get", "/api/v1/secrets", scope),
            ("get", "/api/v1/secrets/{key}", scope),
            ("delete", "/api/v1/secrets/{key}", scope),
            ("get", "/api/v1/secrets/{key}/versions", scope),
            ("post", "/api/v1/secrets/{key}/rollback", scope),
            ("get", "/api/v1/secrets/resolve", &["project", "env"]),
            (
                "get",
                "/api/v1/projects/{p}/environments/{e}/logs",
                &["service", "tail", "follow"],
            ),
            (
                "delete",
                "/api/v1/projects/{p}/environments/{e}",
                &["confirm"],
            ),
            ("delete", "/api/v1/admin/sessions", &["login"]),
            ("get", "/internal/tls-ask", &["domain"]),
        ];
        let mut wrong = Vec::new();
        for (method, path, names) in expected {
            let query: Vec<String> = parameters(&doc, method, path)
                .into_iter()
                .filter(|(place, _)| place == "query")
                .map(|(_, name)| name)
                .collect();
            if query != *names {
                wrong.push(format!("{method} {path}: {query:?}, expected {names:?}"));
            }
        }
        assert_eq!(wrong, Vec::<String>::new());
    }

    #[tokio::test]
    async fn also_served_under_the_api_prefix() {
        let ctx = Ctx::new().await;
        let reply = ctx.send("GET", "/api/v1/openapi.json").await;
        assert_eq!(reply.status, StatusCode::OK);
    }
}

/// Every route (the core's routes without `/modules`, `/git/*` beyond
/// the connections, `/artifacts`, `/a/*`, `/tunnel`, `/.well-known`, the
/// one-time links and `share|unshare`), as the document must list it.
const PHASE_ROUTES: &[(&str, &str)] = &[
    ("post", "/api/v1/auth/token"),
    ("delete", "/api/v1/auth/session"),
    ("get", "/api/v1/me"),
    ("get", "/api/v1/git/connections"),
    ("post", "/api/v1/git/connections"),
    ("delete", "/api/v1/git/connections/{host}"),
    ("get", "/api/v1/projects"),
    ("post", "/api/v1/projects"),
    ("get", "/api/v1/projects/{p}"),
    ("patch", "/api/v1/projects/{p}"),
    ("delete", "/api/v1/projects/{p}"),
    ("post", "/api/v1/projects/{p}/resolve"),
    ("get", "/api/v1/projects/{p}/environments"),
    ("post", "/api/v1/projects/{p}/environments"),
    ("get", "/api/v1/projects/{p}/environments/{e}"),
    ("patch", "/api/v1/projects/{p}/environments/{e}"),
    ("delete", "/api/v1/projects/{p}/environments/{e}"),
    ("post", "/api/v1/projects/{p}/environments/{e}/start"),
    ("post", "/api/v1/projects/{p}/environments/{e}/stop"),
    ("post", "/api/v1/projects/{p}/environments/{e}/redeploy"),
    ("get", "/api/v1/projects/{p}/environments/{e}/deploys"),
    ("get", "/api/v1/projects/{p}/environments/{e}/logs"),
    ("post", "/api/v1/projects/{p}/environments/{e}/bindings"),
    ("delete", "/api/v1/bindings/{host}"),
    ("get", "/api/v1/domains"),
    ("post", "/api/v1/domains"),
    ("patch", "/api/v1/domains/{host}"),
    ("delete", "/api/v1/domains/{host}"),
    ("post", "/api/v1/deploys"),
    ("get", "/api/v1/deploys/{id}"),
    ("get", "/api/v1/deploys/{id}/log"),
    ("post", "/api/v1/projects/{p}/token/rotate"),
    ("get", "/api/v1/projects/{p}/snippet"),
    ("get", "/api/v1/secrets"),
    ("put", "/api/v1/secrets"),
    ("get", "/api/v1/secrets/{key}"),
    ("delete", "/api/v1/secrets/{key}"),
    ("get", "/api/v1/secrets/{key}/versions"),
    ("post", "/api/v1/secrets/{key}/rollback"),
    ("post", "/api/v1/secrets/sync"),
    ("get", "/api/v1/secrets/resolve"),
    ("get", "/api/v1/admin/users"),
    ("post", "/api/v1/admin/token"),
    ("delete", "/api/v1/admin/sessions"),
    ("post", "/api/v1/admin/reconcile"),
    ("get", "/api/v1/healthz"),
    ("get", "/api/v1/readyz"),
    ("get", "/api/v1/server"),
    ("get", "/internal/tls-ask"),
];

mod phase_routes {
    use super::*;

    // TC: REST-10 (every route of the phase is in
    // `/openapi.json`)
    #[tokio::test]
    async fn every_phase_route_documented_when_generated() {
        let ctx = WorldCtx::new().await;
        let doc = ctx.call("GET", "/openapi.json", None, None).await.json();
        let missing: Vec<String> = PHASE_ROUTES
            .iter()
            .filter(|(method, path)| !doc["paths"][*path][*method].is_object())
            .map(|(method, path)| format!("{method} {path}"))
            .collect();
        assert_eq!(missing, Vec::<String>::new());
    }

    // TC: REST-10 (the v2 wire types of `liftoff-api` carry `ToSchema`)
    #[tokio::test]
    async fn v2_schemas_listed_when_generated() {
        let ctx = WorldCtx::new().await;
        let doc = ctx.call("GET", "/openapi.json", None, None).await.json();
        let missing: Vec<&str> = [
            "ConnectionCreate",
            "ConnectionView",
            "ProjectCreate",
            "ProjectPatch",
            "ProjectResolve",
            "ProjectView",
            "EnvironmentCreate",
            "EnvironmentPatch",
            "EnvironmentView",
            "BindingCreate",
            "BindingView",
            "DomainCreate",
            "DomainPatch",
            "DomainView",
            "DeployBody",
            "DeployView",
            "DeploysQueued",
            "RedeployBody",
            "TokenView",
            "SnippetView",
            "SecretPut",
            "SecretView",
            "SecretValueView",
            "SecretRevisionView",
            "SecretRollback",
            "SecretsSync",
            "SecretsSynced",
            "ResolvedEnv",
            "UserView",
            "Deleted",
        ]
        .into_iter()
        .filter(|name| !doc["components"]["schemas"][*name].is_object())
        .collect();
        assert_eq!(missing, Vec::<&str>::new());
    }

    // TC: REST-10 (`/internal/*` lives at the root only)
    #[tokio::test]
    async fn tls_ask_not_documented_under_the_api_prefix() {
        let ctx = WorldCtx::new().await;
        let doc = ctx.call("GET", "/openapi.json", None, None).await.json();
        assert!(
            doc["paths"]["/api/v1/internal/tls-ask"].is_null(),
            "{}",
            doc["paths"]["/api/v1/internal/tls-ask"]
        );
    }
}

mod probes {
    use super::*;

    #[tokio::test]
    async fn ok_without_credentials_when_healthz_under_api_prefix() {
        let ctx = Ctx::new().await;
        let reply = ctx.send("GET", "/api/v1/healthz").await;
        assert_eq!(
            (reply.status, reply.json()),
            (StatusCode::OK, serde_json::json!({"status": "ok"}))
        );
    }

    #[tokio::test]
    async fn problem_json_when_route_unknown_under_api_prefix() {
        let ctx = Ctx::new().await;
        let reply = ctx.send("GET", "/api/v1/nope").await;
        assert_eq!(
            (reply.status, reply.json()["status"].as_u64()),
            (StatusCode::NOT_FOUND, Some(404))
        );
    }
}

/// `liftoff_api::OPERATIONS` (the table the MCP tools are built from)
/// against the generated document.
mod operation_table {
    use std::collections::BTreeSet;

    use liftoff_api::OPERATIONS;
    use serde_json::Value;

    use super::*;

    /// `(method, path)` of every operation of the document below
    /// `/api/v1`.
    fn documented(doc: &Value) -> BTreeSet<(String, String)> {
        doc["paths"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.starts_with("/api/v1/"))
            .flat_map(|(path, item)| {
                item.as_object()
                    .unwrap()
                    .keys()
                    .map(|method| (method.to_uppercase(), path.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn names(values: Option<&Value>) -> BTreeSet<String> {
        match values {
            Some(Value::Object(map)) => map.keys().cloned().collect(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect(),
            _ => BTreeSet::new(),
        }
    }

    /// The shape of a property schema, resolved against `root` (the
    /// document for utoipa, the schema itself for schemars): the JSON
    /// types it admits (`null` for an `Option`) and the values of an
    /// enum. Formats, bounds and descriptions are left out: the two
    /// derives spell them differently (`uint32` and `int32`, `minimum`).
    fn shape(schema: &Value, root: &Value) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut types = BTreeSet::new();
        let mut values = BTreeSet::new();
        if let Some(target) = schema.get("$ref").and_then(Value::as_str) {
            let resolved = target
                .strip_prefix('#')
                .and_then(|pointer| root.pointer(pointer))
                .cloned()
                .unwrap_or(Value::Null);
            return shape(&resolved, root);
        }
        for key in ["anyOf", "oneOf", "allOf"] {
            for inner in schema
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let (inner_types, inner_values) = shape(inner, root);
                types.extend(inner_types);
                values.extend(inner_values);
            }
        }
        match schema.get("type") {
            Some(Value::String(kind)) => {
                types.insert(kind.clone());
            }
            Some(Value::Array(kinds)) => {
                types.extend(
                    kinds
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned),
                );
            }
            _ => {}
        }
        if schema.get("nullable") == Some(&Value::Bool(true)) {
            types.insert("null".to_owned());
        }
        let constants = schema
            .get("enum")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .chain(schema.get("const").cloned());
        for value in constants {
            if value.is_null() {
                types.insert("null".to_owned());
            } else {
                values.insert(value.to_string());
            }
        }
        if types.is_empty() && values.is_empty() {
            types.insert("any".to_owned());
        }
        (types, values)
    }

    /// `name: utoipa shape vs schemars shape` for every property both
    /// sides have whose shapes differ.
    fn shape_mismatches(
        what: &str,
        documented: &[(String, Value)],
        doc: &Value,
        generated: &Value,
    ) -> Vec<String> {
        documented
            .iter()
            .filter_map(|(name, property)| {
                let other = generated.get("properties")?.get(name)?;
                let (left, right) = (shape(property, doc), shape(other, generated));
                (left != right)
                    .then(|| format!("{what}.{name}: utoipa {left:?}, schemars {right:?}"))
            })
            .collect()
    }

    // TC: MCP-02 (each body field has the same JSON types and enum values
    // in the `ToSchema` component and in the `schemars` schema)
    #[tokio::test]
    async fn request_field_types_match_component_when_operation_takes_a_body() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let mut wrong = Vec::new();
        let mut checked = 0_usize;
        for request in OPERATIONS.iter().filter_map(|operation| operation.request) {
            let component = &doc["components"]["schemas"][request.name];
            let documented: Vec<(String, Value)> = component["properties"]
                .as_object()
                .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            checked += documented.len();
            wrong.extend(shape_mismatches(
                request.name,
                &documented,
                &doc,
                &(request.json_schema)(),
            ));
        }
        assert_eq!((checked > 30, wrong), (true, Vec::<String>::new()));
    }

    // TC: MCP-02 (each query field has the same JSON types and enum
    // values as its parameter in the document)
    #[tokio::test]
    async fn query_field_types_match_parameters_when_operation_takes_a_query() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let mut wrong = Vec::new();
        let mut checked = 0_usize;
        for operation in OPERATIONS {
            let Some(query) = operation.query else {
                continue;
            };
            let method = operation.method.as_str().to_lowercase();
            let documented: Vec<(String, Value)> =
                doc["paths"][operation.path][method.as_str()]["parameters"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|parameter| parameter["in"] == "query")
                    .filter_map(|parameter| {
                        Some((
                            parameter["name"].as_str()?.to_owned(),
                            parameter["schema"].clone(),
                        ))
                    })
                    .collect();
            checked += documented.len();
            wrong.extend(shape_mismatches(
                &format!("{method} {}", operation.path),
                &documented,
                &doc,
                &(query.json_schema)(),
            ));
        }
        assert_eq!((checked > 10, wrong), (true, Vec::<String>::new()));
    }

    /// The path of every `.route("...", ...)` in the router's source.
    /// The WebSocket upgrade of `lof dev`.
    const TUNNEL_UPGRADE: &str = "/tunnel";

    /// The page a one-time link opens in a browser: a redirect with a cookie, no JSON operation.
    const LINK_PAGE: &str = "/auth/links/{token}";

    fn router_paths() -> BTreeSet<&'static str> {
        const SOURCE: &str = include_str!("../../src/router.rs");
        SOURCE
            .split(".route(")
            .skip(1)
            .filter_map(|call| {
                let literal = call.trim_start().strip_prefix('"')?;
                literal.split_once('"').map(|(path, _)| path)
            })
            .collect()
    }

    // TC: MCP-01 (every route the router serves below `/api/v1` is a row of
    // the table, so it is a tool too; the routes of modules below `/m/`
    // are the modules' own, `GET /tunnel` is the WebSocket upgrade of
    // `lof dev` and `GET /auth/links/{token}` the browser's page of a
    // one-time link, no JSON operations a tool could call)
    #[tokio::test]
    async fn every_routed_operation_in_table_when_router_serves_it() {
        let ctx = WorldCtx::new().await;
        let table: BTreeSet<(String, String)> = OPERATIONS
            .iter()
            .map(|operation| {
                (
                    operation.method.as_str().to_owned(),
                    operation.path.to_owned(),
                )
            })
            .collect();
        // As a signed-in user, so the session layer lets every request
        // through to the method router: a method it does not serve is
        // `405`, a path it does not serve the fallback's `404` (a handler's
        // own `404` names what it did not find).
        let fallback = detail(&ctx.carol("GET", "/api/v1/no/such/route", None).await);
        let paths = router_paths();
        let mut unlisted = Vec::new();
        let mut routed = 0_usize;
        for path in paths.iter().filter(|path| {
            !path.starts_with("/m/") && **path != TUNNEL_UPGRADE && **path != LINK_PAGE
        }) {
            let documented = format!("/api/v1{path}");
            let uri = documented.replace(['{', '}'], "");
            for method in ["GET", "PUT", "POST", "DELETE", "PATCH"] {
                let reply = ctx.carol(method, &uri, None).await;
                let unrouted = reply.status == StatusCode::METHOD_NOT_ALLOWED
                    || (reply.status == StatusCode::NOT_FOUND && detail(&reply) == fallback);
                if unrouted {
                    continue;
                }
                routed += 1;
                if !table.contains(&(method.to_owned(), documented.clone())) {
                    unlisted.push(format!("{method} {documented} -> {}", reply.status));
                }
            }
        }
        assert_eq!(
            (paths.len() > 30, routed > 40, unlisted),
            (true, true, Vec::<String>::new())
        );
    }

    // TC: MCP-01 (the document below `/api/v1` is the table, row for row)
    #[tokio::test]
    async fn same_operations_when_document_generated_from_table() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let table: BTreeSet<(String, String)> = OPERATIONS
            .iter()
            .map(|operation| {
                (
                    operation.method.as_str().to_owned(),
                    operation.path.to_owned(),
                )
            })
            .collect();
        let documented = documented(&doc);
        assert_eq!(
            (
                documented.difference(&table).collect::<Vec<_>>(),
                table.difference(&documented).collect::<Vec<_>>(),
            ),
            (Vec::new(), Vec::new()),
            "(documented only, table only)"
        );
    }

    // TC: MCP-02 (the body of an operation is the `$ref` of its request
    // in the document)
    #[tokio::test]
    async fn request_ref_named_by_table_when_operation_takes_a_body() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let mut wrong = Vec::new();
        for operation in OPERATIONS {
            let method = operation.method.as_str().to_lowercase();
            let reference = doc["paths"][operation.path][method.as_str()]["requestBody"]["content"]
                ["application/json"]["schema"]["$ref"]
                .as_str()
                .map(ToOwned::to_owned);
            let expected = operation
                .request
                .map(|request| format!("#/components/schemas/{}", request.name));
            if reference != expected {
                wrong.push(format!(
                    "{method} {}: {reference:?}, expected {expected:?}",
                    operation.path
                ));
            }
        }
        assert_eq!(
            (OPERATIONS.is_empty(), wrong),
            (false, Vec::<String>::new())
        );
    }

    // TC: MCP-02 (the `schemars` schema of a request body has the fields
    // of its `ToSchema` component, and requires the same ones)
    #[tokio::test]
    async fn request_fields_match_component_when_operation_takes_a_body() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let mut wrong = Vec::new();
        let mut checked = 0_usize;
        for request in OPERATIONS.iter().filter_map(|operation| operation.request) {
            checked += 1;
            let component = &doc["components"]["schemas"][request.name];
            let schema = (request.json_schema)();
            let documented = (
                names(component.get("properties")),
                names(component.get("required")),
            );
            let generated = (
                names(schema.get("properties")),
                names(schema.get("required")),
            );
            if documented != generated {
                wrong.push(format!(
                    "{}: schemars {generated:?}, utoipa {documented:?}",
                    request.name
                ));
            }
        }
        assert_eq!((checked > 10, wrong), (true, Vec::<String>::new()));
    }

    // TC: MCP-02 (the `schemars` schema of a query has one property per
    // query parameter of the document, required alike)
    #[tokio::test]
    async fn query_fields_match_parameters_when_operation_takes_a_query() {
        let ctx = Ctx::new().await;
        let doc = ctx.send("GET", "/openapi.json").await.json();
        let mut wrong = Vec::new();
        let mut checked = 0_usize;
        for operation in OPERATIONS {
            let Some(query) = operation.query else {
                continue;
            };
            checked += 1;
            let method = operation.method.as_str().to_lowercase();
            let parameters = doc["paths"][operation.path][method.as_str()]["parameters"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let in_query = |required_only: bool| -> BTreeSet<String> {
                parameters
                    .iter()
                    .filter(|parameter| parameter["in"] == "query")
                    .filter(|parameter| !required_only || parameter["required"] == true)
                    .filter_map(|parameter| parameter["name"].as_str().map(ToOwned::to_owned))
                    .collect()
            };
            let schema = (query.json_schema)();
            let generated = (
                names(schema.get("properties")),
                names(schema.get("required")),
            );
            let documented = (in_query(false), in_query(true));
            if documented != generated {
                wrong.push(format!(
                    "{method} {}: schemars {generated:?}, document {documented:?}",
                    operation.path
                ));
            }
        }
        assert_eq!((checked > 8, wrong), (true, Vec::<String>::new()));
    }
}
