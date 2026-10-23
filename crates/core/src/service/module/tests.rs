//! REG-01…07 at the service: [`ModuleService`] over the world's fakes,
//! the fake registry serving [`FakeRelease`]s signed with its own key and
//! the fake module runtime, whose components are their manifests' JSON.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::*;
use crate::System;
use crate::domain::ConfigValue;
use crate::fakes::{Call, FakeRelease, PROBE_MANIFEST, RegistryCall, RuntimeCall, World};
use crate::module::RegistryKey;

use crate::service::{ModuleSettings, Settings};

fn name(raw: &str) -> ModuleName {
    ModuleName::new(raw).unwrap()
}

fn version(raw: &str) -> ModuleVersion {
    ModuleVersion::new(raw).unwrap()
}

/// A configuration of `fields`.
fn config(fields: &[(&str, Value)]) -> Map<String, Value> {
    fields
        .iter()
        .map(|(field, value)| ((*field).to_owned(), value.clone()))
        .collect()
}

/// `probe`'s required configuration: `greeting = "hi"`.
fn greeting() -> Map<String, Value> {
    config(&[("greeting", json!("hi"))])
}

/// The install of `module` (newest release) with `config`.
fn request(module: &str, config: Map<String, Value>) -> InstallRequest {
    InstallRequest {
        name: name(module),
        version: None,
        config,
    }
}

/// The install of `module` at `version` with `config`.
fn request_at(module: &str, version: &str, config: Map<String, Value>) -> InstallRequest {
    InstallRequest {
        version: Some(ModuleVersion::new(version).unwrap()),
        ..request(module, config)
    }
}

/// The world with `releases` published and the log cleared.
async fn world_with(releases: Vec<FakeRelease>) -> World {
    let world = World::new().await;
    for release in releases {
        world.ports.registry.publish(release).unwrap();
    }
    world.log().clear();
    world
}

/// The world with the fake `probe` 0.1.0 published.
async fn world() -> World {
    world_with(vec![FakeRelease::probe()]).await
}

/// The world with `probe` installed (`greeting = "hi"`, `token =
/// "s3cret"`) and the log cleared.
async fn installed() -> World {
    let world = world().await;
    world
        .modules()
        .install(
            &world.owner,
            request(
                "probe",
                config(&[("greeting", json!("hi")), ("token", json!("s3cret"))]),
            ),
        )
        .await
        .unwrap();
    world.log().clear();
    world
}

/// `PROBE_MANIFEST` at another version.
fn probe_at(version: &str) -> String {
    PROBE_MANIFEST.replace(
        r#""version": "0.1.0""#,
        &format!(r#""version": "{version}""#),
    )
}

/// The release of `probe` at `version`.
fn probe_release(version: &str) -> FakeRelease {
    FakeRelease::of_manifest(&probe_at(version)).unwrap()
}

/// A release with nothing but a name, a version and `requires`.
fn bare(module: &str, requires: &[&str]) -> FakeRelease {
    FakeRelease::of_manifest(
        &json!({"name": module, "version": "0.1.0", "requires": requires}).to_string(),
    )
    .unwrap()
}

/// The runtime calls so far.
fn runtime_calls(world: &World) -> Vec<RuntimeCall> {
    world
        .log()
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Runtime(runtime) => Some(runtime),
            _ => None,
        })
        .collect()
}

/// The downloads so far, by URL.
fn downloads(world: &World) -> Vec<String> {
    world
        .log()
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Registry(RegistryCall::Download(url)) => Some(url),
            _ => None,
        })
        .collect()
}

/// Whether anything of `module` was kept: a file, a row, a registration.
async fn kept_anything(world: &World, module: &str) -> bool {
    !world.ports.modules.files().is_empty()
        || world
            .ports
            .store
            .module(&name(module))
            .await
            .unwrap()
            .is_some()
        || world
            .ports
            .modules
            .registry()
            .status(&name(module))
            .is_some()
}

/// The text of the stored config field `field` of `module`: `plain:<json>`
/// or the opened `sealed:<plaintext>`.
async fn stored_field(world: &World, module: &str, field: &str) -> Option<String> {
    let row = world.ports.store.module(&name(module)).await.unwrap()?;
    row.config.get(field).map(|value| match value {
        ConfigValue::Plain(json) => format!("plain:{json}"),
        ConfigValue::Sealed(sealed) => {
            format!("sealed:{}", world.ports.secrets.decrypt(sealed).unwrap())
        }
    })
}

mod install {
    use super::*;

    // TC: REG-01
    #[tokio::test]
    async fn refuses_as_registry_failure_when_signature_is_by_another_key() {
        let world = world_with(vec![FakeRelease::probe().signed_by_another_key()]).await;
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert!(
            matches!(
                &refused,
                Err(Error::Upstream {
                    system: System::Registry,
                    ..
                })
            ),
            "{refused:?}"
        );
    }

    // TC: REG-01
    #[tokio::test]
    async fn keeps_nothing_when_signature_is_by_another_key() {
        let world = world_with(vec![FakeRelease::probe().signed_by_another_key()]).await;
        let _refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert_eq!(
            (
                kept_anything(&world, "probe").await,
                runtime_calls(&world).iter().any(|call| matches!(
                    call,
                    RuntimeCall::Save { .. } | RuntimeCall::Load { .. }
                ))
            ),
            (false, false)
        );
    }

    // TC: REG-01 (a signature of other bytes: the served file was swapped
    // and the index's sha256 swapped with it)
    #[tokio::test]
    async fn refuses_as_registry_failure_when_signature_is_of_other_bytes() {
        let world = world_with(vec![
            FakeRelease::probe().signature_of(b"the original component"),
        ])
        .await;
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert!(
            matches!(
                &refused,
                Err(Error::Upstream {
                    system: System::Registry,
                    ..
                })
            ),
            "{refused:?}"
        );
    }

    // TC: REG-01 (the sha256 of the index is checked too)
    #[tokio::test]
    async fn refuses_without_saving_when_sha256_differs() {
        let world = world_with(vec![FakeRelease::probe().sha256_of(b"another file")]).await;
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert_eq!(
            (
                matches!(
                    &refused,
                    Err(Error::Upstream {
                        system: System::Registry,
                        ..
                    })
                ),
                world.ports.modules.files()
            ),
            (true, Vec::new()),
            "{refused:?}"
        );
    }

    // TC: REG-01 (the placeholder key of a binary whose
    // release key is not pinned refuses every install, as install.sh does
    // without RELEASE_KEY)
    #[tokio::test]
    async fn refuses_unavailable_before_download_when_registry_key_is_unpinned() {
        let world = world().await;
        let unpinned = Settings {
            modules: ModuleSettings {
                registry_key: RegistryKey::new(""),
                ..world.settings.modules.clone()
            },
            ..(*world.settings).clone()
        };
        let refused = ModuleService::new(Arc::clone(&world.ports), Arc::new(unpinned))
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert_eq!(
            (
                matches!(&refused, Err(Error::Unavailable(why)) if why.contains("registry key")),
                downloads(&world)
            ),
            (true, Vec::<String>::new()),
            "{refused:?}"
        );
    }

    // TC: REG-01 (the signed component must be the
    // module and version asked for)
    #[tokio::test]
    async fn refuses_validation_when_component_names_another_module() {
        let other = json!({"name": "other", "version": "0.1.0"}).to_string();
        let world = world_with(vec![
            FakeRelease::new(PROBE_MANIFEST, other.into_bytes()).unwrap(),
        ])
        .await;
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert!(matches!(&refused, Err(Error::Validation(_))), "{refused:?}");
    }

    // TC: REG-02
    #[tokio::test]
    async fn refuses_naming_both_worlds_when_wit_version_differs() {
        let world = world_with(vec![FakeRelease::probe().wit_version("0.2.0")]).await;
        let refused = world
            .modules()
            .install(&world.owner, request_at("probe", "0.1.0", greeting()))
            .await;
        assert!(
            matches!(&refused, Err(Error::Conflict(why)) if why.contains("liftoff:module@0.2.0") && why.contains("liftoff:module@0.1.0")),
            "{refused:?}"
        );
    }

    // TC: REG-02 (the world is checked before the download)
    #[tokio::test]
    async fn refuses_before_download_when_wit_version_differs() {
        let world = world_with(vec![FakeRelease::probe().wit_version("0.2.0")]).await;
        let refused = world
            .modules()
            .install(&world.owner, request_at("probe", "0.1.0", greeting()))
            .await;
        assert_eq!(
            (
                matches!(&refused, Err(Error::Conflict(_))),
                downloads(&world)
            ),
            (true, Vec::<String>::new()),
            "{refused:?}"
        );
    }

    // TC: REG-02 (`liftoffd_min` is checked like the
    // world)
    #[tokio::test]
    async fn refuses_naming_needed_version_when_liftoffd_is_older_than_liftoffd_min() {
        let world = world_with(vec![FakeRelease::probe().liftoffd_min("9.0.0")]).await;
        let refused = world
            .modules()
            .install(&world.owner, request_at("probe", "0.1.0", greeting()))
            .await;
        assert_eq!(
            (
                matches!(&refused, Err(Error::Conflict(why)) if why.contains("9.0.0")),
                downloads(&world)
            ),
            (true, Vec::<String>::new()),
            "{refused:?}"
        );
    }

    // TC: REG-02 (design: without a version the newest
    // release of this world and daemon is picked)
    #[tokio::test]
    async fn installs_newest_compatible_release_when_version_omitted() {
        let world = world_with(vec![
            probe_release("0.1.0"),
            probe_release("0.1.1"),
            probe_release("0.2.0").wit_version("0.2.0"),
            probe_release("0.1.2").liftoffd_min("9.0.0"),
        ])
        .await;
        let installed = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert_eq!(
            installed
                .map(|record| record.installed.version.to_string())
                .ok(),
            Some("0.1.1".to_owned())
        );
    }

    // TC: REG-03
    #[tokio::test]
    async fn refuses_module_required_naming_oidc_when_oidc_is_not_installed() {
        let world = world_with(vec![bare("oidc", &[]), bare("tenants", &["oidc"])]).await;
        let refused = world
            .modules()
            .install(&world.owner, request("tenants", Map::new()))
            .await;
        assert!(
            matches!(&refused, Err(Error::ModuleRequired(module)) if module == "oidc"),
            "{refused:?}"
        );
    }

    // TC: REG-03 (refused before the download)
    #[tokio::test]
    async fn refuses_before_download_when_dependency_is_not_installed() {
        let world = world_with(vec![bare("oidc", &[]), bare("tenants", &["oidc"])]).await;
        let refused = world
            .modules()
            .install(&world.owner, request("tenants", Map::new()))
            .await;
        assert_eq!(
            (
                matches!(&refused, Err(Error::ModuleRequired(_))),
                downloads(&world)
            ),
            (true, Vec::<String>::new()),
            "{refused:?}"
        );
    }

    // TC: REG-03
    #[tokio::test]
    async fn installs_when_dependency_is_installed() {
        let world = world_with(vec![bare("oidc", &[]), bare("tenants", &["oidc"])]).await;
        let modules = world.modules();
        modules
            .install(&world.owner, request("oidc", Map::new()))
            .await
            .unwrap();
        let installed = modules
            .install(&world.owner, request("tenants", Map::new()))
            .await;
        assert_eq!(
            installed
                .map(|record| record.installed.name.to_string())
                .ok(),
            Some("tenants".to_owned())
        );
    }

    /// A release of `module` that replaces `replaces`.
    fn replacing(module: &str, replaces: &[&str]) -> FakeRelease {
        FakeRelease::of_manifest(
            &json!({"name": module, "version": "0.1.0", "replaces": replaces}).to_string(),
        )
        .unwrap()
    }

    // TC: OIDC-01 (one module logs people in: a second module that
    // declares `replaces: ["login"]` is refused, so it can never reach
    // `core-call auth.create_session`)
    #[tokio::test]
    async fn refuses_conflict_when_another_module_replaces_login() {
        let world = world_with(vec![
            replacing("oidc", &["login"]),
            replacing("rogue", &["login"]),
        ])
        .await;
        let modules = world.modules();
        modules
            .install(&world.owner, request("oidc", Map::new()))
            .await
            .unwrap();
        let refused = modules
            .install(&world.owner, request("rogue", Map::new()))
            .await;
        assert_eq!(
            (
                matches!(refused, Err(Error::Conflict(_))),
                world
                    .ports
                    .store
                    .module(&name("rogue"))
                    .await
                    .unwrap()
                    .is_none()
            ),
            (true, true),
            "{refused:?}"
        );
    }

    // TC: OIDC-01 (an installed login module that failed to load at start
    // is not in the runtime's registry, yet it still replaces login: its
    // kept component says so)
    #[tokio::test]
    async fn refuses_conflict_when_login_module_failed_to_load() {
        let world = world_with(vec![
            replacing("oidc", &["login"]),
            replacing("rogue", &["login"]),
        ])
        .await;
        let modules = world.modules();
        modules
            .install(&world.owner, request("oidc", Map::new()))
            .await
            .unwrap();
        world.ports.modules.unload(&name("oidc")).await.unwrap();
        let refused = modules
            .install(&world.owner, request("rogue", Map::new()))
            .await;
        assert_eq!(
            (
                matches!(refused, Err(Error::Conflict(_))),
                world
                    .ports
                    .store
                    .module(&name("rogue"))
                    .await
                    .unwrap()
                    .is_none()
            ),
            (true, true),
            "{refused:?}"
        );
    }

    // TC: OIDC-01 (what the core trusts is the manifest the module is
    // loaded with: one that replaces login only once configured is
    // refused, and nothing of it stays)
    #[tokio::test]
    async fn refuses_conflict_and_keeps_nothing_when_loaded_manifest_replaces_login() {
        let world = world_with(vec![replacing("oidc", &["login"]), replacing("rogue", &[])]).await;
        let modules = world.modules();
        modules
            .install(&world.owner, request("oidc", Map::new()))
            .await
            .unwrap();
        world.ports.modules.load_as(
            "rogue",
            &json!({"name": "rogue", "version": "0.1.0", "replaces": ["login"]}).to_string(),
        );
        let refused = modules
            .install(&world.owner, request("rogue", Map::new()))
            .await;
        assert_eq!(
            (
                matches!(refused, Err(Error::Conflict(_))),
                world
                    .ports
                    .store
                    .module(&name("rogue"))
                    .await
                    .unwrap()
                    .is_none(),
                world
                    .ports
                    .modules
                    .registry()
                    .manifest(&name("rogue"))
                    .is_none(),
                world.ports.modules.files(),
            ),
            (
                true,
                true,
                true,
                vec![("oidc".to_owned(), "0.1.0".to_owned())]
            ),
            "{refused:?}"
        );
    }

    // TC: OIDC-01 (modules that replace nothing in common install side by
    // side)
    #[tokio::test]
    async fn installs_when_no_other_module_replaces_its_commands() {
        let world = world_with(vec![replacing("oidc", &["login"]), replacing("tools", &[])]).await;
        let modules = world.modules();
        modules
            .install(&world.owner, request("oidc", Map::new()))
            .await
            .unwrap();
        let installed = modules
            .install(&world.owner, request("tools", Map::new()))
            .await;
        assert!(installed.is_ok(), "{installed:?}");
    }

    // TC: REG-04
    #[tokio::test]
    async fn refuses_validation_naming_field_when_config_has_wrong_type() {
        let world = world().await;
        let refused = world
            .modules()
            .install(
                &world.owner,
                request("probe", config(&[("greeting", json!(42))])),
            )
            .await;
        assert!(
            matches!(&refused, Err(error @ Error::Validation(_)) if error.to_string().starts_with("config.greeting: ")),
            "{refused:?}"
        );
    }

    // TC: REG-04
    #[tokio::test]
    async fn refuses_validation_naming_field_when_required_field_is_missing() {
        let world = world().await;
        let refused = world
            .modules()
            .install(&world.owner, request("probe", Map::new()))
            .await;
        assert!(
            matches!(&refused, Err(error @ Error::Validation(_)) if error.to_string().starts_with("config.greeting: ")),
            "{refused:?}"
        );
    }

    // TC: REG-04 (a refused configuration keeps nothing)
    #[tokio::test]
    async fn keeps_nothing_when_config_breaks_schema() {
        let world = world().await;
        let _refused = world
            .modules()
            .install(
                &world.owner,
                request("probe", config(&[("greeting", json!(42))])),
            )
            .await;
        assert!(!kept_anything(&world, "probe").await);
    }

    // TC: REG-05
    #[tokio::test]
    async fn seals_secret_field_when_stored() {
        let world = installed().await;
        assert_eq!(
            (
                stored_field(&world, "probe", "greeting").await,
                stored_field(&world, "probe", "token").await
            ),
            (
                Some(r#"plain:"hi""#.to_owned()),
                Some("sealed:s3cret".to_owned())
            )
        );
    }

    // TC: REG-05 (a secret below the top level would be stored in plain
    // text, so the component is refused)
    #[tokio::test]
    async fn keeps_nothing_when_schema_has_nested_secret() {
        let manifest = json!({
            "name": "probe",
            "version": "0.1.0",
            "config_schema": {"properties": {"cloudflare": {
                "type": "object",
                "properties": {"token": {"type": "string", "format": "secret"}}
            }}}
        })
        .to_string();
        let world = world_with(vec![FakeRelease::of_manifest(&manifest).unwrap()]).await;
        let refused = world
            .modules()
            .install(
                &world.owner,
                request(
                    "probe",
                    config(&[("cloudflare", json!({"token": "s3cret"}))]),
                ),
            )
            .await;
        assert_eq!(
            (
                matches!(&refused, Err(Error::Validation(_))),
                kept_anything(&world, "probe").await
            ),
            (true, false),
            "{refused:?}"
        );
    }

    // TC: REG-05 (`config-get` sees the secret opened)
    #[tokio::test]
    async fn loads_module_with_secret_opened_when_installed() {
        let world = installed().await;
        assert_eq!(
            world.ports.modules.config("probe"),
            Some(BTreeMap::from([
                ("greeting".to_owned(), "hi".to_owned()),
                ("token".to_owned(), "s3cret".to_owned()),
            ]))
        );
    }

    // TC: REG-06
    #[tokio::test]
    async fn registers_module_command_when_installed() {
        let world = installed().await;
        assert_eq!(
            world
                .ports
                .modules
                .registry()
                .commands()
                .into_iter()
                .map(|(module, command)| (module.to_string(), command.name))
                .collect::<Vec<_>>(),
            vec![("probe".to_owned(), "hello".to_owned())]
        );
    }

    // TC: REG-06 (the file is kept and the module loaded with its
    // migrations from 0, without a restart)
    #[tokio::test]
    async fn saves_then_loads_migrating_from_zero_when_installed() {
        let world = world().await;
        world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await
            .unwrap();
        let kept: Vec<RuntimeCall> = runtime_calls(&world)
            .into_iter()
            .filter(|call| matches!(call, RuntimeCall::Save { .. } | RuntimeCall::Load { .. }))
            .collect();
        assert_eq!(
            kept,
            vec![
                RuntimeCall::Save {
                    module: "probe".to_owned(),
                    version: "0.1.0".to_owned()
                },
                RuntimeCall::Load {
                    module: "probe".to_owned(),
                    migrate_from: Some(0)
                },
            ]
        );
    }

    // TC: REG-01 (a failure after the checks takes the
    // file and the row back)
    #[tokio::test]
    async fn keeps_nothing_when_module_fails_to_load() {
        let world = world().await;
        world
            .ports
            .modules
            .fail_next("load", Error::invalid("component", "does not instantiate"));
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert_eq!(
            (refused.is_err(), kept_anything(&world, "probe").await),
            (true, false),
            "{refused:?}"
        );
    }

    // TC: REG-01 (the row is stored before the file)
    #[tokio::test]
    async fn keeps_nothing_when_row_cannot_be_stored() {
        let world = world().await;
        world
            .ports
            .store
            .fail_next("insert_module", Error::internal_msg("disk full"));
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert_eq!(
            (refused.is_err(), kept_anything(&world, "probe").await),
            (true, false),
            "{refused:?}"
        );
    }

    // TC: REG-01 (a file that cannot be kept takes the
    // row back)
    #[tokio::test]
    async fn keeps_nothing_when_file_cannot_be_kept() {
        let world = world().await;
        world
            .ports
            .modules
            .fail_next("save", Error::internal_msg("disk full"));
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert_eq!(
            (refused.is_err(), kept_anything(&world, "probe").await),
            (true, false),
            "{refused:?}"
        );
    }

    // TC: REG-06 (of two installs of one release at
    // once, the one whose row loses leaves the other's file alone)
    #[tokio::test]
    async fn leaves_file_of_other_install_when_row_is_taken_meanwhile() {
        let world = world().await;
        world
            .ports
            .modules
            .save(&name("probe"), &version("0.1.0"), PROBE_MANIFEST.as_bytes())
            .await
            .unwrap();
        world
            .ports
            .store
            .fail_next("insert_module", Error::conflict("module probe"));
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert_eq!(
            (
                matches!(&refused, Err(Error::Conflict(_))),
                world.ports.modules.files()
            ),
            (true, vec![("probe".to_owned(), "0.1.0".to_owned())]),
            "{refused:?}"
        );
    }

    // TC: REG-04 (a refused value may be a secret and is never repeated)
    #[tokio::test]
    async fn leaves_value_out_of_refusal_when_secret_field_breaks_schema() {
        let world = world().await;
        let refused = world
            .modules()
            .install(
                &world.owner,
                request(
                    "probe",
                    config(&[("greeting", json!("hi")), ("token", json!(987_654_321))]),
                ),
            )
            .await;
        assert_eq!(
            refused
                .map_err(|error| {
                    let text = error.to_string();
                    (
                        text.starts_with("config.token: "),
                        text.contains("987654321"),
                    )
                })
                .err(),
            Some((true, false))
        );
    }

    // TC: REG-06 (one installation per module)
    #[tokio::test]
    async fn conflicts_when_module_is_installed_already() {
        let world = installed().await;
        let refused = world
            .modules()
            .install(&world.owner, request("probe", greeting()))
            .await;
        assert!(matches!(&refused, Err(Error::Conflict(_))), "{refused:?}");
    }
}

mod list {
    use super::*;

    // TC: REG-06 (`GET /modules` shows the command at once)
    #[tokio::test]
    async fn shows_command_of_module_right_after_install() {
        let world = installed().await;
        let listed = world.modules().list(&world.owner).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|record| (
                    record.installed.name.to_string(),
                    record.installed.enabled,
                    record.manifest.as_ref().map(|manifest| manifest
                        .commands
                        .iter()
                        .map(|command| command.name.clone())
                        .collect::<Vec<_>>())
                ))
                .collect::<Vec<_>>(),
            vec![("probe".to_owned(), true, Some(vec!["hello".to_owned()]))]
        );
    }
    // TC: MOD-09 (the listing shows what the host
    // disabled without writing it; the daemon's periodic sync writes it)
    #[tokio::test]
    async fn shows_host_disable_without_writing_rows_when_listed() {
        let world = installed().await;
        let _disabled = world
            .ports
            .modules
            .registry()
            .disable(&name("probe"), "module probe trapped");
        world.log().clear();
        let listed = world.modules().list(&world.owner).await.unwrap();
        assert_eq!(
            (
                listed
                    .iter()
                    .map(|record| (record.installed.enabled, record.installed.error.clone()))
                    .collect::<Vec<_>>(),
                world.log().calls().contains(&Call::Store("update_module"))
            ),
            (
                vec![(false, Some("module probe trapped".to_owned()))],
                false
            )
        );
    }
}

mod update {
    use super::*;

    fn new_config(config: Map<String, Value>) -> ModuleChange {
        ModuleChange {
            enabled: None,
            config: Some(config),
        }
    }

    // TC: REG-04 (`lof module config` is checked like the install)
    #[tokio::test]
    async fn refuses_validation_naming_field_when_new_config_breaks_schema() {
        let world = installed().await;
        let refused = world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                new_config(config(&[("greeting", json!(42))])),
            )
            .await;
        assert!(
            matches!(&refused, Err(error @ Error::Validation(_)) if error.to_string().starts_with("config.greeting: ")),
            "{refused:?}"
        );
    }

    // TC: REG-05 (a new secret is sealed too)
    #[tokio::test]
    async fn seals_new_secret_field_when_config_changed() {
        let world = installed().await;
        world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                new_config(config(&[
                    ("greeting", json!("hey")),
                    ("token", json!("n3w")),
                ])),
            )
            .await
            .unwrap();
        assert_eq!(
            stored_field(&world, "probe", "token").await,
            Some("sealed:n3w".to_owned())
        );
    }

    // TC: REG-05 (the module is loaded again with the new config, secrets
    // opened, without migrating)
    #[tokio::test]
    async fn reloads_module_with_new_config_when_config_changed() {
        let world = installed().await;
        world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                new_config(config(&[
                    ("greeting", json!("hey")),
                    ("token", json!("n3w")),
                ])),
            )
            .await
            .unwrap();
        assert_eq!(
            (
                world.ports.modules.config("probe"),
                runtime_calls(&world)
                    .into_iter()
                    .filter(|call| matches!(call, RuntimeCall::Load { .. }))
                    .collect::<Vec<_>>()
            ),
            (
                Some(BTreeMap::from([
                    ("greeting".to_owned(), "hey".to_owned()),
                    ("token".to_owned(), "n3w".to_owned()),
                ])),
                vec![RuntimeCall::Load {
                    module: "probe".to_owned(),
                    migrate_from: None
                }]
            )
        );
    }

    // TC: REG-05 (a new configuration is merged field by
    // field, so a sealed secret stays)
    #[tokio::test]
    async fn keeps_sealed_token_when_only_greeting_changed() {
        let world = installed().await;
        world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                new_config(config(&[("greeting", json!("hey"))])),
            )
            .await
            .unwrap();
        assert_eq!(
            (
                stored_field(&world, "probe", "greeting").await,
                stored_field(&world, "probe", "token").await
            ),
            (
                Some(r#"plain:"hey""#.to_owned()),
                Some("sealed:s3cret".to_owned())
            )
        );
    }

    // TC: REG-05 (`null` removes a field)
    #[tokio::test]
    async fn removes_field_when_set_to_null() {
        let world = installed().await;
        world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                new_config(config(&[("token", Value::Null)])),
            )
            .await
            .unwrap();
        assert_eq!(
            (
                stored_field(&world, "probe", "token").await,
                world
                    .ports
                    .modules
                    .config("probe")
                    .is_some_and(|loaded| loaded.contains_key("token"))
            ),
            (None, false)
        );
    }

    // TC: MOD-09 (`PATCH {enabled: false}` disables it, with who did it as the reason, in the registry and in `modules`)
    #[tokio::test]
    async fn disables_module_with_reason_when_disabled() {
        let world = installed().await;
        world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                ModuleChange {
                    enabled: Some(false),
                    config: None,
                },
            )
            .await
            .unwrap();
        let why = format!("disabled by {}", world.owner.login);
        let row = world.ports.store.module(&name("probe")).await.unwrap();
        assert_eq!(
            (
                world.ports.modules.registry().status(&name("probe")),
                row.map(|row| (row.enabled, row.error))
            ),
            (
                Some(crate::module::ModuleStatus {
                    enabled: false,
                    error: Some(why.clone())
                }),
                Some((false, Some(why)))
            )
        );
    }

    // TC: REG-03 (enabling a module that failed to load
    // checks its dependencies again)
    #[tokio::test]
    async fn refuses_module_required_when_enabled_module_lost_its_dependency() {
        let world = world_with(vec![bare("oidc", &[]), bare("tenants", &["oidc"])]).await;
        let modules = world.modules();
        for module in ["oidc", "tenants"] {
            modules
                .install(&world.owner, request(module, Map::new()))
                .await
                .unwrap();
        }
        world.ports.modules.unload(&name("tenants")).await.unwrap();
        world
            .ports
            .store
            .delete_module(&name("oidc"))
            .await
            .unwrap();
        let refused = modules
            .update(
                &world.owner,
                &name("tenants"),
                ModuleChange {
                    enabled: Some(true),
                    config: None,
                },
            )
            .await;
        assert!(
            matches!(&refused, Err(Error::ModuleRequired(module)) if module == "oidc"),
            "{refused:?}"
        );
    }

    // TC: MOD-09 (a module the runtime disabled comes back only through `lof module enable`, also before its row is synced)
    #[tokio::test]
    async fn keeps_module_disabled_with_its_error_when_config_changed_before_sync() {
        let world = installed().await;
        let _disabled = world
            .ports
            .modules
            .registry()
            .disable(&name("probe"), "module probe trapped");
        world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                new_config(config(&[("greeting", json!("hey"))])),
            )
            .await
            .unwrap();
        let trapped = Some("module probe trapped".to_owned());
        let row = world.ports.store.module(&name("probe")).await.unwrap();
        assert_eq!(
            (
                world.ports.modules.registry().status(&name("probe")),
                row.map(|row| (row.enabled, row.error))
            ),
            (
                Some(crate::module::ModuleStatus {
                    enabled: false,
                    error: trapped.clone()
                }),
                Some((false, trapped))
            )
        );
    }

    // TC: MOD-09 (the running module never holds a
    // configuration its row does not)
    #[tokio::test]
    async fn keeps_running_config_when_row_cannot_be_stored() {
        let world = installed().await;
        world
            .ports
            .store
            .fail_next("update_module", Error::internal_msg("disk full"));
        let refused = world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                new_config(config(&[("greeting", json!("hey"))])),
            )
            .await;
        assert_eq!(
            (
                refused.is_err(),
                world
                    .ports
                    .modules
                    .config("probe")
                    .and_then(|loaded| loaded.get("greeting").cloned())
            ),
            (true, Some("hi".to_owned()))
        );
    }

    // TC: MOD-09 (a reload that fails leaves the row as
    // it was)
    #[tokio::test]
    async fn keeps_stored_config_when_reload_fails() {
        let world = installed().await;
        world
            .ports
            .modules
            .fail_next("load", Error::invalid("component", "does not instantiate"));
        let refused = world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                new_config(config(&[("greeting", json!("hey"))])),
            )
            .await;
        assert_eq!(
            (
                refused.is_err(),
                stored_field(&world, "probe", "greeting").await
            ),
            (true, Some(r#"plain:"hi""#.to_owned()))
        );
    }

    // TC: OIDC-01 (enabling a login module that failed to load at start
    // checks `replaces` again: another module took over login meanwhile)
    #[tokio::test]
    async fn refuses_conflict_when_enabled_while_another_module_replaces_login() {
        let world = world_with(vec![
            FakeRelease::of_manifest(
                &json!({"name": "oidc", "version": "0.1.0", "replaces": ["login"]}).to_string(),
            )
            .unwrap(),
        ])
        .await;
        let modules = world.modules();
        modules
            .install(&world.owner, request("oidc", Map::new()))
            .await
            .unwrap();
        world.ports.modules.unload(&name("oidc")).await.unwrap();
        let mut failed = world
            .ports
            .store
            .module(&name("oidc"))
            .await
            .unwrap()
            .unwrap();
        failed.enabled = false;
        failed.error = Some("failed to load".to_owned());
        world.ports.store.update_module(&failed).await.unwrap();
        // A second login module, as an older daemon may have let in.
        let rogue = json!({"name": "rogue", "version": "0.1.0", "replaces": ["login"]}).to_string();
        let runtime = &world.ports.modules;
        runtime
            .save(&name("rogue"), &version("0.1.0"), rogue.as_bytes())
            .await
            .unwrap();
        world
            .ports
            .store
            .insert_module(&InstalledModule {
                name: name("rogue"),
                sha256: sha256_hex(rogue.as_bytes()),
                ..failed.clone()
            })
            .await
            .unwrap();
        runtime
            .load(&name("rogue"), rogue.as_bytes(), &BTreeMap::new(), None)
            .await
            .unwrap();
        let refused = modules
            .update(
                &world.owner,
                &name("oidc"),
                ModuleChange {
                    enabled: Some(true),
                    config: None,
                },
            )
            .await;
        let row = world.ports.store.module(&name("oidc")).await.unwrap();
        assert_eq!(
            (
                matches!(refused, Err(Error::Conflict(_))),
                runtime.registry().manifest(&name("oidc")).is_none(),
                row.map(|row| row.enabled)
            ),
            (true, true, Some(false)),
            "{refused:?}"
        );
    }

    // TC: MOD-09 (`lof module enable` after a fix clears the error, in the registry and in `modules`)
    #[tokio::test]
    async fn enables_module_and_clears_error_when_enabled() {
        let world = installed().await;
        let _disabled = world
            .ports
            .modules
            .registry()
            .disable(&name("probe"), "module probe trapped");
        world
            .modules()
            .update(
                &world.owner,
                &name("probe"),
                ModuleChange {
                    enabled: Some(true),
                    config: None,
                },
            )
            .await
            .unwrap();
        let row = world.ports.store.module(&name("probe")).await.unwrap();
        assert_eq!(
            (
                world
                    .ports
                    .modules
                    .registry()
                    .status(&name("probe"))
                    .map(|status| status.enabled),
                row.map(|row| (row.enabled, row.error))
            ),
            (Some(true), Some((true, None)))
        );
    }
}

mod remove {
    use super::*;

    // TC: REG-07
    #[tokio::test]
    async fn keeps_tables_when_removed_without_purge() {
        let world = installed().await;
        world
            .modules()
            .remove(&world.owner, &name("probe"), false)
            .await
            .unwrap();
        assert_eq!(
            (
                runtime_calls(&world).contains(&RuntimeCall::Purge("probe".to_owned())),
                world
                    .ports
                    .store
                    .module(&name("probe"))
                    .await
                    .unwrap()
                    .is_none()
            ),
            (false, true),
            "{:?}",
            runtime_calls(&world)
        );
    }

    // TC: REG-07
    #[tokio::test]
    async fn drops_tables_when_removed_with_purge() {
        let world = installed().await;
        world
            .modules()
            .remove(&world.owner, &name("probe"), true)
            .await
            .unwrap();
        assert!(
            runtime_calls(&world).contains(&RuntimeCall::Purge("probe".to_owned())),
            "{:?}",
            runtime_calls(&world)
        );
    }

    // TC: REG-07 (its commands, routes and hooks go with it)
    #[tokio::test]
    async fn forgets_module_commands_when_removed() {
        let world = installed().await;
        world
            .modules()
            .remove(&world.owner, &name("probe"), false)
            .await
            .unwrap();
        assert_eq!(world.ports.modules.registry().commands(), Vec::new());
    }

    // TC: REG-03 (a module stays while another installed
    // module requires it)
    #[tokio::test]
    async fn conflicts_when_installed_module_requires_it() {
        let world = world_with(vec![bare("oidc", &[]), bare("tenants", &["oidc"])]).await;
        let modules = world.modules();
        for module in ["oidc", "tenants"] {
            modules
                .install(&world.owner, request(module, Map::new()))
                .await
                .unwrap();
        }
        let refused = modules.remove(&world.owner, &name("oidc"), false).await;
        assert!(
            matches!(&refused, Err(Error::Conflict(why)) if why.contains("tenants")),
            "{refused:?}"
        );
    }

    // TC: REG-03 (a dependent that failed to load at
    // start still holds what it requires)
    #[tokio::test]
    async fn conflicts_when_module_that_requires_it_is_not_loaded() {
        let world = world_with(vec![bare("oidc", &[]), bare("tenants", &["oidc"])]).await;
        let modules = world.modules();
        for module in ["oidc", "tenants"] {
            modules
                .install(&world.owner, request(module, Map::new()))
                .await
                .unwrap();
        }
        world.ports.modules.unload(&name("tenants")).await.unwrap();
        let refused = modules.remove(&world.owner, &name("oidc"), false).await;
        assert!(
            matches!(&refused, Err(Error::Conflict(why)) if why.contains("tenants")),
            "{refused:?}"
        );
    }

    // TC: REG-07
    #[tokio::test]
    async fn not_found_when_module_is_not_installed() {
        let world = world().await;
        let refused = world
            .modules()
            .remove(&world.owner, &name("probe"), false)
            .await;
        assert!(matches!(&refused, Err(Error::NotFound(_))), "{refused:?}");
    }
}

mod restore {
    use super::*;

    // TC: REG-06 (after a restart the installed modules are loaded again, their commands there without an install)
    #[tokio::test]
    async fn loads_installed_module_again_without_migrating_when_restored() {
        let world = installed().await;
        world.ports.modules.unload(&name("probe")).await.unwrap();
        world.log().clear();
        world.modules().restore().await.unwrap();
        assert_eq!(
            (
                runtime_calls(&world)
                    .into_iter()
                    .filter(|call| matches!(call, RuntimeCall::Load { .. }))
                    .collect::<Vec<_>>(),
                world.ports.modules.config("probe"),
                world.ports.modules.registry().commands().len()
            ),
            (
                vec![RuntimeCall::Load {
                    module: "probe".to_owned(),
                    migrate_from: None
                }],
                Some(BTreeMap::from([
                    ("greeting".to_owned(), "hi".to_owned()),
                    ("token".to_owned(), "s3cret".to_owned()),
                ])),
                1
            )
        );
    }

    // TC: MOD-09 (a module that fails to load at start is marked `enabled = false` with the error; the daemon goes on)
    #[tokio::test]
    async fn disables_row_with_error_when_module_fails_to_load() {
        let world = installed().await;
        world.ports.modules.unload(&name("probe")).await.unwrap();
        world
            .ports
            .modules
            .fail_next("load", Error::invalid("component", "does not instantiate"));
        world.modules().restore().await.unwrap();
        let row = world.ports.store.module(&name("probe")).await.unwrap();
        assert_eq!(
            row.map(|row| (
                row.enabled,
                row.error
                    .is_some_and(|error| error.contains("does not instantiate"))
            )),
            Some((false, true))
        );
    }

    // TC: REG-01 (the kept file is checked against the SHA-256 verified at install before it is loaded again)
    #[tokio::test]
    async fn disables_row_without_loading_when_kept_file_was_changed() {
        let world = installed().await;
        world.ports.modules.unload(&name("probe")).await.unwrap();
        world
            .ports
            .modules
            .save(
                &name("probe"),
                &version("0.1.0"),
                format!("{PROBE_MANIFEST} ").as_bytes(),
            )
            .await
            .unwrap();
        world.log().clear();
        world.modules().restore().await.unwrap();
        let row = world.ports.store.module(&name("probe")).await.unwrap();
        assert_eq!(
            (
                row.map(|row| (
                    row.enabled,
                    row.error.is_some_and(|error| error.contains("SHA-256"))
                )),
                runtime_calls(&world)
                    .iter()
                    .any(|call| matches!(call, RuntimeCall::Load { .. }))
            ),
            (Some((false, true)), false)
        );
    }

    // TC: MOD-09 (a module disabled before the restart stays disabled)
    #[tokio::test]
    async fn keeps_module_disabled_with_its_error_when_row_is_disabled() {
        let world = installed().await;
        let _disabled = world
            .ports
            .modules
            .registry()
            .disable(&name("probe"), "module probe trapped");
        world.modules().sync_status().await.unwrap();
        world.ports.modules.unload(&name("probe")).await.unwrap();
        world.modules().restore().await.unwrap();
        assert_eq!(
            world.ports.modules.registry().status(&name("probe")),
            Some(crate::module::ModuleStatus {
                enabled: false,
                error: Some("module probe trapped".to_owned())
            })
        );
    }
}

mod sync_status {
    use super::*;

    // TC: MOD-09 (`enabled` and `error` of a module the host
    // disabled survive a restart in `modules`)
    #[tokio::test]
    async fn writes_disable_of_registry_into_row_when_synced() {
        let world = installed().await;
        let _disabled = world
            .ports
            .modules
            .registry()
            .disable(&name("probe"), "module probe trapped");
        world.modules().sync_status().await.unwrap();
        let row = world.ports.store.module(&name("probe")).await.unwrap();
        assert_eq!(
            row.map(|row| (row.enabled, row.error)),
            Some((false, Some("module probe trapped".to_owned())))
        );
    }
}
