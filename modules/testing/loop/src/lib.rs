//! `loop`: a contract component of the module host (MOD-08).
//!
//! | Tool | Arguments | What it does |
//! |---|---|---|
//! | `spin` | | never returns |
//! | `ping` | | answers `"pong"` |
//!
//! Its manifest is `{"name":"loop","version":"0.1.0"}`.

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
struct Loop;

/// Spins until the host interrupts it.
const fn spin() -> ! {
    let mut turns = 0_u64;
    loop {
        turns = core::hint::black_box(turns.wrapping_add(1));
    }
}

impl Guest for Loop {
    fn manifest() -> String {
        r#"{"name":"loop","version":"0.1.0"}"#.to_owned()
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
        match name.as_str() {
            "spin" => spin(),
            "ping" => Ok("\"pong\"".to_owned()),
            other => Err(Error {
                code: "not_found".to_owned(),
                message: format!("tool {other}"),
            }),
        }
    }

    fn on_hook(_hook: String, _payload: String) -> Result<HookResult, Error> {
        Ok(HookResult::Continue)
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`Loop`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Loop;

    crate::bindings::export!(Loop with_types_in crate::bindings);
}
