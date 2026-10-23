//! The tools: one per [`liftoff_api::Operation`] plus the tools of the
//! installed modules, and how a call becomes a REST request.
//!
//! The arguments of an operation's tool are one flat object: its path
//! parameters (required strings), the fields of its query and the fields
//! of its JSON body, side by side. Their schemas are the `schemars`
//! derives of the same `liftoff-api` types `/openapi.json` describes with
//! `utoipa`, so the tool and the document never drift apart.

use std::collections::BTreeSet;
use std::sync::Arc;

use liftoff_api::{ModuleToolAnswer, ModuleView, OPERATIONS, Operation, Response, ToolView};
use rmcp::model::{JsonObject, Tool};
use serde_json::{Map, Value, json};

use crate::api::{Call, Success, events_text};

/// What every tool name starts with.
pub(crate) const PREFIX: &str = "liftoff_";

/// The tool of every operation, in the order of the table.
pub(crate) fn api_tools() -> impl Iterator<Item = Tool> {
    OPERATIONS.iter().map(|operation| {
        Tool::new(
            format!("{PREFIX}{}", operation.name),
            operation.summary,
            Arc::new(input_schema(operation)),
        )
    })
}

/// The operation whose tool is called `name`.
pub(crate) fn operation_of(name: &str) -> Option<&'static Operation> {
    let name = name.strip_prefix(PREFIX)?;
    liftoff_api::operation(name)
}

/// The tool name of `tool` of `module`: `liftoff_<module>_<tool>`, a dot
/// of the module's name (`dns.cloudflare`) written `_`, since not every
/// client takes dots in a tool name.
pub(crate) fn module_tool_name(module: &str, tool: &str) -> String {
    format!("{PREFIX}{}_{tool}", module.replace('.', "_"))
}

/// The longest tool name MCP clients take (`^[a-zA-Z0-9_-]{1,64}$` for
/// Claude Code and the Anthropic API).
pub(crate) const MAX_TOOL_NAME: usize = 64;

/// The tools of the enabled `modules` that [`offered`] keeps; why each
/// other one is left out goes to stderr, the log of a stdio server (the
/// CLI owns stdout).
pub(crate) fn module_tools(modules: &[ModuleView]) -> Vec<Tool> {
    let (kept, dropped) = offered(modules);
    for why in dropped {
        log(&why);
    }
    kept.into_iter()
        .map(|(module, tool)| {
            let schema = match &tool.schema {
                Value::Object(schema) => schema.clone(),
                _ => object_schema(Map::new(), &[]),
            };
            Tool::new(
                module_tool_name(&module.name, &tool.name),
                tool.description.clone(),
                Arc::new(schema),
            )
        })
        .collect()
}

/// Writes `line` to stderr, the log of a stdio MCP server (stdout
/// carries the protocol); a failed write is no reason to fail a listing.
fn log(line: &str) {
    use std::io::Write as _;
    let _ignored: std::io::Result<()> = writeln!(std::io::stderr(), "liftoff mcp: {line}");
}

/// The REST request of a call of the module tool `name` with
/// `arguments`: `POST /api/v1/modules/{module}/tools/{tool}` with
/// `{"arguments": ...}`. `None` when no enabled module offers `name`.
pub(crate) fn module_call(
    modules: &[ModuleView],
    name: &str,
    arguments: JsonObject,
) -> Option<Result<Call, String>> {
    let (module, tool) = offered(modules)
        .0
        .into_iter()
        .find(|(module, tool)| module_tool_name(&module.name, &tool.name) == name)?;
    let path = path_segment("module", &module.name).and_then(|module| {
        path_segment("tool", &tool.name)
            .map(|tool| format!("/api/v1/modules/{module}/tools/{tool}"))
    });
    Some(path.map(|path| Call {
        method: liftoff_api::Method::Post,
        path,
        query: Vec::new(),
        body: Some(Value::Object(Map::from_iter([(
            "arguments".to_owned(),
            Value::Object(arguments),
        )]))),
    }))
}

/// Every tool of an enabled module that can be offered, with its module,
/// and why each other one cannot. A tool is left out when its name
/// ([`module_tool_name`]) is longer than [`MAX_TOOL_NAME`], is an API
/// tool's, or is already another module's: tool names are unique in a
/// server, so of two modules mapping to one name (`dns.cloudflare` with
/// `zones_list`, `dns` with `cloudflare_zones_list`) the first in
/// `modules` (the order of `GET /modules`) keeps it, and a call never
/// reaches a module other than the one listed.
fn offered(modules: &[ModuleView]) -> (Vec<(&ModuleView, &ToolView)>, Vec<String>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut taken = BTreeSet::new();
    let tools = modules
        .iter()
        .filter(|module| module.enabled)
        .flat_map(|module| module.tools.iter().map(move |tool| (module, tool)));
    for (module, tool) in tools {
        let name = module_tool_name(&module.name, &tool.name);
        let why = if name.len() > MAX_TOOL_NAME {
            Some(format!("longer than {MAX_TOOL_NAME} characters"))
        } else if operation_of(&name).is_some() {
            Some("the name of an API tool".to_owned())
        } else if taken.contains(&name) {
            Some("the name of a tool of a module before it".to_owned())
        } else {
            None
        };
        if let Some(why) = why {
            dropped.push(format!(
                "tool {} of module {} is not offered: {name} is {why}",
                tool.name, module.name
            ));
        } else {
            taken.insert(name);
            kept.push((module, tool));
        }
    }
    (kept, dropped)
}

/// The text of a module tool's answer (`{"result": ...}`): a string as
/// it is, any other JSON as JSON.
pub(crate) fn module_answer_text(success: &Success) -> String {
    let answer: Option<ModuleToolAnswer> = serde_json::from_str(&success.body).ok();
    match answer.map(|answer| answer.result) {
        Some(Value::String(text)) => text,
        Some(other) => other.to_string(),
        None => success.body.clone(),
    }
}

/// `{"type": "object", "properties": ..., "required": ...}`.
fn object_schema(properties: Map<String, Value>, required: &[String]) -> JsonObject {
    let mut schema = Map::new();
    schema.insert("type".to_owned(), json!("object"));
    schema.insert("properties".to_owned(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".to_owned(), json!(required));
    }
    schema
}

/// The property names of a JSON Schema object.
fn property_names(schema: &Value) -> BTreeSet<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default()
}

/// The input schema of `operation`'s tool: its path parameters, then the
/// properties of its query and of its body.
pub(crate) fn input_schema(operation: &Operation) -> JsonObject {
    let mut properties = Map::new();
    let mut required: Vec<String> = Vec::new();
    let mut definitions = Map::new();
    for name in operation.path_parameters() {
        properties.insert(
            name.to_owned(),
            json!({"type": "string", "description": format!("`{{{name}}}` of {}", operation.path)}),
        );
        required.push(name.to_owned());
    }
    let parts = operation
        .query
        .map(|query| (query.json_schema)())
        .into_iter()
        .chain(operation.request.map(|request| (request.json_schema)()));
    for part in parts {
        if let Some(Value::Object(fields)) = part.get("properties") {
            for (name, field) in fields {
                properties.insert(name.clone(), field.clone());
            }
        }
        if let Some(Value::Array(names)) = part.get("required") {
            for name in names.iter().filter_map(Value::as_str) {
                if !required.iter().any(|known| known == name) {
                    required.push(name.to_owned());
                }
            }
        }
        if let Some(Value::Object(defs)) = part.get("$defs") {
            definitions.extend(defs.clone());
        }
    }
    let mut schema = object_schema(properties, &required);
    schema.insert("additionalProperties".to_owned(), json!(false));
    if !definitions.is_empty() {
        schema.insert("$defs".to_owned(), Value::Object(definitions));
    }
    schema
}

/// An argument as the text of a path segment or a query value.
fn scalar(name: &str, value: Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        other => Err(format!("argument {name} must be a string, got {other}")),
    }
}

/// `text` as one path segment of a request: percent-encoded, so no `/`,
/// `?`, `#` or `%` of it shapes the URL.
///
/// # Errors
///
/// The text of the tool error when `text` is empty, `.` or `..`: the URL
/// parser folds such a segment away, and the request would reach another
/// route than the one named.
pub(crate) fn path_segment(name: &str, text: &str) -> Result<String, String> {
    if matches!(text, "" | "." | "..") {
        return Err(format!("argument {name} must be a path segment"));
    }
    Ok(segment(text))
}

/// Percent-encodes one path segment.
fn segment(text: &str) -> String {
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

/// Why a call of an event stream with `follow=true` is refused.
const FOLLOW_REFUSED: &str = "argument follow must be false: a tool answers once and cannot \
                              follow a stream (read the resource \
                              liftoff://projects/{p}/environments/{e}/log)";

/// The REST request of a call of `operation`'s tool with `arguments`.
///
/// A tool cannot stream: an operation answering events (the container
/// logs) is always asked with `follow=false`, and a caller asking
/// `follow=true` is refused rather than left waiting on a stream that
/// never ends.
///
/// # Errors
///
/// The text of the tool error: a path parameter is missing or is no path
/// segment ([`path_segment`]), an argument is not a scalar where the path
/// or the query needs one, `follow` is not `false` on an event stream, or
/// an argument is unknown to an operation without a body.
pub(crate) fn call(operation: &Operation, arguments: JsonObject) -> Result<Call, String> {
    let mut arguments = arguments;
    let mut path = operation.path.to_owned();
    for name in operation.path_parameters() {
        let value = arguments
            .remove(name)
            .ok_or_else(|| format!("missing argument {name}"))?;
        let text = scalar(name, value)?;
        path = path.replace(&format!("{{{name}}}"), &path_segment(name, &text)?);
    }
    let query_names = operation
        .query
        .map(|query| property_names(&(query.json_schema)()))
        .unwrap_or_default();
    let mut query = Vec::new();
    for name in &query_names {
        match arguments.remove(name) {
            None | Some(Value::Null) => {}
            Some(value) => query.push((name.clone(), scalar(name, value)?)),
        }
    }
    if matches!(operation.response, Response::Events) && query_names.contains("follow") {
        match query.iter().find(|(name, _)| name == "follow") {
            None => query.push(("follow".to_owned(), "false".to_owned())),
            Some((_, value)) if value == "false" => {}
            Some(_) => return Err(FOLLOW_REFUSED.to_owned()),
        }
    }
    let body = match (operation.request, arguments.is_empty()) {
        (Some(_), _) => Some(Value::Object(arguments)),
        (None, true) => None,
        (None, false) => {
            let unknown: Vec<String> = arguments.keys().cloned().collect();
            return Err(format!("unknown arguments: {}", unknown.join(", ")));
        }
    };
    Ok(Call {
        method: operation.method,
        path,
        query,
        body,
    })
}

/// The text of a successful answer of `operation`: the JSON or the text
/// as the API sent it, the lines of an event stream, or the status when
/// there is no body.
pub(crate) fn answer_text(operation: &Operation, success: &Success) -> String {
    match operation.response {
        Response::Events => events_text(&success.body),
        _ if success.body.trim().is_empty() => format!("{} (no content)", success.status),
        _ => success.body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> &'static Operation {
        liftoff_api::operation(name).unwrap_or_else(|| panic!("no operation {name}"))
    }

    mod call {
        use super::*;

        // TC: MCP-03
        #[test]
        fn segment_encoded_when_path_parameter_has_a_slash() {
            let built = call(
                named("projects_get"),
                json!({"p": "a/b"}).as_object().cloned().unwrap_or_default(),
            );
            assert_eq!(
                built.map(|built| built.path),
                Ok("/api/v1/projects/a%2Fb".to_owned())
            );
        }

        // TC: MCP-03 (a dot segment would be folded away by the URL
        // parser and send the call to another route)
        #[test]
        fn refused_when_path_parameter_is_a_dot_segment() {
            let refused: Vec<Result<Call, String>> = ["", ".", ".."]
                .into_iter()
                .map(|e| {
                    call(
                        named("environments_delete"),
                        json!({"p": "shop", "e": e})
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect();
            let message = Err("argument e must be a path segment".to_owned());
            assert_eq!(refused, vec![message.clone(), message.clone(), message]);
        }

        // TC: MCP-03 (a `%` is encoded, so `%2E%2E` stays a name and is
        // never decoded into a dot segment)
        #[test]
        fn percent_encoded_when_path_parameter_spells_dots_encoded() {
            let built = call(
                named("projects_get"),
                json!({"p": "%2E%2E"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            assert_eq!(
                built.map(|built| built.path),
                Ok("/api/v1/projects/%252E%252E".to_owned())
            );
        }

        // TC: MCP-03 (a tool answers once: `follow=true` would stream
        // without end)
        #[test]
        fn refused_when_logs_called_with_follow() {
            assert_eq!(
                call(
                    named("environments_logs"),
                    json!({"p": "shop", "e": "prod", "follow": true})
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                ),
                Err(
                    "argument follow must be false: a tool answers once and cannot follow a \
                     stream (read the resource liftoff://projects/{p}/environments/{e}/log)"
                        .to_owned()
                )
            );
        }

        // TC: MCP-03 (`follow=false` passes)
        #[test]
        fn follow_false_kept_when_logs_called_with_follow_false() {
            let built = call(
                named("environments_logs"),
                json!({"p": "shop", "e": "prod", "follow": false})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            assert_eq!(
                built.map(|built| built.query),
                Ok(vec![("follow".to_owned(), "false".to_owned())])
            );
        }

        // TC: MCP-03
        #[test]
        fn missing_argument_named_when_path_parameter_absent() {
            assert_eq!(
                call(named("projects_get"), Map::new()),
                Err("missing argument p".to_owned())
            );
        }

        // TC: MCP-03
        #[test]
        fn unknown_arguments_refused_when_operation_takes_no_body() {
            assert_eq!(
                call(
                    named("projects_list"),
                    json!({"name": "shop"})
                        .as_object()
                        .cloned()
                        .unwrap_or_default()
                ),
                Err("unknown arguments: name".to_owned())
            );
        }

        // TC: MCP-03 (a tool answers once: the logs are asked without follow)
        #[test]
        fn follow_false_when_logs_called_without_follow() {
            let built = call(
                named("environments_logs"),
                json!({"p": "shop", "e": "prod", "tail": 50})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            assert_eq!(
                built.map(|built| built.query),
                Ok(vec![
                    ("tail".to_owned(), "50".to_owned()),
                    ("follow".to_owned(), "false".to_owned()),
                ])
            );
        }
    }

    mod input_schema {
        use super::*;

        /// A body whose field is a type of its own (`$defs`), as
        /// `schemars` writes a nested struct or an enum.
        fn nested() -> Value {
            json!({
                "type": "object",
                "properties": {"target": {"$ref": "#/$defs/Target"}},
                "required": ["target"],
                "$defs": {"Target": {"type": "string", "enum": ["a", "b"]}}
            })
        }

        // TC: MCP-02 (the `$defs` of the body travel with it, so its
        // `$ref`s resolve inside the input schema)
        #[test]
        fn refs_resolve_when_body_nests_a_type() {
            let operation = Operation {
                request: Some(liftoff_api::TypeRef {
                    name: "Nested",
                    json_schema: nested,
                }),
                ..*named("projects_create")
            };
            let schema = Value::Object(input_schema(&operation));
            let target = schema["properties"]["target"]["$ref"]
                .as_str()
                .and_then(|reference| reference.strip_prefix('#'))
                .and_then(|pointer| schema.pointer(pointer))
                .cloned();
            assert_eq!(target, Some(json!({"type": "string", "enum": ["a", "b"]})));
        }
    }

    mod module_tools {
        use super::*;

        // TC: MCP-04
        #[test]
        fn api_tool_wins_when_module_tool_takes_its_name() {
            let modules: Vec<ModuleView> = serde_json::from_value(json!([{
                "name": "projects",
                "version": "0.1.0",
                "enabled": true,
                "error": null,
                "installed_at": "2026-10-20T10:00:00Z",
                "commands": [],
                "routes": [],
                "tools": [{"name": "list", "description": "Shadow", "schema": {"type": "object"}}],
                "config_schema": null
            }]))
            .unwrap_or_default();
            assert_eq!((modules.len(), module_tools(&modules).len()), (1, 0));
        }
    }
}
