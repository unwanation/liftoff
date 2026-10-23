#![cfg(test)]
//! The template sources the repository ships under `templates/<name>/`,
//! each published as a repository of its own; the catalogue reads its
//! `liftoff.toml` with [`ProjectManifest::parse`]. These tests hold that every one of them
//! parses, and that the files a template substitutes are there.

use std::path::{Path, PathBuf};

use liftoff_core::domain::TemplateGroup;
use liftoff_core::manifest::{Deployable, ProjectManifest};
use rstest::rstest;

/// The template sources: `templates/` at the repository root.
fn templates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates")
}

/// The `liftoff.toml` of template `name`, parsed.
fn manifest(name: &str) -> ProjectManifest {
    let path = templates().join(name).join("liftoff.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    ProjectManifest::parse(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

mod sources {
    use super::*;

    // Plan 2.10: the four templates of the phase and nothing unchecked.
    #[test]
    fn lists_the_four_templates_when_reading_the_directory() {
        let mut names: Vec<String> = std::fs::read_dir(templates())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["fn-python", "fn-rust", "fn-ts"]);
    }

    // TC: TPL-01 (the manifest of each shipped template validates)
    #[rstest]
    #[case::fn_ts("fn-ts", TemplateGroup::Function)]
    #[case::fn_rust("fn-rust", TemplateGroup::Function)]
    #[case::fn_python("fn-python", TemplateGroup::Function)]
    fn parses_with_its_name_and_group_when_template(
        #[case] name: &str,
        #[case] group: TemplateGroup,
    ) {
        let template = manifest(name).template.unwrap();
        assert_eq!((template.name.as_str(), template.group), (name, group));
    }

    // Plan 2.10: the function templates build a component and run locally.
    #[rstest]
    #[case::fn_ts("fn-ts")]
    #[case::fn_rust("fn-rust")]
    #[case::fn_python("fn-python")]
    fn declares_function_and_dev_command_when_function_template(#[case] name: &str) {
        let manifest = manifest(name);
        assert!(matches!(manifest.deploy, Deployable::Function { .. }));
        assert!(manifest.dev.is_some());
    }

    // The rule (secrets reach child processes only through the
    // environment): `wasmtime serve` takes `wasi:config` values only as
    // `-Sconfig-var` arguments, which `ps` shows, so the dev command of a
    // function template passes none.
    #[rstest]
    #[case::fn_ts("fn-ts")]
    #[case::fn_rust("fn-rust")]
    #[case::fn_python("fn-python")]
    fn keeps_secrets_off_the_command_line_when_function_template(#[case] name: &str) {
        let command = manifest(name).dev.unwrap().command;
        assert!(!command.contains("config-var"), "{name}: {command}");
    }

    // TC: CRE-05 (only the listed files are substituted: each must exist)
    #[rstest]
    #[case::fn_ts("fn-ts")]
    #[case::fn_rust("fn-rust")]
    #[case::fn_python("fn-python")]
    fn ships_every_substituted_file_when_function_template(#[case] name: &str) {
        let missing: Vec<String> = manifest(name)
            .substitute
            .iter()
            .filter(|file| !templates().join(name).join(file.as_str()).is_file())
            .map(|file| file.as_str().to_owned())
            .collect();
        assert_eq!(missing, Vec::<String>::new());
    }
}
