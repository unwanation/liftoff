//! `wasi:keyvalue` of a function (`store`, `atomics`, `batch`; FN-03, FN-04): the environment's rows of the `kv` table through the
//! [`Store`] port.
//!
//! - Each environment has one bucket, opened as `default` (or `""`); any
//!   other identifier is `no-such-store`. Environments never see each
//!   other's keys: the environment is the instance's, not the guest's.
//! - A write that would make the environment's keys and values pass
//!   [`FunctionLimits::kv_max_bytes`](super::FunctionLimits::kv_max_bytes)
//!   (`functions.kv_max_mb`) is the function's `other` error and writes
//!   nothing (FN-04); so is a key longer than
//!   [`MAX_KV_KEY_BYTES`](liftoff_core::service::MAX_KV_KEY_BYTES). The
//!   rules are the core's ([`check_kv_write`], [`check_kv_key`]); this
//!   adapter reads the sizes they need and writes.
//! - The writes of one environment run one at a time in this runtime, so
//!   `increment` and the limit's check are atomic among its instances.
//! - `atomics.increment` keeps the counter as its decimal text (what
//!   wasmtime's own `wasi:keyvalue` does), so `get` reads `"3"`.
//! - `list-keys` is not supported (the port has no listing): `other`.
//! - A failure of the store itself traps the request with a
//!   [`HostFault`]: the daemon's failure, `HostFailed`, not the function's.

use core::future::ready;
use std::collections::BTreeMap;
use std::sync::Arc;

use liftoff_core::domain::EnvironmentId;
use liftoff_core::ports::Store;
use liftoff_core::service::{KvRefusal, KvWrite, check_kv_key, check_kv_write};
use time::OffsetDateTime;
use wasmtime::component::Resource;

use super::pool::{FnState, HostFault};

/// The host side of `wasi:keyvalue/imports`.
#[expect(
    clippy::error_impl_error,
    clippy::missing_asserts_for_indexing,
    reason = "code wasmtime's bindgen! generates"
)]
pub(super) mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/function/deps/keyvalue",
        world: "wasi:keyvalue/imports",
        imports: { default: async | trappable },
        with: {
            "wasi:keyvalue/store.bucket": super::Bucket,
        },
    });
}

use bindings::wasi::keyvalue::{atomics, batch, store};

/// A `wasi:keyvalue/store.bucket`: the environment's one bucket.
#[derive(Debug)]
#[expect(
    unreachable_pub,
    reason = "bindgen! re-exports the resource type with `pub use`"
)]
pub struct Bucket;

/// What a `wasi:keyvalue` call answers: the daemon's failure as a trap,
/// the function's error as its `store.error`.
type Answer<T> = wasmtime::Result<Result<T, store::Error>>;

/// The `kv` rows of one environment, as its instances reach them.
pub(super) struct Kv<S> {
    pub(super) store: Arc<S>,
    pub(super) environment: EnvironmentId,
    /// `functions.kv_max_mb`, in bytes.
    pub(super) max_bytes: u64,
    /// Held by every write of the environment in this runtime.
    pub(super) writes: Arc<tokio::sync::Mutex<()>>,
}

/// The trap of a failed store under a `wasi:keyvalue` call.
fn fault(environment: EnvironmentId, error: &liftoff_core::Error) -> wasmtime::Error {
    tracing::error!(%environment, %error, "the store behind wasi:keyvalue failed");
    wasmtime::Error::new(HostFault)
}

/// The function's `other` error for a refusal of the core's rules.
fn refused(environment: EnvironmentId, refusal: &KvRefusal) -> store::Error {
    tracing::warn!(%environment, %refusal, "a function's wasi:keyvalue call is refused");
    store::Error::Other(refusal.to_string())
}

impl<S: Store> Kv<S> {
    async fn get(&self, key: &str) -> Answer<Option<Vec<u8>>> {
        if let Err(refusal) = check_kv_key(key) {
            return Ok(Err(refused(self.environment, &refusal)));
        }
        self.store
            .kv_get(self.environment, key)
            .await
            .map(Ok)
            .map_err(|error| fault(self.environment, &error))
    }

    /// Writes `values` (key → value), all or none: none when one key is
    /// too long or the environment's keys and values would pass
    /// `kv_max_mb`. The caller holds [`Kv::writes`].
    async fn put(&self, values: &BTreeMap<String, Vec<u8>>) -> Answer<()> {
        let env = self.environment;
        if let Some(refusal) = values.keys().find_map(|key| check_kv_key(key).err()) {
            return Ok(Err(refused(env, &refusal)));
        }
        let failed = |error: liftoff_core::Error| fault(env, &error);
        let usage = self.store.kv_usage(env).await.map_err(failed)?;
        let mut writes = Vec::with_capacity(values.len());
        for (key, value) in values {
            let old = self.store.kv_get(env, key).await.map_err(failed)?;
            writes.push(KvWrite {
                key,
                old: old.map(|old| old.len()),
                new: value.len(),
            });
        }
        if let Err(refusal) = check_kv_write(usage, writes, self.max_bytes) {
            return Ok(Err(refused(env, &refusal)));
        }
        let at = OffsetDateTime::now_utc();
        for (key, value) in values {
            self.store
                .kv_set(env, key, value, at)
                .await
                .map_err(failed)?;
        }
        Ok(Ok(()))
    }

    async fn set(&self, key: String, value: Vec<u8>) -> Answer<()> {
        let _writing = self.writes.lock().await;
        self.put(&BTreeMap::from([(key, value)])).await
    }

    async fn delete(&self, keys: &[String]) -> Answer<()> {
        if let Some(refusal) = keys.iter().find_map(|key| check_kv_key(key).err()) {
            return Ok(Err(refused(self.environment, &refusal)));
        }
        let _writing = self.writes.lock().await;
        for key in keys {
            self.store
                .kv_delete(self.environment, key)
                .await
                .map_err(|error| fault(self.environment, &error))?;
        }
        Ok(Ok(()))
    }

    async fn increment(&self, key: String, delta: u64) -> Answer<u64> {
        let _writing = self.writes.lock().await;
        let current = match self.get(&key).await? {
            Err(refused) => return Ok(Err(refused)),
            Ok(None) => 0,
            Ok(Some(raw)) => {
                let parsed = core::str::from_utf8(&raw)
                    .ok()
                    .and_then(|text| text.parse::<u64>().ok());
                let Some(current) = parsed else {
                    return Ok(Err(store::Error::Other(format!(
                        "the value of {key} is not a counter (a decimal number)"
                    ))));
                };
                current
            }
        };
        let Some(next) = current.checked_add(delta) else {
            return Ok(Err(store::Error::Other(format!(
                "incrementing {key} overflows"
            ))));
        };
        let written = self
            .put(&BTreeMap::from([(key, next.to_string().into_bytes())]))
            .await?;
        Ok(written.map(|()| next))
    }
}

impl<S: Store + 'static> store::Host for FnState<S> {
    fn open(
        &mut self,
        identifier: String,
    ) -> impl Future<Output = Answer<Resource<Bucket>>> + Send {
        ready(if identifier.is_empty() || identifier == "default" {
            self.table
                .push(Bucket)
                .map(Ok)
                .map_err(wasmtime::Error::from)
        } else {
            Ok(Err(store::Error::NoSuchStore))
        })
    }
}

impl<S: Store + 'static> store::HostBucket for FnState<S> {
    async fn get(&mut self, self_: Resource<Bucket>, key: String) -> Answer<Option<Vec<u8>>> {
        let _bucket = self.table.get(&self_)?;
        self.kv.get(&key).await
    }

    async fn set(&mut self, self_: Resource<Bucket>, key: String, value: Vec<u8>) -> Answer<()> {
        let _bucket = self.table.get(&self_)?;
        self.kv.set(key, value).await
    }

    async fn delete(&mut self, self_: Resource<Bucket>, key: String) -> Answer<()> {
        let _bucket = self.table.get(&self_)?;
        self.kv.delete(&[key]).await
    }

    async fn exists(&mut self, self_: Resource<Bucket>, key: String) -> Answer<bool> {
        let _bucket = self.table.get(&self_)?;
        Ok(self.kv.get(&key).await?.map(|found| found.is_some()))
    }

    fn list_keys(
        &mut self,
        self_: Resource<Bucket>,
        _cursor: Option<u64>,
    ) -> impl Future<Output = Answer<store::KeyResponse>> + Send {
        ready(
            self.table
                .get(&self_)
                .map_err(wasmtime::Error::from)
                .map(|_bucket| {
                    Err(store::Error::Other(
                        "list-keys is not supported by liftoff".to_owned(),
                    ))
                }),
        )
    }

    fn drop(&mut self, rep: Resource<Bucket>) -> impl Future<Output = wasmtime::Result<()>> + Send {
        ready(
            self.table
                .delete(rep)
                .map(|_bucket| ())
                .map_err(wasmtime::Error::from),
        )
    }
}

impl<S: Store + 'static> atomics::Host for FnState<S> {
    async fn increment(
        &mut self,
        bucket: Resource<Bucket>,
        key: String,
        delta: u64,
    ) -> Answer<u64> {
        let _bucket = self.table.get(&bucket)?;
        self.kv.increment(key, delta).await
    }
}

impl<S: Store + 'static> batch::Host for FnState<S> {
    async fn get_many(
        &mut self,
        bucket: Resource<Bucket>,
        keys: Vec<String>,
    ) -> Answer<Vec<Option<(String, Vec<u8>)>>> {
        let _bucket = self.table.get(&bucket)?;
        let mut found = Vec::with_capacity(keys.len());
        for key in keys {
            match self.kv.get(&key).await? {
                Err(refused) => return Ok(Err(refused)),
                Ok(value) => found.push(value.map(|value| (key, value))),
            }
        }
        Ok(Ok(found))
    }

    async fn set_many(
        &mut self,
        bucket: Resource<Bucket>,
        key_values: Vec<(String, Vec<u8>)>,
    ) -> Answer<()> {
        let _bucket = self.table.get(&bucket)?;
        // The last value of a key repeated in the batch wins.
        let values: BTreeMap<String, Vec<u8>> = key_values.into_iter().collect();
        let _writing = self.kv.writes.lock().await;
        self.kv.put(&values).await
    }

    async fn delete_many(&mut self, bucket: Resource<Bucket>, keys: Vec<String>) -> Answer<()> {
        let _bucket = self.table.get(&bucket)?;
        self.kv.delete(&keys).await
    }
}
