//! The tools of the standalone artifacts server as aliases of the artifact operations: `publish_artifact`,
//! `update_artifact`, `share_artifact`, `unshare_artifact`,
//! `list_artifacts`, `list_artifact_versions` and `delete_artifact`, with
//! the predecessor's names and argument shapes, so the instructions
//! Claude already follows keep working. Each one is the same REST call
//! as its `liftoff_artifacts_*` tool (the JSON form, files in base64),
//! answered with the predecessor's fields.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rmcp::model::{JsonObject, Tool};
use serde_json::{Map, Value, json};

use crate::api::{Call, Success};
use crate::tools;

/// The names of the aliases.
pub(crate) const NAMES: [&str; 7] = [
    "publish_artifact",
    "update_artifact",
    "share_artifact",
    "unshare_artifact",
    "list_artifacts",
    "list_artifact_versions",
    "delete_artifact",
];

/// What the predecessor's `publish_artifact` told the agent, with the
/// TTL named as the stdio server knows it: the installation's.
const PUBLISH: &str = "Publishes a complete HTML document at a sign-in-protected URL and returns \
it. Use this for any HTML the user should look at; give the user the returned url as is (and, \
when the answer has one, the one-time login_url that opens it signed in). The artifact is \
private (visible after sign-in) and disappears after the installation's TTL (5 hours by \
default) unless the user shares it (share_artifact). Inside the document, script/img/link tags \
to relative files in \"files\" work; fetch() of those files and localStorage do not (sandboxed \
origin), so inline data into the HTML. Write the HTML straight into this call rather than into \
a file first.";

const UPDATE: &str = "Replaces the HTML and files of an existing artifact as its new version; \
the url stays the same. A private artifact gets a fresh TTL; a shared one stays shared.";

const SHARE: &str = "Makes the artifact public: visible to anyone with the link, and kept \
indefinitely (no TTL). Only on the user's request: the content becomes public.";

const UNSHARE: &str = "Makes the artifact private again: it requires sign-in to view and is \
removed after the installation's TTL from now.";

const LIST: &str = "Every artifact of the token's owner with its url, shared flag, expiry and \
current version.";

const VERSIONS: &str = "The kept versions of an artifact, oldest first.";

const DELETE: &str = "Removes the artifact with every version and its files now.";

/// The schema of an artifact's `id`.
fn id_schema() -> Value {
    json!({"type": "string", "description": "The artifact id (12 characters)"})
}

/// The properties of publish's schema: `html`, `title` and `files`.
fn body_properties() -> Map<String, Value> {
    let file = json!({
        "anyOf": [
            {"type": "string", "description": "The file's text (UTF-8)"},
            {
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The file's content: text, or base64 when encoding is \"base64\""
                    },
                    "encoding": {"type": "string", "enum": ["utf8", "base64"]}
                },
                "required": ["content"],
                "additionalProperties": false
            }
        ]
    });
    Map::from_iter([
        (
            "html".to_owned(),
            json!({"type": "string", "description": "The complete HTML document"}),
        ),
        (
            "title".to_owned(),
            json!({
                "type": ["string", "null"],
                "description": "Shown in the top bar; defaults to the document's <title>"
            }),
        ),
        (
            "files".to_owned(),
            json!({
                "type": "object",
                "description": "Extra files referenced by relative URL from the HTML: relative \
                                path (\"assets/app.js\") to its content",
                "additionalProperties": file
            }),
        ),
    ])
}

/// `{"type": "object", ...}` of `properties` with `required`, closed.
fn schema(properties: Map<String, Value>, required: &[&str]) -> Arc<JsonObject> {
    let mut schema = Map::new();
    schema.insert("type".to_owned(), json!("object"));
    schema.insert("properties".to_owned(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_owned(), json!(required));
    }
    schema.insert("additionalProperties".to_owned(), json!(false));
    Arc::new(schema)
}

/// The schema of a tool taking the `id` alone.
fn by_id() -> Arc<JsonObject> {
    schema(Map::from_iter([("id".to_owned(), id_schema())]), &["id"])
}

/// The tool of every alias, with the predecessor's input schema and a
/// description of what its tool did.
pub(crate) fn tools() -> Vec<Tool> {
    let mut update = body_properties();
    update.insert("id".to_owned(), id_schema());
    vec![
        Tool::new(
            "publish_artifact",
            PUBLISH,
            schema(body_properties(), &["html"]),
        ),
        Tool::new("update_artifact", UPDATE, schema(update, &["id", "html"])),
        Tool::new("share_artifact", SHARE, by_id()),
        Tool::new("unshare_artifact", UNSHARE, by_id()),
        Tool::new("list_artifacts", LIST, schema(Map::new(), &[])),
        Tool::new("list_artifact_versions", VERSIONS, by_id()),
        Tool::new("delete_artifact", DELETE, by_id()),
    ]
}

/// A call of an alias: the REST request it becomes and how its answer
/// is shaped.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AliasCall {
    /// The request of the artifact operation the alias stands for.
    pub(crate) call: Call,
    /// The alias called.
    pub(crate) name: &'static str,
}

impl AliasCall {
    /// The answer of the tool: the API's answer with the fields of the
    /// predecessor's answer (structured content, so always an object);
    /// `None` when the API answered no JSON (the body is passed on as it
    /// is then).
    pub(crate) fn answer(&self, success: &Success) -> Option<Value> {
        let body: Value = serde_json::from_str(&success.body).ok()?;
        match self.name {
            "list_artifacts" => Some(json!({"artifacts": body})),
            "list_artifact_versions" => Some(json!({"versions": body})),
            "delete_artifact" => {
                let id = body.get("deleted").cloned()?;
                Some(json!({"deleted": true, "id": id}))
            }
            _ => body.is_object().then_some(body),
        }
    }
}

/// The REST request of a call of the alias `name` with `arguments`;
/// `None` when `name` is no alias.
///
/// # Errors
///
/// The text of the tool error when the arguments do not fit the alias's
/// schema.
pub(crate) fn call(name: &str, arguments: JsonObject) -> Option<Result<AliasCall, String>> {
    let (name, operation, known): (&'static str, &str, &[&str]) = match name {
        "publish_artifact" => (NAMES[0], "artifacts_publish", &["html", "title", "files"]),
        "update_artifact" => (
            NAMES[1],
            "artifacts_update",
            &["id", "html", "title", "files"],
        ),
        "share_artifact" => (NAMES[2], "artifacts_share", &["id"]),
        "unshare_artifact" => (NAMES[3], "artifacts_unshare", &["id"]),
        "list_artifacts" => (NAMES[4], "artifacts_list", &[]),
        "list_artifact_versions" => (NAMES[5], "artifacts_versions", &["id"]),
        "delete_artifact" => (NAMES[6], "artifacts_delete", &["id"]),
        _ => return None,
    };
    let operation = liftoff_api::operation(operation)?;
    Some(arguments_of(known, arguments).and_then(|arguments| {
        tools::call(operation, arguments).map(|call| AliasCall { call, name })
    }))
}

/// The arguments of the operation from those of an alias taking `known`:
/// any other is refused, `html` and `title` must be text, and `files` is
/// turned into the API's `{path: base64}`.
fn arguments_of(known: &[&str], arguments: JsonObject) -> Result<JsonObject, String> {
    let unknown: Vec<&str> = arguments
        .keys()
        .map(String::as_str)
        .filter(|key| !known.contains(key))
        .collect();
    if !unknown.is_empty() {
        return Err(format!("unknown arguments: {}", unknown.join(", ")));
    }
    let mut arguments = arguments;
    if known.contains(&"html") && !matches!(arguments.get("html"), Some(Value::String(_))) {
        return Err("argument html must be a string".to_owned());
    }
    match arguments.get("title") {
        None | Some(Value::String(_)) => {}
        Some(Value::Null) => {
            arguments.remove("title");
        }
        Some(_) => return Err("argument title must be a string".to_owned()),
    }
    match arguments.remove("files") {
        None | Some(Value::Null) => {}
        Some(Value::Object(files)) => {
            let encoded = files
                .into_iter()
                .map(|(path, file)| file_base64(&path, file).map(|file| (path, file)))
                .collect::<Result<Map<String, Value>, String>>()?;
            arguments.insert("files".to_owned(), Value::Object(encoded));
        }
        Some(_) => {
            return Err("argument files must be an object of path to content".to_owned());
        }
    }
    Ok(arguments)
}

/// The base64 of the file at `path`: of the UTF-8 bytes of a text, or
/// the `content` of `{content, encoding: "base64"}` as it is.
fn file_base64(path: &str, file: Value) -> Result<Value, String> {
    let refused = || {
        format!(
            "file {path} must be its text or {{\"content\": ..., \"encoding\": \"utf8\" | \"base64\"}}"
        )
    };
    let (content, encoding) = match file {
        Value::String(text) => (text, None),
        Value::Object(mut file) => {
            let Some(Value::String(content)) = file.remove("content") else {
                return Err(refused());
            };
            let encoding = match file.remove("encoding") {
                None | Some(Value::Null) => None,
                Some(Value::String(encoding)) => Some(encoding),
                Some(_) => return Err(refused()),
            };
            if !file.is_empty() {
                return Err(refused());
            }
            (content, encoding)
        }
        _ => return Err(refused()),
    };
    match encoding.as_deref() {
        None | Some("utf8" | "utf-8") => Ok(Value::String(STANDARD.encode(content))),
        Some("base64") => Ok(Value::String(content)),
        Some(_) => Err(refused()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> JsonObject {
        match value {
            Value::Object(object) => object,
            _ => JsonObject::new(),
        }
    }

    mod call {
        use super::*;

        // TC: MCPA-01 (an alias takes the predecessor's arguments only:
        // `list_artifacts {}` never reaches `?all=true`)
        #[test]
        fn refused_when_argument_unknown_to_alias() {
            let refused = call("list_artifacts", object(json!({"all": true})));
            assert_eq!(refused, Some(Err("unknown arguments: all".to_owned())));
        }

        // TC: MCPA-02 (`encoding: "utf8"` is the text form)
        #[test]
        fn text_encoded_when_file_encoding_is_utf8() {
            let built = call(
                "publish_artifact",
                object(
                    json!({"html": "x", "files": {"a.txt": {"content": "hi", "encoding": "utf8"}}}),
                ),
            );
            assert_eq!(
                built.and_then(Result::ok).and_then(|alias| alias.call.body),
                Some(json!({"html": "x", "files": {"a.txt": "aGk="}}))
            );
        }

        // TC: MCPA-02
        #[test]
        fn refused_when_file_encoding_unknown() {
            let refused = call(
                "publish_artifact",
                object(json!({"html": "x", "files": {"a": {"content": "hi", "encoding": "hex"}}})),
            );
            assert!(matches!(refused, Some(Err(_))), "{refused:?}");
        }

        // TC: MCPA-02 (`html` is required text)
        #[test]
        fn refused_when_html_missing() {
            assert_eq!(
                call("publish_artifact", object(json!({"title": "t"}))),
                Some(Err("argument html must be a string".to_owned()))
            );
        }

        #[test]
        fn none_when_name_is_no_alias() {
            assert_eq!(call("liftoff_artifacts_list", JsonObject::new()), None);
        }
    }
}
