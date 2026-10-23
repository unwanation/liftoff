//! Resource limits of an environment slice.

use core::fmt;

use crate::error::{Problem, ValidationError};

/// CPU allowance in hundredths of a CPU (`100` = one CPU).
///
/// Integer hundredths keep `CPUQuota=<cpus*100>%` exact: `0.5` is `50%`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cpus(u32);

impl Cpus {
    /// Smallest allowance: 0.01 CPU.
    pub const MIN_HUNDREDTHS: u32 = 1;
    /// Largest allowance: 64 CPUs.
    pub const MAX_HUNDREDTHS: u32 = 6_400;
    /// One CPU, the default environment allowance.
    pub const ONE: Self = Self(100);

    /// Validates an allowance given in hundredths of a CPU.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `cpus` outside 1..=6400.
    pub fn from_hundredths(hundredths: u32) -> Result<Self, ValidationError> {
        if (Self::MIN_HUNDREDTHS..=Self::MAX_HUNDREDTHS).contains(&hundredths) {
            Ok(Self(hundredths))
        } else {
            Err(ValidationError::new(
                "cpus",
                Problem::OutOfRange {
                    min: Self::MIN_HUNDREDTHS.into(),
                    max: Self::MAX_HUNDREDTHS.into(),
                },
            ))
        }
    }

    /// Parses a decimal like `1`, `1.0`, `0.5` or `0.25` (at most two
    /// fractional digits).
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `cpus` when `raw` is not such a decimal
    /// or is out of range.
    pub fn parse(raw: &str) -> Result<Self, ValidationError> {
        let invalid = || {
            ValidationError::new(
                "cpus",
                Problem::Pattern {
                    expected: "a decimal with at most two fractional digits, e.g. 0.5",
                },
            )
        };
        let (whole, frac) = raw.split_once('.').unwrap_or((raw, ""));
        let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
        if whole.is_empty() || whole.len() > 4 || frac.len() > 2 || !digits(whole) || !digits(frac)
        {
            return Err(invalid());
        }
        let whole: u32 = whole.parse().map_err(|_digits| invalid())?;
        let frac: u32 = format!("{frac:0<2}").parse().map_err(|_digits| invalid())?;
        let hundredths = whole
            .checked_mul(100)
            .and_then(|w| w.checked_add(frac))
            .ok_or_else(invalid)?;
        Self::from_hundredths(hundredths)
    }

    /// The allowance in hundredths of a CPU, which is also the systemd
    /// `CPUQuota` percentage.
    #[must_use]
    pub const fn hundredths(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Cpus {
    /// `1`, `0.5`, `0.25`, `1.05`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0.div_euclid(100);
        let frac = self.0.rem_euclid(100);
        match (frac, frac.rem_euclid(10)) {
            (0, _) => write!(f, "{whole}"),
            (_, 0) => write!(f, "{whole}.{}", frac.div_euclid(10)),
            _ => write!(f, "{whole}.{frac:02}"),
        }
    }
}

/// Memory allowance in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoryMb(u32);

impl MemoryMb {
    /// Smallest allowance.
    pub const MIN: u32 = 32;
    /// Largest allowance (256 GiB).
    pub const MAX: u32 = 262_144;
    /// The default environment allowance.
    pub const DEFAULT: Self = Self(1024);

    /// Validates an allowance in MiB.
    ///
    /// # Errors
    ///
    /// A [`ValidationError`] on field `memory_mb` outside [`MemoryMb::MIN`]..=[`MemoryMb::MAX`].
    pub fn new(mb: u32) -> Result<Self, ValidationError> {
        if (Self::MIN..=Self::MAX).contains(&mb) {
            Ok(Self(mb))
        } else {
            Err(ValidationError::new(
                "memory_mb",
                Problem::OutOfRange {
                    min: Self::MIN.into(),
                    max: Self::MAX.into(),
                },
            ))
        }
    }

    /// The allowance in MiB.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for MemoryMb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// CPU and memory limits applied to an environment's slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Limits {
    /// CPU allowance.
    pub cpus: Cpus,
    /// Memory allowance.
    pub memory_mb: MemoryMb,
}

impl Default for Limits {
    /// One CPU and 1024 MiB (the v1 defaults).
    fn default() -> Self {
        Self {
            cpus: Cpus::ONE,
            memory_mb: MemoryMb::DEFAULT,
        }
    }
}

/// The quotas of a tenant (the `tenants` module): how many
/// projects it may hold and the limits its new environments get. A field
/// the tenant does not set is `None`: no quota, the core's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TenantQuotas {
    /// `max_projects` (TEN-04).
    pub max_projects: Option<u32>,
    /// `env_cpus` (TEN-05).
    pub env_cpus: Option<Cpus>,
    /// `env_memory_mb` (TEN-05).
    pub env_memory_mb: Option<MemoryMb>,
}

impl TenantQuotas {
    /// The limits of a new environment of the tenant: its own where it
    /// sets them, else the default (TEN-05).
    #[must_use]
    pub fn environment_limits(&self) -> Limits {
        let default = Limits::default();
        Limits {
            cpus: self.env_cpus.unwrap_or(default.cpus),
            memory_mb: self.env_memory_mb.unwrap_or(default.memory_mb),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    mod cpus_parse {
        use super::*;

        #[rstest]
        #[case("1", 100)]
        #[case("1.0", 100)]
        #[case("0.5", 50)]
        #[case("0.25", 25)]
        #[case("64", 6_400)]
        #[case("0.01", 1)]
        fn yields_hundredths_when_decimal(#[case] raw: &str, #[case] hundredths: u32) {
            assert_eq!(Cpus::parse(raw).unwrap().hundredths(), hundredths);
        }

        #[rstest]
        #[case::zero("0")]
        #[case::above_max("64.01")]
        #[case::three_decimals("0.125")]
        #[case::negative("-1")]
        #[case::empty("")]
        #[case::text("one")]
        #[case::leading_dot(".5")]
        fn rejects_when_not_in_range_decimal(#[case] raw: &str) {
            assert_eq!(Cpus::parse(raw).unwrap_err().field, "cpus");
        }
    }

    mod cpus_display {
        use super::*;

        #[rstest]
        #[case(100, "1")]
        #[case(50, "0.5")]
        #[case(25, "0.25")]
        #[case(105, "1.05")]
        fn renders_shortest_decimal(#[case] hundredths: u32, #[case] shown: &str) {
            assert_eq!(
                Cpus::from_hundredths(hundredths).unwrap().to_string(),
                shown
            );
        }
    }

    mod memory_mb {
        use super::*;

        #[rstest]
        #[case(31)]
        #[case(262_145)]
        fn rejects_when_out_of_range(#[case] mb: u32) {
            assert_eq!(MemoryMb::new(mb).unwrap_err().field, "memory_mb");
        }

        #[test]
        fn accepts_when_default() {
            assert_eq!(MemoryMb::new(1024).unwrap(), MemoryMb::DEFAULT);
        }
    }
}
