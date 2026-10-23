//! Short names: project and environment names, domain labels, compose
//! service names, and the compose project and systemd slice derived from
//! them.

use core::fmt;

use crate::error::{Problem, ValidationError};

/// The shape shared by project names, environment names and domain labels.
pub const LABEL_PATTERN: &str = "^[a-z0-9][a-z0-9-]{0,31}$";

/// Maximum length of a [`LABEL_PATTERN`] value.
pub const LABEL_MAX_LEN: usize = 32;

fn check_label(raw: &str, field: &'static str) -> Result<(), ValidationError> {
    let fail = |problem| Err(ValidationError::new(field, problem));
    if raw.is_empty() {
        return fail(Problem::Empty);
    }
    let len = raw.chars().count();
    if len > LABEL_MAX_LEN {
        return fail(Problem::TooLong {
            max: LABEL_MAX_LEN,
            actual: len,
        });
    }
    let mut chars = raw.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let tail_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if head_ok && tail_ok {
        Ok(())
    } else {
        fail(Problem::Pattern {
            expected: LABEL_PATTERN,
        })
    }
}

/// Implements the string-newtype boilerplate: accessors, `Display`,
/// `FromStr`, `TryFrom<String>` and `AsRef<str>`, all through `new`.
macro_rules! string_newtype {
    ($name:ident) => {
        impl $name {
            /// Borrows the validated value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl core::str::FromStr for $name {
            type Err = $crate::error::ValidationError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = $crate::error::ValidationError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(&value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}
pub(crate) use string_newtype;

macro_rules! label_type {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Validates `raw` against [`LABEL_PATTERN`]; errors name the field `", $field, "`.")]
            ///
            /// # Errors
            ///
            /// A [`ValidationError`] when `raw` is empty, longer than 32
            /// characters or not of the label shape.
            pub fn new(raw: &str) -> Result<Self, ValidationError> {
                check_label(raw, $field)?;
                Ok(Self(raw.to_owned()))
            }
        }

        string_newtype!($name);
    };
}

label_type!(
    /// A project's name, e.g. `shop`.
    ProjectName,
    "project"
);

label_type!(
    /// A tenant's slug, e.g. `acme` (the `tenants` module keeps tenants, the core passes the slug a caller named).
    TenantSlug,
    "tenant"
);

label_type!(
    /// An environment's name, unique within its project, e.g. `prod`.
    EnvName,
    "env"
);

label_type!(
    /// The left-most label of a managed domain, e.g. `example-qa` in
    /// `example-qa.example.com`.
    Label,
    "label"
);

/// The name of a service inside a compose file, e.g. `web`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceName(String);

impl ServiceName {
    /// The shape compose accepts for service names.
    pub const PATTERN: &'static str = "^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,62}$";

    /// Validates a compose service name.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `service` when `raw` does not match
    /// [`ServiceName::PATTERN`].
    pub fn new(raw: &str) -> Result<Self, ValidationError> {
        let fail = |problem| Err(ValidationError::new("service", problem));
        if raw.is_empty() {
            return fail(Problem::Empty);
        }
        let len = raw.chars().count();
        if len > 63 {
            return fail(Problem::TooLong {
                max: 63,
                actual: len,
            });
        }
        let mut chars = raw.chars();
        let head_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
        let tail_ok = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if head_ok && tail_ok {
            Ok(Self(raw.to_owned()))
        } else {
            fail(Problem::Pattern {
                expected: Self::PATTERN,
            })
        }
    }
}

string_newtype!(ServiceName);

/// The compose project (and its directory) name: `lf_<project>_<env>`.
///
/// The separator is `_`, which [`LABEL_PATTERN`] does not allow, so the
/// derivation is injective: project `a-b` with environment `c` and project
/// `a` with environment `b-c` get different compose projects and
/// directories. Compose accepts `[a-z0-9_-]` in project names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComposeProject(String);

impl ComposeProject {
    /// Derives the compose project of environment `env` of `project`.
    #[must_use]
    pub fn new(project: &ProjectName, env: &EnvName) -> Self {
        Self(format!("lf_{project}_{env}"))
    }

    /// Borrows the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ComposeProject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `-` as systemd escapes it in a unit name component.
fn escape_dashes(raw: &str) -> String {
    raw.replace('-', "\\x2d")
}

/// The systemd slice of an environment: `apps-lf-<project>-<env>.slice`.
///
/// Every `-` *inside* the project and environment names is escaped as
/// `\x2d` (what `systemd-escape` does), e.g.
/// `apps-lf-shop-qa\x2d2.slice` for environment `qa-2`.
///
/// systemd reads each unescaped `-` as one level of hierarchy, so the
/// escaping makes every environment a direct child of
/// `apps-lf-<project>.slice`: the limits of `qa` never cap `qa-2`, and
/// stopping `qa` never stops `qa-2`. It also keeps names like `qa-` (a
/// trailing dash) valid slice names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SliceName(String);

impl SliceName {
    /// Derives the slice name of environment `env` of `project`.
    #[must_use]
    pub fn new(project: &ProjectName, env: &EnvName) -> Self {
        Self(format!(
            "apps-lf-{}-{}.slice",
            escape_dashes(project.as_str()),
            escape_dashes(env.as_str())
        ))
    }

    /// The slice of all environments of `project`, the parent of
    /// [`SliceName::new`]: `apps-lf-<project>.slice`. It carries no limits.
    #[must_use]
    pub fn of_project(project: &ProjectName) -> Self {
        Self(format!("apps-lf-{}.slice", escape_dashes(project.as_str())))
    }

    /// The parent slice systemd derives from the name: everything before
    /// the last unescaped `-`, plus `.slice`; `None` for a top-level slice.
    #[must_use]
    pub fn parent(&self) -> Option<String> {
        let stem = self.0.strip_suffix(".slice")?;
        let (parent, _) = stem.rsplit_once('-')?;
        Some(format!("{parent}.slice"))
    }

    /// Borrows the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SliceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod label_pattern {
        use super::*;

        // TC: ENV-01 (project and environment names), v1 DOM-02 (labels)
        #[rstest]
        #[case::single_letter("a")]
        #[case::single_digit("7")]
        #[case::with_dash("example-qa")]
        #[case::trailing_dash("qa-")]
        #[case::digit_first("1st")]
        #[case::max_len("a234567890123456789012345678901b")]
        fn accepts_when_shape_matches(#[case] raw: &str) {
            assert!(
                ProjectName::new(raw).is_ok(),
                "{raw:?} should be a valid project name"
            );
            assert!(
                EnvName::new(raw).is_ok(),
                "{raw:?} should be a valid env name"
            );
            assert!(Label::new(raw).is_ok(), "{raw:?} should be a valid label");
        }

        // TC: ENV-01
        #[rstest]
        #[case::uppercase("Acme")]
        #[case::leading_dash("-x")]
        #[case::underscore("a_b")]
        #[case::dot("a.b")]
        #[case::space("a b")]
        #[case::non_ascii("é")]
        fn rejects_pattern_when_shape_differs(#[case] raw: &str) {
            assert_eq!(
                ProjectName::new(raw).unwrap_err(),
                ValidationError::new(
                    "project",
                    Problem::Pattern {
                        expected: LABEL_PATTERN
                    }
                )
            );
            // TC: v1 DOM-02
            assert_eq!(
                Label::new(raw).unwrap_err(),
                ValidationError::new(
                    "label",
                    Problem::Pattern {
                        expected: LABEL_PATTERN
                    }
                )
            );
        }

        // TC: ENV-01
        #[test]
        fn rejects_too_long_when_33_chars() {
            assert_eq!(
                ProjectName::new(&"a".repeat(33)).unwrap_err(),
                ValidationError::new(
                    "project",
                    Problem::TooLong {
                        max: 32,
                        actual: 33
                    }
                )
            );
            // TC: v1 DOM-02
            assert_eq!(
                Label::new(&"a".repeat(33)).unwrap_err(),
                ValidationError::new(
                    "label",
                    Problem::TooLong {
                        max: 32,
                        actual: 33
                    }
                )
            );
        }

        #[test]
        fn rejects_empty_when_blank() {
            assert_eq!(
                EnvName::new("").unwrap_err(),
                ValidationError::new("env", Problem::Empty)
            );
        }

        #[test]
        fn names_its_own_field_when_label() {
            assert_eq!(Label::new("A").unwrap_err().field, "label");
        }
    }

    mod catalogue_names {
        use super::*;

        // TC: ENV-01
        #[rstest]
        #[case::upper_case("Shop")]
        #[case::leading_dash("-a")]
        #[case::too_long("a23456789012345678901234567890123")]
        fn validation_error_when_catalogue_bad_name(#[case] raw: &str) {
            let as_error = |e: ValidationError| match crate::Error::from(e) {
                crate::Error::Validation(v) => Some(v.field),
                _ => None,
            };
            assert_eq!(
                (
                    ProjectName::new(raw).map_err(as_error),
                    EnvName::new(raw).map_err(as_error),
                ),
                (Err(Some("project")), Err(Some("env")))
            );
        }

        // TC: ENV-01
        #[test]
        fn accepts_when_shop_2() {
            assert_eq!(
                (
                    ProjectName::new("shop-2").map(|n| n.to_string()),
                    EnvName::new("shop-2").map(|n| n.to_string()),
                ),
                (Ok("shop-2".to_owned()), Ok("shop-2".to_owned()))
            );
        }
    }

    mod service_name {
        use super::*;

        #[rstest]
        #[case("web")]
        #[case("Web_1.api-x")]
        fn accepts_when_compose_shape(#[case] raw: &str) {
            assert!(ServiceName::new(raw).is_ok(), "{raw:?} should be valid");
        }

        #[rstest]
        #[case::empty("")]
        #[case::leading_dash("-web")]
        #[case::slash("a/b")]
        #[case::too_long(&"a".repeat(64))]
        fn rejects_when_not_compose_shape(#[case] raw: &str) {
            assert_eq!(ServiceName::new(raw).unwrap_err().field, "service");
        }
    }

    mod derived_names {
        use super::*;

        fn project(raw: &str) -> ProjectName {
            ProjectName::new(raw).unwrap()
        }

        fn env(raw: &str) -> EnvName {
            EnvName::new(raw).unwrap()
        }

        #[test]
        fn compose_project_is_lf_project_env_when_shop_qa() {
            let compose = ComposeProject::new(&project("shop"), &env("qa"));
            assert_eq!(compose.as_str(), "lf_shop_qa");
        }

        #[test]
        fn compose_projects_differ_when_dash_moves_between_project_and_env() {
            assert_ne!(
                ComposeProject::new(&project("a-b"), &env("c")),
                ComposeProject::new(&project("a"), &env("b-c"))
            );
        }

        #[test]
        fn slice_is_under_apps_when_shop_qa() {
            let slice = SliceName::new(&project("shop"), &env("qa"));
            assert_eq!(slice.to_string(), "apps-lf-shop-qa.slice");
        }

        #[test]
        fn slice_escapes_inner_dashes_when_names_contain_dashes() {
            let slice = SliceName::new(&project("my-co"), &env("qa-2"));
            assert_eq!(slice.as_str(), r"apps-lf-my\x2dco-qa\x2d2.slice");
        }

        #[rstest]
        #[case::plain("acme", "qa")]
        #[case::dashed_env("acme", "qa-2")]
        #[case::dashed_project("a-b", "c")]
        #[case::trailing_dash("acme", "qa-")]
        fn slice_parent_is_project_slice_when_any_names(
            #[case] name: &str,
            #[case] env_name: &str,
        ) {
            let slice = SliceName::new(&project(name), &env(env_name));
            assert_eq!(
                slice.parent(),
                Some(SliceName::of_project(&project(name)).to_string())
            );
        }

        #[test]
        fn slices_are_not_nested_when_one_name_prefixes_another() {
            let qa = SliceName::new(&project("acme"), &env("qa"));
            let qa2 = SliceName::new(&project("acme"), &env("qa-2"));
            assert_ne!(qa2.parent().as_deref(), Some(qa.as_str()));
        }

        #[test]
        fn slices_differ_when_dash_moves_between_project_and_env() {
            assert_ne!(
                SliceName::new(&project("a-b"), &env("c")),
                SliceName::new(&project("a"), &env("b-c"))
            );
        }

        #[test]
        fn project_slice_parent_is_apps_lf_when_acme() {
            assert_eq!(
                SliceName::of_project(&project("acme")).parent().as_deref(),
                Some("apps-lf.slice")
            );
        }
    }

    mod conversions {
        use super::*;

        #[test]
        fn from_str_matches_new_when_valid() {
            assert_eq!(
                "acme".parse::<ProjectName>().unwrap(),
                ProjectName::new("acme").unwrap()
            );
        }

        #[test]
        fn try_from_string_matches_new_when_valid() {
            assert_eq!(
                EnvName::try_from(String::from("qa")).unwrap().as_str(),
                "qa"
            );
        }
    }
}
