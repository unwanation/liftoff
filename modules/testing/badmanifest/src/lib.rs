//! `badmanifest`: a contract component of the module host (MOD-06).
//!
//! Its manifest has a field the world does not know, `colour`, so the
//! host refuses to load it. Its exports answer nothing useful.

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
use bindings::liftoff::module::host::Error;

/// The component's export.
struct BadManifest;

impl Guest for BadManifest {
    fn manifest() -> String {
        r#"{"name":"badmanifest","version":"0.1.0","colour":"blue"}"#.to_owned()
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

/// The exported symbols `wit-bindgen` generates for [`BadManifest`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::BadManifest;

    crate::bindings::export!(BadManifest with_types_in crate::bindings);
}
