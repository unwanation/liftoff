//! The aliases of the standalone artifacts server tools (`MCPA-01…04`): listed beside `liftoff_artifacts_*` with the
//! predecessor's names and argument shapes, each one the REST call of its
//! artifact operation (the JSON form, files in base64), answered with the
//! predecessor's fields plus `version` and `login_url`.

use std::collections::BTreeSet;

use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

use crate::support::{Client, Seen, Stub, args, connect, json as answer, problem};

/// The id of the artifact the stub knows.
const ID: &str = "aB3dE6gH9jK2";

/// Every tool `tools/list` returns, as JSON.
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

/// The property names and the `required` names of a tool's input schema.
fn shape(tool: &Value) -> (BTreeSet<String>, BTreeSet<String>) {
    let schema = &tool["inputSchema"];
    let properties = schema["properties"]
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    let required = schema["required"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (properties, required)
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Calls `name` with `arguments`; the result as JSON.
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

/// The text of the first content block as JSON (`null` when it is none).
fn answered(result: &Value) -> Value {
    serde_json::from_str(&text(result)).unwrap_or(Value::Null)
}

fn only(calls: Vec<Seen>) -> Seen {
    assert_eq!(calls.len(), 1, "{calls:?}");
    calls.into_iter().next().unwrap()
}

/// The `ArtifactView` the API answers for version `version`, private.
fn view(version: u32, login_url: bool) -> Value {
    let mut view = json!({
        "id": ID,
        "url": format!("https://liftoff.test/a/{ID}"),
        "title": "Report",
        "shared": false,
        "expires_at": "2026-10-21T15:00:00Z",
        "version": version,
        "created_at": "2026-10-21T10:00:00Z",
        "updated_at": "2026-10-21T10:00:00Z"
    });
    if login_url {
        view["login_url"] = json!("https://liftoff.test/api/v1/auth/links/lnk_9c2e");
    }
    view
}

/// The fields of `answer` named `fields`, `null` where it has none.
fn fields(answer: &Value, fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|field| ((*field).to_owned(), answer[*field].clone()))
            .collect(),
    )
}

mod listing {
    use super::*;

    // TC: MCPA-01
    #[tokio::test]
    async fn aliases_listed_beside_artifact_tools_when_started() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let names = names(&listed(&client).await);
        let expected = [
            "liftoff_artifacts_publish",
            "liftoff_artifacts_update",
            "liftoff_artifacts_share",
            "liftoff_artifacts_unshare",
            "liftoff_artifacts_list",
            "liftoff_artifacts_versions",
            "liftoff_artifacts_rollback",
            "liftoff_artifacts_delete",
            "publish_artifact",
            "update_artifact",
            "share_artifact",
            "unshare_artifact",
            "list_artifacts",
            "list_artifact_versions",
            "delete_artifact",
        ];
        let missing: Vec<&str> = expected
            .into_iter()
            .filter(|name| !names.contains(*name))
            .collect();
        assert_eq!(missing, Vec::<&str>::new());
    }

    // TC: MCPA-01 (the predecessor's schema: `html`, an optional `title`
    // and `files`)
    #[tokio::test]
    async fn html_title_files_when_publish_artifact_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        assert_eq!(
            shape(tool(&tools, "publish_artifact")),
            (set(&["html", "title", "files"]), set(&["html"]))
        );
    }

    // TC: MCPA-01 (`files` is `{name: content}`, an object whose values
    // the schema describes)
    #[tokio::test]
    async fn files_keyed_by_name_when_publish_artifact_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let files = &tool(&tools, "publish_artifact")["inputSchema"]["properties"]["files"];
        assert_eq!(
            (
                files["type"].clone(),
                files["additionalProperties"].is_object()
            ),
            (json!("object"), true),
            "{files}"
        );
    }

    // TC: MCPA-01 (`update_artifact` is publish's schema plus the `id`)
    #[tokio::test]
    async fn id_html_title_files_when_update_artifact_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        assert_eq!(
            shape(tool(&tools, "update_artifact")),
            (set(&["id", "html", "title", "files"]), set(&["id", "html"]))
        );
    }

    // TC: MCPA-01 (the others take the `id` alone)
    #[tokio::test]
    async fn id_alone_when_artifact_alias_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let shapes: Vec<(BTreeSet<String>, BTreeSet<String>)> = [
            "share_artifact",
            "unshare_artifact",
            "list_artifact_versions",
            "delete_artifact",
        ]
        .into_iter()
        .map(|name| shape(tool(&tools, name)))
        .collect();
        let by_id = (set(&["id"]), set(&["id"]));
        assert_eq!(
            shapes,
            vec![by_id.clone(), by_id.clone(), by_id.clone(), by_id]
        );
    }

    // TC: MCPA-01 (`list_artifacts {}`)
    #[tokio::test]
    async fn no_arguments_when_list_artifacts_listed() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        assert_eq!(
            shape(tool(&tools, "list_artifacts")),
            (BTreeSet::new(), BTreeSet::new())
        );
    }

    // TC: MCPA-01 (the description says what the predecessor's did: use
    // it for any HTML a person should look at, give the url as is, the
    // TTL of a private artifact and share, no fetch() inside)
    #[tokio::test]
    async fn predecessor_guidance_when_publish_artifact_described() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let description = tool(&tools, "publish_artifact")["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let missing: Vec<&str> = [
            "any HTML",
            "as is",
            "private",
            "disappears",
            "TTL",
            "unless the user shares it",
            "fetch()",
        ]
        .into_iter()
        .filter(|phrase| !description.contains(phrase))
        .collect();
        assert_eq!(missing, Vec::<&str>::new(), "{description}");
    }

    // TC: MCPA-01 (share makes it public and permanent)
    #[tokio::test]
    async fn public_named_when_share_artifact_described() {
        let stub = Stub::start().await;
        let client = connect(&stub).await;
        let tools = listed(&client).await;
        let description = tool(&tools, "share_artifact")["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let missing: Vec<&str> = ["public", "indefinitely", "no TTL"]
            .into_iter()
            .filter(|phrase| !description.contains(phrase))
            .collect();
        assert_eq!(missing, Vec::<&str>::new(), "{description}");
    }
}

mod publish {
    use super::*;

    // TC: MCPA-02 (the JSON form of `POST /artifacts`: a file given as
    // text travels as the base64 of its UTF-8 bytes)
    #[tokio::test]
    async fn posts_files_in_base64_when_files_given_as_text() {
        let stub = Stub::start()
            .await
            .route("POST", "/artifacts", answer(201, &view(1, true)));
        let client = connect(&stub).await;
        call(
            &client,
            "publish_artifact",
            json!({
                "html": "<p>hi</p>",
                "title": "Report",
                "files": {"app.js": "console.log(1)"}
            }),
        )
        .await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path.as_str(), seen.json()),
            (
                "POST",
                "/api/v1/artifacts",
                json!({
                    "html": "<p>hi</p>",
                    "title": "Report",
                    "files": {"app.js": "Y29uc29sZS5sb2coMSk="}
                })
            )
        );
    }

    // TC: MCPA-02 (a binary file as `{content, encoding: "base64"}`, as
    // the predecessor took it: its base64 passes as it is)
    #[tokio::test]
    async fn passes_base64_when_file_given_with_base64_encoding() {
        let stub = Stub::start()
            .await
            .route("POST", "/artifacts", answer(201, &view(1, true)));
        let client = connect(&stub).await;
        call(
            &client,
            "publish_artifact",
            json!({
                "html": "<img src=\"logo.png\">",
                "files": {"logo.png": {"content": "iVBORw0KGgo=", "encoding": "base64"}}
            }),
        )
        .await;
        assert_eq!(
            only(stub.calls()).json()["files"],
            json!({"logo.png": "iVBORw0KGgo="})
        );
    }

    // TC: MCPA-02
    #[tokio::test]
    async fn answers_id_url_version_and_login_url_when_published() {
        let stub = Stub::start()
            .await
            .route("POST", "/artifacts", answer(201, &view(1, true)));
        let client = connect(&stub).await;
        let result = call(
            &client,
            "publish_artifact",
            json!({"html": "<p>hi</p>", "files": {"app.js": "console.log(1)"}}),
        )
        .await;
        assert_eq!(
            (
                result["isError"].as_bool().unwrap_or(false),
                fields(
                    &answered(&result),
                    &["id", "url", "shared", "expires_at", "version", "login_url"]
                )
            ),
            (
                false,
                json!({
                    "id": ID,
                    "url": format!("https://liftoff.test/a/{ID}"),
                    "shared": false,
                    "expires_at": "2026-10-21T15:00:00Z",
                    "version": 1,
                    "login_url": "https://liftoff.test/api/v1/auth/links/lnk_9c2e"
                })
            ),
            "{result}"
        );
    }
    // TC: MCPA-02 (the answer travels as text and as `structuredContent`,
    // the same JSON)
    #[tokio::test]
    async fn structured_content_equals_text_when_published() {
        let stub = Stub::start()
            .await
            .route("POST", "/artifacts", answer(201, &view(1, true)));
        let client = connect(&stub).await;
        let result = call(&client, "publish_artifact", json!({"html": "<p>hi</p>"})).await;
        let structured = result["structuredContent"].clone();
        assert_eq!(
            (
                structured.clone(),
                fields(&structured, &["id", "url", "version", "login_url"])
            ),
            (
                answered(&result),
                json!({
                    "id": ID,
                    "url": format!("https://liftoff.test/a/{ID}"),
                    "version": 1,
                    "login_url": "https://liftoff.test/api/v1/auth/links/lnk_9c2e"
                })
            ),
            "{result}"
        );
    }
}

mod update {
    use super::*;

    // TC: MCPA-03 (the same call as `liftoff_artifacts_update`: `PUT
    // /artifacts/{id}` with the JSON form)
    #[tokio::test]
    async fn puts_version_when_update_artifact_called() {
        let stub = Stub::start().await.route(
            "PUT",
            &format!("/artifacts/{ID}"),
            answer(200, &view(2, false)),
        );
        let client = connect(&stub).await;
        call(
            &client,
            "update_artifact",
            json!({"id": ID, "html": "<p>v2</p>", "files": {"a.css": "p{}"}}),
        )
        .await;
        let seen = only(stub.calls());
        assert_eq!(
            (
                seen.method.as_str(),
                seen.path.as_str(),
                seen.json()["html"].clone(),
                seen.json()["files"].clone(),
            ),
            (
                "PUT",
                format!("/api/v1/artifacts/{ID}").as_str(),
                json!("<p>v2</p>"),
                json!({"a.css": "cHt9"})
            )
        );
    }

    // TC: MCPA-03
    #[tokio::test]
    async fn answers_version_2_when_updated() {
        let stub = Stub::start().await.route(
            "PUT",
            &format!("/artifacts/{ID}"),
            answer(200, &view(2, false)),
        );
        let client = connect(&stub).await;
        let result = call(
            &client,
            "update_artifact",
            json!({"id": ID, "html": "<p>v2</p>"}),
        )
        .await;
        assert_eq!(
            (
                result["isError"].as_bool().unwrap_or(false),
                fields(&answered(&result), &["id", "url", "version"])
            ),
            (
                false,
                json!({
                    "id": ID,
                    "url": format!("https://liftoff.test/a/{ID}"),
                    "version": 2
                })
            ),
            "{result}"
        );
    }
}

mod share {
    use super::*;

    // TC: MCPA-01 (`share_artifact {id}` is `POST /artifacts/{id}/share`)
    #[tokio::test]
    async fn posts_share_when_share_artifact_called() {
        let mut shared = view(2, false);
        shared["shared"] = json!(true);
        shared["expires_at"] = Value::Null;
        let stub = Stub::start().await.route(
            "POST",
            &format!("/artifacts/{ID}/share"),
            answer(200, &shared),
        );
        let client = connect(&stub).await;
        let result = call(&client, "share_artifact", json!({"id": ID})).await;
        assert_eq!(
            (
                only(stub.calls()).path,
                fields(&answered(&result), &["id", "shared", "expires_at"])
            ),
            (
                format!("/api/v1/artifacts/{ID}/share"),
                json!({"id": ID, "shared": true, "expires_at": null})
            ),
            "{result}"
        );
    }

    // TC: MCPA-01 (`unshare_artifact {id}` is `POST
    // /artifacts/{id}/unshare`)
    #[tokio::test]
    async fn posts_unshare_when_unshare_artifact_called() {
        let stub = Stub::start().await.route(
            "POST",
            &format!("/artifacts/{ID}/unshare"),
            answer(200, &view(2, false)),
        );
        let client = connect(&stub).await;
        let result = call(&client, "unshare_artifact", json!({"id": ID})).await;
        assert_eq!(
            (
                only(stub.calls()).path,
                fields(&answered(&result), &["id", "shared"])
            ),
            (
                format!("/api/v1/artifacts/{ID}/unshare"),
                json!({"id": ID, "shared": false})
            ),
            "{result}"
        );
    }
}

mod list {
    use super::*;

    // TC: MCPA-01 (`list_artifacts {}` answers `{artifacts: [...]}`, as
    // the predecessor did)
    #[tokio::test]
    async fn artifacts_listed_under_artifacts_when_list_artifacts_called() {
        let stub =
            Stub::start()
                .await
                .route("GET", "/artifacts", answer(200, &json!([view(2, false)])));
        let client = connect(&stub).await;
        let result = call(&client, "list_artifacts", json!({})).await;
        assert_eq!(
            (only(stub.calls()).path, answered(&result)),
            (
                "/api/v1/artifacts".to_owned(),
                json!({"artifacts": [view(2, false)]})
            ),
            "{result}"
        );
    }

    // TC: MCPA-01 (`list_artifact_versions {id}` is `GET
    // /artifacts/{id}/versions`, answered as `{versions: [...]}`)
    #[tokio::test]
    async fn versions_listed_under_versions_when_list_artifact_versions_called() {
        let versions = json!([
            {"n": 1, "title": "Report", "size": 120, "created_at": "2026-10-21T10:00:00Z"},
            {"n": 2, "title": "Report", "size": 140, "created_at": "2026-10-21T11:00:00Z"}
        ]);
        let stub = Stub::start().await.route(
            "GET",
            &format!("/artifacts/{ID}/versions"),
            answer(200, &versions),
        );
        let client = connect(&stub).await;
        let result = call(&client, "list_artifact_versions", json!({"id": ID})).await;
        assert_eq!(
            (only(stub.calls()).path, answered(&result)),
            (
                format!("/api/v1/artifacts/{ID}/versions"),
                json!({"versions": versions})
            ),
            "{result}"
        );
    }
}

mod delete {
    use super::*;

    // TC: MCPA-04 (the API's `404` is a tool error that names it: the
    // status and title of the problem, then its detail)
    #[tokio::test]
    async fn tool_error_naming_not_found_when_artifact_unknown() {
        let stub = Stub::start().await.route(
            "DELETE",
            "/artifacts/zzzzzzzzzzzz",
            problem(404, "artifact zzzzzzzzzzzz"),
        );
        let client = connect(&stub).await;
        let result = call(&client, "delete_artifact", json!({"id": "zzzzzzzzzzzz"})).await;
        assert_eq!(
            (result["isError"].as_bool(), text(&result)),
            (
                Some(true),
                "404 Not Found: artifact zzzzzzzzzzzz".to_owned()
            ),
            "{result}"
        );
    }

    // TC: MCPA-04 (the call is `DELETE /artifacts/{id}`)
    #[tokio::test]
    async fn deletes_by_id_when_delete_artifact_called() {
        let stub = Stub::start().await.route(
            "DELETE",
            &format!("/artifacts/{ID}"),
            answer(200, &json!({"deleted": ID})),
        );
        let client = connect(&stub).await;
        call(&client, "delete_artifact", json!({"id": ID})).await;
        let seen = only(stub.calls());
        assert_eq!(
            (seen.method.as_str(), seen.path),
            ("DELETE", format!("/api/v1/artifacts/{ID}"))
        );
    }

    // TC: MCPA-04 (the predecessor's answer of a delete: `{deleted:
    // true, id}`)
    #[tokio::test]
    async fn answers_deleted_and_id_when_deleted() {
        let stub = Stub::start().await.route(
            "DELETE",
            &format!("/artifacts/{ID}"),
            answer(200, &json!({"deleted": ID})),
        );
        let client = connect(&stub).await;
        let result = call(&client, "delete_artifact", json!({"id": ID})).await;
        assert_eq!(
            answered(&result),
            json!({"deleted": true, "id": ID}),
            "{result}"
        );
    }
}
