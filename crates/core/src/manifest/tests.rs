//! `liftoff.toml` parsing.

use super::*;
use crate::error::ValidationError;

/// The documented `liftoff.toml`, `[service]` variant.
const SPEC_EXAMPLE: &str = r#"
[template]
group = "web"
name = "acme-app"
description = "SolidJS + acme-ui, Vite, Bun"

[service]
compose = "deploy/compose.yaml"
expose = "web:3000"

[dev]
command = "bun dev"

[secrets]
SESSION_SECRET = { hint = "32+ random bytes", generate = "random:32" }
DATABASE_URL = { hint = "postgres://…", scope = "environment" }

[substitute]
files = ["README.md", "package.json", "deploy/compose.yaml"]
"#;

/// The `[function]` documented variant.
const FUNCTION_EXAMPLE: &str = r#"
[template]
group = "function"
name = "fn-ts"
description = "TypeScript function"

[function]
build = "bun install && bun run build"
component = "dist/fn.wasm"
"#;

/// `SPEC_EXAMPLE` with its `[service]` table replaced by `service`.
fn with_service(service: &str) -> String {
    format!(
        "[template]\ngroup = \"web\"\nname = \"acme-app\"\ndescription = \"d\"\n\n[service]\n{service}\n"
    )
}

/// The validation error of `text`, or a panic naming what parsed instead.
fn invalid(text: &str) -> ValidationError {
    match ProjectManifest::parse(text) {
        Err(Error::Validation(error)) => error,
        other => panic!("expected a validation error, got {other:?}"),
    }
}

/// `(field, path)` of the validation error of `text`.
fn location(text: &str) -> (&'static str, Option<String>) {
    let error = invalid(text);
    (error.field, error.path)
}

fn key(raw: &str) -> SecretKey {
    SecretKey::new(raw).unwrap()
}

mod parse {
    use super::*;

    // TC: TPL-01 (the documented example is a valid manifest)
    #[test]
    fn typed_manifest_when_spec_example() {
        let manifest = ProjectManifest::parse(SPEC_EXAMPLE).unwrap();
        let secrets = BTreeMap::from([
            (
                key("SESSION_SECRET"),
                SecretSpec {
                    hint: Some("32+ random bytes".to_owned()),
                    generate: Some(Generate::Random(32)),
                    scope: None,
                },
            ),
            (
                key("DATABASE_URL"),
                SecretSpec {
                    hint: Some("postgres://…".to_owned()),
                    generate: None,
                    scope: Some(ScopeKind::Environment),
                },
            ),
        ]);
        let (compose, expose) = match &manifest.deploy {
            Deployable::Service { compose, expose } => (compose.as_str(), expose.clone()),
            Deployable::Function { .. } => panic!("a [service] manifest: {manifest:?}"),
        };
        assert_eq!(
            (
                manifest.template.clone(),
                compose,
                expose,
                manifest.dev.clone(),
                manifest.secrets.clone(),
                manifest
                    .substitute
                    .iter()
                    .map(RepoPath::as_str)
                    .collect::<Vec<_>>(),
            ),
            (
                Some(TemplateInfo {
                    group: TemplateGroup::Web,
                    name: "acme-app".to_owned(),
                    description: "SolidJS + acme-ui, Vite, Bun".to_owned(),
                }),
                "deploy/compose.yaml",
                Some(Expose {
                    service: ServiceName::new("web").unwrap(),
                    port: ContainerPort::new(3000).unwrap(),
                }),
                Some(DevCommand {
                    command: "bun dev".to_owned(),
                }),
                secrets,
                vec!["README.md", "package.json", "deploy/compose.yaml"],
            )
        );
    }

    // TC: TPL-02 (`[function]` instead of `[service]`)
    #[test]
    fn function_when_only_function_table() {
        let manifest = ProjectManifest::parse(FUNCTION_EXAMPLE).unwrap();
        match &manifest.deploy {
            Deployable::Function { build, component } => assert_eq!(
                (build.as_str(), component.as_str()),
                ("bun install && bun run build", "dist/fn.wasm")
            ),
            Deployable::Service { .. } => panic!("a [function] manifest: {manifest:?}"),
        }
    }

    // TC: TPL-01 (`[template]` is for template repositories; a project's
    // own manifest may go without it)
    #[test]
    fn no_template_when_template_table_missing() {
        let manifest = ProjectManifest::parse(
            "[service]\ncompose = \"compose.yaml\"\nexpose = \"web:8080\"\n",
        )
        .unwrap();
        assert_eq!(
            (manifest.template, manifest.dev, manifest.secrets.len()),
            (None, None, 0)
        );
    }

    // TC: TPL-01 (every scope may be named)
    #[test]
    fn tenant_scope_when_secret_names_it() {
        let text = format!(
            "{}\n[secrets]\nSHARED = {{ scope = \"tenant\" }}\n",
            with_service("compose = \"compose.yaml\"")
        );
        let manifest = ProjectManifest::parse(&text).unwrap();
        assert_eq!(
            manifest.secrets.get(&key("SHARED")).map(|spec| spec.scope),
            Some(Some(ScopeKind::Tenant))
        );
    }
}

mod unknown_keys {
    use super::*;

    // TC: TPL-01
    #[test]
    fn validation_naming_the_table_when_top_level_key_unknown() {
        let text = format!(
            "{}\n[deploy]\nimage = \"x\"\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("deploy".to_owned()))
        );
    }

    // TC: TPL-01
    #[test]
    fn validation_naming_the_key_when_service_key_unknown() {
        let text = with_service("compose = \"compose.yaml\"\nexposee = \"web:3000\"");
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("service.exposee".to_owned()))
        );
    }

    // TC: TPL-01
    #[test]
    fn validation_naming_the_key_when_template_key_unknown() {
        let text = "[template]\ngroup = \"web\"\nname = \"a\"\ndescription = \"d\"\nicon = \"x\"\n\n[service]\ncompose = \"compose.yaml\"\n";
        assert_eq!(
            location(text),
            (ProjectManifest::FIELD, Some("template.icon".to_owned()))
        );
    }

    // TC: TPL-01
    #[test]
    fn validation_naming_the_key_when_secret_field_unknown() {
        let text = format!(
            "{}\n[secrets]\nSESSION_SECRET = {{ hnt = \"typo\" }}\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(
            location(&text),
            (
                ProjectManifest::FIELD,
                Some("secrets.SESSION_SECRET.hnt".to_owned())
            )
        );
    }

    // TC: TPL-01
    #[test]
    fn message_names_the_path_when_key_unknown() {
        let text = with_service("compose = \"compose.yaml\"\nexposee = \"web:3000\"");
        let message = Error::from(invalid(&text)).to_string();
        assert!(
            message.starts_with("manifest.service.exposee: "),
            "{message}"
        );
    }
}

mod deploy_kind {
    use super::*;

    // TC: TPL-02
    #[test]
    fn validation_when_service_and_function_together() {
        let text = format!(
            "{}\n[function]\nbuild = \"make\"\ncomponent = \"dist/fn.wasm\"\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(invalid(&text).field, ProjectManifest::FIELD);
    }

    // TC: TPL-02
    #[test]
    fn validation_when_neither_service_nor_function() {
        let text = "[template]\ngroup = \"web\"\nname = \"a\"\ndescription = \"d\"\n";
        assert_eq!(invalid(text).field, ProjectManifest::FIELD);
    }
}

mod values {
    use super::*;

    // TC: TPL-01 (`expose` is `service:port`)
    #[test]
    fn validation_on_expose_when_port_missing() {
        let text = with_service("compose = \"compose.yaml\"\nexpose = \"web\"");
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("service.expose".to_owned()))
        );
    }

    // TC: TPL-01 (the port is 1-65535)
    #[test]
    fn validation_on_expose_when_port_zero() {
        let text = with_service("compose = \"compose.yaml\"\nexpose = \"web:0\"");
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("service.expose".to_owned()))
        );
    }

    // TC: TPL-01 (the port is 1-65535)
    #[test]
    fn validation_on_expose_when_port_above_65535() {
        let text = with_service("compose = \"compose.yaml\"\nexpose = \"web:70000\"");
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("service.expose".to_owned()))
        );
    }

    // TC: TPL-01 (the service is a compose service name)
    #[test]
    fn validation_on_expose_when_service_name_invalid() {
        let text = with_service("compose = \"compose.yaml\"\nexpose = \"-web:3000\"");
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("service.expose".to_owned()))
        );
    }

    // TC: TPL-01 (`generate` is `random:<n>`)
    #[test]
    fn validation_on_generate_when_not_random() {
        let text = format!(
            "{}\n[secrets]\nSESSION_SECRET = {{ generate = \"uuid\" }}\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(
            location(&text),
            (
                ProjectManifest::FIELD,
                Some("secrets.SESSION_SECRET.generate".to_owned())
            )
        );
    }

    // TC: TPL-01 (`random:<n>` takes a number)
    #[test]
    fn validation_on_generate_when_length_not_a_number() {
        let text = format!(
            "{}\n[secrets]\nSESSION_SECRET = {{ generate = \"random:many\" }}\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(
            location(&text),
            (
                ProjectManifest::FIELD,
                Some("secrets.SESSION_SECRET.generate".to_owned())
            )
        );
    }

    // TC: TPL-01 (environment, project or tenant)
    #[test]
    fn validation_on_scope_when_unknown() {
        let text = format!(
            "{}\n[secrets]\nDATABASE_URL = {{ scope = \"global\" }}\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(
            location(&text),
            (
                ProjectManifest::FIELD,
                Some("secrets.DATABASE_URL.scope".to_owned())
            )
        );
    }

    // TC: TPL-01 (secret keys are `^[A-Z][A-Z0-9_]{0,63}$`)
    #[test]
    fn validation_on_secret_when_key_invalid() {
        let text = format!(
            "{}\n[secrets]\nsession_secret = {{ hint = \"h\" }}\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(
            location(&text),
            (
                ProjectManifest::FIELD,
                Some("secrets.session_secret".to_owned())
            )
        );
    }

    // TC: TPL-01 (web | api | function | service)
    #[test]
    fn validation_on_group_when_unknown() {
        let text = "[template]\ngroup = \"cli\"\nname = \"a\"\ndescription = \"d\"\n\n[service]\ncompose = \"compose.yaml\"\n";
        assert_eq!(
            location(text),
            (ProjectManifest::FIELD, Some("template.group".to_owned()))
        );
    }

    // TC: TPL-01, TPL-03 (the catalogue shows the description on a
    // terminal: an escape sequence in it is refused, not printed)
    #[test]
    fn validation_on_description_when_control_characters() {
        let text = "[template]\ngroup = \"web\"\nname = \"a\"\ndescription = \"x\\u001B]0;owned\\u0007\"\n\n[service]\ncompose = \"compose.yaml\"\n";
        assert_eq!(
            location(text),
            (
                ProjectManifest::FIELD,
                Some("template.description".to_owned())
            )
        );
    }

    // TC: TPL-01, TPL-03 (nor in the name)
    #[test]
    fn validation_on_name_when_control_characters() {
        let text = "[template]\ngroup = \"web\"\nname = \"a\\u001B[2J\"\ndescription = \"d\"\n\n[service]\ncompose = \"compose.yaml\"\n";
        assert_eq!(
            location(text),
            (ProjectManifest::FIELD, Some("template.name".to_owned()))
        );
    }

    // TC: TPL-01, CRE-11 (`lof create` shows a secret's hint with its
    // question: an escape sequence in it is refused, not printed)
    #[test]
    fn validation_on_secret_hint_when_control_characters() {
        let text = format!(
            "{}\n[secrets]\nDATABASE_URL = {{ hint = \"h\\u001B[2J\" }}\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(
            location(&text),
            (
                ProjectManifest::FIELD,
                Some("secrets.DATABASE_URL.hint".to_owned())
            )
        );
    }

    // TC: TPL-01 (malformed TOML is a validation error, not internal)
    #[test]
    fn validation_when_not_toml() {
        assert_eq!(
            invalid("[service\ncompose = ").field,
            ProjectManifest::FIELD
        );
    }
}

mod paths {
    use super::*;

    // TC: TPL-01 (paths stay inside the repository)
    #[test]
    fn validation_on_compose_when_absolute() {
        let text = with_service("compose = \"/etc/compose.yaml\"");
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("service.compose".to_owned()))
        );
    }

    // TC: TPL-01 (paths stay inside the repository)
    #[test]
    fn validation_on_compose_when_it_leaves_the_repository() {
        let text = with_service("compose = \"deploy/../../compose.yaml\"");
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("service.compose".to_owned()))
        );
    }

    // TC: TPL-01 (paths stay inside the repository)
    #[test]
    fn validation_on_component_when_parent_dir() {
        let text = "[function]\nbuild = \"make\"\ncomponent = \"../fn.wasm\"\n";
        assert_eq!(
            location(text),
            (
                ProjectManifest::FIELD,
                Some("function.component".to_owned())
            )
        );
    }

    // TC: TPL-01 (paths stay inside the repository)
    #[test]
    fn validation_on_substitute_when_a_file_leaves_the_repository() {
        let text = format!(
            "{}\n[substitute]\nfiles = [\"README.md\", \"../outside.txt\"]\n",
            with_service("compose = \"compose.yaml\"")
        );
        let (field, path) = location(&text);
        assert!(
            field == ProjectManifest::FIELD
                && path
                    .as_deref()
                    .is_some_and(|p| p.starts_with("substitute.files")),
            "{field} {path:?}"
        );
    }

    // TC: TPL-01, CRE-11 (`lof create` shows the `[substitute]` paths in
    // its plan: a path with a control character is refused)
    #[test]
    fn validation_on_substitute_when_a_file_has_control_characters() {
        let text = format!(
            "{}\n[substitute]\nfiles = [\"README\\u001B]0;owned\\u0007\"]\n",
            with_service("compose = \"compose.yaml\"")
        );
        assert_eq!(
            location(&text),
            (
                ProjectManifest::FIELD,
                Some("substitute.files[0]".to_owned())
            )
        );
    }

    // TC: TPL-01 (an empty path names no file)
    #[test]
    fn validation_on_compose_when_empty() {
        let text = with_service("compose = \"\"");
        assert_eq!(
            location(&text),
            (ProjectManifest::FIELD, Some("service.compose".to_owned()))
        );
    }

    // TC: TPL-01 (a `./` prefix stays inside)
    #[test]
    fn accepted_when_path_starts_with_current_dir() {
        let manifest =
            ProjectManifest::parse(&with_service("compose = \"./deploy/compose.yaml\"")).unwrap();
        assert!(
            matches!(manifest.deploy, Deployable::Service { .. }),
            "{manifest:?}"
        );
    }
}

/// `ProjectManifest::to_toml`: the text `lof create` commits for a
/// detected stack.
mod to_toml {
    use super::*;

    /// `text` parsed, rendered and parsed again.
    fn round_trip(text: &str) -> (ProjectManifest, ProjectManifest) {
        let manifest = ProjectManifest::parse(text).unwrap();
        let again = ProjectManifest::parse(&manifest.to_toml()).unwrap();
        (manifest, again)
    }

    // TC: DET-01 (the proposal is written as `liftoff.toml`)
    #[test]
    fn reads_back_unchanged_when_spec_example() {
        let (manifest, again) = round_trip(SPEC_EXAMPLE);
        assert_eq!(again, manifest);
    }

    // TC: DET-08 (the `[function]` documented variant)
    #[test]
    fn reads_back_unchanged_when_function_example() {
        let (manifest, again) = round_trip(FUNCTION_EXAMPLE);
        assert_eq!(again, manifest);
    }

    // TC: DET-01
    #[test]
    fn escapes_quotes_backslashes_and_controls_when_in_values() {
        let text = "[service]\ncompose = \"deploy/compose.yaml\"\n\n[dev]\ncommand = \"sh -c \\\"echo \\\\\\\\ a\\tb\\u0001\\\"\\n\"\n";
        let (manifest, again) = round_trip(text);
        assert_eq!(again, manifest);
    }

    // TC: DET-01 (a secret with no field is an empty inline table)
    #[test]
    fn writes_empty_inline_table_when_secret_has_no_field() {
        let text = "[service]\ncompose = \"c.yaml\"\n\n[secrets]\nTOKEN = {}\n";
        let manifest = ProjectManifest::parse(text).unwrap();
        assert_eq!(manifest.to_toml(), text);
    }

    // TC: DET-01 (tables in the documented order)
    #[test]
    fn orders_tables_as_spec_when_every_table_present() {
        let rendered = ProjectManifest::parse(SPEC_EXAMPLE).unwrap().to_toml();
        let headers: Vec<&str> = rendered
            .lines()
            .filter(|line| line.starts_with('['))
            .collect();
        assert_eq!(
            headers,
            [
                "[template]",
                "[service]",
                "[dev]",
                "[secrets]",
                "[substitute]"
            ]
        );
    }

    // TC: DET-01 (one blank line between tables, never two)
    #[test]
    fn separates_tables_by_one_blank_line_when_every_table_present() {
        let rendered = ProjectManifest::parse(SPEC_EXAMPLE).unwrap().to_toml();
        assert!(!rendered.contains("\n\n\n"), "{rendered}");
    }
}

mod constant_paths {
    use super::*;

    // TC: DET-01, DET-08 (the paths the detector proposes are valid)
    #[test]
    fn valid_when_detector_proposes_them() {
        for path in ["deploy/compose.yaml", "compose.yaml", "dist/fn.wasm"] {
            assert_eq!(RepoPath::constant(path), RepoPath::new(path).unwrap());
        }
    }
}
