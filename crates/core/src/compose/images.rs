//! The images a compose file pulls (CMP-10).

use std::collections::BTreeMap;

use super::yaml::Value;

use super::interpolate::interpolate;
use super::model::ComposeFile;
use crate::domain::{ImageTag, ServiceName};

/// Variables the docker process gets from liftoff itself (see
/// `ProcessDocker`). Compose resolves them from the process environment
/// before `.env`, so liftoff cannot resolve them from `.env` the same way.
const PROCESS_VARS: [&str; 3] = ["HOME", "PATH", "DOCKER_CONFIG"];

/// Where an image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// `<registry>/<owner>/<package>:<tag>`: checked against Forgejo
    /// with the `Sudo` of whoever deploys. No tag means `latest`.
    Internal {
        /// Forgejo user or organisation.
        owner: String,
        /// Container package name.
        package: String,
        /// The tag.
        tag: ImageTag,
    },
    /// Any other registry; never checked.
    External,
    /// On the internal registry but not of the shape above (nested path, digest,
    /// bad tag, unresolved variable). The deploy must fail: letting it
    /// through would skip the permission check.
    Invalid(String),
}

/// One service's image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// The service.
    pub service: ServiceName,
    /// The reference as written (or as interpolated, see
    /// [`ComposeFile::images_resolved`]).
    pub reference: String,
    /// The reference as written in the compose file, before interpolation:
    /// what a message may show, since the interpolated one may carry a
    /// secret.
    pub written: String,
    /// Internal or foreign.
    pub source: ImageSource,
}

impl ImageSource {
    fn classify(reference: &str, registry: &str) -> Self {
        if reference.is_empty() {
            return Self::Invalid("image is empty".to_owned());
        }
        if reference.contains(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '#' | '\\')) {
            return Self::Invalid("invalid character in image".to_owned());
        }
        let Some(path) = reference
            .strip_prefix(registry)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            return if reference.contains('$') {
                Self::Invalid("unresolved variable in image".to_owned())
            } else {
                Self::External
            };
        };
        let shape = || Self::Invalid(format!("expected {registry}/<owner>/<package>[:<tag>]"));
        if path.contains('@') {
            return Self::Invalid("digest references are not supported".to_owned());
        }
        let Some((owner, rest)) = path.split_once('/') else {
            return shape();
        };
        let (package, tag) = rest.split_once(':').unwrap_or((rest, "latest"));
        if owner.is_empty() || package.is_empty() || package.contains('/') {
            return shape();
        }
        ImageTag::new(tag).map_or_else(
            |_| Self::Invalid(format!("invalid tag \"{tag}\"")),
            |tag| Self::Internal {
                owner: owner.to_owned(),
                package: package.to_owned(),
                tag,
            },
        )
    }
}

impl ComposeFile {
    /// The image of every service, in file order, exactly as written.
    /// `registry` is the configured internal registry (`Settings::registry`).
    /// Services without a string `image` are skipped (validation reports
    /// them). `${VAR}` placeholders are *not* resolved, so such an image
    /// shows up as [`ImageSource::Invalid`]; use
    /// [`ComposeFile::images_resolved`] for deploys.
    #[must_use]
    pub fn images(&self, registry: &str) -> Vec<ImageRef> {
        self.images_with(registry, |reference| Ok(reference.to_owned()))
    }

    /// Like [`ComposeFile::images`] after interpolating `vars` (the final
    /// `.env`, see [`merge_env`](super::merge_env)), which is what
    /// `docker compose` will pull.
    #[must_use]
    pub fn images_resolved(
        &self,
        vars: &BTreeMap<String, String>,
        registry: &str,
    ) -> Vec<ImageRef> {
        self.images_with(registry, |reference| {
            if let Some(name) = process_var(reference) {
                return Err(format!(
                    "${name} comes from the process environment, which liftoff does not resolve"
                ));
            }
            interpolate(reference, vars)
        })
    }

    fn images_with(
        &self,
        registry: &str,
        resolve: impl Fn(&str) -> Result<String, String>,
    ) -> Vec<ImageRef> {
        self.service_entries()
            .filter_map(|(name, body)| {
                let service = ServiceName::new(name).ok()?;
                let written = body.get("image").and_then(Value::as_str)?.trim();
                let (reference, source) = match resolve(written) {
                    Err(reason) => (written.to_owned(), ImageSource::Invalid(reason)),
                    Ok(reference) => {
                        let source = ImageSource::classify(&reference, registry);
                        (reference, source)
                    }
                };
                Some(ImageRef {
                    service,
                    reference,
                    written: written.to_owned(),
                    source,
                })
            })
            .collect()
    }
}

/// The first process-provided variable `text` mentions, in any spelling
/// (`$NAME`, `${NAME}`, `${NAME:-d}`); `$$` is an escaped dollar.
fn process_var(text: &str) -> Option<&'static str> {
    let mut rest = text;
    while let Some((_, tail)) = rest.split_once('$') {
        if let Some(after) = tail.strip_prefix('$') {
            rest = after;
            continue;
        }
        let name = tail.strip_prefix('{').unwrap_or(tail);
        let end = name
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(name.len());
        let name = name.get(..end).unwrap_or_default();
        if let Some(found) = PROCESS_VARS.iter().find(|var| **var == name) {
            return Some(found);
        }
        rest = tail;
    }
    None
}
