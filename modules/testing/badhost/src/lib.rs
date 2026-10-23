//! `badhost`: a contract component of the module host (MOD-07).
//!
//! Its manifest's `http_allow` is the comma-separated configuration
//! value `http_allow` (so a check can allow a listener's port), else
//! `["127.0.0.1"]`, serialised by `serde_json`; its tool `fetch`
//! (`{"url"}`) sends `GET <url>` through `wasi:http/outgoing-handler` to
//! whatever host the URL names and answers the response status as a JSON
//! number. A refused or failed request is the tool's error with code
//! `http` and the `wasi:http` error code as its message. Its tool `hoard`
//! (`{"count"}`) makes `count` `wasi:http` field sets and holds them all
//! until it answers `count`: handles the host keeps for it (MOD-10).

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
use bindings::liftoff::module::host::{self, Error};
use bindings::wasi::http::outgoing_handler;
use bindings::wasi::http::types::{Fields, Method, OutgoingRequest, Scheme};
use serde::Deserialize;

/// The component's export.
struct BadHost;

#[derive(Deserialize)]
struct Fetch {
    url: String,
}

#[derive(Deserialize)]
struct Hoard {
    count: u32,
}

fn error(code: &str, message: impl Into<String>) -> Error {
    Error {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// `GET url`; the response status.
fn fetch(url: &str) -> Result<u16, Error> {
    let invalid = || error("validation", format!("url {url}"));
    let (scheme, rest) = url.split_once("://").ok_or_else(invalid)?;
    let scheme = match scheme {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        _ => return Err(invalid()),
    };
    let (authority, path) = rest.split_once('/').map_or_else(
        || (rest, "/".to_owned()),
        |(authority, path)| (authority, format!("/{path}")),
    );
    let request = OutgoingRequest::new(Fields::new());
    request.set_method(&Method::Get).map_err(|()| invalid())?;
    request.set_scheme(Some(&scheme)).map_err(|()| invalid())?;
    request
        .set_authority(Some(authority))
        .map_err(|()| invalid())?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|()| invalid())?;
    let http = |code: &dyn core::fmt::Debug| error("http", format!("{code:?}"));
    let pending = outgoing_handler::handle(request, None).map_err(|code| http(&code))?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or_else(|| error("http", "no response"))?
        .map_err(|()| error("http", "response taken twice"))?
        .map_err(|code| http(&code))?;
    Ok(response.status())
}

impl Guest for BadHost {
    fn manifest() -> String {
        let allow: Vec<String> = host::config_get("http_allow").map_or_else(
            || vec!["127.0.0.1".to_owned()],
            |listed| listed.split(',').map(ToOwned::to_owned).collect(),
        );
        serde_json::json!({"name": "badhost", "version": "0.1.0", "http_allow": allow}).to_string()
    }

    fn migrate(_from: u32) -> Result<(), Error> {
        Ok(())
    }

    fn handle_route(_req: Request) -> Response {
        Response {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn handle_tool(name: String, args: String) -> Result<String, Error> {
        match name.as_str() {
            "fetch" => {
                let Fetch { url } = serde_json::from_str(&args)
                    .map_err(|why| error("validation", why.to_string()))?;
                fetch(&url).map(|status| status.to_string())
            }
            "hoard" => {
                let Hoard { count } = serde_json::from_str(&args)
                    .map_err(|why| error("validation", why.to_string()))?;
                let mut held = Vec::new();
                for _ in 0..count {
                    held.push(Fields::new());
                }
                Ok(held.len().to_string())
            }
            other => Err(error("not_found", format!("tool {other}"))),
        }
    }

    fn on_hook(_hook: String, _payload: String) -> Result<HookResult, Error> {
        Ok(HookResult::Continue)
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`BadHost`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::BadHost;

    crate::bindings::export!(BadHost with_types_in crate::bindings);
}
