//! `probe`: the contract component of the module host (MOD-01..03).
//!
//! It exercises the storage and the scheduler of the module host through
//! its tools (`handle-tool`), so a host test can drive it:
//!
//! | Tool | Arguments | What it does |
//! |---|---|---|
//! | `kv-set` | `{"key","value"}` | `store-set`; answers `null` |
//! | `kv-get` | `{"key"}` | `store-get`; the value as a JSON string or `null` |
//! | `items-add` | `{"name"}` | inserts into its table `m_probe_items` |
//! | `items-list` | | the rows of `m_probe_items`, oldest first |
//! | `read-users` | | tries `SELECT login FROM users` (the core's table) |
//! | `query` | `{"sql","params"}` | any statement through `store-query` |
//! | `schedule` | `{"name","every_secs"}` | `schedule`; answers `null` |
//! | `trap` | | panics: the call traps |
//! | `log` | `{"message","times"}` | `log` at `info` that many times (default 1); answers `null` |
//! | `grow` | `{"bytes"}` | allocates and fills that many bytes; answers `null` (traps when its memory cap is reached) |
//!
//! Plan makes it an installable module too, as the core's fake
//! `PROBE_MANIFEST` describes it: a configuration with a required
//! `greeting` and a secret `token`, the CLI command `hello` with a required
//! `who`, and its route `POST /hello`, which answers `<greeting>, <who>`
//! (the `greeting` from `config-get`) for the body `{"who"}`; every other
//! route answers 404.
//!
//! `store-query` errors come back unchanged as the tool's error.
//! `migrate(0)` creates `m_probe_items (name TEXT NOT NULL)`. `tick(name)`
//! logs `tick <name> begin`, counts the call in the key `ticks:<name>`
//! (decimal text) and logs `tick <name> end`.

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
struct Probe;

#[derive(Deserialize)]
struct KeyValue {
    key: String,
    value: String,
}

#[derive(Deserialize)]
struct Key {
    key: String,
}

#[derive(Deserialize)]
struct Item {
    name: String,
}

#[derive(Deserialize)]
struct Query {
    sql: String,
    #[serde(default)]
    params: Vec<String>,
}

#[derive(Deserialize)]
struct Hello {
    who: String,
}

#[derive(Deserialize)]
struct Grow {
    bytes: usize,
}

#[derive(Deserialize)]
struct Log {
    message: String,
    #[serde(default = "once")]
    times: u32,
}

const fn once() -> u32 {
    1
}

#[derive(Deserialize)]
struct Schedule {
    name: String,
    every_secs: u32,
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

fn json(value: &serde_json::Value) -> String {
    value.to_string()
}

fn ticks_key(name: &str) -> String {
    format!("ticks:{name}")
}

impl Guest for Probe {
    fn manifest() -> String {
        json(&serde_json::json!({
            "name": "probe",
            "version": "0.1.0",
            "config_schema": {
                "type": "object",
                "properties": {
                    "greeting": {"type": "string", "description": "How to greet"},
                    "token": {"type": "string", "format": "secret", "description": "A token to keep secret"},
                },
                "required": ["greeting"],
                "additionalProperties": false,
            },
            "commands": [{
                "name": "hello",
                "description": "Says hello",
                "args": [{"name": "who", "type": "string", "required": true, "hint": "Who to greet"}],
            }],
            "routes": [{"method": "POST", "path": "/hello"}],
            "migrations": 1,
        }))
    }

    fn migrate(from: u32) -> Result<(), Error> {
        if from == 0 {
            host::store_query("CREATE TABLE m_probe_items (name TEXT NOT NULL)", &[])?;
        }
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        let respond = |status: u16, body: String| Response {
            status,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: body.into_bytes(),
        };
        if req.method != "POST" || req.path != "/hello" {
            return respond(404, String::new());
        }
        match serde_json::from_slice::<Hello>(&req.body) {
            Ok(Hello { who }) => {
                let greeting = host::config_get("greeting").unwrap_or_else(|| "hello".to_owned());
                respond(200, format!("{greeting}, {who}"))
            }
            Err(why) => respond(400, why.to_string()),
        }
    }

    fn handle_tool(name: String, args: String) -> Result<String, Error> {
        match name.as_str() {
            "kv-set" => {
                let KeyValue { key, value } = parse(&args)?;
                host::store_set(&key, value.as_bytes());
                Ok(json(&serde_json::Value::Null))
            }
            "kv-get" => {
                let Key { key } = parse(&args)?;
                let value =
                    host::store_get(&key).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
                Ok(json(&serde_json::json!(value)))
            }
            "items-add" => {
                let Item { name } = parse(&args)?;
                host::store_query("INSERT INTO m_probe_items (name) VALUES (?1)", &[name])
            }
            "items-list" => host::store_query("SELECT name FROM m_probe_items ORDER BY rowid", &[]),
            "read-users" => host::store_query("SELECT login FROM users", &[]),
            "query" => {
                let Query { sql, params } = parse(&args)?;
                host::store_query(&sql, &params)
            }
            "schedule" => {
                let Schedule { name, every_secs } = parse(&args)?;
                host::schedule(&name, every_secs);
                Ok(json(&serde_json::Value::Null))
            }
            "trap" => {
                #[expect(clippy::panic, reason = "the tool exists to trap")]
                {
                    panic!("probe traps on request")
                }
            }
            "log" => {
                let Log { message, times } = parse(&args)?;
                for _ in 0..times {
                    host::log(Level::Info, &message);
                }
                Ok(json(&serde_json::Value::Null))
            }
            "grow" => {
                let Grow { bytes } = parse(&args)?;
                let filled = vec![1_u8; bytes];
                core::hint::black_box(&filled);
                Ok(json(&serde_json::Value::Null))
            }
            other => Err(error("not_found", format!("tool {other}"))),
        }
    }

    fn on_hook(_hook: String, _payload: String) -> Result<HookResult, Error> {
        Ok(HookResult::Continue)
    }

    fn tick(name: String) {
        host::log(Level::Info, &format!("tick {name} begin"));
        let key = ticks_key(&name);
        let count = host::store_get(&key)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        host::store_set(&key, count.to_string().as_bytes());
        host::log(Level::Info, &format!("tick {name} end"));
    }
}

/// The exported symbols `wit-bindgen` generates for [`Probe`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Probe;

    crate::bindings::export!(Probe with_types_in crate::bindings);
}
