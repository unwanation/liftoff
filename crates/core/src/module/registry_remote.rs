//! The module registry as the core reads it: its `index.json`, the releases it lists and the key that
//! signs them.
//!
//! `index.json` (built and signed by `modules/registry/` for the
//! `liftoff-modules` releases) maps a module to its versions, each a
//! [`Release`]:
//!
//! ```json
//! {
//!   "modules": {
//!     "tenants": {
//!       "0.1.0": {
//!         "url": "https://…/tenants-0.1.0.wasm",
//!         "sha256": "<hex of the .wasm>",
//!         "signature": "<minisign signature of the .wasm, as `minisign -S` writes it>",
//!         "wit_version": "0.1.0",
//!         "liftoffd_min": "2.0.0-alpha.2",
//!         "requires": ["oidc"],
//!         "config_schema": {"type": "object"}
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! `requires` and `config_schema` repeat what the component's manifest
//! declares, so dependencies are checked before the download and the CLI can ask for the configuration before it sends the
//! install; the daemon checks the signed component's own manifest again
//! after the download. Only the `.wasm` is signed: everything the index
//! says besides `sha256` and `signature` is a hint until the component
//! is verified.

use core::fmt::Write as _;
use std::collections::BTreeMap;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::domain::{ModuleName, ModuleVersion};
use crate::error::{Error, System};

/// The minisign public key (its base64 line, `RW…`) of the module
/// registry's releases.
///
/// One key for the daemon and the CLI's local mode, whose releases `unwanation/liftoff-modules` signs. Pinned here by
/// the release process before `v2.0.0`; a build
/// with the placeholder `""` refuses every install (REG-01), as
/// `install.sh` refuses without its `RELEASE_KEY`.
pub const REGISTRY_PUBLIC_KEY: &str = "RWQ+ptYbY49orb6+bHMzrSPivCnRRGH1WfqnZCks+Y/WPkdFDqKpBkal";

/// The version of the WIT world `liftoff:module` this core speaks
/// (`wit/liftoff-module.wit`): a release of another world is refused
/// before it is downloaded (REG-02).
pub const WIT_VERSION: &str = "0.1.0";

/// The minisign public key the registry's releases are signed with, as
/// `liftoffd` pins it in its binary.
///
/// The text is the key's base64 line (`RW…`). An empty text is the
/// placeholder of a binary whose release key is not pinned yet: every
/// install is refused then, as `install.sh` refuses without its
/// `RELEASE_KEY` (REG-01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryKey(String);

impl RegistryKey {
    /// The key whose base64 line is `raw` (trimmed); `""` is the
    /// placeholder.
    #[must_use]
    pub fn new(raw: &str) -> Self {
        Self(raw.trim().to_owned())
    }

    /// Whether a key is pinned at all (not the placeholder).
    #[must_use]
    pub const fn is_pinned(&self) -> bool {
        !self.0.is_empty()
    }

    /// The base64 line of the key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Checks that `signature` (as `minisign -S` writes it) is this key's
    /// signature of `bytes`, the `.wasm` of `release` (REG-01). Legacy
    /// (non pre-hashed) signatures are refused, as `minisign -V` does by
    /// default.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] for the placeholder key;
    /// [`Error::Internal`] for a pinned key that does not decode (a broken
    /// build); [`Error::Upstream`] of [`System::Registry`] when the
    /// signature does not decode or does not verify.
    pub fn verify(&self, release: &Release, bytes: &[u8]) -> Result<(), Error> {
        if !self.is_pinned() {
            return Err(unpinned());
        }
        let key = minisign_verify::PublicKey::from_base64(&self.0).map_err(|error| {
            Error::internal_msg(format!("the pinned registry key does not decode: {error}"))
        })?;
        let refused = |why: &dyn core::fmt::Display| {
            Error::upstream(
                System::Registry,
                format!(
                    "the signature of {} {} does not verify: {why}",
                    release.name, release.version
                ),
            )
        };
        let signature = minisign_verify::Signature::decode(&release.signature)
            .map_err(|error| refused(&error))?;
        key.verify(bytes, &signature, false)
            .map_err(|error| refused(&error))
    }
}

/// The refusal of every install by a binary whose registry key is the
/// placeholder.
pub(crate) fn unpinned() -> Error {
    Error::unavailable(
        "this liftoffd has no registry key pinned; modules cannot be installed until a release pins one",
    )
}

/// Lower-case hex of the SHA-256 of `bytes`, as `index.json` and
/// `sha256sum` write it.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _infallible = write!(hex, "{byte:02x}");
            hex
        })
}

/// One version of a module in `index.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The module.
    pub name: ModuleName,
    /// Its version.
    pub version: ModuleVersion,
    /// Where the `.wasm` is.
    pub url: String,
    /// The SHA-256 of the `.wasm`, lower-case hex.
    pub sha256: String,
    /// The minisign signature of the `.wasm`.
    pub signature: String,
    /// The WIT world it was built against.
    pub wit_version: String,
    /// The oldest `liftoffd` it runs on (semver).
    pub liftoffd_min: String,
    /// The modules that must be installed first.
    pub requires: Vec<ModuleName>,
    /// The JSON Schema of its configuration, if it has one.
    pub config_schema: Option<serde_json::Value>,
}

impl Release {
    /// Checks that this daemon can run the release: it is of
    /// [`WIT_VERSION`] and `liftoffd_version` meets its `liftoffd_min`
    /// (REG-02). Nothing is downloaded for that.
    ///
    /// # Errors
    ///
    /// [`Error::Conflict`] naming both worlds, or the `liftoffd` it needs
    /// and this one.
    pub fn check_runs_on(&self, liftoffd_version: &str) -> Result<(), Error> {
        if self.wit_version != WIT_VERSION {
            return Err(Error::conflict(format!(
                "{} {} is built for the WIT world liftoff:module@{}; this liftoffd runs liftoff:module@{WIT_VERSION}",
                self.name, self.version, self.wit_version
            )));
        }
        let needs = || {
            Error::conflict(format!(
                "{} {} needs liftoffd {} or newer; this one is {liftoffd_version}",
                self.name, self.version, self.liftoffd_min
            ))
        };
        let min = semver::Version::parse(&self.liftoffd_min).map_err(|_unparsed| needs())?;
        let this = semver::Version::parse(liftoffd_version).map_err(|_unparsed| needs())?;
        if this < min {
            return Err(needs());
        }
        Ok(())
    }
}

/// The releases of `index.json`, by module in name order, each module's
/// newest first.
///
/// An entry whose module name or version does not parse is left out with
/// a warning: the index is the registry's, and one broken entry must not
/// hide the others.
///
/// # Errors
///
/// [`Error::Upstream`] of [`System::Registry`] when the text is not an
/// index.
pub fn parse_index(text: &str) -> Result<Vec<Release>, Error> {
    let index: raw::Index = serde_json::from_str(text).map_err(|error| {
        Error::upstream(
            System::Registry,
            format!("index.json does not parse: {error}"),
        )
    })?;
    let mut releases = Vec::new();
    for (module, versions) in index.modules {
        let Ok(name) = ModuleName::new(&module) else {
            tracing::warn!(%module, "index.json lists a module whose name does not parse; skipped");
            continue;
        };
        let mut of_module: Vec<Release> = versions
            .into_iter()
            .filter_map(|(version, entry)| {
                let release = release(&name, &version, entry);
                if release.is_none() {
                    tracing::warn!(%module, %version, "index.json lists a release that does not parse; skipped");
                }
                release
            })
            .collect();
        of_module.sort_by(|a, b| b.version.cmp(&a.version));
        releases.extend(of_module);
    }
    Ok(releases)
}

/// The release of `name` at `version` that `entry` describes, if it
/// parses.
fn release(name: &ModuleName, version: &str, entry: serde_json::Value) -> Option<Release> {
    let entry: raw::Entry = serde_json::from_value(entry).ok()?;
    let requires = entry
        .requires
        .iter()
        .map(|dep| ModuleName::new(dep))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(Release {
        name: name.clone(),
        version: ModuleVersion::new(version).ok()?,
        url: entry.url,
        sha256: entry.sha256,
        signature: entry.signature,
        wit_version: entry.wit_version,
        liftoffd_min: entry.liftoffd_min,
        requires,
        config_schema: entry.config_schema,
    })
}

/// The wire shape of `index.json`. Unknown fields are left alone: a newer
/// registry may say more than this daemon reads.
mod raw {
    use super::{BTreeMap, Deserialize};

    #[derive(Debug, Deserialize)]
    pub(super) struct Index {
        /// Each entry is read on its own ([`super::release`]), so one
        /// that does not parse hides none of the others.
        pub(super) modules: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    }

    #[derive(Debug, Deserialize)]
    pub(super) struct Entry {
        pub(super) url: String,
        pub(super) sha256: String,
        pub(super) signature: String,
        pub(super) wit_version: String,
        pub(super) liftoffd_min: String,
        #[serde(default)]
        pub(super) requires: Vec<String>,
        #[serde(default)]
        pub(super) config_schema: Option<serde_json::Value>,
    }
}

/// A module of the registry with the releases this daemon can run,
/// newest first (`GET /modules/available`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableModule {
    /// The module.
    pub name: ModuleName,
    /// Its releases of [`WIT_VERSION`] whose `liftoffd_min` this daemon
    /// meets, newest first.
    pub releases: Vec<Release>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// An index entry that parses, at `url`.
    fn entry(url: &str) -> Value {
        json!({
            "url": url,
            "sha256": "00",
            "signature": "",
            "wit_version": WIT_VERSION,
            "liftoffd_min": "2.0.0",
        })
    }

    /// `(module, version)` of every release of `index`.
    fn listed(index: &Value) -> Vec<(String, String)> {
        parse_index(&index.to_string())
            .unwrap()
            .into_iter()
            .map(|release| (release.name.to_string(), release.version.to_string()))
            .collect()
    }

    fn is_registry_failure<T: core::fmt::Debug>(result: &Result<T, Error>) -> bool {
        matches!(
            result,
            Err(Error::Upstream {
                system: System::Registry,
                ..
            })
        )
    }

    mod parse_index {
        use super::*;

        // TC: REG-01 (index.json is untrusted: one broken entry hides none
        // of the others)
        #[test]
        fn skips_broken_entry_and_keeps_the_others_when_index_has_one() {
            let mut bad_requires = entry("https://r.test/probe-0.3.0.wasm");
            bad_requires["requires"] = json!(["Not A Name"]);
            let index = json!({"modules": {
                "probe": {
                    "0.1.0": entry("https://r.test/probe-0.1.0.wasm"),
                    "not-semver": entry("https://r.test/probe-x.wasm"),
                    "0.2.0": {"url": 5},
                    "0.3.0": bad_requires,
                },
                "Not A Name": {"0.1.0": entry("https://r.test/bad.wasm")},
                "tenants": {"0.1.0": entry("https://r.test/tenants-0.1.0.wasm")},
            }});
            assert_eq!(
                listed(&index),
                vec![
                    ("probe".to_owned(), "0.1.0".to_owned()),
                    ("tenants".to_owned(), "0.1.0".to_owned())
                ]
            );
        }

        // TC: REG-02 (the newest release is picked by semver, not by
        // how the index orders its keys)
        #[test]
        fn lists_releases_newest_first_when_index_is_unordered() {
            let index = json!({"modules": {"probe": {
                "0.10.0": entry("a"),
                "0.2.0": entry("b"),
                "1.0.0": entry("c"),
                "0.9.0-rc.1": entry("d"),
            }}});
            assert_eq!(
                listed(&index)
                    .into_iter()
                    .map(|(_, version)| version)
                    .collect::<Vec<_>>(),
                ["1.0.0", "0.10.0", "0.9.0-rc.1", "0.2.0"]
            );
        }

        // TC: REG-01
        #[test]
        fn refuses_as_registry_failure_when_index_is_not_json() {
            assert!(is_registry_failure(&parse_index("<html>not json</html>")));
        }

        // TC: REG-01
        #[test]
        fn refuses_as_registry_failure_when_index_has_no_modules() {
            assert!(is_registry_failure(&parse_index(r#"{"releases": []}"#)));
        }
    }

    mod verify {
        use super::*;

        /// A minisign key (seed `0..32`, key id `0102030405060708`).
        const KEY: &str = "RWQBAgMEBQYHCAOhB7/zzhC+HXDdGOdLwJln5NYwm6UNXx3chmQSVTG4";
        /// The signed bytes.
        const SIGNED: &[u8] = b"legacy component";
        /// `KEY`'s legacy (`Ed`, not pre-hashed) signature of `SIGNED`, as
        /// `minisign -S -l` writes it.
        const LEGACY: &str = "untrusted comment: legacy
RWQBAgMEBQYHCMfdTQJKm86/x1UlRrZfWGh8g+UxSjDj/OnubyqUdXUGN5op00v3ggoZ0lulTNZOOBiaEYBOlE2qPwkAwE779gM=
trusted comment: liftoff module release
hHYIirFau5ppcYZkI+mZKL8VPFAkWTJjIs6GFxqCgnBt67cknQ58VbR36QbCI1TW+w5AsDqf0TBvbYy44C1kCw==
";

        fn release(signature: &str) -> Release {
            let mut entry = entry("https://r.test/probe-0.1.0.wasm");
            entry["signature"] = json!(signature);
            parse_index(&json!({"modules": {"probe": {"0.1.0": entry}}}).to_string())
                .unwrap()
                .remove(0)
        }

        // TC: REG-01 (the fixture is a valid signature: only its legacy
        // form is refused)
        #[test]
        fn fixture_verifies_when_legacy_is_allowed() {
            let key = minisign_verify::PublicKey::from_base64(KEY).unwrap();
            let signature = minisign_verify::Signature::decode(LEGACY).unwrap();
            assert!(key.verify(SIGNED, &signature, true).is_ok());
        }

        // TC: REG-01 (as `minisign -V` does by default)
        #[test]
        fn refuses_as_registry_failure_when_signature_is_legacy() {
            let verified = RegistryKey::new(KEY).verify(&release(LEGACY), SIGNED);
            assert!(is_registry_failure(&verified), "{verified:?}");
        }

        // TC: REG-01
        #[test]
        fn refuses_as_registry_failure_when_signature_does_not_decode() {
            let verified = RegistryKey::new(KEY).verify(&release("not a signature"), SIGNED);
            assert!(is_registry_failure(&verified), "{verified:?}");
        }

        // TC: REG-01
        #[test]
        fn refuses_unavailable_when_key_is_placeholder() {
            let verified = RegistryKey::new("").verify(&release(LEGACY), SIGNED);
            assert!(
                matches!(verified, Err(Error::Unavailable(_))),
                "{verified:?}"
            );
        }
    }
}
