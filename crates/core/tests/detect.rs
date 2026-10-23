#![cfg(test)]
//! DET-01..DET-10: stack detection.
//!
//! Each case is a directory under `tests/fixtures/detect/<case>/`, read
//! through [`DirTree`]; the proposed manifest is compared, as
//! `liftoff.toml` text, with a snapshot reviewed by hand
//! (`tests/snapshots/det*.snap`). The empty directory (DET-10) proposes
//! nothing, and its snapshot is the list of questions the CLI asks.

use std::path::PathBuf;

use liftoff_core::detect::{Detected, DirTree, StackDetector};
use liftoff_core::domain::TemplateGroup;
use liftoff_core::manifest::ProjectManifest;
use rstest::rstest;

/// The detection of fixture `case`.
fn detect(case: &str) -> Detected {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/detect")
        .join(case);
    StackDetector::detect(&DirTree::new(root))
}

/// The proposed manifest of fixture `case` as `liftoff.toml` text.
fn proposed_toml(case: &str) -> String {
    detect(case)
        .manifest()
        .expect("the fixture proposes a manifest")
        .to_toml()
}

/// Compares `value` with `tests/snapshots/<name>.snap`.
fn assert_matches_snapshot(name: &str, value: &str) {
    insta::with_settings!({ prepend_module_to_snapshot => false, omit_expression => true }, {
        insta::assert_snapshot!(name, value);
    });
}

mod detect {
    use super::*;

    // TC: DET-01
    #[test]
    fn proposes_vite_service_on_5173_with_bun_dev_when_vite_and_bun_lock() {
        assert_matches_snapshot("det01_vite_bun", &proposed_toml("vite-bun"));
    }

    // TC: DET-01 (`bun.lockb` is Bun's binary lockfile: present, not UTF-8)
    #[test]
    fn proposes_bun_dev_when_only_binary_bun_lockb() {
        let dev = detect("vite-bun-lockb").dev.map(|dev| dev.command);
        assert_eq!(dev.as_deref(), Some("bun dev"));
    }

    // TC: DET-02
    #[test]
    fn proposes_next_service_on_3000_with_pnpm_dev_when_next_and_pnpm_lock() {
        assert_matches_snapshot("det02_next_pnpm", &proposed_toml("next-pnpm"));
    }

    // TC: DET-03
    #[test]
    fn proposes_hono_service_on_3000_with_bun_dev_when_hono_and_bun_lock() {
        assert_matches_snapshot("det03_hono_bun", &proposed_toml("hono-bun"));
    }

    // TC: DET-04
    #[test]
    fn proposes_axum_service_on_8080_with_cargo_run_when_cargo_toml_has_axum() {
        assert_matches_snapshot("det04_axum", &proposed_toml("axum"));
    }

    // TC: DET-05
    #[test]
    fn proposes_fastapi_service_on_8000_with_uv_uvicorn_when_pyproject_and_uv_lock() {
        assert_matches_snapshot("det05_fastapi_uv", &proposed_toml("fastapi-uv"));
    }

    // TC: DET-06
    #[test]
    fn proposes_go_service_on_8080_with_go_run_when_go_mod() {
        assert_matches_snapshot("det06_go", &proposed_toml("go"));
    }

    // TC: DET-07
    #[test]
    fn proposes_the_exposed_compose_service_when_one_service_has_expose() {
        assert_matches_snapshot("det07_compose_expose", &proposed_toml("compose-expose"));
    }

    // TC: DET-08
    #[test]
    fn proposes_componentize_js_function_when_dependency_on_componentize_js() {
        assert_matches_snapshot("det08_function_ts", &proposed_toml("function-ts"));
    }

    // TC: DET-09
    #[test]
    fn proposes_componentize_py_function_when_dependency_on_componentize_py() {
        assert_matches_snapshot("det09_function_python", &proposed_toml("function-python"));
    }

    // TC: DET-10
    #[test]
    fn proposes_no_manifest_when_directory_is_empty() {
        assert_eq!(detect("empty").manifest(), None);
    }

    // TC: DET-10
    #[test]
    fn asks_kind_expose_build_dev_and_group_when_directory_is_empty() {
        let questions = format!("{:#?}", detect("empty").questions);
        assert_matches_snapshot("det10_empty_questions", &questions);
    }

    // TC: DET-01, DET-02, DET-03, DET-04, DET-05, DET-06, DET-07, DET-08, DET-09
    #[rstest]
    #[case::vite_bun("vite-bun")]
    #[case::next_pnpm("next-pnpm")]
    #[case::hono_bun("hono-bun")]
    #[case::axum("axum")]
    #[case::fastapi_uv("fastapi-uv")]
    #[case::go("go")]
    #[case::compose_expose("compose-expose")]
    #[case::function_ts("function-ts")]
    #[case::function_python("function-python")]
    fn asks_nothing_when_the_files_settle_the_manifest(#[case] case: &str) {
        assert_eq!(detect(case).questions, Vec::new());
    }

    // TC: DET-01, DET-02, DET-03, DET-04, DET-05, DET-06, DET-07, DET-08, DET-09
    #[rstest]
    #[case::vite_bun("vite-bun")]
    #[case::next_pnpm("next-pnpm")]
    #[case::hono_bun("hono-bun")]
    #[case::axum("axum")]
    #[case::fastapi_uv("fastapi-uv")]
    #[case::go("go")]
    #[case::compose_expose("compose-expose")]
    #[case::function_ts("function-ts")]
    #[case::function_python("function-python")]
    fn reads_back_the_proposal_when_rendered_as_liftoff_toml(#[case] case: &str) {
        let proposed = detect(case).manifest().expect("a proposal");
        assert_eq!(
            ProjectManifest::parse(&proposed.to_toml()).unwrap(),
            proposed
        );
    }
}

/// The template group by default, which the manifest proposal
/// does not carry.
mod group {
    use super::*;

    // TC: DET-01, DET-02, DET-03, DET-04, DET-05, DET-06, DET-07, DET-08, DET-09, DET-10
    #[rstest]
    #[case::vite_bun("vite-bun", Some(TemplateGroup::Web))]
    #[case::next_pnpm("next-pnpm", Some(TemplateGroup::Web))]
    #[case::hono_bun("hono-bun", Some(TemplateGroup::Api))]
    #[case::axum("axum", Some(TemplateGroup::Api))]
    #[case::fastapi_uv("fastapi-uv", Some(TemplateGroup::Api))]
    #[case::go("go", Some(TemplateGroup::Api))]
    #[case::compose_expose("compose-expose", Some(TemplateGroup::Service))]
    #[case::function_ts("function-ts", Some(TemplateGroup::Function))]
    #[case::function_python("function-python", Some(TemplateGroup::Function))]
    #[case::empty("empty", None)]
    fn defaults_by_the_detected_stack_when_fixture(
        #[case] case: &str,
        #[case] expected: Option<TemplateGroup>,
    ) {
        assert_eq!(detect(case).group, expected);
    }
}
