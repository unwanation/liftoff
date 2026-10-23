//! The host side of the WIT world `liftoff:module/liftoff-module`.

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "liftoff:module/liftoff-module",
    imports: { default: async | trappable },
    exports: { default: async },
    with: {
        "wasi:http": wasmtime_wasi_http::p2::bindings::http,
        "wasi:io": wasmtime_wasi::p2::bindings::io,
    },
});
