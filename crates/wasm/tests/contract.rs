#![cfg(test)]
//! The contract of the module host: MOD-06..MOD-12 of `liftoff_wasm::testing::contract` against
//! the built contract components of `modules/testing/` — `trap`, `loop`,
//! `badhost`, `badmanifest`, `allimports` (and `probe` of) —
//! with the core's
//! `InMemoryStore` behind each host. The CLI host runs the
//! same checks through the same macro over its own store.
//!
//! The components are built by `just modules` (part of `just test` and
//! `just ci`): `cargo build --target wasm32-wasip2 --release` in
//! `modules/`. The checks read them from the directory in
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.

use liftoff_core::fakes::InMemoryStore;

liftoff_wasm::module_host_contract_tests!(InMemoryStore::new);
