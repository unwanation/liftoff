#![cfg(test)]
//! CMP-01..CMP-10: compose parsing, validation and images.
//!
//! Valid and invalid files live in `tests/fixtures/compose/{ok,bad}`; a
//! `bad/<name>.yaml` has a `bad/<name>.expected` with one `path: message`
//! per line, in the order `validate` reports them (sorted by path).

use liftoff_core::compose::{ComposeError, ComposeFile, ImageSource};
use rstest::rstest;

const REGISTRY: &str = "git.example.com";

fn parse(yaml: &str) -> ComposeFile {
    ComposeFile::parse(yaml).unwrap()
}

fn errors_of(yaml: &str) -> Vec<String> {
    match parse(yaml).validate() {
        Ok(()) => Vec::new(),
        Err(errors) => errors.iter().map(ToString::to_string).collect(),
    }
}

mod validate {
    use super::*;

    // TC: v1 CMP-01
    #[test]
    fn accepts_when_one_service_has_an_image() {
        let file = parse(include_str!("fixtures/compose/ok/minimal.yaml"));
        assert_eq!(
            (file.validate(), file.services()),
            (Ok(()), vec!["web".parse().unwrap()])
        );
    }

    // TC: v1 CMP-02
    #[test]
    fn accepts_when_every_allowed_feature_is_used() {
        let file = parse(include_str!("fixtures/compose/ok/full.yaml"));
        assert_eq!(file.validate(), Ok(()));
        assert_eq!(
            file.services()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["db", "web"]
        );
    }

    // TC: v1 CMP-03
    #[rstest]
    #[case::missing_key(include_str!("fixtures/compose/bad/missing_image.yaml"))]
    #[case::null_service("services:\n  web:\n")]
    fn rejects_when_service_has_no_image(#[case] yaml: &str) {
        assert_eq!(errors_of(yaml), ["services.web.image: required"]);
    }

    // TC: v1 CMP-04
    #[rstest]
    #[case("build")]
    #[case("privileged")]
    #[case("cap_add")]
    #[case("devices")]
    #[case("security_opt")]
    #[case("network_mode")]
    #[case("pid")]
    #[case("ipc")]
    #[case("userns_mode")]
    #[case("cgroup_parent")]
    #[case("ports")]
    #[case("extends")]
    #[case("sysctls")]
    #[case("ulimits")]
    #[case("platform")]
    #[case("volumes_from")]
    #[case("runtime")]
    #[case("deploy")]
    fn rejects_with_one_error_when_service_uses_forbidden_key(#[case] key: &str) {
        let yaml = format!("services:\n  web:\n    image: nginx\n    {key}: x\n");
        assert_eq!(errors_of(&yaml), [format!("services.web.{key}: forbidden")]);
    }

    // TC: v1 CMP-05
    #[rstest]
    #[case::service_key(
        "services:\n  web:\n    image: nginx\n    foo: 1\n",
        "services.web.foo: unknown key"
    )]
    #[case::top_level_key("services:\n  web:\n    image: nginx\nfoo: 1\n", "foo: unknown key")]
    #[case::extension_field(
        "x-anchors: {}\nservices:\n  web:\n    image: nginx\n",
        "x-anchors: unknown key"
    )]
    fn rejects_when_key_is_unknown(#[case] yaml: &str, #[case] expected: &str) {
        assert_eq!(errors_of(yaml), [expected]);
    }

    // TC: v1 CMP-06
    #[rstest]
    #[case::relative_bind("./data:/x", "services.web.volumes[0]: bind mounts are forbidden")]
    #[case::docker_socket(
        "/var/run/docker.sock:/x",
        "services.web.volumes[0]: bind mounts are forbidden"
    )]
    #[case::home_bind("~/data:/x", "services.web.volumes[0]: bind mounts are forbidden")]
    #[case::undeclared_named(
        "cache:/x",
        "services.web.volumes[0]: volume \"cache\" is not declared in top-level volumes"
    )]
    #[case::anonymous(
        "/data",
        "services.web.volumes[0]: anonymous volumes are not allowed, declare a named volume"
    )]
    fn rejects_when_short_volume_is_not_a_declared_named_volume(
        #[case] volume: &str,
        #[case] expected: &str,
    ) {
        let yaml = format!("services:\n  web:\n    image: nginx\n    volumes:\n      - {volume}\n");
        assert_eq!(errors_of(&yaml), [expected]);
    }

    // TC: v1 CMP-06 (compose interpolates `${X}` in the value but not in the
    // declared key, so a declared `${X}` volume must not legitimise a bind)
    #[test]
    fn rejects_when_volume_source_is_a_variable() {
        let yaml = "services:\n  web:\n    image: nginx\n    volumes:\n      - ${X}:/sock\nvolumes:\n  ${X}: {}\n";
        assert_eq!(
            errors_of(yaml),
            [
                "services.web.volumes[0]: variables are not allowed in names and volume sources",
                "volumes.${X}: variables are not allowed in names and volume sources"
            ]
        );
    }

    // TC: v1 CMP-06 (compose-go reads `C:/etc` as a Windows bind path)
    #[test]
    fn rejects_when_volume_source_is_a_drive_letter_path() {
        let yaml = "services:\n  web:\n    image: nginx\n    volumes:\n      - C:/etc:/x\nvolumes:\n  C: {}\n";
        assert_eq!(
            errors_of(yaml),
            ["services.web.volumes[0]: bind mounts are forbidden"]
        );
    }

    // TC: v1 CMP-06
    #[rstest]
    #[case::bind(
        "type: bind\n        source: /srv\n        target: /x",
        "services.web.volumes[0].type: bind mounts are forbidden"
    )]
    #[case::undeclared(
        "type: volume\n        source: cache\n        target: /x",
        "services.web.volumes[0].source: volume \"cache\" is not declared in top-level volumes"
    )]
    #[case::unknown_type(
        "type: npipe\n        source: a\n        target: /x",
        "services.web.volumes[0].type: unsupported volume type \"npipe\""
    )]
    fn rejects_when_long_volume_is_not_a_declared_named_volume(
        #[case] volume: &str,
        #[case] expected: &str,
    ) {
        let yaml = format!("services:\n  web:\n    image: nginx\n    volumes:\n      - {volume}\n");
        assert_eq!(errors_of(&yaml), [expected]);
    }

    // TC: v1 CMP-06
    #[rstest]
    #[case::external("external: true", "volumes.data.external: forbidden")]
    #[case::driver_opts("driver_opts: {type: nfs}", "volumes.data.driver_opts: forbidden")]
    #[case::driver("driver: local", "volumes.data.driver: forbidden")]
    #[case::name("name: other_project_data", "volumes.data.name: forbidden")]
    fn rejects_when_top_level_volume_is_not_plain(#[case] body: &str, #[case] expected: &str) {
        let yaml = format!("services:\n  web:\n    image: nginx\nvolumes:\n  data:\n    {body}\n");
        assert_eq!(errors_of(&yaml), [expected]);
    }

    // TC: v1 CMP-06
    #[test]
    fn accepts_when_service_mounts_tmpfs() {
        let yaml = "services:\n  web:\n    image: nginx\n    tmpfs:\n      - /tmp\n    volumes:\n      - type: tmpfs\n        target: /cache\n";
        assert_eq!(errors_of(yaml), Vec::<String>::new());
    }

    // TC: v1 CMP-07
    #[rstest]
    #[case::external("external: true", "networks.back.external: forbidden")]
    #[case::driver_opts("driver_opts: {a: b}", "networks.back.driver_opts: forbidden")]
    #[case::ipam("ipam: {driver: default}", "networks.back.ipam: forbidden")]
    #[case::host_driver("driver: host", "networks.back.driver: forbidden")]
    fn rejects_when_top_level_network_is_not_plain(#[case] body: &str, #[case] expected: &str) {
        let yaml = format!("services:\n  web:\n    image: nginx\nnetworks:\n  back:\n    {body}\n");
        assert_eq!(errors_of(&yaml), [expected]);
    }

    // TC: v1 CMP-07
    #[rstest]
    #[case::list_form(
        "networks: [nowhere]",
        "services.web.networks[0]: network \"nowhere\" is not declared in top-level networks"
    )]
    #[case::map_form(
        "networks:\n      nowhere: {}",
        "services.web.networks.nowhere: network \"nowhere\" is not declared in top-level networks"
    )]
    fn rejects_when_service_uses_undeclared_network(#[case] body: &str, #[case] expected: &str) {
        let yaml = format!("services:\n  web:\n    image: nginx\n    {body}\n");
        assert_eq!(errors_of(&yaml), [expected]);
    }

    // TC: v1 CMP-07
    #[test]
    fn accepts_when_service_uses_the_implicit_default_network() {
        let yaml = "services:\n  web:\n    image: nginx\n    networks: [default]\n";
        assert_eq!(errors_of(yaml), Vec::<String>::new());
    }

    // TC: v1 CMP-08
    #[rstest]
    #[case::include(
        "include:\n  - other.yaml\nservices:\n  web:\n    image: nginx\n",
        "include: forbidden"
    )]
    #[case::secret_from_file(
        "secrets:\n  token:\n    file: ./token\nservices:\n  web:\n    image: nginx\n",
        "secrets.token.file: forbidden"
    )]
    #[case::env_file_other_name(
        "services:\n  web:\n    image: nginx\n    env_file: prod.env\n",
        "services.web.env_file: must be \".env\""
    )]
    #[case::env_file_list(
        "services:\n  web:\n    image: nginx\n    env_file:\n      - .env\n      - ../secret.env\n",
        "services.web.env_file[1]: must be \".env\""
    )]
    fn rejects_when_file_is_pulled_in(#[case] yaml: &str, #[case] expected: &str) {
        assert_eq!(errors_of(yaml), [expected]);
    }

    // TC: v1 CMP-08
    #[rstest]
    #[case::string("env_file: .env")]
    #[case::list("env_file: [.env]")]
    fn accepts_when_env_file_is_dot_env(#[case] line: &str) {
        let yaml = format!("services:\n  web:\n    image: nginx\n    {line}\n");
        assert_eq!(errors_of(&yaml), Vec::<String>::new());
    }

    // TC: v1 CMP-09
    #[rstest]
    #[case::everything_wrong("everything_wrong")]
    fn lists_every_error_sorted_by_path_when_file_has_many(#[case] name: &str) {
        let (yaml, expected) = fixture_pair(name);
        let expected: Vec<&str> = expected.lines().collect();
        let first = errors_of(&yaml);
        assert_eq!(first, expected);
        assert_eq!(errors_of(&yaml), first, "second run gives the same order");
    }

    // TC: v1 CMP-03 (fixture-driven)
    #[rstest]
    #[case::missing_image("missing_image")]
    fn reports_the_fixture_errors_when_bad_file(#[case] name: &str) {
        let (yaml, expected) = fixture_pair(name);
        assert_eq!(errors_of(&yaml), expected.lines().collect::<Vec<_>>());
    }

    fn fixture_pair(name: &str) -> (String, String) {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compose/bad");
        (
            std::fs::read_to_string(dir.join(format!("{name}.yaml"))).unwrap(),
            std::fs::read_to_string(dir.join(format!("{name}.expected"))).unwrap(),
        )
    }

    #[rstest]
    #[case::not_yaml("services: [", "yaml")]
    #[case::not_a_mapping("- a\n- b\n", "mapping")]
    fn parse_fails_when_text_is_not_a_compose_mapping(#[case] text: &str, #[case] mention: &str) {
        let err: ComposeError = ComposeFile::parse(text).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains(mention),
            "{err} should mention {mention}"
        );
    }

    #[test]
    fn rejects_when_services_is_missing() {
        assert_eq!(errors_of("volumes: {}\n"), ["services: required"]);
    }

    #[test]
    fn rejects_when_service_name_is_not_a_compose_name() {
        assert_eq!(
            errors_of("services:\n  \"-bad\":\n    image: nginx\n"),
            ["services.-bad: invalid service name"]
        );
    }

    #[test]
    fn rejects_when_config_is_not_inline_content() {
        let yaml = "services:\n  web:\n    image: nginx\n    configs: [conf, missing]\nconfigs:\n  conf:\n    file: ./conf\n";
        assert_eq!(
            errors_of(yaml),
            [
                "configs.conf.content: required",
                "configs.conf.file: forbidden",
                "services.web.configs[1]: config \"missing\" is not declared in top-level configs",
            ]
        );
    }
}

mod images {
    use super::*;

    const YAML: &str = "services:
  web:
    image: git.example.com/bob/app:sha-1
  db:
    image: postgres:17
  untagged:
    image: git.example.com/bob/tool
  nested:
    image: git.example.com/bob/app/sub:sha-1
  templated:
    image: git.example.com/bob/app:${IMAGE_TAG}
";

    // TC: v1 CMP-10
    #[test]
    fn splits_internal_images_into_owner_package_and_tag() {
        let images = parse(YAML).images(REGISTRY);
        let got: Vec<(String, String, ImageSource)> = images
            .into_iter()
            .map(|i| (i.service.to_string(), i.reference, i.source))
            .collect();
        let tag = |t: &str| t.parse().unwrap();
        assert_eq!(
            got,
            vec![
                (
                    "web".to_owned(),
                    "git.example.com/bob/app:sha-1".to_owned(),
                    ImageSource::Internal {
                        owner: "bob".to_owned(),
                        package: "app".to_owned(),
                        tag: tag("sha-1")
                    }
                ),
                (
                    "db".to_owned(),
                    "postgres:17".to_owned(),
                    ImageSource::External
                ),
                (
                    "untagged".to_owned(),
                    "git.example.com/bob/tool".to_owned(),
                    ImageSource::Internal {
                        owner: "bob".to_owned(),
                        package: "tool".to_owned(),
                        tag: tag("latest")
                    }
                ),
                (
                    "nested".to_owned(),
                    "git.example.com/bob/app/sub:sha-1".to_owned(),
                    ImageSource::Invalid(
                        "expected git.example.com/<owner>/<package>[:<tag>]".to_owned()
                    )
                ),
                (
                    "templated".to_owned(),
                    "git.example.com/bob/app:${IMAGE_TAG}".to_owned(),
                    ImageSource::Invalid("invalid tag \"${IMAGE_TAG}\"".to_owned())
                ),
            ]
        );
    }

    // TC: v1 CMP-10 (the tag liftoff puts in .env resolves `${IMAGE_TAG}`)
    #[test]
    fn resolves_the_tag_when_image_interpolates_env() {
        let vars = [("IMAGE_TAG".to_owned(), "sha-9".to_owned())].into();
        let image = parse(YAML)
            .images_resolved(&vars, REGISTRY)
            .into_iter()
            .find(|i| i.service.as_str() == "templated")
            .unwrap();
        assert_eq!(
            (image.reference, image.source),
            (
                "git.example.com/bob/app:sha-9".to_owned(),
                ImageSource::Internal {
                    owner: "bob".to_owned(),
                    package: "app".to_owned(),
                    tag: "sha-9".parse().unwrap()
                }
            )
        );
    }

    // TC: v1 CMP-10 (syntax compose understands but liftoff does not must
    // fail closed, or the Forgejo permission check would be skipped)
    #[rstest]
    #[case::required(
        "${IMG:?x}",
        "IMG=git.example.com/alice/private:v1",
        "unsupported variable syntax"
    )]
    #[case::required_plain(
        "${IMG?x}",
        "IMG=git.example.com/alice/private:v1",
        "unsupported variable syntax"
    )]
    #[case::alternative("${X:+git.example.com/a/b:1}", "X=1", "unsupported variable syntax")]
    #[case::alternative_plain(
        "${IMG+x}",
        "IMG=git.example.com/alice/private:v1",
        "unsupported variable syntax"
    )]
    #[case::nested(
        "${NOPE:-${A}}",
        "A=git.example.com/alice/private:v1",
        "containing \"$\""
    )]
    #[case::unset_everything("${NOPE}", "A=1", "image is empty")]
    fn rejects_when_image_interpolation_cannot_be_resolved(
        #[case] image: &str,
        #[case] env: &str,
        #[case] reason: &str,
    ) {
        let vars = liftoff_core::compose::parse_env(env).unwrap();
        let images = parse(&format!("services:\n  web:\n    image: \"{image}\"\n"))
            .images_resolved(&vars, REGISTRY);
        assert!(
            matches!(&images[0].source, ImageSource::Invalid(r) if r.contains(reason)),
            "{:?}",
            images[0]
        );
    }

    // TC: v1 CMP-10 (a `.env` compose reads differently from liftoff must be
    // refused, not skipped)
    #[rstest]
    #[case::colon("IMG: git.example.com/alice/private:v1")]
    #[case::export_tab("export\tIMG=git.example.com/alice/private:v1")]
    #[case::comment("IMG=\"git.example.com/alice/private:v1\" # pinned")]
    #[case::quote_comment("IMG='git.example.com/alice/private:v1'#x")]
    fn refuses_when_env_line_is_read_differently_by_compose(#[case] env: &str) {
        assert!(liftoff_core::compose::parse_env(env).is_err(), "{env}");
    }

    // TC: v1 CMP-10 (a value that compose would trim must not slip past as External)
    #[test]
    fn never_external_when_env_value_has_leading_space() {
        let env = "IMG= git.example.com/alice/private:v1";
        let Ok(vars) = liftoff_core::compose::parse_env(env) else {
            return;
        };
        let images =
            parse("services:\n  web:\n    image: \"${IMG}\"\n").images_resolved(&vars, REGISTRY);
        assert!(
            !matches!(images[0].source, ImageSource::External),
            "{:?}",
            images[0]
        );
    }

    // TC: v1 CMP-10 (the classifier never lets a misparse come out External)
    #[test]
    fn rejects_when_image_reference_has_quote_or_comment() {
        let images =
            parse("services:\n  web:\n    image: \"\\\"git.example.com/a/b:1\"\n").images(REGISTRY);
        assert!(
            matches!(images[0].source, ImageSource::Invalid(_)),
            "{:?}",
            images[0]
        );
    }

    // TC: v1 CMP-10 (the registry is the configured one, not a built-in)
    #[test]
    fn classifies_by_the_configured_registry_when_it_is_not_the_default() {
        let yaml = "services:\n  a:\n    image: reg.example/bob/app:1\n  b:\n    image: git.example.com/bob/app:1\n  c:\n    image: reg.example/bob/a/b:1\n";
        let got: Vec<ImageSource> = parse(yaml)
            .images("reg.example")
            .into_iter()
            .map(|i| i.source)
            .collect();
        assert_eq!(
            got,
            vec![
                ImageSource::Internal {
                    owner: "bob".to_owned(),
                    package: "app".to_owned(),
                    tag: "1".parse().unwrap()
                },
                ImageSource::External,
                ImageSource::Invalid("expected reg.example/<owner>/<package>[:<tag>]".to_owned()),
            ]
        );
    }

    // TC: v1 CMP-10 (compose reads these from the docker process, which wins
    // over `.env`; liftoff cannot know their value, so it must refuse)
    #[rstest]
    #[case::home("git.example.com${HOME}/alice/private:v1")]
    #[case::home_bare("git.example.com$HOME/alice/private:v1")]
    #[case::home_default("${HOME:-x}")]
    #[case::path("${PATH}")]
    #[case::docker_config("${DOCKER_CONFIG-x}")]
    fn rejects_when_image_uses_a_variable_of_the_docker_process(#[case] image: &str) {
        let vars = liftoff_core::compose::parse_env("HOME=\nPATH=x\nDOCKER_CONFIG=y").unwrap();
        let images = parse(&format!("services:\n  web:\n    image: \"{image}\"\n"))
            .images_resolved(&vars, REGISTRY);
        assert!(
            matches!(&images[0].source, ImageSource::Invalid(r) if r.contains("process environment")),
            "{:?}",
            images[0]
        );
    }
}
