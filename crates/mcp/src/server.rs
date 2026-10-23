//! The MCP handler: `tools/list`, `tools/call`, the resource templates
//! and `resources/read`, each answered through the API.

use liftoff_api::ModuleView;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListResourceTemplatesResult, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ResourceContents,
    ResourceTemplate, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};

use crate::api::{Api, Call, Failure, events_text};
use crate::{Session, aliases, tools};

/// The template of a deploy's log.
pub(crate) const DEPLOY_LOG: &str = "liftoff://deploys/{id}/log";
/// The template of an environment's container log.
pub(crate) const ENVIRONMENT_LOG: &str = "liftoff://projects/{p}/environments/{e}/log";
/// How many lines of container log a read returns.
const ENVIRONMENT_LOG_TAIL: &str = "500";

/// The server: of one session (stdio), or of the session each HTTP
/// request carries (Streamable HTTP: the transport puts the [`Session`]
/// its backend granted into the request's extensions, which `rmcp` hands
/// every handler call in `http::request::Parts`).
#[derive(Debug, Clone)]
pub(crate) struct Server {
    api: Api,
    per_request: bool,
}

impl Server {
    /// The server calling the API with `api`'s session.
    pub(crate) const fn new(api: Api) -> Self {
        Self {
            api,
            per_request: false,
        }
    }

    /// The server calling the API with the session of each request,
    /// through `api`'s HTTP client.
    pub(crate) const fn per_request(api: Api) -> Self {
        Self {
            api,
            per_request: true,
        }
    }

    /// The API of the call of `context`.
    fn api(&self, context: &RequestContext<RoleServer>) -> Result<Api, ErrorData> {
        if !self.per_request {
            return Ok(self.api.clone());
        }
        context
            .extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Session>())
            .map(|session| self.api.with_session(session))
            .ok_or_else(|| ErrorData::internal_error("the request carries no session", None))
    }
}

/// The calls of one request, with the API of its session.
struct Handler<'a> {
    api: &'a Api,
}

impl Handler<'_> {
    /// The installed modules, or the text of the tool error saying why
    /// the API did not tell.
    async fn modules(&self) -> Result<Vec<ModuleView>, String> {
        let success = self
            .api
            .send(&Call::get("/api/v1/modules".to_owned()))
            .await
            .map_err(|failure| format!("cannot list the modules: {}", failure.text()))?;
        serde_json::from_str(&success.body)
            .map_err(|error| format!("cannot list the modules: the API answered no list ({error})"))
    }

    /// The API tools, then the module tools; the API tools stay usable
    /// when the modules cannot be read (none of theirs is listed then).
    async fn tools(&self) -> Vec<Tool> {
        let mut listed: Vec<Tool> = tools::api_tools().collect();
        listed.extend(aliases::tools());
        listed.extend(tools::module_tools(
            &self.modules().await.unwrap_or_default(),
        ));
        listed
    }

    async fn call(&self, request: CallToolRequestParams) -> CallToolResult {
        let arguments = request.arguments.unwrap_or_default();
        if aliases::NAMES.contains(&request.name.as_ref()) {
            return self.alias_call(&request.name, arguments).await;
        }
        let Some(operation) = tools::operation_of(&request.name) else {
            return self.module_call(&request.name, arguments).await;
        };
        let call = match tools::call(operation, arguments) {
            Ok(call) => call,
            Err(text) => return failed(text),
        };
        match self.api.send(&call).await {
            Ok(success) => CallToolResult::success(vec![ContentBlock::text(tools::answer_text(
                operation, &success,
            ))]),
            Err(failure) => failed(failure.text().to_owned()),
        }
    }

    /// A call of an alias of the standalone artifacts server: the REST call of
    /// its artifact operation, answered with the predecessor's fields.
    async fn alias_call(&self, name: &str, arguments: JsonObject) -> CallToolResult {
        let alias = match aliases::call(name, arguments) {
            None => return failed(format!("no tool {name}")),
            Some(Err(text)) => return failed(text),
            Some(Ok(alias)) => alias,
        };
        match self.api.send(&alias.call).await {
            Ok(success) => alias.answer(&success).map_or_else(
                || CallToolResult::success(vec![ContentBlock::text(success.body)]),
                CallToolResult::structured,
            ),
            Err(failure) => failed(failure.text().to_owned()),
        }
    }

    /// A call of a tool that is no API operation: a module's,
    /// run through `POST /modules/{name}/tools/{tool}`, or none.
    async fn module_call(&self, name: &str, arguments: JsonObject) -> CallToolResult {
        let modules = match self.modules().await {
            Ok(modules) => modules,
            Err(text) => return failed(text),
        };
        let call = match tools::module_call(&modules, name, arguments) {
            None => return failed(format!("no tool {name}")),
            Some(Err(text)) => return failed(text),
            Some(Ok(call)) => call,
        };
        match self.api.send(&call).await {
            Ok(success) => CallToolResult::success(vec![ContentBlock::text(
                tools::module_answer_text(&success),
            )]),
            Err(failure) => failed(failure.text().to_owned()),
        }
    }

    async fn read(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let (call, events) = resource_call(uri)
            .ok_or_else(|| ErrorData::resource_not_found(format!("no resource {uri}"), None))?;
        let success = self
            .api
            .send(&call)
            .await
            .map_err(|failure| match failure {
                Failure::Refused { status: 404, text } => ErrorData::resource_not_found(text, None),
                other => ErrorData::internal_error(other.text().to_owned(), None),
            })?;
        let text = if events {
            events_text(&success.body)
        } else {
            success.body
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text, uri,
        )]))
    }
}

/// A tool error with `text`.
fn failed(text: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}

/// The request that reads `uri`, and whether it answers events.
///
/// Every part of the URI is one path segment of the request: a part that
/// is empty, a dot segment, or holds `%`, `?` or `#` names no resource
/// (it would reshape the path or the query); any other character is
/// percent-encoded.
fn resource_call(uri: &str) -> Option<(Call, bool)> {
    let rest = uri.strip_prefix("liftoff://")?;
    let parts = rest
        .split('/')
        .map(|part| {
            if part.contains(['%', '?', '#']) {
                return None;
            }
            tools::path_segment("uri", part).ok()
        })
        .collect::<Option<Vec<String>>>()?;
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    match parts.as_slice() {
        ["deploys", id, "log"] => Some((Call::get(format!("/api/v1/deploys/{id}/log")), false)),
        ["projects", project, "environments", env, "log"] => {
            let mut call = Call::get(format!(
                "/api/v1/projects/{project}/environments/{env}/logs"
            ));
            call.query = vec![
                ("tail".to_owned(), ENVIRONMENT_LOG_TAIL.to_owned()),
                ("follow".to_owned(), "false".to_owned()),
            ];
            Some((call, true))
        }
        _ => None,
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("liftoff", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "The liftoff API as tools (liftoff_<resource>_<action>) with the caller's session \
             (the CLI config's over stdio, the access token's over HTTP), plus the tools of the installed modules (liftoff_<module>_<tool>). \
             Logs are the resources liftoff://deploys/{id}/log and \
             liftoff://projects/{p}/environments/{e}/log.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let api = self.api(&context)?;
        Ok(ListToolsResult::with_all_items(
            Handler { api: &api }.tools().await,
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let api = self.api(&context)?;
        Ok(Handler { api: &api }.call(request).await.into())
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new(DEPLOY_LOG, "deploy_log")
                .with_description("The log of a deploy")
                .with_mime_type("text/plain"),
            ResourceTemplate::new(ENVIRONMENT_LOG, "environment_log")
                .with_description("The last container log lines of an environment")
                .with_mime_type("text/plain"),
        ])))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let api = self.api(&context)?;
        Ok(Handler { api: &api }.read(&request.uri).await?.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod resource_call {
        use super::*;

        // TC: MCP-05 (an environment log is read once, never followed)
        #[test]
        fn environment_logs_without_follow_when_environment_log_read() {
            assert_eq!(
                resource_call("liftoff://projects/shop/environments/prod/log"),
                Some((
                    Call {
                        method: liftoff_api::Method::Get,
                        path: "/api/v1/projects/shop/environments/prod/logs".to_owned(),
                        query: vec![
                            ("tail".to_owned(), "500".to_owned()),
                            ("follow".to_owned(), "false".to_owned()),
                        ],
                        body: None,
                    },
                    true
                ))
            );
        }

        // TC: MCP-05 (a part of the URI is one path segment: a `?`, `#`,
        // `%` or dot segment would reshape the request it becomes)
        #[test]
        fn none_when_uri_part_reshapes_the_path() {
            let reshaped: Vec<Option<(Call, bool)>> = [
                "liftoff://projects/shop/environments/prod#/log",
                "liftoff://projects/shop/environments/prod?follow=true&x=/log",
                "liftoff://projects/shop/environments/../log",
                "liftoff://projects/./environments/prod/log",
                "liftoff://deploys/%2E%2E/log",
            ]
            .into_iter()
            .map(resource_call)
            .collect();
            assert_eq!(reshaped, vec![None, None, None, None, None]);
        }

        // TC: MCP-05 (any other character is percent-encoded)
        #[test]
        fn part_encoded_when_uri_part_has_a_space() {
            assert_eq!(
                resource_call("liftoff://deploys/a b/log").map(|(call, _)| call.path),
                Some("/api/v1/deploys/a%20b/log".to_owned())
            );
        }

        // TC: MCP-05 (a URI of no template names no resource)
        #[test]
        fn none_when_uri_is_unknown() {
            assert_eq!(
                (
                    resource_call("liftoff://deploys//log"),
                    resource_call("file:///etc/passwd"),
                    resource_call("liftoff://deploys/1/log/x"),
                ),
                (None, None, None)
            );
        }
    }
}
