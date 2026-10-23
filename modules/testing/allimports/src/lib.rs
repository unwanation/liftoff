//! `allimports`: a contract component of the module host: it reaches every `host` import through a tool and
//! declares a manifest with every field (MOD-06, MOD-11).
//!
//! | Tool | Arguments | What it does |
//! |---|---|---|
//! | `core-call` | `{"service","method","args"}` | `core-call`; its answer, or its error as the tool's error |
//! | `config-get` | `{"key"}` | `config-get`; the value as a JSON string or `null` |
//! | `kv-set` | `{"key","value"}` | `store-set`; answers `null` |
//! | `kv-get` | `{"key"}` | `store-get`; the value as a JSON string or `null` |
//! | `query` | `{"sql","params"}` | `store-query`; its rows or its error |
//! | `schedule` | `{"name","every_secs"}` | `schedule`; answers `null` |
//! | `log` | `{"message"}` | `log` at `info`; answers `null` |

#[expect(
    clippy::error_impl_error,
    clippy::mem_forget,
    clippy::same_length_and_capacity,
    reason = "code wit-bindgen generates"
)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../wit",
        world: "liftoff:module/liftoff-module",
        generate_all,
    });
}

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error, Level};
use serde::Deserialize;

/// The component's export.
struct AllImports;

/// The manifest, with every field.
const MANIFEST: &str = r#"{
  "name": "allimports",
  "version": "0.1.0",
  "requires": ["oidc"],
  "config_schema": {"type": "object", "properties": {"greeting": {"type": "string"}}},
  "http_allow": ["127.0.0.1"],
  "commands": [{"name": "greet", "description": "Says hello",
                "args": [{"name": "who", "type": "string", "required": true, "hint": "Who to greet"}]}],
  "routes": [{"method": "GET", "path": "/hello", "public": true}],
  "tools": [{"name": "core-call", "description": "Calls the core", "schema": {"type": "object"}}],
  "migrations": 1,
  "hooks": ["authorize", "before_deploy"],
  "replaces": ["login"],
  "git_provider": {"hosts": ["git.example.org"], "oauth": false}
}"#;

#[derive(Deserialize)]
struct CoreCall {
    service: String,
    method: String,
    args: String,
}

#[derive(Deserialize)]
struct Key {
    key: String,
}

#[derive(Deserialize)]
struct KeyValue {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct Query {
    sql: String,
    #[serde(default)]
    params: Vec<String>,
}

#[derive(Deserialize)]
struct Schedule {
    name: String,
    every_secs: u32,
}

#[derive(Deserialize)]
struct Log {
    message: String,
}

fn error(code: &str, message: impl Into<String>) -> Error {
    Error {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn parse<'a, T: Deserialize<'a>>(raw: &'a str) -> Result<T, Error> {
    serde_json::from_str(raw).map_err(|why| error("validation", why.to_string()))
}

const NULL: &str = "null";

impl Guest for AllImports {
    fn manifest() -> String {
        MANIFEST.to_owned()
    }

    fn migrate(from: u32) -> Result<(), Error> {
        if from == 0 {
            host::store_query("CREATE TABLE m_allimports_rows (value TEXT)", &[])?;
        }
        Ok(())
    }

    fn handle_route(_req: Request) -> Response {
        Response {
            status: 200,
            headers: Vec::new(),
            body: b"hello".to_vec(),
        }
    }

    fn handle_tool(name: String, args: String) -> Result<String, Error> {
        match name.as_str() {
            "core-call" => {
                let CoreCall {
                    service,
                    method,
                    args,
                } = parse(&args)?;
                host::core_call(&service, &method, &args)
            }
            "config-get" => {
                let Key { key } = parse(&args)?;
                Ok(serde_json::json!(host::config_get(&key)).to_string())
            }
            "kv-set" => {
                let KeyValue { key, value } = parse(&args)?;
                host::store_set(&key, value.as_bytes());
                Ok(NULL.to_owned())
            }
            "kv-get" => {
                let Key { key } = parse(&args)?;
                let value =
                    host::store_get(&key).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
                Ok(serde_json::json!(value).to_string())
            }
            "query" => {
                let Query { sql, params } = parse(&args)?;
                host::store_query(&sql, &params)
            }
            "schedule" => {
                let Schedule { name, every_secs } = parse(&args)?;
                host::schedule(&name, every_secs);
                Ok(NULL.to_owned())
            }
            "log" => {
                let Log { message } = parse(&args)?;
                host::log(Level::Info, &message);
                Ok(NULL.to_owned())
            }
            other => Err(error("not_found", format!("tool {other}"))),
        }
    }

    fn on_hook(_hook: String, _payload: String) -> Result<HookResult, Error> {
        Ok(HookResult::Continue)
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`AllImports`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::AllImports;

    crate::bindings::export!(AllImports with_types_in crate::bindings);
}
