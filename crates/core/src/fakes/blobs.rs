//! [`FakeBlobs`].

use core::future::ready;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use super::log::{BlobCall, Call, CallLog, Faults, lock, scriptable};
use crate::domain::{ArtifactId, ArtifactPath, BlobHash, VersionNumber};
use crate::error::Error;
use crate::ports::Blobs;

#[derive(Debug, Default)]
struct BlobState {
    /// The stored blobs by hash.
    blobs: BTreeMap<BlobHash, Vec<u8>>,
    /// The files of versions: `(artifact, version, name) → hash`.
    links: BTreeMap<(ArtifactId, VersionNumber, ArtifactPath), BlobHash>,
    /// The puts not released yet, by hash.
    pending: BTreeMap<BlobHash, usize>,
}

impl BlobState {
    /// Drops the blobs among `removed` that no link names any more and no
    /// `put` waits on.
    fn collect(&mut self, removed: &BTreeSet<BlobHash>) {
        let named: BTreeSet<&BlobHash> = self.links.values().collect();
        let orphans: Vec<BlobHash> = removed
            .iter()
            .filter(|hash| !named.contains(hash) && !self.pending.contains_key(*hash))
            .cloned()
            .collect();
        for hash in orphans {
            let _gone = self.blobs.remove(&hash);
        }
    }
}

/// The [`Blobs`] port in memory, with the port's rules: one blob per
/// hash, links of versions to blobs, and a blob removed with the last
/// link that named it once no `put` of it waits ([`Blobs::release`]).
///
/// Every mutating call is logged ([`BlobCall`]; `read` is not) and
/// scriptable by its name (`fail_next("put", …)`). The stored blobs and
/// the files of each version can be inspected.
#[derive(Debug, Clone)]
pub struct FakeBlobs {
    log: CallLog,
    faults: Faults,
    state: Arc<Mutex<BlobState>>,
}

scriptable!(FakeBlobs);

impl FakeBlobs {
    /// No blobs, logging to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            faults: Faults::default(),
            state: Arc::default(),
        }
    }

    /// The bytes of the blob `hash`, if stored.
    #[must_use]
    pub fn blob(&self, hash: &BlobHash) -> Option<Vec<u8>> {
        lock(&self.state).blobs.get(hash).cloned()
    }

    /// The hashes of every stored blob, sorted.
    #[must_use]
    pub fn blobs(&self) -> Vec<BlobHash> {
        lock(&self.state).blobs.keys().cloned().collect()
    }

    /// The files of version `version` of `artifact` and their blobs, by
    /// name.
    #[must_use]
    pub fn files(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
    ) -> Vec<(ArtifactPath, BlobHash)> {
        lock(&self.state)
            .links
            .iter()
            .filter(|((id, number, _), _)| *id == artifact && *number == version)
            .map(|((_, _, name), hash)| (name.clone(), hash.clone()))
            .collect()
    }

    /// The versions of `artifact` that have files, oldest first.
    #[must_use]
    pub fn versions(&self, artifact: ArtifactId) -> Vec<VersionNumber> {
        let numbers: BTreeSet<VersionNumber> = lock(&self.state)
            .links
            .keys()
            .filter(|(id, _, _)| *id == artifact)
            .map(|(_, number, _)| *number)
            .collect();
        numbers.into_iter().collect()
    }

    /// The hashes `put` was called with, in order.
    #[must_use]
    pub fn puts(&self) -> Vec<BlobHash> {
        self.log
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Blob(BlobCall::Put(hash)) => Some(hash),
                _ => None,
            })
            .collect()
    }

    fn record(&self, call: BlobCall) {
        self.log.push(Call::Blob(call));
    }

    /// Removes the links `matches` selects and the blobs left without one.
    fn unlink(&self, matches: impl Fn(&(ArtifactId, VersionNumber, ArtifactPath)) -> bool) {
        let mut state = lock(&self.state);
        let mut removed = BTreeSet::new();
        state.links.retain(|key, hash| {
            if matches(key) {
                let _new = removed.insert(hash.clone());
                false
            } else {
                true
            }
        });
        state.collect(&removed);
    }
}

impl Blobs for FakeBlobs {
    fn put(&self, hash: &BlobHash, bytes: &[u8]) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(BlobCall::Put(hash.clone()));
        let result = self.faults.check("put");
        if result.is_ok() {
            let mut state = lock(&self.state);
            let _kept = state
                .blobs
                .entry(hash.clone())
                .or_insert_with(|| bytes.to_vec());
            let waiting = state.pending.entry(hash.clone()).or_default();
            *waiting = waiting.saturating_add(1);
            drop(state);
        }
        ready(result)
    }

    fn release(&self, hash: &BlobHash) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(BlobCall::Release(hash.clone()));
        let result = self.faults.check("release");
        if result.is_ok() {
            let mut state = lock(&self.state);
            if let Some(waiting) = state.pending.get_mut(hash) {
                *waiting = waiting.saturating_sub(1);
                if *waiting == 0 {
                    let _released = state.pending.remove(hash);
                }
            }
            state.collect(&BTreeSet::from([hash.clone()]));
            drop(state);
        }
        ready(result)
    }

    fn link(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
        name: &ArtifactPath,
        hash: &BlobHash,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(BlobCall::Link {
            artifact,
            version,
            name: name.clone(),
            hash: hash.clone(),
        });
        let result = self.faults.check("link").and_then(|()| {
            let mut state = lock(&self.state);
            if !state.blobs.contains_key(hash) {
                return Err(Error::not_found(format!("blob {hash}")));
            }
            if let Some(previous) = state
                .links
                .insert((artifact, version, name.clone()), hash.clone())
            {
                state.collect(&BTreeSet::from([previous]));
            }
            drop(state);
            Ok(())
        });
        ready(result)
    }

    fn read(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
        name: &ArtifactPath,
    ) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        let result = self.faults.check("read").and_then(|()| {
            let state = lock(&self.state);
            state
                .links
                .get(&(artifact, version, name.clone()))
                .and_then(|hash| state.blobs.get(hash))
                .cloned()
                .ok_or_else(|| {
                    Error::not_found(format!("{name} of version {version} of {artifact}"))
                })
        });
        ready(result)
    }

    fn remove_version(
        &self,
        artifact: ArtifactId,
        version: VersionNumber,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(BlobCall::RemoveVersion { artifact, version });
        let result = self.faults.check("remove_version");
        if result.is_ok() {
            self.unlink(|(id, number, _)| *id == artifact && *number == version);
        }
        ready(result)
    }

    fn remove_artifact(
        &self,
        artifact: ArtifactId,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.record(BlobCall::RemoveArtifact(artifact));
        let result = self.faults.check("remove_artifact");
        if result.is_ok() {
            self.unlink(|(id, _, _)| *id == artifact);
        }
        ready(result)
    }
}
