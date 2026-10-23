//! `web`: the module of the panel `liftoff-web` (the `web` row, 5.2).
//!
//! The panel is a project of its own, deployed like any other, that
//! talks to the REST API from its own origin. The module keeps that
//! origin and says which of the daemon's pages the panel takes over:
//!
//! | Field | Meaning |
//! |---|---|
//! | `origin` | the panel's origin, `https://<host>[:<port>]` (plain `http://` only on `localhost` or `127.0.0.1`, for development) |
//! | `take_pages` | optional, `false` by default: the panel renders the login, forbidden, unavailable and offline pages |
//!
//! Its hooks answer data (`handle-route` on paths below `/.hook/`):
//! `web_panel` answers `200 {"origin"}` from the configuration (the core
//! then answers CORS for that origin on `/api/v1` and sets the browser
//! session's cookie `SameSite=None`); `render_page(<kind>)` sends the
//! browser to `<origin>/<kind>?return=<url>&host=<host>` while
//! `take_pages` is set, else has no opinion. No routes, no tables, no
//! outgoing HTTP.

#[expect(
    clippy::error_impl_error,
    clippy::mem_forget,
    clippy::same_length_and_capacity,
    reason = "code wit-bindgen generates"
)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "liftoff:module/liftoff-module",
        generate_all,
    });
}

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error};
use serde_json::{Value, json};

/// The component's export.
struct Web;

/// The JSON Schema of the configuration.
fn schema() -> Value {
    json!({
        "type": "object",
        "required": ["origin"],
        "additionalProperties": false,
        "properties": {
            "origin": {
                "type": "string",
                "pattern": "^(https://[^/?#@\\s]+|http://(localhost|127\\.0\\.0\\.1)(:[0-9]+)?)$",
                "description": "Origin of the panel liftoff-web, https://<host>[:<port>] without a path (http:// only on localhost or 127.0.0.1)",
            },
            "take_pages": {
                "type": "boolean",
                "default": false,
                "description": "The panel renders the login, forbidden, unavailable and offline pages",
            },
        },
    })
}

/// The pages the panel takes over while `take_pages` is set: the core's
/// `login`, `forbidden` (403), `unavailable` (503) and `offline`.
const PAGES: [&str; 4] = ["login", "forbidden", "unavailable", "offline"];

/// `origin` of the configuration, if set.
fn origin() -> Option<String> {
    host::config_get("origin")
        .map(|origin| origin.trim().trim_end_matches('/').to_owned())
        .filter(|origin| !origin.is_empty())
}

/// Whether `take_pages` is set (a boolean is handed over as its JSON
/// text).
fn take_pages() -> bool {
    host::config_get("take_pages").is_some_and(|value| value.trim() == "true")
}

/// `web_panel()`: `200 {"origin"}` of the configuration, `404` while it
/// is not set.
fn panel() -> Response {
    origin().map_or_else(
        || problem(404, "Not Found", "the web module has no origin configured"),
        |origin| Response {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: json!({ "origin": origin }).to_string().into_bytes(),
        },
    )
}

/// `render_page(kind)` with the core's `host` and `return` in `query`:
/// `302` to `<origin>/<kind>?return=<url>&host=<host>` (each when the
/// core sent it) while `take_pages` is set, else `404`, no opinion.
fn render_page(kind: &str, query: &str) -> Response {
    let Some(origin) = origin().filter(|_| take_pages()) else {
        return problem(404, "Not Found", "the panel does not take this page");
    };
    let params: Vec<String> = ["return", "host"]
        .into_iter()
        .filter_map(|name| {
            let value = query_value(query, name)?;
            Some(format!("{name}={}", encode(&value)))
        })
        .collect();
    let mut location = format!("{origin}/{kind}");
    if !params.is_empty() {
        location.push('?');
        location.push_str(&params.join("&"));
    }
    Response {
        status: 302,
        headers: vec![
            ("location".to_owned(), location),
            ("cache-control".to_owned(), "no-store".to_owned()),
        ],
        body: Vec::new(),
    }
}

/// The decoded value of `name` in the query `query`, if present.
fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(key) == name).then(|| decode(value))
    })
}

/// Percent-decodes a query component (`+` is a space); a broken escape
/// is kept as it is.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let escaped = (byte == b'%')
            .then(|| bytes.get(at.saturating_add(1)..at.saturating_add(3)))
            .flatten()
            .and_then(|hex| core::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(decoded) = escaped {
            out.push(decoded);
            at = at.saturating_add(3);
        } else {
            out.push(if byte == b'+' { b' ' } else { byte });
            at = at.saturating_add(1);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes everything but the unreserved characters (RFC 3986).
fn encode(text: &str) -> String {
    use core::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _written = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A problem document (RFC 9457) with `detail`.
fn problem(status: u16, title: &str, detail: &str) -> Response {
    Response {
        status,
        headers: vec![(
            "content-type".to_owned(),
            "application/problem+json".to_owned(),
        )],
        body: json!({"type": "about:blank", "title": title, "status": status, "detail": detail})
            .to_string()
            .into_bytes(),
    }
}

impl Guest for Web {
    fn manifest() -> String {
        json!({
            "name": "web",
            "version": env!("CARGO_PKG_VERSION"),
            "config_schema": schema(),
            "hooks": ["web_panel", "render_page"],
        })
        .to_string()
    }

    fn migrate(_from: u32) -> Result<(), Error> {
        // No tables.
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        let (path, query) = req.path.split_once('?').unwrap_or((req.path.as_str(), ""));
        match (req.method.as_str(), path) {
            ("POST", "/.hook/web_panel") => panel(),
            ("GET", page) => page
                .strip_prefix("/.hook/render_page/")
                .filter(|kind| PAGES.contains(kind))
                .map_or_else(
                    || problem(404, "Not Found", "no such route"),
                    |kind| render_page(kind, query),
                ),
            _ => problem(404, "Not Found", "no such route"),
        }
    }

    fn handle_tool(name: String, _args: String) -> Result<String, Error> {
        Err(Error {
            code: "not_found".to_owned(),
            message: format!("tool {name}"),
        })
    }

    fn on_hook(_hook: String, _payload: String) -> Result<HookResult, Error> {
        Ok(HookResult::Continue)
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`Web`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Web;

    crate::bindings::export!(Web with_types_in crate::bindings);
}
