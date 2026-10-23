//! Compose files: parsing and validation, the images they pull,
//! the generated override and the `.env` liftoff writes.
//!
//! # Pipeline
//!
//! 1. [`ComposeFile::parse`] reads the YAML text the user sent. Only syntax
//!    and "the top level is a mapping" are checked here.
//! 2. [`ComposeFile::validate`] applies every compose rule and returns
//!    **all** violations at once, each with its path
//!    (`services.web.privileged: forbidden`), sorted by path so the order is
//!    stable.
//! 3. [`ComposeFile::images_resolved`] lists the images, split into
//!    `git.example.com/<owner>/<pkg>:<tag>` (checked against Forgejo) and
//!    foreign ones. Pass the final `.env` values so `${IMAGE_TAG}` resolves
//!    to the tag that is really deployed.
//! 4. [`render_override`] produces `liftoff.override.yaml` and [`merge_env`]
//!    the `.env`, from [`LiftoffVars`]; [`parse_env`] reads an `.env` back
//!    for step 3.
//!
//! The file is kept as a YAML tree rather than derived structs: the point
//! of validation is to report every problem with a path, which a
//! `deny_unknown_fields` derive (first error only) cannot do.

mod env;
mod images;
mod interpolate;
mod model;
mod overlay;
mod validate;
mod yaml;

pub use env::{LiftoffVars, merge_env, parse_env};
pub use images::{ImageRef, ImageSource};
pub use model::{ComposeError, ComposeFile};
pub use overlay::{PublishedPort, render_override};
