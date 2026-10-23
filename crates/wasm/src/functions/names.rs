//! The names a function's component exports and imports, read with
//! `wasmparser` before it compiles (FN-01), so a component the runtime
//! refuses never reaches the engine and its compile cache.
//!
//! - The bytes are a component, not a core module.
//! - It exports `wasi:http/incoming-handler@0.2.x` (the world
//!   `wasi:http/proxy@0.2`).
//! - It imports `wasi:` interfaces only: the runtime links WASI 0.2,
//!   `wasi:config` and `wasi:keyvalue`, and nothing of liftoff's own
//!   (no `store-query`). Whether the types of those imports match is the
//!   linker's check, after the compile.

use wasmparser::{Encoding, Parser, Payload};

use super::FunctionError;

/// The export every function has (the world `wasi:http/proxy@0.2`).
pub(super) const INCOMING_HANDLER: &str = "wasi:http/incoming-handler@0.2";

/// The namespace of every import the runtime links.
const LINKED: &str = "wasi:";

const fn load(reason: String) -> FunctionError {
    FunctionError::Load { reason }
}

/// Whether `name` is `wasi:http/incoming-handler@0.2` or a `0.2.x`.
fn is_incoming_handler(name: &str) -> bool {
    name.strip_prefix(INCOMING_HANDLER)
        .is_some_and(|version| version.is_empty() || version.starts_with('.'))
}

/// Checks the top-level names of `bytes`; a missing export is reported
/// before an import the runtime does not link.
///
/// # Errors
///
/// [`FunctionError::Load`] naming what is wrong.
pub(super) fn check(bytes: &[u8]) -> Result<(), FunctionError> {
    // Nested modules and components have names of their own; only the
    // outermost component's (depth 0) are the function's.
    let mut depth = 0_usize;
    let mut handler = false;
    let mut unlinked: Option<String> = None;
    for payload in Parser::new(0).parse_all(bytes) {
        let payload =
            payload.map_err(|error| load(format!("it is not a WebAssembly component: {error}")))?;
        match payload {
            Payload::Version { encoding, .. } if depth == 0 && encoding != Encoding::Component => {
                return Err(load(
                    "it is a core WebAssembly module, not a component of the world wasi:http/proxy@0.2"
                        .to_owned(),
                ));
            }
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => {
                depth = depth.saturating_add(1);
            }
            Payload::End(_) => depth = depth.saturating_sub(1),
            Payload::ComponentImportSection(imports) if depth == 0 => {
                for import in imports {
                    let import = import
                        .map_err(|error| load(format!("its imports do not parse: {error}")))?;
                    let name = import.name.name;
                    if unlinked.is_none() && !name.starts_with(LINKED) {
                        unlinked = Some(name.to_owned());
                    }
                }
            }
            Payload::ComponentExportSection(exports) if depth == 0 => {
                for export in exports {
                    let export = export
                        .map_err(|error| load(format!("its exports do not parse: {error}")))?;
                    handler |= is_incoming_handler(export.name.name);
                }
            }
            _ => {}
        }
    }
    if !handler {
        return Err(load(format!(
            "it does not export {INCOMING_HANDLER}: a function is a component of the world wasi:http/proxy@0.2"
        )));
    }
    unlinked.map_or(Ok(()), |name| {
        Err(load(format!(
            "it imports {name}, which a function cannot use: a function imports WASI interfaces only"
        )))
    })
}
