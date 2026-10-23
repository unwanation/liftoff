//! [`ComposeFile`] and [`ComposeError`].

use core::fmt;

use super::yaml::{Mapping, Value};

use crate::domain::{ContainerPort, ServiceName};
use crate::error::Error;

/// One problem found in a compose file: where, and what.
///
/// Displays as `services.web.privileged: forbidden`. A problem with the
/// document as a whole has an empty path and displays as the bare message.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComposeError {
    /// Dotted path of the offending key, `[n]` for list items.
    pub path: String,
    /// What is wrong with it.
    pub message: String,
}

impl ComposeError {
    pub(super) fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Folds a list of problems into one [`Error::Validation`] on field
    /// `compose`, one `path: message` per `; `.
    #[must_use]
    pub fn into_error(errors: &[Self]) -> Error {
        Error::invalid(
            "compose",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl core::error::Error for ComposeError {}

/// A compose file as the user wrote it. Not trusted until
/// [`ComposeFile::validate`] returned `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeFile {
    pub(super) root: Mapping,
}

impl ComposeFile {
    /// Parses YAML text.
    ///
    /// # Errors
    ///
    /// A [`ComposeError`] (empty path) when the text is not YAML, has
    /// duplicate keys, or its top level is not a mapping.
    pub fn parse(text: &str) -> Result<Self, ComposeError> {
        match super::yaml::parse(text) {
            Ok(Value::Mapping(root)) => Ok(Self { root }),
            Ok(_) => Err(ComposeError::new(
                "",
                "compose file must be a mapping at the top level",
            )),
            Err(err) => Err(ComposeError::new("", format!("invalid yaml: {err}"))),
        }
    }

    /// Checks every compose rule.
    ///
    /// # Errors
    ///
    /// Every violation, sorted by path then message, without duplicates.
    pub fn validate(&self) -> Result<(), Vec<ComposeError>> {
        super::validate::validate(self)
    }

    /// The names of the services, in file order. Names that are not valid
    /// compose service names are skipped ([`ComposeFile::validate`]
    /// reports them).
    #[must_use]
    pub fn services(&self) -> Vec<ServiceName> {
        self.service_entries()
            .filter_map(|(name, _)| ServiceName::new(name).ok())
            .collect()
    }

    /// Each service that carries `expose`, in file order, with its
    /// exposed ports in list order (stack detection). An entry
    /// that is not one port (`3000`, `"3000"`, `"3000/tcp"`) is skipped:
    /// a range names no single port for the domain.
    #[must_use]
    pub fn exposed(&self) -> Vec<(ServiceName, Vec<ContainerPort>)> {
        self.service_entries()
            .filter_map(|(name, body)| {
                let ports = body.get("expose")?.as_sequence()?;
                let ports = ports.iter().filter_map(single_port).collect();
                Some((ServiceName::new(name).ok()?, ports))
            })
            .collect()
    }

    /// `(name, body)` of each service whose key is a string.
    pub(super) fn service_entries(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.root
            .get("services")
            .and_then(Value::as_mapping)
            .into_iter()
            .flat_map(Mapping::iter)
            .filter_map(|(key, value)| Some((key.as_str()?, value)))
    }
}

/// One `expose` entry as a port: `3000`, `"3000"` or `"3000/tcp"`.
fn single_port(entry: &Value) -> Option<ContainerPort> {
    let port = if let Some(port) = entry.as_u64() {
        u16::try_from(port).ok()?
    } else {
        let text = entry.as_str()?;
        let port = text.split_once('/').map_or(text, |(port, _)| port);
        port.trim().parse().ok()?
    };
    ContainerPort::new(port).ok()
}
