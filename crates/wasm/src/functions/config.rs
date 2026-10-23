//! `wasi:config/store` of a function: the resolved secrets of its
//! environment, environment over project (FN-02).

use liftoff_core::ports::Store;

use super::pool::FnState;

/// The host side of `wasi:config/imports`.
#[expect(
    clippy::error_impl_error,
    reason = "code wasmtime's bindgen! generates"
)]
pub(super) mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/function/deps/config",
        world: "wasi:config/imports",
        imports: { default: trappable },
    });
}

use bindings::wasi::config::store;

impl<S: Store + 'static> store::Host for FnState<S> {
    fn get(&mut self, key: String) -> wasmtime::Result<Result<Option<String>, store::Error>> {
        Ok(Ok(self.config.get(&key).cloned()))
    }

    fn get_all(&mut self) -> wasmtime::Result<Result<Vec<(String, String)>, store::Error>> {
        Ok(Ok(self
            .config
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()))
    }
}
