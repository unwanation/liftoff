//! The template catalogue over the shared [`World`] (`TPL-03`…`TPL-06`).

use time::Duration as TimeDuration;

use super::*;
use crate::domain::{Login, RepoRef};
use crate::fakes::{FakePorts, GitCall, World};
use crate::ports::{HookResult, RepoRole, Secrets as _};

/// A template's `liftoff.toml` with `group` and `name`.
fn manifest(group: &str, name: &str) -> String {
    format!(
        "[template]\ngroup = \"{group}\"\nname = \"{name}\"\ndescription = \"{name} template\"\n\n[service]\ncompose = \"deploy/compose.yaml\"\nexpose = \"web:3000\"\n"
    )
}

fn repo(raw: &str) -> RepoRef {
    RepoRef::new(raw).unwrap()
}

fn host(raw: &str) -> Host {
    Host::new(raw).unwrap()
}

/// `owner/name` on `git.example.com`, the host of `owner`'s connection.
fn on_forge(raw: &str) -> TemplateSource {
    TemplateSource {
        host: Some(host(World::GIT_HOST)),
        repo: repo(raw),
    }
}

/// `owner/name` without a host.
fn bare(raw: &str) -> TemplateSource {
    TemplateSource {
        host: None,
        repo: repo(raw),
    }
}

/// Gives `raw` the `liftoff.toml` `toml`, readable by `owner`.
fn publish(world: &World, raw: &str, toml: &str) {
    world.ports.git.template(&repo(raw), toml);
    world
        .ports
        .git
        .grant(&world.owner.login, &repo(raw), RepoRole::Read);
}

/// The catalogue of the world with `[templates] repos = server`.
fn catalog(world: &World, server: &[TemplateSource]) -> TemplateCatalog<FakePorts> {
    TemplateCatalog::new(
        Arc::clone(&world.ports),
        Arc::new(Settings {
            templates: server.to_vec(),
            ..(*world.settings).clone()
        }),
    )
}

/// `(group, [name, ...])` of every group, in answer order.
fn shape(catalog: &Catalog) -> Vec<(TemplateGroup, Vec<String>)> {
    catalog
        .groups
        .iter()
        .map(|group| {
            (
                group.group,
                group
                    .templates
                    .iter()
                    .map(|entry| {
                        entry
                            .manifest
                            .template
                            .as_ref()
                            .map_or_else(String::new, |t| t.name.clone())
                    })
                    .collect(),
            )
        })
        .collect()
}

/// The sources the catalogue warned about, as written.
fn warned(catalog: &Catalog) -> Vec<String> {
    catalog
        .warnings
        .iter()
        .map(|warning| warning.source.to_string())
        .collect()
}

/// The `list_templates` calls in the log, as `(login, [repo, ...])`.
fn template_calls(world: &World) -> Vec<(Login, Vec<String>)> {
    world
        .ports
        .git
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            GitCall::ListTemplates { login, repos } => {
                Some((login, repos.iter().map(ToString::to_string).collect()))
            }
            _ => None,
        })
        .collect()
}

/// The three templates of the catalogue test, on `git.example.com`: the
/// `api` one configured first, so that only the order of
/// [`TemplateGroup`] (not the order of configuration) puts `web` first.
fn three(world: &World) -> Vec<TemplateSource> {
    publish(world, "acme/app", &manifest("web", "acme-app"));
    publish(world, "acme/hono-api", &manifest("api", "hono-api"));
    publish(world, "acme/astro-site", &manifest("web", "astro-site"));
    vec![
        on_forge("acme/hono-api"),
        on_forge("acme/app"),
        on_forge("acme/astro-site"),
    ]
}

/// Gives `owner` a second connection, on `code.example.org` as `login`.
/// The fake forge grants by login: as `owner` it reads what
/// [`publish`] gave, as any other login nothing.
async fn second_connection(world: &World, login: &str) {
    let mut second = world
        .records
        .git_connection(
            &world.owner,
            "code.example.org",
            world.ports.secrets.encrypt("token-on-code").unwrap(),
        )
        .unwrap();
    second.account_login = Login::new(login).unwrap();
    world.ports.store.put_git_connection(&second).await.unwrap();
}

mod grouping {
    use super::*;

    // TC: TPL-03
    #[tokio::test]
    async fn grouped_in_spec_order_and_sorted_by_name_when_three_repos() {
        let world = World::new().await;
        let server = three(&world);
        let listed = catalog(&world, &server)
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        assert_eq!(
            (shape(&listed), warned(&listed)),
            (
                vec![
                    (
                        TemplateGroup::Web,
                        vec!["acme-app".to_owned(), "astro-site".to_owned()]
                    ),
                    (TemplateGroup::Api, vec!["hono-api".to_owned()]),
                ],
                Vec::<String>::new()
            )
        );
    }

    // TC: TPL-03 (`lof templates --group G`)
    #[tokio::test]
    async fn only_that_group_when_group_named() {
        let world = World::new().await;
        let server = three(&world);
        let listed = catalog(&world, &server)
            .list(&world.owner, &[], Some(TemplateGroup::Api))
            .await
            .unwrap();
        assert_eq!(
            shape(&listed),
            vec![(TemplateGroup::Api, vec!["hono-api".to_owned()])]
        );
    }

    // TC: TPL-03 (an entry says where `lof create --template` finds it)
    #[tokio::test]
    async fn entry_names_its_host_and_repo_when_listed() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        let listed = catalog(&world, &[on_forge("acme/app")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let places: Vec<(String, String)> = listed
            .groups
            .iter()
            .flat_map(|group| &group.templates)
            .map(|entry| (entry.host.to_string(), entry.repo.to_string()))
            .collect();
        assert_eq!(
            places,
            vec![("git.example.com".to_owned(), "acme/app".to_owned())]
        );
    }

    // TC: TPL-03 (`list_templates` takes the list; one call per
    // host, as the actor's own connection)
    #[tokio::test]
    async fn one_forge_call_per_host_when_repos_share_a_host() {
        let world = World::new().await;
        let server = three(&world);
        world.log().clear();
        let _listed = catalog(&world, &server)
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        assert_eq!(
            template_calls(&world),
            vec![(
                Login::new("owner").unwrap(),
                vec![
                    "acme/hono-api".to_owned(),
                    "acme/app".to_owned(),
                    "acme/astro-site".to_owned()
                ]
            )]
        );
    }

    // TC: TPL-03 (nothing configured, nothing asked)
    #[tokio::test]
    async fn empty_without_forge_call_when_no_repos() {
        let world = World::new().await;
        let listed = catalog(&world, &[])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        assert_eq!(
            (
                listed.groups.len(),
                listed.warnings.len(),
                template_calls(&world)
            ),
            (0, 0, Vec::new())
        );
    }
}

mod skipping {
    use super::*;

    // TC: TPL-04
    #[tokio::test]
    async fn skipped_with_warning_when_repo_has_no_manifest() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("acme/plain"), RepoRole::Read);
        let listed = catalog(&world, &[on_forge("acme/app"), on_forge("acme/plain")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        assert_eq!(
            (shape(&listed), warned(&listed)),
            (
                vec![(TemplateGroup::Web, vec!["acme-app".to_owned()])],
                vec!["git.example.com/acme/plain".to_owned()]
            )
        );
    }

    // TC: TPL-04
    #[tokio::test]
    async fn warning_names_the_missing_file_when_repo_has_no_manifest() {
        let world = World::new().await;
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("acme/plain"), RepoRole::Read);
        let listed = catalog(&world, &[on_forge("acme/plain")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let reasons: Vec<&str> = listed.warnings.iter().map(|w| w.reason.as_str()).collect();
        assert!(
            reasons.len() == 1 && reasons.iter().all(|r| r.contains("liftoff.toml")),
            "{reasons:?}"
        );
    }

    // TC: TPL-04 (an invalid manifest of one repo does
    // not break the list)
    #[tokio::test]
    async fn rest_returned_when_one_manifest_invalid() {
        let world = World::new().await;
        let mut server = three(&world);
        publish(
            &world,
            "acme/broken",
            "[template]\ngroup = \"web\"\nname = \"broken\"\ndescription = \"d\"\n\n[service]\ncompose = \"compose.yaml\"\nexposee = \"web:3000\"\n",
        );
        server.insert(1, on_forge("acme/broken"));
        let listed = catalog(&world, &server)
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        assert_eq!(
            (shape(&listed), warned(&listed)),
            (
                vec![
                    (
                        TemplateGroup::Web,
                        vec!["acme-app".to_owned(), "astro-site".to_owned()]
                    ),
                    (TemplateGroup::Api, vec!["hono-api".to_owned()]),
                ],
                vec!["git.example.com/acme/broken".to_owned()]
            )
        );
    }

    // TC: TPL-04 (the warning says what is wrong with the manifest)
    #[tokio::test]
    async fn warning_names_the_bad_key_when_manifest_invalid() {
        let world = World::new().await;
        publish(
            &world,
            "acme/broken",
            "[template]\ngroup = \"web\"\nname = \"broken\"\ndescription = \"d\"\n\n[service]\ncompose = \"compose.yaml\"\nexposee = \"web:3000\"\n",
        );
        let listed = catalog(&world, &[on_forge("acme/broken")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let reasons: Vec<&str> = listed.warnings.iter().map(|w| w.reason.as_str()).collect();
        assert!(
            reasons.len() == 1 && reasons.iter().all(|r| r.contains("service.exposee")),
            "{reasons:?}"
        );
    }

    // TC: TPL-04 (a catalogue entry needs `[template]`)
    #[tokio::test]
    async fn skipped_with_warning_when_manifest_has_no_template_table() {
        let world = World::new().await;
        publish(
            &world,
            "acme/untitled",
            "[service]\ncompose = \"compose.yaml\"\nexpose = \"web:3000\"\n",
        );
        let listed = catalog(&world, &[on_forge("acme/untitled")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        assert_eq!(
            (shape(&listed), warned(&listed)),
            (Vec::new(), vec!["git.example.com/acme/untitled".to_owned()])
        );
    }

    // TC: TPL-04 (a host the actor has no connection to is skipped, the
    // forge is not asked)
    #[tokio::test]
    async fn skipped_with_warning_naming_the_host_when_no_connection_there() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        let elsewhere = TemplateSource {
            host: Some(host("github.com")),
            repo: repo("acme/tpl"),
        };
        world.log().clear();
        let listed = catalog(&world, &[on_forge("acme/app"), elsewhere])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let reasons: Vec<&str> = listed.warnings.iter().map(|w| w.reason.as_str()).collect();
        assert_eq!(
            (
                shape(&listed),
                warned(&listed),
                reasons.iter().all(|r| r.contains("github.com")),
                template_calls(&world).len()
            ),
            (
                vec![(TemplateGroup::Web, vec!["acme-app".to_owned()])],
                vec!["github.com/acme/tpl".to_owned()],
                true,
                1
            )
        );
    }

    // TC: TPL-04 (a repo without a host: the actor's only forge reads it)
    #[tokio::test]
    async fn read_through_the_only_connection_when_repo_has_no_host() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        let listed = catalog(&world, &[bare("acme/app")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let hosts: Vec<String> = listed
            .groups
            .iter()
            .flat_map(|group| &group.templates)
            .map(|entry| entry.host.to_string())
            .collect();
        assert_eq!(
            (hosts, warned(&listed)),
            (vec!["git.example.com".to_owned()], Vec::<String>::new())
        );
    }

    // TC: TPL-04 (9 spell repos without a host: with several forges the first, by host, that has its `liftoff.toml` reads it)
    #[tokio::test]
    async fn read_through_the_connection_that_has_it_when_repo_has_no_host_and_actor_has_several_connections()
     {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        second_connection(&world, "owner-on-code").await;
        world.log().clear();
        let listed = catalog(&world, &[bare("acme/app")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let hosts: Vec<String> = listed
            .groups
            .iter()
            .flat_map(|group| &group.templates)
            .map(|entry| entry.host.to_string())
            .collect();
        assert_eq!(
            (hosts, warned(&listed), template_calls(&world).len()),
            (vec!["git.example.com".to_owned()], Vec::<String>::new(), 2)
        );
    }

    // TC: TPL-04 («по порядку хостов … берётся первое, где нашёлся» — two forges have it, the first by host wins)
    #[tokio::test]
    async fn first_host_in_order_when_repo_has_no_host_and_several_forges_have_it() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        second_connection(&world, "owner").await;
        let listed = catalog(&world, &[bare("acme/app")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let hosts: Vec<String> = listed
            .groups
            .iter()
            .flat_map(|group| &group.templates)
            .map(|entry| entry.host.to_string())
            .collect();
        assert_eq!(
            (hosts, warned(&listed)),
            (vec!["code.example.org".to_owned()], Vec::<String>::new())
        );
    }

    // TC: TPL-04 (a repo without a host found on no forge is one warning
    // naming every host asked)
    #[tokio::test]
    async fn one_warning_naming_every_host_when_repo_has_no_host_and_no_forge_has_it() {
        let world = World::new().await;
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("acme/plain"), RepoRole::Read);
        second_connection(&world, "owner-on-code").await;
        let listed = catalog(&world, &[bare("acme/plain")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let reasons: Vec<&str> = listed.warnings.iter().map(|w| w.reason.as_str()).collect();
        assert!(
            warned(&listed) == vec!["acme/plain".to_owned()]
                && reasons.iter().all(|r| r.contains("code.example.org")
                    && r.contains("git.example.com")
                    && r.contains("liftoff.toml")),
            "{:?}",
            listed.warnings
        );
    }

    // TC: TPL-04 (a forge that fails is a warning: the other host's
    // templates are still listed, and an internal source is never shown)
    #[tokio::test]
    async fn rest_returned_with_warning_when_one_host_forge_fails() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        second_connection(&world, "owner").await;
        world
            .ports
            .git
            .fail_next("list_templates", Error::internal_msg("secret detail"));
        let elsewhere = TemplateSource {
            host: Some(host("code.example.org")),
            repo: repo("acme/app"),
        };
        let listed = catalog(&world, &[elsewhere, on_forge("acme/app")])
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        let reasons: Vec<&str> = listed.warnings.iter().map(|w| w.reason.as_str()).collect();
        assert_eq!(
            (shape(&listed), warned(&listed), reasons),
            (
                vec![(TemplateGroup::Web, vec!["acme-app".to_owned()])],
                vec!["code.example.org/acme/app".to_owned()],
                vec!["internal error"]
            )
        );
    }

    // TC: TPL-04 (one repository the forge fails on
    // does not hide the other templates of its host)
    #[tokio::test]
    async fn only_the_failing_repo_warned_when_a_batch_fails() {
        let world = World::new().await;
        let server = three(&world);
        // The batch fails, then the retry of its first repository alone.
        for _ in 0..2 {
            world.ports.git.fail_next(
                "list_templates",
                Error::upstream(crate::error::System::Forge, "liftoff.toml is not UTF-8"),
            );
        }
        let listed = catalog(&world, &server)
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        assert_eq!(
            (shape(&listed), warned(&listed)),
            (
                vec![(
                    TemplateGroup::Web,
                    vec!["acme-app".to_owned(), "astro-site".to_owned()]
                )],
                vec!["git.example.com/acme/hono-api".to_owned()]
            )
        );
    }
}

mod cache {
    use super::*;

    // TC: TPL-05
    #[tokio::test]
    async fn no_forge_call_when_listed_again_within_ten_minutes() {
        let world = World::new().await;
        let server = three(&world);
        let service = catalog(&world, &server);
        let first = service.list(&world.owner, &[], None).await.unwrap();
        world.log().clear();
        world
            .ports
            .clock
            .advance(TimeDuration::minutes(10) - TimeDuration::seconds(1));
        let second = service.list(&world.owner, &[], None).await.unwrap();
        assert_eq!((template_calls(&world), second), (Vec::new(), first));
    }

    // TC: TPL-05 (the cache lives in the store: a new service over the same
    // store does not ask either)
    #[tokio::test]
    async fn no_forge_call_when_another_catalog_over_the_same_store_lists() {
        let world = World::new().await;
        let server = three(&world);
        let _first = catalog(&world, &server)
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        world.log().clear();
        let _second = catalog(&world, &server)
            .list(&world.owner, &[], None)
            .await
            .unwrap();
        assert_eq!(template_calls(&world), Vec::new());
    }

    // TC: TPL-05 (expiry through the fake clock)
    #[tokio::test]
    async fn asks_the_forge_again_when_ten_minutes_passed() {
        let world = World::new().await;
        let server = three(&world);
        let service = catalog(&world, &server);
        let _first = service.list(&world.owner, &[], None).await.unwrap();
        world.log().clear();
        world
            .ports
            .clock
            .advance(TimeDuration::minutes(10) + TimeDuration::seconds(1));
        let _second = service.list(&world.owner, &[], None).await.unwrap();
        assert_eq!(template_calls(&world).len(), 1);
    }

    // TC: TPL-05 (an expired entry is replaced: the new manifest shows)
    #[tokio::test]
    async fn new_manifest_shown_when_cache_expired() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        let service = catalog(&world, &[on_forge("acme/app")]);
        let _first = service.list(&world.owner, &[], None).await.unwrap();
        world
            .ports
            .git
            .template(&repo("acme/app"), &manifest("api", "acme-app"));
        world
            .ports
            .clock
            .advance(TimeDuration::minutes(10) + TimeDuration::seconds(1));
        let second = service.list(&world.owner, &[], None).await.unwrap();
        assert_eq!(
            shape(&second),
            vec![(TemplateGroup::Api, vec!["acme-app".to_owned()])]
        );
    }

    // TC: TPL-05, TPL-04 (a repository without `liftoff.toml` is cached
    // too and still warned about)
    #[tokio::test]
    async fn no_forge_call_and_same_warning_when_missing_manifest_listed_again() {
        let world = World::new().await;
        world
            .ports
            .git
            .grant(&world.owner.login, &repo("acme/plain"), RepoRole::Read);
        let service = catalog(&world, &[on_forge("acme/plain")]);
        let _first = service.list(&world.owner, &[], None).await.unwrap();
        world.log().clear();
        let second = service.list(&world.owner, &[], None).await.unwrap();
        assert_eq!(
            (template_calls(&world), warned(&second)),
            (Vec::new(), vec!["git.example.com/acme/plain".to_owned()])
        );
    }

    // TC: TPL-05, TPL-04 (a failed read is not cached: the next list asks
    // the forge again)
    #[tokio::test]
    async fn asks_the_forge_again_when_previous_read_failed() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        let service = catalog(&world, &[on_forge("acme/app")]);
        world
            .ports
            .git
            .fail_next("list_templates", Error::internal_msg("forge down"));
        let first = service.list(&world.owner, &[], None).await.unwrap();
        world.log().clear();
        let second = service.list(&world.owner, &[], None).await.unwrap();
        assert_eq!(
            (warned(&first), template_calls(&world).len(), shape(&second)),
            (
                vec!["git.example.com/acme/app".to_owned()],
                1,
                vec![(TemplateGroup::Web, vec!["acme-app".to_owned()])]
            )
        );
    }

    // TC: TPL-05 (always the user's own connection; what one user read is never served to another)
    #[tokio::test]
    async fn asks_the_forge_as_the_other_user_when_another_user_lists() {
        let world = World::new().await;
        let server = three(&world);
        let bob = world.add_connected_user("bob").await.unwrap();
        for raw in ["acme/app", "acme/hono-api", "acme/astro-site"] {
            world
                .ports
                .git
                .grant(&bob.login, &repo(raw), RepoRole::Read);
        }
        let service = catalog(&world, &server);
        let _owners = service.list(&world.owner, &[], None).await.unwrap();
        world.log().clear();
        // A module (tenants) opens `git.templates` to members.
        world
            .ports
            .hooks
            .answer_next("authorize", HookResult::Allow);
        let _bobs = service.list(&bob, &[], None).await.unwrap();
        let logins: Vec<Login> = template_calls(&world)
            .into_iter()
            .map(|(login, _)| login)
            .collect();
        assert_eq!(logins, vec![Login::new("bob").unwrap()]);
    }

    // TC: TPL-05 (a repo without a host is cached on every host it was
    // read through: listed again, no forge is asked and the same host wins)
    #[tokio::test]
    async fn no_forge_call_when_repo_without_host_listed_again() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        second_connection(&world, "owner").await;
        let service = catalog(&world, &[bare("acme/app")]);
        let first = service.list(&world.owner, &[], None).await.unwrap();
        world.log().clear();
        let second = service.list(&world.owner, &[], None).await.unwrap();
        assert_eq!((template_calls(&world), second), (Vec::new(), first));
    }

    // TC: TPL-05 (the cache is per repository: a newly named one is read)
    #[tokio::test]
    async fn asks_only_for_the_uncached_repo_when_request_adds_one() {
        let world = World::new().await;
        let server = three(&world);
        publish(&world, "me/my-tpl", &manifest("service", "my-tpl"));
        let service = catalog(&world, &server);
        let _first = service.list(&world.owner, &[], None).await.unwrap();
        world.log().clear();
        let _second = service
            .list(&world.owner, &[on_forge("me/my-tpl")], None)
            .await
            .unwrap();
        assert_eq!(
            template_calls(&world),
            vec![(Login::new("owner").unwrap(), vec!["me/my-tpl".to_owned()])]
        );
    }
}

mod request_repos {
    use super::*;

    // TC: TPL-06
    #[tokio::test]
    async fn added_to_the_server_repos_when_request_names_them() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        publish(&world, "me/my-api", &manifest("api", "my-api"));
        let listed = catalog(&world, &[on_forge("acme/app")])
            .list(&world.owner, &[bare("me/my-api")], None)
            .await
            .unwrap();
        assert_eq!(
            shape(&listed),
            vec![
                (TemplateGroup::Web, vec!["acme-app".to_owned()]),
                (TemplateGroup::Api, vec!["my-api".to_owned()]),
            ]
        );
    }

    // TC: TPL-06 (a repo both configs name is listed once)
    #[tokio::test]
    async fn listed_once_when_request_repeats_a_server_repo() {
        let world = World::new().await;
        publish(&world, "acme/app", &manifest("web", "acme-app"));
        let listed = catalog(&world, &[on_forge("acme/app")])
            .list(&world.owner, &[on_forge("acme/app")], None)
            .await
            .unwrap();
        assert_eq!(
            shape(&listed),
            vec![(TemplateGroup::Web, vec!["acme-app".to_owned()])]
        );
    }

    // TC: TPL-06 (the request names at most `MAX_REQUESTED_TEMPLATES`
    // repositories: more is a validation error on `repos`, before any
    // forge read)
    #[tokio::test]
    async fn validation_on_repos_without_forge_call_when_request_names_too_many() {
        let world = World::new().await;
        let many: Vec<TemplateSource> = (0..=MAX_REQUESTED_TEMPLATES)
            .map(|index| on_forge(&format!("me/tpl-{index}")))
            .collect();
        let listed = catalog(&world, &[]).list(&world.owner, &many, None).await;
        assert!(
            matches!(&listed, Err(Error::Validation(error)) if error.field == "repos")
                && template_calls(&world).is_empty(),
            "{listed:?}"
        );
    }

    // TC: TPL-06 (exactly `MAX_REQUESTED_TEMPLATES` is still read)
    #[tokio::test]
    async fn listed_when_request_names_the_maximum() {
        let world = World::new().await;
        let many: Vec<TemplateSource> = (0..MAX_REQUESTED_TEMPLATES)
            .map(|index| {
                let raw = format!("me/tpl-{index}");
                publish(&world, &raw, &manifest("api", &format!("tpl-{index}")));
                on_forge(&raw)
            })
            .collect();
        let listed = catalog(&world, &[])
            .list(&world.owner, &many, None)
            .await
            .unwrap();
        let count: usize = listed
            .groups
            .iter()
            .map(|group| group.templates.len())
            .sum();
        assert_eq!((count, listed.warnings.len()), (MAX_REQUESTED_TEMPLATES, 0));
    }

    // TC: TPL-06 (only the request's repos when the server has none)
    #[tokio::test]
    async fn only_request_repos_when_server_has_none() {
        let world = World::new().await;
        publish(&world, "me/my-api", &manifest("api", "my-api"));
        let listed = catalog(&world, &[])
            .list(&world.owner, &[on_forge("me/my-api")], None)
            .await
            .unwrap();
        assert_eq!(
            shape(&listed),
            vec![(TemplateGroup::Api, vec!["my-api".to_owned()])]
        );
    }
}
