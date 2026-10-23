//! [`FakeModuleSource`] and its [`FakeRelease`]s.

use core::future::ready;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::log::{Call, CallLog, Faults, RegistryCall, lock, scriptable};
use crate::error::{Error, System};
use crate::module::WIT_VERSION;
use crate::ports::ModuleSource;

/// The `liftoffd_min` of a [`FakeRelease`] unless it says otherwise:
/// older than every daemon of the tests.
pub const FAKE_LIFTOFFD_MIN: &str = "2.0.0-alpha.1";

/// The manifest of the `probe` contract component (`modules/testing/probe`).
///
/// Extended: a configuration with a required
/// `greeting` and a secret `token`, the command `hello` with a required
/// `who`, its route `POST /hello` and one migration.
pub const PROBE_MANIFEST: &str = r#"{
  "name": "probe",
  "version": "0.1.0",
  "config_schema": {
    "type": "object",
    "properties": {
      "greeting": {"type": "string", "description": "How to greet"},
      "token": {"type": "string", "format": "secret", "description": "A token to keep secret"}
    },
    "required": ["greeting"],
    "additionalProperties": false
  },
  "commands": [{"name": "hello", "description": "Says hello",
                "args": [{"name": "who", "type": "string", "required": true, "hint": "Who to greet"}]}],
  "routes": [{"method": "POST", "path": "/hello"}],
  "migrations": 1
}"#;

/// Who signs a [`FakeRelease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signer {
    /// The registry's own key ([`FakeModuleSource::public_key`]).
    Registry,
    /// A key of its own, which the daemon does not trust.
    Other,
}

/// One version of a module the fake registry serves.
///
/// Built from the component's bytes and its manifest; by default it is
/// signed with the registry's key, its `sha256` is that of the bytes, its
/// world is [`WIT_VERSION`], its `liftoffd_min` [`FAKE_LIFTOFFD_MIN`], and
/// `requires` and `config_schema` repeat the manifest's. The builder
/// methods break one of these at a time.
#[derive(Debug, Clone)]
pub struct FakeRelease {
    name: String,
    version: String,
    component: Vec<u8>,
    wit_version: String,
    liftoffd_min: String,
    requires: Vec<String>,
    config_schema: Option<Value>,
    signer: Signer,
    signed_bytes: Option<Vec<u8>>,
    sha256_of: Option<Vec<u8>>,
}

impl FakeRelease {
    /// The release of `component`, whose manifest is `manifest` (its
    /// `name`, `version`, `requires` and `config_schema` are read from
    /// it).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] when `manifest` is not JSON with a `name` and
    /// a `version`.
    pub fn new(manifest: &str, component: Vec<u8>) -> Result<Self, Error> {
        let parsed: Value = serde_json::from_str(manifest)
            .map_err(|error| Error::invalid("manifest", error.to_string()))?;
        let text = |field: &'static str| {
            parsed
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error::invalid("manifest", format!("no {field}")))
        };
        let requires = parsed
            .get("requires")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            name: text("name")?,
            version: text("version")?,
            component,
            wit_version: WIT_VERSION.to_owned(),
            liftoffd_min: FAKE_LIFTOFFD_MIN.to_owned(),
            requires,
            config_schema: parsed.get("config_schema").cloned(),
            signer: Signer::Registry,
            signed_bytes: None,
            sha256_of: None,
        })
    }

    /// A release whose component *is* its manifest's JSON: what
    /// [`FakeModuleRuntime`](super::FakeModuleRuntime) reads a manifest
    /// from.
    ///
    /// # Errors
    ///
    /// As [`FakeRelease::new`].
    pub fn of_manifest(manifest: &str) -> Result<Self, Error> {
        Self::new(manifest, manifest.as_bytes().to_vec())
    }

    /// The fake `probe` 0.1.0 ([`PROBE_MANIFEST`]), its component its
    /// manifest.
    ///
    /// # Panics
    ///
    /// Never: the manifest is a constant.
    #[must_use]
    #[expect(clippy::expect_used, reason = "the manifest is a constant")]
    pub fn probe() -> Self {
        Self::of_manifest(PROBE_MANIFEST).expect("the probe manifest has a name and a version")
    }

    /// Built against the WIT world `version` instead of [`WIT_VERSION`].
    #[must_use]
    pub fn wit_version(self, version: &str) -> Self {
        Self {
            wit_version: version.to_owned(),
            ..self
        }
    }

    /// Needs `liftoffd` `version` or newer.
    #[must_use]
    pub fn liftoffd_min(self, version: &str) -> Self {
        Self {
            liftoffd_min: version.to_owned(),
            ..self
        }
    }

    /// The index lists `modules` as its `requires`.
    #[must_use]
    pub fn requires(self, modules: &[&str]) -> Self {
        Self {
            requires: modules.iter().map(|name| (*name).to_owned()).collect(),
            ..self
        }
    }

    /// Signed with a key the daemon does not trust.
    #[must_use]
    pub fn signed_by_another_key(self) -> Self {
        Self {
            signer: Signer::Other,
            ..self
        }
    }

    /// The signature is of `bytes`, not of the served component (whose
    /// `sha256` the index still has right).
    #[must_use]
    pub fn signature_of(self, bytes: &[u8]) -> Self {
        Self {
            signed_bytes: Some(bytes.to_vec()),
            ..self
        }
    }

    /// The index's `sha256` is that of `bytes`, not of the served
    /// component (whose signature is right).
    #[must_use]
    pub fn sha256_of(self, bytes: &[u8]) -> Self {
        Self {
            sha256_of: Some(bytes.to_vec()),
            ..self
        }
    }

    /// The URL the component is served at.
    #[must_use]
    pub fn url(&self) -> String {
        format!("https://registry.test/{}-{}.wasm", self.name, self.version)
    }
}

pub use crate::module::sha256_hex;

/// A minisign key pair.
#[derive(Clone)]
struct Key {
    public: minisign::PublicKey,
    secret: minisign::SecretKey,
}

impl core::fmt::Debug for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Key")
    }
}

impl Key {
    fn generate() -> Result<Self, Error> {
        let pair = minisign::KeyPair::generate_unencrypted_keypair()
            .map_err(|error| Error::internal_msg(error.to_string()))?;
        Ok(Self {
            public: pair.pk,
            secret: pair.sk,
        })
    }

    /// The minisign signature of `bytes`, as `minisign -S` writes it.
    fn sign(&self, bytes: &[u8]) -> Result<String, Error> {
        minisign::sign(
            Some(&self.public),
            &self.secret,
            std::io::Cursor::new(bytes),
            Some("liftoff module release"),
            None,
        )
        .map(minisign::SignatureBox::into_string)
        .map_err(|error| Error::internal_msg(error.to_string()))
    }
}

#[derive(Debug, Default)]
struct Served {
    /// The index entries, by module and version.
    index: BTreeMap<String, BTreeMap<String, Value>>,
    /// The bytes by URL.
    files: BTreeMap<String, Vec<u8>>,
}

/// The module registry in memory, behind the same [`ModuleSource`] as the
/// daemon's.
///
/// It serves `index.json` and the files of the [`FakeRelease`]s published
/// to it, signed with a minisign key of its own
/// ([`FakeModuleSource::public_key`], the test key the daemon of the tests
/// pins).
///
/// `index` and `download` are logged ([`RegistryCall`]) and scriptable
/// (`fail_next("download", …)`); a URL nothing was published at answers
/// [`Error::Upstream`] of [`System::Registry`].
#[derive(Debug, Clone)]
pub struct FakeModuleSource {
    log: CallLog,
    faults: Faults,
    key: Key,
    other: Key,
    served: Arc<Mutex<Served>>,
}

scriptable!(FakeModuleSource);

impl FakeModuleSource {
    /// An empty registry with fresh keys, logging to `log`.
    ///
    /// # Panics
    ///
    /// When the operating system has no randomness for the keys.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "a test fixture without randomness cannot run any test"
    )]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            faults: Faults::default(),
            key: Key::generate().expect("a minisign key for the fake registry"),
            other: Key::generate().expect("a second minisign key for the fake registry"),
            served: Arc::default(),
        }
    }

    /// The base64 line (`RW…`) of the key the registry signs with.
    #[must_use]
    pub fn public_key(&self) -> String {
        self.key.public.to_base64()
    }

    /// Publishes `release`: its entry in `index.json` and its component at
    /// [`FakeRelease::url`].
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] when it cannot be signed.
    pub fn publish(&self, release: FakeRelease) -> Result<(), Error> {
        let key = match release.signer {
            Signer::Registry => &self.key,
            Signer::Other => &self.other,
        };
        let signature = key.sign(
            release
                .signed_bytes
                .as_deref()
                .unwrap_or(&release.component),
        )?;
        let sha256 = sha256_hex(release.sha256_of.as_deref().unwrap_or(&release.component));
        let url = release.url();
        let mut entry = json!({
            "url": url,
            "sha256": sha256,
            "signature": signature,
            "wit_version": release.wit_version,
            "liftoffd_min": release.liftoffd_min,
            "requires": release.requires,
        });
        if let (Some(schema), Some(object)) = (release.config_schema, entry.as_object_mut()) {
            let _none = object.insert("config_schema".to_owned(), schema);
        }
        let mut served = lock(&self.served);
        let _previous = served
            .index
            .entry(release.name)
            .or_default()
            .insert(release.version, entry);
        let _previous = served.files.insert(url, release.component);
        drop(served);
        Ok(())
    }

    /// The text of `index.json` as served.
    #[must_use]
    pub fn index_json(&self) -> String {
        json!({ "modules": lock(&self.served).index }).to_string()
    }
}

impl ModuleSource for FakeModuleSource {
    fn index(&self) -> impl Future<Output = Result<String, Error>> + Send {
        self.log.push(Call::Registry(RegistryCall::Index));
        ready(self.faults.check("index").map(|()| self.index_json()))
    }

    fn download(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, Error>> + Send {
        self.log
            .push(Call::Registry(RegistryCall::Download(url.to_owned())));
        let answer =
            self.faults.check("download").and_then(|()| {
                lock(&self.served).files.get(url).cloned().ok_or_else(|| {
                    Error::upstream(System::Registry, format!("{url}: 404 Not Found"))
                })
            });
        ready(answer)
    }
}
