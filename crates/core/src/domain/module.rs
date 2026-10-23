//! [`ModuleName`]: the name a module is installed under,
//! [`ModuleVersion`]: the semver of one of its releases, and
//! [`InstalledModule`], its row of the `modules` table.

use std::collections::BTreeMap;

use super::{SealedEnv, Timestamp};
use crate::error::{Problem, ValidationError};

/// A row of `modules`: an installed module, its configuration
/// with the secret fields sealed, and whether it takes calls.
///
/// The component itself is a file of the module runtime
/// (`paths.data/modules/<name>-<version>.wasm`), not a column.
#[derive(Debug, Clone)]
pub struct InstalledModule {
    /// `name`: unique.
    pub name: ModuleName,
    /// `version`: the semver of the installed release.
    pub version: ModuleVersion,
    /// `sha256`: the lower-case hex SHA-256 of the component verified at
    /// install; the kept file is checked against it before every load.
    pub sha256: String,
    /// `enabled`: `false` after [`crate::module::FAILURES_TO_DISABLE`]
    /// failures in a row, a failed load at start, or a `PATCH`.
    pub enabled: bool,
    /// `config`: the configuration by top-level field, the fields the
    /// schema marks `"format": "secret"` sealed (REG-05).
    pub config: ModuleConfigRecord,
    /// `error`: why it is disabled.
    pub error: Option<String>,
    /// `installed_at`: also the order hooks run in (installation order).
    pub installed_at: Timestamp,
}

/// The stored configuration of a module, by top-level field.
pub type ModuleConfigRecord = BTreeMap<String, ConfigValue>;

/// One field of a [`ModuleConfigRecord`].
#[derive(Debug, Clone)]
pub enum ConfigValue {
    /// A field as given (any JSON value).
    Plain(serde_json::Value),
    /// A secret field (`"format": "secret"` in the schema), sealed with
    /// the master key; only `config-get` sees it opened (REG-05).
    Sealed(SealedEnv),
}

/// The version of a module release: semver (`X.Y.Z`, optionally
/// `-pre.release` and `+build`), validated where it enters (the
/// registry's `index.json`, a request, a stored row).
///
/// Its text is ASCII letters, digits, `.`, `-` and `+` only, so it is
/// safe in a file name (`<name>-<version>.wasm`: no `/`, no `..`
/// segment). Versions order as semver does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleVersion(semver::Version);

impl ModuleVersion {
    /// Validates `raw` as a release version.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] of `version` for anything but semver.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        semver::Version::parse(raw).map(Self).map_err(|_unparsed| {
            ValidationError::new(
                "version",
                Problem::Pattern {
                    expected: "semver X.Y.Z[-pre][+build]",
                },
            )
        })
    }
}

impl core::fmt::Display for ModuleVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

impl PartialEq<str> for ModuleVersion {
    fn eq(&self, other: &str) -> bool {
        Self::new(other).is_ok_and(|other| *self == other)
    }
}

/// The name of a module: `oidc`, `tenants`, `dns.cloudflare`,
/// `git.forgejo`. Lower-case ASCII words joined by dots.
///
/// Its tables live in their own namespace, [`ModuleName::table_prefix`]
/// (`m_<module>_*`), and its key space of `store-get`/`store-set`
/// is its own (MOD-01).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleName(String);

impl ModuleName {
    /// The longest name accepted: its tables still fit SQLite's habits and
    /// a log line.
    pub const MAX_LEN: usize = 64;

    /// Validates `raw` as a module name.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] for anything but lower-case ASCII words
    /// (letters and digits, starting with a letter) joined by single dots.
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        const FIELD: &str = "module";
        if raw.is_empty() {
            return Err(ValidationError::new(FIELD, Problem::Empty));
        }
        let actual = raw.chars().count();
        if actual > Self::MAX_LEN {
            return Err(ValidationError::new(
                FIELD,
                Problem::TooLong {
                    max: Self::MAX_LEN,
                    actual,
                },
            ));
        }
        let word = |word: &str| {
            let mut chars = word.chars();
            chars.next().is_some_and(|first| first.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        };
        if !raw.split('.').all(word) {
            return Err(ValidationError::new(
                FIELD,
                Problem::Pattern {
                    expected: "^[a-z][a-z0-9]*(\\.[a-z][a-z0-9]*)*$",
                },
            ));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The prefix every table of the module starts with: `m_`, the name
    /// with `.` spelled `_`, and `_` (`m_probe_`, `m_dns_cloudflare_`).
    #[must_use]
    pub fn table_prefix(&self) -> String {
        format!("m_{}_", self.0.replace('.', "_"))
    }

    /// Whether the schema object (table or index) `name` is the module's:
    /// [`ModuleName::table_prefix`] followed by one word of lower-case
    /// letters and digits (`m_probe_items`, `m_dns_cloudflare_records`).
    ///
    /// The word has no `_`, so every name has at most one owner although
    /// one prefix can begin another: `m_dns_cloudflare_records` is
    /// `dns.cloudflare`'s and never `dns`'s, `m_dns_cloudflare` is `dns`'s
    /// and never `dns.cloudflare`'s (MOD-01).
    #[must_use]
    pub fn owns_table(&self, name: &str) -> bool {
        name.strip_prefix(&self.table_prefix()).is_some_and(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
    }
}

impl core::fmt::Display for ModuleName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod new {
        use super::*;

        #[test]
        fn accepts_dotted_words_when_lower_case() {
            for raw in ["oidc", "dns.cloudflare", "git.forgejo", "probe2"] {
                assert_eq!(ModuleName::new(raw).unwrap().as_str(), raw);
            }
        }

        #[test]
        fn rejects_name_when_not_dotted_lower_case_words() {
            for raw in [
                "",
                "Probe",
                "dns..cloudflare",
                ".dns",
                "dns.",
                "2fa",
                "m_x",
                "a-b",
            ] {
                assert!(ModuleName::new(raw).is_err(), "{raw:?}");
            }
        }

        #[test]
        fn rejects_name_when_longer_than_max() {
            assert!(ModuleName::new(&"a".repeat(ModuleName::MAX_LEN + 1)).is_err());
        }
    }

    mod module_version {
        use super::*;

        #[test]
        fn keeps_text_when_semver() {
            for raw in ["0.1.0", "2.0.0-alpha.2", "1.0.0+build.5", "10.20.30-rc.1+x"] {
                assert_eq!(ModuleVersion::new(raw).unwrap().to_string(), raw);
            }
        }

        // TC: REG-06 (the version names the kept file: nothing that walks
        // out of the modules directory)
        #[test]
        fn rejects_version_when_not_semver() {
            for raw in [
                "",
                "1.0",
                "v1.0.0",
                "../../etc/passwd",
                "1.0.0/../x",
                "1.0.0-a/b",
                "01.0.0",
                "1.0.0 ",
            ] {
                assert!(ModuleVersion::new(raw).is_err(), "{raw:?}");
            }
        }

        #[test]
        fn orders_as_semver_when_compared() {
            let version = |raw| ModuleVersion::new(raw).unwrap();
            assert!(
                version("0.10.0") > version("0.9.0") && version("1.0.0") > version("1.0.0-rc.1")
            );
        }
    }

    mod table_prefix {
        use super::*;

        // TC: MOD-01 (each module has its own table namespace)
        #[test]
        fn spells_dots_as_underscores_when_name_is_dotted() {
            assert_eq!(
                ModuleName::new("dns.cloudflare").unwrap().table_prefix(),
                "m_dns_cloudflare_"
            );
            assert_eq!(ModuleName::new("probe").unwrap().table_prefix(), "m_probe_");
        }
    }

    mod owns_table {
        use super::*;

        fn owns(module: &str, table: &str) -> bool {
            ModuleName::new(module).unwrap().owns_table(table)
        }

        // TC: MOD-01 (each module has its own table namespace)
        #[test]
        fn owns_prefixed_word_when_table_is_its_own() {
            assert!(
                owns("probe", "m_probe_items")
                    && owns("dns.cloudflare", "m_dns_cloudflare_records")
            );
        }

        // TC: MOD-01 (a name that begins another's prefix owns none of the
        // other's tables, and the other owns none of its)
        #[test]
        fn disowns_table_of_dotted_module_when_its_name_begins_the_others() {
            assert_eq!(
                (
                    owns("dns", "m_dns_cloudflare_records"),
                    owns("dns.cloudflare", "m_dns_cloudflare"),
                    owns("dns", "m_dns_cloudflare"),
                ),
                (false, false, true)
            );
        }

        // TC: MOD-01
        #[test]
        fn disowns_table_when_not_prefix_and_one_lower_case_word() {
            for table in [
                "m_probe_",
                "m_probe_Items",
                "M_PROBE_ITEMS",
                "m_probe_a_b",
                "users",
                "m_probeitems",
                "m_other_items",
            ] {
                assert!(!owns("probe", table), "{table}");
            }
        }
    }
}
