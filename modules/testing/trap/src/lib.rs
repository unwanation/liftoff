//! `trap`: a contract component of the module host (MOD-09, MOD-10).
//!
//! | Tool | Arguments | What it does |
//! |---|---|---|
//! | `panic` | | panics: the call traps |
//! | `ok` | | answers `"ok"` |
//! | `grow` | `{"bytes"}` as a decimal string | allocates and fills that many bytes; answers `"ok"` (traps past the memory cap) |
//!
//! | Route | What it does |
//! |---|---|
//! | `GET /ok` | answers `200` with the request's actor (or `none`) as the body |
//! | `GET /panic` | panics: the call traps |
//!
//! | Hook payload | `on-hook` answers |
//! |---|---|
//! | `panic` | panics: the call traps |
//! | `deny` | `deny("denied by trap")` |
//! | `allow` | `allow` |
//! | anything else | `continue` |
//!
//! Its manifest declares both routes and the hook `authorize`.

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
struct Trap;

fn error(code: &str, message: impl Into<String>) -> Error {
    Error {
        code: code.to_owned(),
        message: message.into(),
    }
}

impl Guest for Trap {
    fn manifest() -> String {
        r#"{"name":"trap","version":"0.1.0",
            "routes":[{"method":"GET","path":"/ok"},{"method":"GET","path":"/panic"}],
            "hooks":["authorize"]}"#
            .to_owned()
    }

    fn migrate(_from: u32) -> Result<(), Error> {
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        match req.path.as_str() {
            "/ok" => Response {
                status: 200,
                headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
                body: req.actor.unwrap_or_else(|| "none".to_owned()).into_bytes(),
            },
            "/panic" => {
                #[expect(clippy::panic, reason = "the route exists to trap")]
                {
                    panic!("trap traps on request")
                }
            }
            _ => Response {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
            },
        }
    }

    fn handle_tool(name: String, args: String) -> Result<String, Error> {
        match name.as_str() {
            "panic" => {
                #[expect(clippy::panic, reason = "the tool exists to trap")]
                {
                    panic!("trap traps on request")
                }
            }
            "ok" => Ok("\"ok\"".to_owned()),
            "grow" => {
                let bytes: usize =
                    args.trim()
                        .parse()
                        .map_err(|why: core::num::ParseIntError| {
                            error("validation", why.to_string())
                        })?;
                let filled = vec![1_u8; bytes];
                core::hint::black_box(&filled);
                Ok("\"ok\"".to_owned())
            }
            other => Err(error("not_found", format!("tool {other}"))),
        }
    }

    fn on_hook(_hook: String, payload: String) -> Result<HookResult, Error> {
        match payload.as_str() {
            "panic" => {
                #[expect(clippy::panic, reason = "the hook exists to trap")]
                {
                    panic!("trap traps on request")
                }
            }
            "deny" => Ok(HookResult::Deny("denied by trap".to_owned())),
            "allow" => Ok(HookResult::Allow),
            _ => Ok(HookResult::Continue),
        }
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`Trap`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Trap;

    crate::bindings::export!(Trap with_types_in crate::bindings);
}
